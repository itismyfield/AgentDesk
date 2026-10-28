#!/usr/bin/env python3
"""Classify which library tests need PostgreSQL, from source alone; nothing here runs cargo.

The single source of PG classification: native-OS runs plan only its non-PG tests, and Script checks
fail when part of the tree cannot be analysed.

  classify  PG test IDs with their files, minus the exemptions below; with --list also the non-PG IDs
            of an unfiltered nextest list, which native-OS runs execute without a PG service
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from functools import lru_cache
import json
import os
from pathlib import Path
import posixpath
import re
import sys
from typing import Iterable

sys.path.insert(0, str(Path(__file__).resolve().parent))
import rust_test_sources  # noqa: E402

# Classifier false positives: exact test ID -> reason. An entry the classifier no longer reports is stale.
EXEMPT = {
    "services::discord::turn_bridge::resume_pin_tests::c1_actual_postlude_resume_pin_runtime_proof":
        "#5707: passes response=None; the shared fixture never creates or connects PostgreSQL",
    "server::worker_registry::leader_takeover_tests::worker_profile_starts_local_workers_without_spawning_leader_waiters":
        "the lazy pool is never queried; leader workers are rejected and only the DB-free WsBatchFlusher starts",
}
# Findings that mean part of the tree was not analysed; omitted scope must not read as non-PG.
UNANALYZABLE = frozenset({
    "unresolved-external-test-module", "pg-reach-depth-exhausted", "test-not-in-inventory", "unresolved-self-exec",
})


_RAW_STRING_OPEN = re.compile(r'(?:r|br)(#*)"')


_CHAR_LITERAL = re.compile(r"'(?:\\.|[^'\\])'")


class StripState:
    """Cross-line state for Rust comments and string literals."""

    __slots__ = ("block_depth", "in_string", "raw_hashes")

    def __init__(self) -> None:
        self.block_depth = 0
        self.in_string = False
        self.raw_hashes: int | None = None


def strip_rust(source: str) -> str:
    """Blank Rust strings/comments while preserving offsets and newlines."""
    state = StripState()
    out: list[str] = []
    i = 0
    while i < len(source):
        if state.block_depth:
            if source.startswith("/*", i):
                state.block_depth += 1
                out.extend("  ")
                i += 2
            elif source.startswith("*/", i):
                state.block_depth -= 1
                out.extend("  ")
                i += 2
            else:
                out.append("\n" if source[i] == "\n" else " ")
                i += 1
            continue
        if state.raw_hashes is not None:
            closer = '"' + "#" * state.raw_hashes
            if source.startswith(closer, i):
                state.raw_hashes = None
                out.extend(" " * len(closer))
                i += len(closer)
            else:
                out.append("\n" if source[i] == "\n" else " ")
                i += 1
            continue
        if state.in_string:
            if source[i] == "\\" and i + 1 < len(source):
                out.extend(" \n" if source[i + 1] == "\n" else "  ")
                i += 2
            else:
                if source[i] == '"':
                    state.in_string = False
                out.append("\n" if source[i] == "\n" else " ")
                i += 1
            continue

        if source.startswith("//", i):
            end = source.find("\n", i)
            if end < 0:
                out.extend(" " * (len(source) - i))
                break
            out.extend(" " * (end - i))
            i = end
            continue
        if source.startswith("/*", i):
            state.block_depth = 1
            out.extend("  ")
            i += 2
            continue
        raw = _RAW_STRING_OPEN.match(source, i)
        if raw:
            state.raw_hashes = len(raw.group(1))
            out.extend(" " * (raw.end() - i))
            i = raw.end()
            continue
        if source[i] == '"' or source.startswith('b"', i):
            width = 2 if source[i] == "b" else 1
            state.in_string = True
            out.extend(" " * width)
            i += width
            continue
        if source[i] == "'":
            char = _CHAR_LITERAL.match(source, i)
            if char:
                out.extend(" " * (char.end() - i))
                i = char.end()
                continue
        out.append(source[i])
        i += 1
    return "".join(out)


SEEDS = (
    "TestPostgresDb", "DispatchPostgresTestDb", "PgRecoveryTestDatabase",
    "PgPool", "connect_and_migrate", "create_test_database",
    "connect_test_pool", "PostgresTestLifecycleGuard", "lock_test_lifecycle",
)


CONNECT_SEEDS = tuple(seed for seed in SEEDS if seed != "PgPool")


def _seed_pattern(seed: str) -> re.Pattern[str]:
    """Match exact type seeds and supported function-name suffix variants."""
    suffix = r"[A-Za-z0-9_]*" if seed in {
        "connect_and_migrate",
        "connect_test_pool",
        "lock_test_lifecycle",
    } else ""
    return re.compile(rf"\b{re.escape(seed)}{suffix}\b")


SEED_PATTERNS = tuple(_seed_pattern(seed) for seed in SEEDS)


CONNECT_SEED_PATTERNS = tuple(_seed_pattern(seed) for seed in CONNECT_SEEDS)


PG_REACH_MAX_DEPTH = 16


@dataclass(frozen=True)
class PgInventory:
    tests: dict[str, str]

    @property
    def files(self) -> set[str]:
        return set(self.tests.values())

    @property
    def modules(self) -> set[str]:
        return {name.rpartition("::")[0] for name in self.tests}


@dataclass(frozen=True)
class Finding:
    kind: str
    source: str
    detail: str


@dataclass(frozen=True)
class ModuleRange:
    start: int
    end: int
    name: str
    is_test: bool


_ATTR_MOD = re.compile(
    r"(?P<attrs>(?:#\s*\[[^\]]*\]\s*)+)"
    r"(?:(?:pub(?:\s*\([^)]*\))?)\s+)?"
    r"mod\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*(?P<term>[{;])",
    re.MULTILINE,
)


_MOD = re.compile(r"\bmod\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*(?P<term>[{;])")


_CFG_TEST = re.compile(r"#\s*\[\s*cfg\s*\([^\]]*\btest\b[^\]]*\)\s*\]")


_PATH_ATTR = re.compile(r'#\s*\[\s*path\s*=\s*"(?P<path>[^"]+)"\s*\]')


_ATTR_FN = re.compile(
    r"(?P<attrs>(?:#\s*\[[^\]]*\]\s*)+)"
    r"(?:(?:pub(?:\s*\([^)]*\))?)\s+)?"
    r"(?:async\s+)?(?:unsafe\s+)?fn\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)",
    re.MULTILINE,
)


_TEST_ATTR = re.compile(
    r"#\s*\[\s*(?:(?:tokio|async_std|actix_rt)::)?test\b(?:\([^\]]*\))?\s*\]"
)


_STRUCT = re.compile(r"\bstruct\s+([A-Za-z_][A-Za-z0-9_]*)\b")


_IMPL = re.compile(r"\bimpl(?:\s*<[^>{}]*>)?\s+(?:[^{}]*?\s+for\s+)?([A-Za-z_][A-Za-z0-9_]*)\b[^{};]*\{")


_FN = re.compile(r"\b(?:async\s+)?(?:unsafe\s+)?fn\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)")


_CALL = re.compile(
    r"(?<![.:])\b(?P<path>[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*)\s*!?\s*\("
)


_UFCS_CALL = re.compile(
    r"<\s*(?P<type>[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*)"
    r"\s+as\s+[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*\s*>"
    r"\s*::\s*(?P<method>[A-Za-z_][A-Za-z0-9_]*)\s*\("
)


_BARE_REFERENCE = re.compile(
    r"(?<![.:])\b(?P<name>[A-Za-z_][A-Za-z0-9_]*)\b(?!\s*::)"
)


_USE = re.compile(r"\buse\s+(?P<path>[^;]+);")


_CONST = re.compile(r"\b(?:const|static)\s+(?:mut\s+)?(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*:")


# A test that re-runs the test binary with `--exact` runs the tests it names there, so their PG need is its own.
_SELF_EXEC = re.compile(r"\bcurrent_exe\b")


_STRING_LITERAL = re.compile(r'"((?:\\.|[^"\\])*)"')


_ID_PATH = re.compile(r"[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*")


# libtest names each test thread after its test, so a child named by the current thread is the test itself.
_OWN_THREAD = re.compile(r"\bthread::current\s*\(\s*\)")


@lru_cache(maxsize=128)
def _matching_brace_cached(clean: str, opening: int) -> int:
    depth = 0
    for index in range(opening, len(clean)):
        if clean[index] == "{":
            depth += 1
        elif clean[index] == "}":
            depth -= 1
            if depth == 0:
                return index
    raise ValueError(f"unclosed brace at byte {opening}")


def _matching_brace(clean: str, opening: int, counter: list[int] | None = None) -> int:
    """Return a brace boundary while caching repeated ownership lookups."""
    if counter is not None:
        counter[0] += 1
    return _matching_brace_cached(clean, opening)


def _opening_brace(clean: str, start: int) -> int | None:
    brace = clean.find("{", start)
    semicolon = clean.find(";", start)
    if brace < 0 or (semicolon >= 0 and semicolon < brace):
        return None
    return brace


def _function_body_opening(clean: str, start: int) -> int | None:
    """Find a function body brace without mistaking const-generic braces for it."""
    angle_depth = 0
    paren_depth = 0
    bracket_depth = 0
    index = start
    while index < len(clean):
        char = clean[index]
        if char == "<":
            angle_depth += 1
        elif char == ">" and angle_depth:
            angle_depth -= 1
        elif char == "(" and angle_depth == 0:
            paren_depth += 1
        elif char == ")" and paren_depth:
            paren_depth -= 1
        elif char == "[" and angle_depth == 0 and paren_depth == 0:
            bracket_depth += 1
        elif char == "]" and bracket_depth:
            bracket_depth -= 1
        elif char == ";" and angle_depth == 0 and paren_depth == 0 and bracket_depth == 0:
            return None
        elif char == "{" and angle_depth == 0 and paren_depth == 0 and bracket_depth == 0:
            return index
        index += 1
    return None


def _edge_boundary(clean: str, opening: int, counter: list[int] | None = None) -> str:
    """Return exactly one caller body, excluding adjacent functions/items."""
    return clean[opening:_matching_brace(clean, opening, counter) + 1]


def _mask_nested_items(
    body: str, counter: list[int] | None = None
) -> str:
    """Blank block-local functions/impls so their seeds do not taint a test."""
    spans: list[tuple[int, int]] = []
    for pattern in (_FN, _IMPL):
        for match in pattern.finditer(body):
            opening = match.end() - 1 if pattern is _IMPL else _function_body_opening(body, match.end())
            if opening is None:
                continue
            spans.append((match.start(), _matching_brace(body, opening, counter) + 1))
    masked = list(body)
    for start, end in spans:
        masked[start:end] = ("\n" if char == "\n" else " " for char in masked[start:end])
    return "".join(masked)


def _impl_method_bodies(body: str, counter: list[int] | None = None) -> list[str]:
    """Collect method bodies, retaining calls but masking block-local items."""
    methods: list[str] = []
    end = 0
    for match in _FN.finditer(body):
        if match.start() < end:
            continue
        opening = _function_body_opening(body, match.end())
        if opening is None:
            continue
        end = _matching_brace(body, opening, counter) + 1
        methods.append(_mask_nested_items(body[opening:end], counter))
    return methods


def _argument_spans(clean: str, opening: int) -> list[tuple[int, int]]:
    """Split delimited arguments without splitting nested expressions."""
    closers = {"(": ")", "[": "]", "{": "}"}
    stack, spans, start = [closers[clean[opening]]], [], opening + 1
    for offset in range(start, len(clean)):
        char = clean[offset]
        if char in closers:
            stack.append(closers[char])
        elif char == stack[-1]:
            stack.pop()
            if not stack:
                return [*spans, (start, offset)]
        elif char == "," and len(stack) == 1:
            spans.append((start, offset))
            start = offset + 1
    return []


def _parameters(clean: str, start: int, body: int) -> tuple[str | None, ...]:
    depth = 0
    for offset in range(start, body):
        char = clean[offset]
        if char == "<":
            depth += 1
        elif char == ">" and depth:
            depth -= 1
        elif char == "(" and not depth:
            params = []
            for left, right in _argument_spans(clean, offset):
                if clean[left:right].strip():
                    name = re.match(r"\s*(?:mut\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*:", clean[left:right])
                    params.append(name[1] if name else None)
            return tuple(params)
    return ()


def _exact_targets(clean: str, raw: str) -> list[tuple[str, int]]:
    targets = []
    for call in re.finditer(r"\.\s*args\s*\(\s*\[", clean):
        args = _argument_spans(clean, call.end() - 1)
        for index, (left, right) in enumerate(args):
            if raw[left:right].strip() != '"--exact"':
                continue
            candidates = [i for i in (index + 1, index - 1) if 0 <= i < len(args)]
            target = next((args[i] for i in candidates if not raw[slice(*args[i])].strip().startswith('"--')), None)
            targets.append((raw[slice(*target)], left) if target else ("", left))
    return targets


def _transitive_closure(
    referenced: set[tuple[tuple[str, ...], str, str]],
    seeded: set[tuple[tuple[str, ...], str, str]],
    edges: dict[
        tuple[tuple[str, ...], str, str],
        set[tuple[tuple[str, ...], str, str]],
    ],
    max_depth: int = PG_REACH_MAX_DEPTH,
) -> bool | None:
    """Whether a module-scoped helper reaches a PG seed within ``max_depth``.

    ``None`` means call paths were left unexplored at the depth limit, which
    is not evidence that the test needs no PG."""
    frontier = set(referenced)
    seen: set[tuple[tuple[str, ...], str, str]] = set()
    for _depth in range(max_depth):
        if frontier & seeded:
            return True
        seen.update(frontier)
        frontier = {
            target
            for caller in frontier
            for target in edges.get(caller, set())
            if target not in seen
        }
    if frontier & seeded:
        return True
    return None if frontier else False


def _module_ranges(
    source: str, clean: str, counter: list[int] | None = None
) -> list[ModuleRange]:
    attrs = {match.start("name"): match.group("attrs") for match in _ATTR_MOD.finditer(clean)}
    ranges: list[ModuleRange] = []
    for match in _MOD.finditer(clean):
        if match.group("term") != "{":
            continue
        opening = match.end() - 1
        ranges.append(ModuleRange(
            opening, _matching_brace(clean, opening, counter), match.group("name"),
            bool(_CFG_TEST.search(attrs.get(match.start("name"), ""))),
        ))
    return ranges


def _scope_at(offset: int, ranges: Iterable[ModuleRange]) -> tuple[str, ...]:
    containing = [item for item in ranges if item.start < offset < item.end]
    return tuple(item.name for item in sorted(containing, key=lambda item: item.start))


def _inside_test_region(offset: int, ranges: Iterable[ModuleRange], external: bool) -> bool:
    return external or any(item.is_test and item.start < offset < item.end for item in ranges)


def _external_test_files(repo_root: Path, counter: list[int] | None = None, findings: list[Finding] | None = None) -> set[Path]:
    src_root = (repo_root / "src").resolve()
    declarations: dict[Path, list[tuple[Path | None, bool, Finding]]] = {}
    targets: set[Path] = set()
    for path in sorted(src_root.rglob("*.rs")):
        source = path.read_text("utf-8")
        clean = strip_rust(source)
        ranges = _module_ranges(source, clean, counter)
        attributes = {match.start("name"): match for match in _ATTR_MOD.finditer(clean)}
        children = declarations.setdefault(path.resolve(), [])
        for match in _MOD.finditer(clean):
            if match.group("term") != ";":
                continue
            attributed = attributes.get(match.start("name"))
            attrs = attributed.group("attrs") if attributed else ""
            is_test = bool(_CFG_TEST.search(attrs)) or _inside_test_region(match.start(), ranges, False)
            path_attr = re.search(r"#\s*\[\s*path\s*=", attrs)
            redirect = _PATH_ATTR.match(source, attributed.start("attrs") + path_attr.start(), attributed.end("attrs")) if path_attr else None
            parents = _scope_at(match.start(), ranges)
            if redirect:
                base = path.with_suffix("") if parents and path.name not in {"mod.rs", "lib.rs", "main.rs"} else path.parent
                candidates = (base.joinpath(*parents, redirect.group("path")),)
            else:
                base = path.parent if path.name in {"mod.rs", "lib.rs", "main.rs"} else path.with_suffix("")
                base = base.joinpath(*parents)
                name = match.group("name")
                candidates = (base / f"{name}.rs", base / name / "mod.rs")
            target = next((candidate.resolve() for candidate in candidates if candidate.is_file()), None)
            if target and not target.is_relative_to(src_root):
                target = None
            location = attributed.start() if attributed else match.start()
            line = source.count("\n", 0, location) + 1
            tried = ", ".join(Path(os.path.relpath(candidate, repo_root)).as_posix() for candidate in candidates)
            finding = Finding("unresolved-external-test-module", f"{path.relative_to(repo_root).as_posix()}:{line}", f"mod {match.group('name')}; did not resolve inside src; tried: {tried}")
            children.append((target, is_test, finding))
            if is_test:
                if target:
                    targets.add(target)
                elif findings is not None:
                    findings.append(finding)
    pending = list(targets)
    while pending:
        for target, is_test, finding in declarations.get(pending.pop(), []):
            if target and target not in targets:
                targets.add(target)
                pending.append(target)
            elif target is None and not is_test and findings is not None:
                findings.append(finding)
    return targets


def discover_pg_inventory(
    repo_root: Path, findings: list[Finding] | None = None
) -> PgInventory:
    """Discover PG tests with a deliberately bounded Rust call model.

    Supported indirect references are bare/free-function calls, fully-qualified
    free functions, single- and multi-segment associated calls, and UFCS calls of
    the form ``<Type as Trait>::method()``. Names resolve through nested ``use …::*``
    globs and ``use`` re-exports, and a path starting at a child module is relative to it.
    Test and helper bodies use the same reference extractor and both mask
    block-local functions and impls. A test that re-runs the test binary
    (``current_exe`` with ``--exact``) needs PG when a test named by a string
    literal it reaches does; naming no resolvable test is an unanalysable finding.

    Self-exec evidence follows named associated and Self:: calls with call-local
    string arguments; unrelated impl methods lend no raw literals.

    Out of scope are block-local call graphs, value-receiver dispatch and PG method
    dispatch (PG reach treats an impl as one type-level item), plus UFCS
    receivers containing generic, tuple, reference, or ``dyn`` type syntax. A
    real Rust parser would be required to cover those forms without broad false
    positives. Only files the lib root reaches (mod, ``#[path]``, ``include!``)
    take part, under the module path that walk gives them. ``#[ignore]`` tests
    remain selection/debt because their execution belongs to the ignore ledger.
    """
    repo_root = repo_root.resolve()
    _matching_brace_cached.cache_clear()
    src_root = (repo_root / "src").resolve()
    brace_counter = [0]
    external_tests = _external_test_files(repo_root, brace_counter, findings)
    inventory = rust_test_sources.lib_inventory(repo_root)
    declared_tests = set(inventory.tests)
    item_bodies: dict[tuple[tuple[str, ...], str, str], str] = {}
    impl_methods: dict[tuple[tuple[str, ...], str, str], list[str]] = {}
    # Unstripped item text and file, read only for the test IDs a self-exec names.
    item_raw: dict[tuple[tuple[str, ...], str, str], list[tuple[str, str]]] = {}
    raw_bodies: dict[tuple[tuple[str, ...], str, str], list[str]] = {}
    raw_parameters: dict[tuple[tuple[str, ...], str, str], list[tuple[str | None, ...]]] = {}
    seeded: set[tuple[tuple[str, ...], str, str]] = set()
    records: list[tuple[str, str, tuple[str, ...], str, str, int]] = []
    use_aliases: dict[tuple[str, ...], dict[str, tuple[str, ...]]] = {}
    wildcard_uses: dict[tuple[str, ...], list[tuple[str, ...]]] = {}
    # Every module the walk reaches; a path's first segment naming a child of the current module is relative.
    known_modules: set[tuple[str, ...]] = {base for bases in inventory.modules.values() for base in bases}
    module_uses: list[tuple[tuple[str, ...], str]] = []

    def absolute_path(module: tuple[str, ...], raw: str) -> tuple[str, ...]:
        parts = tuple(part for part in raw.strip().split("::") if part)
        if not parts:
            return ()
        if parts[0] == "crate":
            return parts[1:]
        if parts[0] == "self":
            return (*module, *parts[1:])
        if parts[0] == "super":
            drop = 0
            while drop < len(parts) and parts[drop] == "super":
                drop += 1
            return (*module[: max(0, len(module) - drop)], *parts[drop:])
        if (*module, parts[0]) in known_modules:
            return (*module, *parts)
        return parts

    def import_bindings(module: tuple[str, ...], raw: str):
        brace = re.fullmatch(r"(?P<base>.+)::\{(?P<items>[^{}]+)\}", raw.strip())
        expanded = [f"{brace.group('base')}::{item.strip()}" for item in brace.group("items").split(",") if item.strip()] if brace else [raw.strip()]
        for entry in expanded:
            target_text, separator, alias = entry.partition(" as ")
            target = absolute_path(module, target_text)
            if target:
                yield alias.strip() if separator else target[-1], target

    def nested(offset: int, ranges: Iterable[tuple[int, int]]) -> bool:
        return any(start < offset < end for start, end in ranges)

    def seed_match(body: str) -> bool:
        # Keep the cheap prefilter confined to seed discovery. Edge and test-body
        # extraction below always inspect their complete brace-bounded bodies.
        return any(seed in body for seed in CONNECT_SEEDS) and any(
            pattern.search(body) for pattern in CONNECT_SEED_PATTERNS
        )

    # Files the lib does not compile lend nothing; a file reached as several modules is read once per module.
    reached = sorted((path, base) for path in src_root.rglob("*.rs") for base in inventory.modules.get(path.resolve(), ()))
    for path, reached_base in reached:
        source = path.read_text("utf-8")
        clean = strip_rust(source)
        ranges = _module_ranges(source, clean, brace_counter)
        external = path.resolve() in external_tests
        rel = path.relative_to(repo_root).as_posix()

        def logical_at(*tail: str, reached_base=reached_base) -> tuple[str, ...]:
            return (*reached_base, *tail)

        known_modules.update(logical_at(*_scope_at(item.start + 1, ranges)) for item in ranges)

        fn_ranges: list[tuple[int, int, re.Match[str], int]] = []
        for match in _FN.finditer(clean):
            opening = _function_body_opening(clean, match.end())
            if opening is not None:
                fn_ranges.append(
                    (match.start(), _matching_brace(clean, opening, brace_counter), match, opening)
                )
        impl_ranges: list[tuple[int, int]] = []
        for match in _IMPL.finditer(clean):
            opening = match.end() - 1
            impl_ranges.append(
                (match.start(), _matching_brace(clean, opening, brace_counter))
            )

        test_name_offsets = {
            match.start("name")
            for match in _ATTR_FN.finditer(clean)
            if _TEST_ATTR.search(match.group("attrs"))
        }

        for use in _USE.finditer(clean):
            if nested(use.start(), ((start, end) for start, end, _, _ in fn_ranges)) or nested(
                use.start(), impl_ranges
            ):
                continue
            module_uses.append((logical_at(*_scope_at(use.start(), ranges)), use.group("path")))

        all_fn_ranges = [(start, end) for start, end, _, _ in fn_ranges]
        for start, end, match, opening in fn_ranges:
            if match.start("name") in test_name_offsets:
                continue
            other_fns = ((outer_start, outer_end) for outer_start, outer_end in all_fn_ranges if outer_start != start)
            if nested(start, other_fns) or nested(start, impl_ranges):
                continue
            if not _inside_test_region(start, ranges, external):
                continue
            module = logical_at(*_scope_at(start, ranges))
            key = (module, match.group("name"), "fn")
            body = _edge_boundary(clean, opening, brace_counter)
            item_bodies[key] = body
            item_raw[key] = [(source[opening:opening + len(body)], rel)]
            raw_bodies[key] = [_mask_nested_items(body, brace_counter)]
            raw_parameters[key] = [_parameters(clean, match.end(), opening)]
            if seed_match(body):
                seeded.add(key)

        for match in _STRUCT.finditer(clean):
            if not _inside_test_region(match.start(), ranges, external):
                continue
            if nested(match.start(), all_fn_ranges) or nested(match.start(), impl_ranges):
                continue
            opening = _function_body_opening(clean, match.end())
            if opening is None:
                continue
            body = _edge_boundary(clean, opening, brace_counter)
            module = logical_at(*_scope_at(match.start(), ranges))
            key = (module, match.group(1), "struct")
            item_bodies[key] = body
            if seed_match(body):
                seeded.add(key)
        for match in _IMPL.finditer(clean):
            if not _inside_test_region(match.start(), ranges, external):
                continue
            if nested(match.start(), all_fn_ranges):
                continue
            opening = match.end() - 1
            body = _edge_boundary(clean, opening, brace_counter)
            module = logical_at(*_scope_at(match.start(), ranges))
            key = (module, match.group(1), "impl")
            methods = _impl_method_bodies(body, brace_counter)
            impl_methods.setdefault(key, []).extend(methods)
            item_bodies[key] = "\n".join(impl_methods[key])
            for method_start, method_end, method, method_opening in fn_ranges:
                if not opening < method_start < opening + len(body):
                    continue
                if nested(method_start, ((a, b) for a, b in all_fn_ranges if a != method_start)):
                    continue
                method_key = (module, match.group(1), "method:" + method.group("name"))
                method_body = clean[method_opening:method_end + 1]
                item_raw.setdefault(method_key, []).append((source[method_opening:method_end + 1], rel))
                raw_bodies.setdefault(method_key, []).append(_mask_nested_items(method_body, brace_counter))
                raw_parameters.setdefault(method_key, []).append(_parameters(clean, method.end(), method_opening))
            if any(seed_match(method) for method in methods):
                seeded.add(key)

        for match in _CONST.finditer(clean):
            if not _inside_test_region(match.start(), ranges, external):
                continue
            if nested(match.start(), all_fn_ranges) or nested(match.start(), impl_ranges):
                continue
            end = clean.find(";", clean.find("=", match.end()))
            if end < 0:
                continue
            key = (logical_at(*_scope_at(match.start(), ranges)), match.group("name"), "const")
            item_bodies[key] = clean[match.start():end + 1]
            item_raw[key] = [(source[match.start():end + 1], rel)]
            raw_bodies[key] = [item_bodies[key]]
            raw_parameters[key] = [()]

        for match in _ATTR_FN.finditer(clean):
            if not _TEST_ATTR.search(match.group("attrs")):
                continue
            if not _inside_test_region(match.start(), ranges, external):
                continue
            opening = _function_body_opening(clean, match.end())
            if opening is None:
                continue
            body = _edge_boundary(clean, opening, brace_counter)
            logical = logical_at(*_scope_at(match.start(), ranges), match.group("name"))
            name = "::".join(logical)
            # Reaching an external file through a cfg(test) declaration already
            # proves that its direct tests are part of the library test target.
            if name in declared_tests or external:
                line = clean.count("\n", 0, match.start("name")) + 1
                records.append((name, rel, logical[:-1], body, source[opening:opening + len(body)], line))
            elif findings is not None:
                # A recognised test in a file the lib walk reached must be listed, else it would run as non-PG.
                findings.append(Finding(
                    "test-not-in-inventory", name,
                    f"{path.relative_to(repo_root).as_posix()}: a test the root inventory does not list"))

    # Bound once every module is known, so a relative `use child::item` resolves under its module.
    for module, raw in module_uses:
        for binding, target in import_bindings(module, raw):
            if binding == "*":
                wildcard_uses.setdefault(module, []).append(target[:-1])
            else:
                use_aliases.setdefault(module, {})[binding] = target

    by_path: dict[tuple[tuple[str, ...], str], set[tuple[tuple[str, ...], str, str]]] = {}
    for key in item_bodies:
        module, name, _ = key
        by_path.setdefault((module, name), set()).add(key)

    glob_closure: dict[tuple[str, ...], list[tuple[str, ...]]] = {}

    def glob_bases(module: tuple[str, ...], local_wildcards=()) -> list[tuple[str, ...]]:
        """Modules a bare name can come from through `use …::*`, followed through the globs they hold."""
        if not local_wildcards and module in glob_closure:
            return glob_closure[module]
        seen, pending = [], [*wildcard_uses.get(module, []), *local_wildcards]
        while pending:
            base = pending.pop(0)
            if base not in seen:
                seen.append(base)
                pending.extend(wildcard_uses.get(base, []))
        if not local_wildcards:
            glob_closure[module] = seen
        return seen

    def lookup(target: tuple[str, ...], hops: int = 8) -> set[tuple[tuple[str, ...], str, str]]:
        """Items at a path, following `use` re-exports of the module the path names."""
        found = set(by_path.get((target[:-1], target[-1]), set()))
        reexport = use_aliases.get(target[:-1], {}).get(target[-1])
        if not found and hops and reexport and reexport != target:
            found = lookup(reexport, hops - 1)
        return found

    def resolve(module: tuple[str, ...], raw: str, local_aliases=None, local_wildcards=()) -> set[tuple[tuple[str, ...], str, str]]:
        bindings = {**use_aliases.get(module, {}), **(local_aliases or {})}
        if "::" in raw:
            parts = tuple(part for part in raw.split("::") if part)
            alias = bindings.get(parts[0]) if parts else None
            target = (*alias, *parts[1:]) if alias else absolute_path(module, raw)
            return lookup(target) if target else set()
        if raw in (local_aliases or {}):
            return lookup(local_aliases[raw])
        targets = set(by_path.get((module, raw), set()))
        alias = bindings.get(raw)
        if alias:
            targets.update(lookup(alias))
        for base in glob_bases(module, local_wildcards):
            targets.update(lookup((*base, raw)))
        return targets

    def resolve_call(
        module: tuple[str, ...], raw: str, local_aliases=None, local_wildcards=(), methods=False
    ) -> set[tuple[tuple[str, ...], str, str]]:
        """Resolve a free call or one whole associated-call receiver path.

        ``crate::support::h()`` resolves to the fully-qualified free function,
        while ``a::b::TestDatabase::create()`` falls back to resolving the whole
        ``a::b::TestDatabase`` receiver. Individual qualifier segments are never
        reinterpreted in the caller's module.
        """
        targets = set(resolve(module, raw, local_aliases, local_wildcards))
        receiver, separator, method = raw.rpartition("::")
        if separator and not targets:
            owners = resolve(module, receiver, local_aliases, local_wildcards)
            if methods:
                targets.update((owner[0], owner[1], "method:" + method) for owner in owners
                               if (owner[0], owner[1], "method:" + method) in item_raw)
            else:
                targets.update(owners)
        return targets

    def body_references(
        module: tuple[str, ...], body: str, *, methods=False, self_type=None, call_sites=None, context=None
    ) -> set[tuple[tuple[str, ...], str, str]]:
        """Resolve body-local imports without leaking aliases into sibling tests."""
        local_aliases = {"Self": (*module, self_type)} if self_type else {}
        local_wildcards = []
        visible = list(body)
        for use in _USE.finditer(body if context is None else context):
            for binding, target in import_bindings(module, use.group("path")):
                if binding == "*":
                    local_wildcards.append(target[:-1])
                else:
                    local_aliases[binding] = target
            if context is None:
                visible[use.start():use.end()] = " " * (use.end() - use.start())
        body = "".join(visible)
        calls = list(_CALL.finditer(body))
        ufcs_calls = list(_UFCS_CALL.finditer(body))
        occupied = [match.span() for match in (*calls, *ufcs_calls)]
        targets = set()
        for call in (*calls, *ufcs_calls):
            if call.re is _UFCS_CALL and not methods:
                found = resolve(module, call.group("type"), local_aliases, local_wildcards)
            else:
                path = call.group("path") if call.re is _CALL else call.group("type") + "::" + call.group("method")
                found = resolve_call(module, path, local_aliases, local_wildcards, methods)
            targets.update(found)
            if call_sites is not None:
                call_sites.extend((target, call.end() - 1) for target in found if target in item_raw)
        targets.update(
            target
            for mention in _BARE_REFERENCE.finditer(body)
            if not any(start <= mention.start() < end for start, end in occupied)
            for target in resolve(module, mention.group("name"), local_aliases, local_wildcards)
        )
        return targets

    edges: dict[tuple[tuple[str, ...], str, str], set[tuple[tuple[str, ...], str, str]]] = {}
    for key, body in item_bodies.items():
        if key[2] not in {"fn", "impl"}:
            continue
        bodies = impl_methods[key] if key[2] == "impl" else [_mask_nested_items(body, brace_counter)]
        edges[key] = {target for caller_body in bodies for target in body_references(key[0], caller_body)}

    raw_edges, raw_calls = {}, {}
    for key, bodies in raw_bodies.items():
        raw_edges[key] = set()
        for index, body in enumerate(bodies):
            sites = []
            raw_edges[key].update(body_references(key[0], body, methods=True,
                self_type=key[1] if key[2].startswith("method:") else None, call_sites=sites))
            raw_calls[key, index] = sites

    def reached_items(referenced, graph=edges) -> set[tuple[tuple[str, ...], str, str]]:
        seen, frontier = set(), set(referenced)
        for _depth in range(PG_REACH_MAX_DEPTH):
            frontier -= seen
            if not frontier:
                break
            seen |= frontier
            frontier = {target for caller in frontier for target in graph.get(caller, set())}
        return seen

    files_of: dict[str, set[str]] = {}
    by_last: dict[str, set[str]] = {}
    for test_id, sites in inventory.tests.items():
        files_of[test_id] = {posixpath.normpath(site.rpartition(":")[0].replace("\\", "/")) for site, _cfg in sites}
        by_last.setdefault(test_id.rpartition("::")[2], set()).add(test_id)

    def named_tests(literal: str, files: set[str]) -> set[str]:
        """Test IDs a string names: the whole ID, the ID under a crate prefix, or its tail in a nearby file."""
        parts = literal.split("::")
        for start in range(min(2, len(parts))):
            candidate = "::".join(parts[start:])
            if candidate in declared_tests:
                return {candidate}
        tail = "::".join(parts[-2:]) if len(parts) > 1 else parts[-1]
        hits = {test_id for test_id in by_last.get(parts[-1], ()) if test_id == tail or test_id.endswith("::" + tail)}
        return hits if len(parts) > 1 else {test_id for test_id in hits if files_of.get(test_id, set()) & files}

    method_exec = {key for key, bodies in raw_bodies.items() if key[2].startswith("method:")
                     and any(_SELF_EXEC.search(body) and '"--exact"' in item_raw[key][i][0]
                             for i, body in enumerate(bodies))}
    method_needed = set(method_exec)
    value_calls = {key: re.compile(rf"\.\s*{re.escape(key[2].removeprefix('method:'))}\s*\(") for key in method_exec}
    reverse = {}
    for caller, targets in raw_edges.items():
        for target in targets:
            reverse.setdefault(target, set()).add(caller)
    pending = list(method_needed)
    while pending:
        for caller in reverse.get(pending.pop(), set()) - method_needed:
            method_needed.add(caller)
            pending.append(caller)

    def literal_tests(text: str, files: set[str]) -> set[str]:
        return {test for literal in _STRING_LITERAL.findall(text)
                for token in _ID_PATH.findall(literal) for test in named_tests(token, files)}

    def unresolved_method_target(module, body, raw, root_sites, root_refs, reached, files, test_name) -> bool:
        """Prove each reached method's exact argument in its own call context."""
        owners = {(key[0], key[1]) for key in root_refs | reached}
        bodies = [body, *(text for key in reached for text in raw_bodies.get(key, ()))]
        # Value-receiver dispatch is not modelled; possible self-exec must remain unresolved.
        if any(owner[:2] in owners and any(pattern.search(text) for text in bodies)
               for owner, pattern in value_calls.items()):
            return True
        if not root_refs & method_needed:
            return False

        def node(key, index):
            return (key[0], raw_bodies[key][index], item_raw[key][index][0]) if key else (module, body, raw)

        def argument_tests(expression, key, index, values, position, seen=frozenset()):
            identity = (key, index, expression, position)
            if identity in seen or len(seen) >= PG_REACH_MAX_DEPTH:
                return set()
            seen = seen | {identity}
            scope, clean, source = node(key, index)
            locals_ = {}
            for binding in re.finditer(r"\b(?:let|const|static)\s+(?:mut\s+)?(\w+)\b[^;={}]*=", clean[:position]):
                end = clean.find(";", binding.end(), position)
                if end >= 0:
                    locals_[binding[1]] = (source[binding.end():end], binding.start())

            def value(name):
                if name in locals_:
                    text, before = locals_[name]
                    return argument_tests(text, key, index, values, before, seen)
                return values.get(name, set())

            simple = re.fullmatch(r"\s*&?\s*([A-Za-z_][A-Za-z0-9_]*)\s*", expression)
            if simple and (simple[1] in locals_ or simple[1] in values):
                return value(simple[1])
            hits = literal_tests(expression, files)
            for mention in _BARE_REFERENCE.finditer(strip_rust(expression)):
                if mention["name"] in locals_ or mention["name"] in values:
                    hits.update(value(mention["name"]))
            refs = body_references(scope, strip_rust(expression), methods=True, context=clean,
                self_type=key[1] if key and key[2].startswith("method:") else None)
            texts = [expression, *(text for target in reached_items(refs, raw_edges)
                                   for text, _ in item_raw.get(target, ()))]
            for text in texts:
                hits.update(literal_tests(text, files))
                if _OWN_THREAD.search(strip_rust(text)):
                    hits.add(test_name)
            return hits

        pending, seen = [(None, 0, {}, 0)], set()
        while pending:
            key, index, values, depth = pending.pop()
            identity = (key, index, tuple(sorted((p, tuple(sorted(v))) for p, v in values.items())))
            if identity in seen:
                continue
            seen.add(identity)
            scope, clean, source = node(key, index)
            if key and key[2].startswith("method:") and _SELF_EXEC.search(clean) and '"--exact"' in source:
                exact = _exact_targets(clean, source)
                if not exact or any(not argument_tests(expr, key, index, values, pos) for expr, pos in exact):
                    return True
            sites = raw_calls[key, index] if key else root_sites
            refs = raw_edges.get(key, set()) if key else root_refs
            if depth >= PG_REACH_MAX_DEPTH and refs & method_needed:
                return True
            called = set()
            for target, opening in sites:
                called.add(target)
                if target not in method_needed:
                    continue
                args = [argument_tests(source[left:right], key, index, values, opening)
                        for left, right in _argument_spans(clean, opening)]
                for child, params in enumerate(raw_parameters[target]):
                    bound = {param: args[i] if i < len(args) else set()
                             for i, param in enumerate(params) if param is not None}
                    pending.append((target, child, bound, depth + 1))
            for target in (refs - called) & method_needed:
                for child in range(len(item_raw.get(target, ()))):
                    pending.append((target, child, {}, depth + 1))
        return False

    tests: dict[str, str] = {}
    children: dict[str, set[str]] = {}
    for name, path, module, body, raw, line in records:
        visible_body = _mask_nested_items(body, brace_counter)
        direct = any(pattern.search(visible_body) for pattern in SEED_PATTERNS)
        referenced = body_references(module, visible_body)
        indirect = _transitive_closure(referenced, seeded, edges)
        sites = []
        raw_refs = body_references(module, visible_body, methods=True, call_sites=sites)
        reached = reached_items(raw_refs, raw_edges)
        reach = [text for key in reached for text in item_raw.get(key, ())]
        texts = [(raw, path), *reach]
        files = {file for _, file in texts}
        unresolved = unresolved_method_target(module, visible_body, raw, sites, raw_refs, reached, files, name)
        if unresolved or _SELF_EXEC.search(visible_body) or any(_SELF_EXEC.search(strip_rust(text)) for text, _ in reach):
            if unresolved or any('"--exact"' in text for text, _ in texts):
                named = {test_id for text, _ in texts for test_id in literal_tests(text, files)}
                if any(_OWN_THREAD.search(strip_rust(text)) for text, _ in texts):
                    named.add(name)
                if named:
                    children[name] = named - {name}
                    if findings is not None:
                        findings.append(Finding("self-exec", f"{path}:{line}", " ".join([name, *sorted(children[name])])))
                if findings is not None and (not named or unresolved):
                    findings.append(Finding(
                        "unresolved-self-exec", f"{path}:{line}",
                        f"{name} re-runs the test binary with --exact but a child target cannot be resolved"))
        if direct or indirect:
            tests[name] = path
        elif indirect is None and findings is not None:
            findings.append(Finding(
                "pg-reach-depth-exhausted",
                name,
                f"{path}: helper calls continue past the call-depth limit, so "
                "whether this test needs PG is undecided",
            ))
    # A self-exec parent needs PG when a test it runs does; repeat until stable so cycles settle.
    paths = {name: path for name, path, *_rest in records}
    changed = True
    while changed:
        changed = False
        for parent, named in children.items():
            if parent not in tests and named & set(tests):
                tests[parent] = paths[parent]
                changed = True
    cache_info = _matching_brace_cached.cache_info()
    if findings is not None:
        findings.append(Finding(
            "operation-counter",
            "discover_pg_inventory",
            f"_matching_brace calls={brace_counter[0]} "
            f"cache_hits={cache_info.hits} cache_misses={cache_info.misses}",
        ))
    _matching_brace_cached.cache_clear()
    return PgInventory(tests)


def classify(repo_root: Path, exempt: dict[str, str] | None = None) -> tuple[dict[str, str], list[str]]:
    """(active PG test -> file, problems); problems make the classification unusable."""
    exempt = EXEMPT if exempt is None else exempt
    findings: list[Finding] = []
    inventory = discover_pg_inventory(repo_root, findings)
    problems = [f"{f.kind}: {f.source}: {f.detail}" for f in findings if f.kind in UNANALYZABLE]
    problems += [f"stale exemption: {name} is no longer classified as PG"
                 for name in sorted(set(exempt) - set(inventory.tests))]
    return {name: path for name, path in inventory.tests.items() if name not in exempt}, problems


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[1])
    parser.add_argument("--repo-root", type=Path, default=Path(__file__).resolve().parents[2])
    sub = parser.add_subparsers(dest="command", required=True)
    c = sub.add_parser("classify")
    c.add_argument("--json-out", required=True)
    c.add_argument("--list", help="unfiltered `cargo nextest list --message-format json` to split")
    args = parser.parse_args(argv)
    pg, problems = classify(args.repo_root.resolve())
    result = {"pg": dict(sorted(pg.items())), "exempt": dict(sorted(EXEMPT.items())), "problems": problems}
    if args.list:
        try:
            listed = rust_test_sources.listed_lib_tests(args.list)
        except (OSError, ValueError) as exc:
            problems.append(f"nextest list: {exc}")
        else:
            result["non_pg"] = sorted(listed - set(pg))
    Path(args.json_out).write_text(json.dumps(result, indent=1) + "\n")
    for line in problems:
        print(f"ERROR {line}", file=sys.stderr)
    print(f"{len(pg)} PG tests; {len(problems)} problem(s)")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
