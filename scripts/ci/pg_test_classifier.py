#!/usr/bin/env python3
"""Classify which library tests need PostgreSQL, from source alone; nothing here runs cargo.

A copy of the classifier in scripts/check_pg_test_lane_membership.py and the Rust helpers it borrows
from scripts/check_test_lane_coverage.py; it becomes the single source when slice D deletes those files.

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
UNANALYZABLE = frozenset({"unresolved-external-test-module", "pg-reach-depth-exhausted", "test-not-in-inventory"})


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


def file_module_path(src_root: Path, path: Path) -> tuple[str, ...]:
    """Return a source file's conventional physical module path."""
    rel = path.relative_to(src_root)
    if rel.name == "lib.rs":
        return ()
    if rel.name == "mod.rs":
        return rel.parent.parts
    return (*rel.parent.parts, rel.stem)


def _module_records(
    source: str, base: tuple[str, ...]
) -> tuple[
    set[str],
    dict[str, set[str]],
    list[tuple[tuple[str, ...], str, tuple[str, ...]]],
]:
    """Return test modules/functions and path aliases from one source file.

    Alias records carry the declaration's inline-parent stack separately. Rust
    resolves a ``#[path]`` inside ``mod outer { ... }`` relative to a physical
    ``outer/`` directory even though the source file itself is the parent file.
    """
    clean = strip_rust(source)
    attributes: dict[int, str] = {}
    for match in _ATTR_MOD.finditer(clean):
        attributes[match.start("name")] = source[
            match.start("attrs") : match.end("attrs")
        ]

    declarations: list[tuple[int, int, str, str, str]] = []
    for match in _MOD.finditer(clean):
        attrs = attributes.get(match.start("name"), "")
        declarations.append(
            (match.start(), match.end(), match.group("name"), match.group("term"), attrs)
        )

    test_functions = {
        match.start("name"): match.group("name")
        for match in _ATTR_FN.finditer(clean)
        if _TEST_ATTR.search(source[match.start("attrs") : match.end("attrs")])
    }
    events = sorted(
        [(start, "mod", (end, name, term, attrs)) for start, end, name, term, attrs in declarations]
        + [(start, "test_fn", name) for start, name in test_functions.items()],
        key=lambda event: event[0],
    )

    modules: set[str] = set()
    tests_by_module: dict[str, set[str]] = {}
    aliases: list[tuple[tuple[str, ...], str, tuple[str, ...]]] = []
    inline_stack: list[tuple[int, str, bool]] = []
    depth = 0
    cursor = 0
    for offset, kind, payload in events:
        between = clean[cursor:offset]
        for brace in re.finditer(r"[{}]", between):
            if brace.group() == "{":
                depth += 1
            else:
                depth -= 1
                while inline_stack and inline_stack[-1][0] > depth:
                    inline_stack.pop()

        if kind == "test_fn":
            test_module = next(
                (
                    index
                    for index in range(len(inline_stack) - 1, -1, -1)
                    if inline_stack[index][2]
                ),
                None,
            )
            if test_module is not None:
                module = "::".join(
                    (*base, *(item[1] for item in inline_stack[: test_module + 1]))
                )
                full_name = "::".join(
                    (*base, *(item[1] for item in inline_stack), str(payload))
                )
                tests_by_module.setdefault(module, set()).add(full_name)
            cursor = offset
            continue

        end, name, term, attrs = payload
        parent_names = tuple(item[1] for item in inline_stack)
        logical = (*base, *parent_names, name)
        is_test_module = bool(_CFG_TEST.search(attrs))
        if is_test_module:
            modules.add("::".join(logical))
            tests_by_module.setdefault("::".join(logical), set())
        path_attr = _PATH_ATTR.search(attrs)
        if path_attr and term == ";":
            aliases.append((logical, path_attr.group("path"), parent_names))

        if term == "{":
            depth += 1
            inline_stack.append((depth, name, is_test_module))
        cursor = end
    return modules, tests_by_module, aliases


def _normalize_alias_path(
    path: tuple[str, ...], aliases: dict[tuple[str, ...], tuple[str, ...]]
) -> tuple[str, ...]:
    """Replace the longest physical prefix until the logical path is stable."""
    seen: set[tuple[str, ...]] = set()
    current = path
    while current not in seen:
        seen.add(current)
        replacement = next(
            (
                (physical, logical)
                for physical, logical in sorted(
                    aliases.items(), key=lambda item: len(item[0]), reverse=True
                )
                if current[: len(physical)] == physical
                and (*logical, *current[len(physical) :]) != current
            ),
            None,
        )
        if replacement is None:
            break
        physical, logical = replacement
        updated = (*logical, *current[len(physical) :])
        if updated == current:
            break
        current = updated
    return current


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
    r"\s*::\s*[A-Za-z_][A-Za-z0-9_]*\s*\("
)


_BARE_REFERENCE = re.compile(
    r"(?<![.:])\b(?P<name>[A-Za-z_][A-Za-z0-9_]*)\b(?!\s*::)"
)


_USE = re.compile(r"\buse\s+(?P<path>[^;]+);")


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


def _aliases(repo_root: Path) -> dict[tuple[str, ...], tuple[str, ...]]:
    """Build aliases, delegating every normalization step to the coverage gate."""
    src_root = (repo_root / "src").resolve()
    raw: dict[tuple[str, ...], tuple[str, ...]] = {}
    for path in sorted(src_root.rglob("*.rs")):
        source = path.read_text("utf-8")
        _, _, records = _module_records(
            source, file_module_path(src_root, path)
        )
        for logical, relative, parents in records:
            target = path.parent.joinpath(*parents, relative).resolve()
            physical = file_module_path(src_root, target)
            if physical in raw and raw[physical] != logical:
                raise ValueError(f"conflicting #[path] aliases for {target}")
            raw[physical] = logical
    aliases = dict(raw)
    for _ in range(len(aliases) + 1):
        updated = {
            physical: _normalize_alias_path(logical, aliases)
            for physical, logical in aliases.items()
        }
        if updated == aliases:
            break
        aliases = updated
    return aliases


def discover_pg_inventory(
    repo_root: Path, findings: list[Finding] | None = None
) -> PgInventory:
    """Discover PG tests with a deliberately bounded Rust call model.

    Supported indirect references are bare/free-function calls, fully-qualified
    free functions, single- and multi-segment associated calls, and UFCS calls of
    the form ``<Type as Trait>::method()``. Test and helper bodies use the same
    reference extractor and both mask block-local functions and impls.

    Out of scope are block-local call graphs, method-level dispatch within an
    impl (an impl is conservatively treated as one type-level item), and UFCS
    receivers containing generic, tuple, reference, or ``dyn`` type syntax. A
    real Rust parser would be required to cover those forms without broad false
    positives. It skips ``src/main.rs``, ``src/bin/**`` (the current tree has
    no such directory), sources injected by ``include!``. ``#[ignore]`` tests
    remain selection/debt because their execution belongs to the ignore ledger.
    """
    repo_root = repo_root.resolve()
    _matching_brace_cached.cache_clear()
    src_root = (repo_root / "src").resolve()
    aliases = _aliases(repo_root)
    brace_counter = [0]
    external_tests = _external_test_files(repo_root, brace_counter, findings)
    inventory = rust_test_sources.lib_inventory(repo_root)
    declared_tests = set(inventory.tests)
    item_bodies: dict[tuple[tuple[str, ...], str, str], str] = {}
    seeded: set[tuple[tuple[str, ...], str, str]] = set()
    records: list[tuple[str, str, tuple[str, ...], str]] = []
    use_aliases: dict[tuple[str, ...], dict[str, tuple[str, ...]]] = {}
    wildcard_uses: dict[tuple[str, ...], list[tuple[str, ...]]] = {}

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

    for path in sorted(src_root.rglob("*.rs")):
        rel = path.relative_to(src_root)
        if rel.name == "main.rs" or (rel.parts and rel.parts[0] == "bin"):
            continue
        source = path.read_text("utf-8")
        clean = strip_rust(source)
        ranges = _module_ranges(source, clean, brace_counter)
        external = path.resolve() in external_tests
        physical_base = file_module_path(src_root, path)
        reached_base = inventory.modules.get(path.resolve())

        def logical_at(*tail: str, physical_base=physical_base, reached_base=reached_base) -> tuple[str, ...]:
            # The root walk knows a reached file's real module; #[path] aliasing is the fallback.
            if reached_base is not None:
                return (*reached_base, *tail)
            return _normalize_alias_path((*physical_base, *tail), aliases)

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
            module = logical_at(*_scope_at(use.start(), ranges))
            for binding, target in import_bindings(module, use.group("path")):
                if binding == "*":
                    wildcard_uses.setdefault(module, []).append(target[:-1])
                else:
                    use_aliases.setdefault(module, {})[binding] = target

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
            item_bodies[key] = body
            if seed_match(body):
                seeded.add(key)

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
                records.append((name, path.relative_to(repo_root).as_posix(), logical[:-1], body))
            elif findings is not None and reached_base is not None:
                # A recognised test in a file the lib walk reached must be listed, else it would run as non-PG.
                findings.append(Finding(
                    "test-not-in-inventory", name,
                    f"{path.relative_to(repo_root).as_posix()}: a test the root inventory does not list"))

    by_path: dict[tuple[tuple[str, ...], str], set[tuple[tuple[str, ...], str, str]]] = {}
    for key in item_bodies:
        module, name, _ = key
        by_path.setdefault((module, name), set()).add(key)

    def resolve(module: tuple[str, ...], raw: str, local_aliases=None, local_wildcards=()) -> set[tuple[tuple[str, ...], str, str]]:
        bindings = {**use_aliases.get(module, {}), **(local_aliases or {})}
        if "::" in raw:
            parts = tuple(part for part in raw.split("::") if part)
            alias = bindings.get(parts[0]) if parts else None
            target = (*alias, *parts[1:]) if alias else absolute_path(module, raw)
            return by_path.get((target[:-1], target[-1]), set()) if target else set()
        if raw in (local_aliases or {}):
            alias = local_aliases[raw]
            return set(by_path.get((alias[:-1], alias[-1]), set()))
        targets = set(by_path.get((module, raw), set()))
        alias = bindings.get(raw)
        if alias:
            targets.update(by_path.get((alias[:-1], alias[-1]), set()))
        for base in (*wildcard_uses.get(module, []), *local_wildcards):
            targets.update(by_path.get((base, raw), set()))
        return targets

    def resolve_call(
        module: tuple[str, ...], raw: str, local_aliases=None, local_wildcards=()
    ) -> set[tuple[tuple[str, ...], str, str]]:
        """Resolve a free call or one whole associated-call receiver path.

        ``crate::support::h()`` resolves to the fully-qualified free function,
        while ``a::b::TestDatabase::create()`` falls back to resolving the whole
        ``a::b::TestDatabase`` receiver. Individual qualifier segments are never
        reinterpreted in the caller's module.
        """
        targets = set(resolve(module, raw, local_aliases, local_wildcards))
        receiver, separator, _method = raw.rpartition("::")
        if separator and not targets:
            targets.update(resolve(module, receiver, local_aliases, local_wildcards))
        return targets

    def body_references(
        module: tuple[str, ...], body: str
    ) -> set[tuple[tuple[str, ...], str, str]]:
        """Resolve body-local imports without leaking aliases into sibling tests."""
        local_aliases = {}
        local_wildcards = []
        visible = list(body)
        for use in _USE.finditer(body):
            for binding, target in import_bindings(module, use.group("path")):
                if binding == "*":
                    local_wildcards.append(target[:-1])
                else:
                    local_aliases[binding] = target
            visible[use.start():use.end()] = " " * (use.end() - use.start())
        body = "".join(visible)
        calls = list(_CALL.finditer(body))
        ufcs_calls = list(_UFCS_CALL.finditer(body))
        occupied = [match.span() for match in (*calls, *ufcs_calls)]
        targets = {
            target
            for call in calls
            for target in resolve_call(module, call.group("path"), local_aliases, local_wildcards)
        }
        targets.update(
            target
            for call in ufcs_calls
            for target in resolve(module, call.group("type"), local_aliases, local_wildcards)
        )
        targets.update(
            target
            for mention in _BARE_REFERENCE.finditer(body)
            if not any(start <= mention.start() < end for start, end in occupied)
            for target in resolve(module, mention.group("name"), local_aliases, local_wildcards)
        )
        return targets

    edges: dict[tuple[tuple[str, ...], str, str], set[tuple[tuple[str, ...], str, str]]] = {}
    for key, body in item_bodies.items():
        if key[2] != "fn":
            continue
        edges[key] = body_references(
            key[0], _mask_nested_items(body, brace_counter)
        )

    tests: dict[str, str] = {}
    for name, path, module, body in records:
        visible_body = _mask_nested_items(body, brace_counter)
        direct = any(pattern.search(visible_body) for pattern in SEED_PATTERNS)
        referenced = body_references(module, visible_body)
        indirect = _transitive_closure(referenced, seeded, edges)
        if direct or indirect:
            tests[name] = path
        elif indirect is None and findings is not None:
            findings.append(Finding(
                "pg-reach-depth-exhausted",
                name,
                f"{path}: helper calls continue past the call-depth limit, so "
                "whether this test needs PG is undecided",
            ))
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
