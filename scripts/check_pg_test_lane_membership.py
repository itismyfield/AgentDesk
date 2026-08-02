#!/usr/bin/env python3
"""Enforce PostgreSQL test-lane membership and workflow wiring (#4979 S1).

The gate identifies PG-dependent Rust tests only inside test regions, checks four
lane contracts, and ratchets existing violations through a sectioned baseline.
During T0, newly discovered live debt is warn-only. Return code 1 is reserved for
manifest drift, candidate baseline growth, and stale baseline entries; malformed
inputs and configuration errors return code 2. T1 promotes new debt to enforcement.
"""

from __future__ import annotations

import argparse
import fnmatch
import importlib.util
import os
import re
import shlex
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable

REPO_ROOT = Path(__file__).resolve().parents[1]
BASELINE_REL = Path("scripts/pg_test_lane_baseline.txt")
MANIFEST_REL = Path("scripts/pg_test_lane_manifest.txt")
ALLOWLIST_REL = Path("scripts/pg_test_lane_allowlist.txt")
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
SECTIONS = ("rule1", "rule2", "rule3", "rule4")
CONFIGURATION_ERROR_FINDINGS = frozenset({"jobs-empty"})


def _load_coverage_module(repo_root: Path):
    path = repo_root / "scripts/check_test_lane_coverage.py"
    spec = importlib.util.spec_from_file_location("pg_lane_coverage", path)
    if spec is None or spec.loader is None:
        raise ValueError(f"cannot import coverage parser: {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


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
class Analysis:
    inventory: PgInventory
    debts: dict[str, set[str]]
    allowlist_count: int
    findings: tuple["Finding", ...] = ()


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


@dataclass(frozen=True)
class Job:
    workflow: str
    name: str
    text: str

    @property
    def key(self) -> str:
        return f"{self.workflow}:{self.name}"


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
_JOBS_KEY = re.compile(
    r"^(?:jobs|'jobs'|\"jobs\")[^\S\n]*:(?=[^\S\n]|$)",
    re.MULTILINE,
)
_JOBS_BLOCK_KEY = re.compile(
    r"^(?:jobs|'jobs'|\"jobs\"):[^\S\n]*(?:&[^\s\[\]{},]+[^\S\n]*)?(?:#.*)?$",
    re.MULTILINE,
)
_JOB = re.compile(
    r"^(?P<indent>[^\S\r\n]+)"
    r"(?P<name>[A-Za-z0-9_-]+|'[^']+'|\"[^\"]+\")"
    r"(?P<pre_colon>[^\S\r\n]*):[^\S\r\n]*"
    r"(?P<decorator>[&*][^\s\[\]{},]+)?[^\S\r\n]*(?:#.*)?$",
    re.MULTILINE,
)
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
_TOP_LEVEL_KEY = re.compile(
    r"^(?:[A-Za-z_][A-Za-z0-9_-]*|'[^']+'|\"[^\"]+\")\s*:"
)
_BLOCK_SCALAR_HEADER = re.compile(
    r":\s*[|>](?:[1-9][+-]?|[+-][1-9]?)?\s*(?:#.*)?$"
)


def _matching_brace(clean: str, opening: int, counter: list[int] | None = None) -> int:
    if counter is not None:
        counter[0] += 1
    depth = 0
    for index in range(opening, len(clean)):
        if clean[index] == "{":
            depth += 1
        elif clean[index] == "}":
            depth -= 1
            if depth == 0:
                return index
    raise ValueError(f"unclosed brace at byte {opening}")


def _opening_brace(clean: str, start: int) -> int | None:
    brace = clean.find("{", start)
    semicolon = clean.find(";", start)
    if brace < 0 or (semicolon >= 0 and semicolon < brace):
        return None
    return brace


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
            opening = match.end() - 1 if pattern is _IMPL else _opening_brace(body, match.end())
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
    max_depth: int = 3,
) -> bool:
    """Whether a module-scoped helper reaches a PG seed within ``max_depth``."""
    frontier = set(referenced)
    seen: set[tuple[tuple[str, ...], str, str]] = set()
    for _depth in range(1, max_depth + 1):
        if frontier & seeded:
            return True
        seen.update(frontier)
        frontier = {
            target
            for caller in frontier
            for target in edges.get(caller, set())
            if target not in seen
        }
    return False


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


def _external_test_files(
    repo_root: Path, coverage, counter: list[int] | None = None
) -> set[Path]:
    src_root = (repo_root / "src").resolve()
    targets: set[Path] = set()
    for path in sorted(src_root.rglob("*.rs")):
        source = path.read_text("utf-8")
        clean = coverage.strip_rust(source)
        ranges = _module_ranges(source, clean, counter)
        for match in _ATTR_MOD.finditer(clean):
            if match.group("term") != ";" or not _CFG_TEST.search(match.group("attrs")):
                continue
            redirect = _PATH_ATTR.search(match.group("attrs"))
            parents = _scope_at(match.start(), ranges)
            if redirect:
                target = path.parent.joinpath(*parents, redirect.group("path"))
                candidates = (target,)
            else:
                base = path.parent.joinpath(*parents)
                name = match.group("name")
                candidates = (base / f"{name}.rs", base / name / "mod.rs")
            target = next((candidate.resolve() for candidate in candidates if candidate.is_file()), None)
            if target and target.is_relative_to(src_root):
                targets.add(target)
    return targets


def _aliases(repo_root: Path, coverage) -> dict[tuple[str, ...], tuple[str, ...]]:
    """Build aliases, delegating every normalization step to the coverage gate."""
    src_root = (repo_root / "src").resolve()
    raw: dict[tuple[str, ...], tuple[str, ...]] = {}
    for path in sorted(src_root.rglob("*.rs")):
        source = path.read_text("utf-8")
        _, _, records = coverage._module_records(
            source, coverage.file_module_path(src_root, path)
        )
        for logical, relative, parents in records:
            target = path.parent.joinpath(*parents, relative).resolve()
            physical = coverage.file_module_path(src_root, target)
            if physical in raw and raw[physical] != logical:
                raise ValueError(f"conflicting #[path] aliases for {target}")
            raw[physical] = logical
    aliases = dict(raw)
    for _ in range(len(aliases) + 1):
        updated = {
            physical: coverage._normalize_alias_path(logical, aliases)
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
    positives.
    """
    repo_root = repo_root.resolve()
    coverage = _load_coverage_module(repo_root)
    src_root = (repo_root / "src").resolve()
    aliases = _aliases(repo_root, coverage)
    brace_counter = [0]
    external_tests = _external_test_files(repo_root, coverage, brace_counter)
    declared_tests = {
        test
        for test_names in coverage.discover_test_inventory(repo_root).values()
        for test in test_names
    }
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
        clean = coverage.strip_rust(source)
        ranges = _module_ranges(source, clean, brace_counter)
        external = path.resolve() in external_tests
        physical_base = coverage.file_module_path(src_root, path)

        fn_ranges: list[tuple[int, int, re.Match[str], int]] = []
        for match in _FN.finditer(clean):
            opening = _opening_brace(clean, match.end())
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
            physical_module = (*physical_base, *_scope_at(use.start(), ranges))
            module = coverage._normalize_alias_path(physical_module, aliases)
            raw = use.group("path").strip()
            expanded: list[str]
            brace = re.fullmatch(r"(?P<base>.+)::\{(?P<items>[^{}]+)\}", raw)
            if brace:
                expanded = [
                    f"{brace.group('base')}::{item.strip()}"
                    for item in brace.group("items").split(",")
                    if item.strip()
                ]
            else:
                expanded = [raw]
            for entry in expanded:
                target_text, separator, alias = entry.partition(" as ")
                target = absolute_path(module, target_text)
                if not target:
                    continue
                binding = alias.strip() if separator else target[-1]
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
            physical_module = (*physical_base, *_scope_at(start, ranges))
            module = coverage._normalize_alias_path(physical_module, aliases)
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
            opening = _opening_brace(clean, match.end())
            if opening is None:
                continue
            body = _edge_boundary(clean, opening, brace_counter)
            physical_module = (*physical_base, *_scope_at(match.start(), ranges))
            module = coverage._normalize_alias_path(physical_module, aliases)
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
            physical_module = (*physical_base, *_scope_at(match.start(), ranges))
            module = coverage._normalize_alias_path(physical_module, aliases)
            key = (module, match.group(1), "impl")
            item_bodies[key] = body
            if seed_match(body):
                seeded.add(key)

        for match in _ATTR_FN.finditer(clean):
            if not _TEST_ATTR.search(match.group("attrs")):
                continue
            if not _inside_test_region(match.start(), ranges, external):
                continue
            opening = _opening_brace(clean, match.end())
            if opening is None:
                continue
            body = _edge_boundary(clean, opening, brace_counter)
            physical = (*physical_base, *_scope_at(match.start(), ranges), match.group("name"))
            logical = coverage._normalize_alias_path(physical, aliases)
            name = "::".join(logical)
            if name in declared_tests:
                records.append((name, str(path.relative_to(repo_root)), logical[:-1], body))

    by_path: dict[tuple[tuple[str, ...], str], set[tuple[tuple[str, ...], str, str]]] = {}
    for key in item_bodies:
        module, name, _ = key
        by_path.setdefault((module, name), set()).add(key)

    def resolve(module: tuple[str, ...], raw: str) -> set[tuple[tuple[str, ...], str, str]]:
        if "::" in raw:
            parts = tuple(part for part in raw.split("::") if part)
            alias = use_aliases.get(module, {}).get(parts[0]) if parts else None
            target = (*alias, *parts[1:]) if alias else absolute_path(module, raw)
            return by_path.get((target[:-1], target[-1]), set()) if target else set()
        targets = set(by_path.get((module, raw), set()))
        alias = use_aliases.get(module, {}).get(raw)
        if alias:
            targets.update(by_path.get((alias[:-1], alias[-1]), set()))
        for base in wildcard_uses.get(module, []):
            targets.update(by_path.get((base, raw), set()))
        return targets

    def resolve_call(
        module: tuple[str, ...], raw: str
    ) -> set[tuple[tuple[str, ...], str, str]]:
        """Resolve a free call or one whole associated-call receiver path.

        ``crate::support::h()`` resolves to the fully-qualified free function,
        while ``a::b::TestDatabase::create()`` falls back to resolving the whole
        ``a::b::TestDatabase`` receiver. Individual qualifier segments are never
        reinterpreted in the caller's module.
        """
        targets = set(resolve(module, raw))
        receiver, separator, _method = raw.rpartition("::")
        if separator and not targets:
            targets.update(resolve(module, receiver))
        return targets

    def body_references(
        module: tuple[str, ...], body: str
    ) -> set[tuple[tuple[str, ...], str, str]]:
        """Extract identical module-scoped references from tests and helpers."""
        calls = list(_CALL.finditer(body))
        ufcs_calls = list(_UFCS_CALL.finditer(body))
        occupied = [match.span() for match in (*calls, *ufcs_calls)]
        targets = {
            target
            for call in calls
            for target in resolve_call(module, call.group("path"))
        }
        targets.update(
            target
            for call in ufcs_calls
            for target in resolve(module, call.group("type"))
        )
        targets.update(
            target
            for mention in _BARE_REFERENCE.finditer(body)
            if not any(start <= mention.start() < end for start, end in occupied)
            for target in resolve(module, mention.group("name"))
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
    if findings is not None:
        findings.append(Finding(
            "operation-counter",
            "discover_pg_inventory",
            f"_matching_brace calls={brace_counter[0]}",
        ))
    return PgInventory(tests)


def _indent_width(indent: str) -> int:
    return len(indent.expandtabs(8))


def _jobs_section_end(text: str, start: int) -> int:
    """Find the next top-level mapping key, respecting block scalar bodies."""
    offset = start
    block_parent_indent: int | None = None
    for line in text[start:].splitlines(keepends=True):
        content = line.rstrip("\r\n")
        stripped = content.strip()
        indent_text = content[: len(content) - len(content.lstrip())]
        indent = _indent_width(indent_text)

        if block_parent_indent is not None:
            if not stripped or indent > block_parent_indent:
                offset += len(line)
                continue
            block_parent_indent = None

        if _TOP_LEVEL_KEY.match(content):
            return offset
        if _BLOCK_SCALAR_HEADER.search(content):
            block_parent_indent = indent
        offset += len(line)
    return len(text)


def parse_jobs(
    path: Path, repo_root: Path, findings: list[Finding] | None = None
) -> list[Job]:
    """Parse top-level jobs without treating workflow trigger keys as jobs.

    This intentionally stays dependency-free instead of relying on PyYAML, which
    is not declared by AgentDesk's script-check environment. It supports the
    repository's block-style workflows and tracks scalar bodies while locating
    the next column-zero mapping key. A literal top-level ``jobs:`` may carry
    one anchor after the colon; a tag in that slot (``jobs: !!map &a``) remains
    unsupported and is reported as ``jobs-empty``.

    Two distinct gaps follow from parsing YAML with regexes, and neither is
    closed here. First, top-level ``jobs`` presence is detected by *spelling*:
    the probe matches the literal characters, so a key that YAML resolves to
    ``jobs`` without spelling it that way — ``"jo\\u0062s"``, ``? jobs``,
    ``!!str jobs`` — is not seen at all. Such a file returns no jobs and no
    finding, so the whole membership check silently passes it. Second, once a
    literal ``jobs:`` block is found, individual jobs are limited to supported
    block headers; other individual job forms can still be returned under
    their uninterpreted names rather than omitted. That can put a name YAML
    did not resolve into the list and silently skew name-based comparisons,
    while their siblings are still reported.

    So the ``jobs-empty`` finding means "spelled ``jobs`` was found but nothing
    under it parsed", not "this file has no unparsed job map". Do not read a
    clean result as proof that the file was understood. Both gaps are tracked
    as sites on umbrella #5071; closing either needs a real YAML parser, which
    the script-check environment does not guarantee.
    """
    text = path.read_text("utf-8")
    # Normalize only the top-level key probe. Escaped, explicit, and tagged
    # top-level key forms do not match the literal probe, so they return no
    # jobs and no finding rather than a configuration error.
    has_bom = text.startswith("\ufeff")
    present_jobs_key = _JOBS_KEY.search(text.removeprefix("\ufeff"))
    if present_jobs_key is None:
        return []
    jobs_key = None if has_bom else _JOBS_BLOCK_KEY.match(text, present_jobs_key.start())
    section_end = len(text)
    candidates: list[re.Match[str]] = []
    if jobs_key is not None:
        jobs_indent = 0
        section_end = _jobs_section_end(text, jobs_key.end())
        in_section = [
            match for match in _JOB.finditer(text, jobs_key.end(), section_end)
            if _indent_width(match.group("indent")) > jobs_indent
        ]
        job_indent = min(
            (_indent_width(match.group("indent")) for match in in_section),
            default=None,
        )
        candidates = [
            match for match in in_section
            if _indent_width(match.group("indent")) == job_indent
        ]
    rel = str(path.relative_to(repo_root))
    if not candidates and findings is not None:
        findings.append(
            Finding(
                "jobs-empty",
                rel,
                "top-level jobs: found but no job keys parsed; this gate reads a "
                "plain block header (optionally one anchor) with block-style job "
                "keys under it, so flow mappings, tags, and a space before the "
                "colon are not read — rewrite the jobs block in that form. A "
                "leading UTF-8 BOM also lands here even when the block itself is "
                "already plain; strip the BOM in that case",
            )
        )
    return [
        Job(
            rel,
            match.group("name").strip("'\""),
            text[
                match.end():
                candidates[index + 1].start() if index + 1 < len(candidates) else section_end
            ],
        )
        for index, match in enumerate(candidates)
    ]


def _cargo_commands(text: str) -> list[str]:
    """Extract only YAML ``run:`` scalar command lines, never step names."""
    commands: list[str] = []
    block_indent: int | None = None
    for line in text.splitlines():
        indent = len(line) - len(line.lstrip())
        stripped = line.strip()
        run = re.match(r"^(?:-\s+)?run:\s*(.*)$", stripped)
        if run:
            value = run.group(1).strip().strip('"\'')
            if value in ("", "|"):
                block_indent = indent
            else:
                block_indent = None
                start = value.find("cargo test")
                if start >= 0:
                    commands.append(value[start:])
            continue
        if block_indent is not None:
            if not stripped or stripped.startswith("#"):
                continue
            if indent <= block_indent:
                block_indent = None
                continue
            value = stripped.strip('"\'')
            start = value.find("cargo test")
            if start >= 0:
                commands.append(value[start:])
    return commands


def pg_lane_filters(repo_root: Path, jobs: Iterable[Job], coverage) -> tuple:
    just_text = (repo_root / "justfile").read_text("utf-8")
    commands = list(coverage.just_recipe_commands(just_text, "test-postgres"))
    for job in jobs:
        if "postgres-service.sh start" not in job.text:
            continue
        commands.extend(_cargo_commands(job.text))
        for recipe in re.findall(r"\bjust\s+([A-Za-z0-9_-]+)", job.text):
            try:
                commands.extend(coverage.just_recipe_commands(just_text, recipe))
            except ValueError:
                pass
    lanes = [coverage.cargo_test_filter(command) for command in commands]
    return tuple(dict.fromkeys(lane for lane in lanes if lane is not None))


def pgless_lane_filters(jobs: Iterable[Job], coverage) -> tuple:
    lanes = []
    for job in jobs:
        if "postgres-service.sh start" in job.text:
            continue
        for command in _cargo_commands(job.text):
            if "--all-targets" not in command or "--skip" not in command:
                continue
            lane = coverage.cargo_test_filter(command)
            if lane is not None:
                lanes.append(lane)
    return tuple(dict.fromkeys(lanes))


def parse_pg_db_patterns(path: Path) -> tuple[str, ...]:
    """Parse the block-style dorny ``pg_db`` filter by relative indentation."""
    lines = path.read_text("utf-8").splitlines()
    start = next(
        (
            (index, len(line) - len(line.lstrip()))
            for index, line in enumerate(lines)
            if re.match(r"^\s+pg_db:\s*$", line)
        ),
        None,
    )
    if start is None:
        raise ValueError(f"missing pg_db path filter in {path}")
    start_index, section_indent = start
    patterns: list[str] = []
    for line in lines[start_index + 1:]:
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        indent = len(line) - len(line.lstrip())
        if indent <= section_indent:
            break
        match = re.match(r"^-\s+['\"]([^'\"]+)['\"]\s*$", stripped)
        if match:
            patterns.append(match.group(1))
    if not patterns:
        raise ValueError(f"empty pg_db path filter in {path}")
    return tuple(patterns)


def path_selected(path: str, patterns: Iterable[str]) -> bool:
    selected = False
    for raw in patterns:
        negated = raw.startswith("!")
        pattern = raw[1:] if negated else raw
        if fnmatch.fnmatchcase(path, pattern) or (
            pattern.endswith("/**") and (path == pattern[:-3] or path.startswith(pattern[:-2]))
        ):
            selected = not negated
    return selected


def load_allowlist(path: Path) -> tuple[set[str], set[str]]:
    tests: set[str] = set()
    files: set[str] = set()
    if not path.is_file():
        return tests, files
    for lineno, raw in enumerate(path.read_text("utf-8").splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        entry, marker, reason = line.partition("#")
        if not marker or not reason.strip():
            raise ValueError(f"allowlist entry requires an inline reason comment: {path}:{lineno}")
        kind, separator, value = entry.strip().partition(":")
        if not separator or kind not in ("test", "file") or not value.strip():
            raise ValueError(f"invalid allowlist entry: {path}:{lineno}")
        (tests if kind == "test" else files).add(value.strip())
    return tests, files


def analyze(repo_root: Path, allowlist_path: Path | None = None) -> Analysis:
    coverage = _load_coverage_module(repo_root)
    findings: list[Finding] = []
    workflows = sorted(
        set((repo_root / ".github/workflows").glob("*.yml"))
        | set((repo_root / ".github/workflows").glob("*.yaml"))
    )
    jobs = [job for path in workflows for job in parse_jobs(path, repo_root, findings)]
    inventory = discover_pg_inventory(repo_root, findings)
    allowed_tests, allowed_files = load_allowlist(allowlist_path or repo_root / ALLOWLIST_REL)
    active_tests = {name: path for name, path in inventory.tests.items() if name not in allowed_tests and path not in allowed_files}
    pg_lanes = pg_lane_filters(repo_root, jobs, coverage)
    pgless_lanes = pgless_lane_filters(jobs, coverage)
    patterns = parse_pg_db_patterns(repo_root / ".github/workflows/ci-pr.yml")
    debts = {
        "rule1": {name for name in active_tests if not any(lane.selects_test(name) for lane in pg_lanes)},
        "rule2": {name for name in active_tests if any(lane.selects_test(name) for lane in pgless_lanes)},
        "rule3": {path for path in set(active_tests.values()) if not path_selected(path, patterns)},
        "rule4": {job.key for job in jobs if "postgres-service.sh start" in job.text and not re.search(r"^\s+AGENTDESK_REQUIRE_PG:\s*['\"]?1['\"]?\s*$", job.text, re.MULTILINE)},
    }
    return Analysis(
        PgInventory(active_tests),
        debts,
        len(allowed_tests) + len(allowed_files),
        tuple(findings),
    )


def render_manifest(inventory: PgInventory) -> str:
    lines = ["# Generated by scripts/check_pg_test_lane_membership.py --write-snapshots.", "[files]"]
    lines.extend(sorted(inventory.files))
    lines.append("[modules]")
    lines.extend(sorted(inventory.modules))
    lines.append("[tests]")
    lines.extend(sorted(inventory.tests))
    return "\n".join(lines) + "\n"


def render_baseline(debts: dict[str, set[str]]) -> str:
    lines = ["# Existing PostgreSQL lane debt. Sections and entries are sorted; debt may only shrink."]
    for section in SECTIONS:
        lines.append(f"[{section}]")
        lines.extend(sorted(debts[section]))
    return "\n".join(lines) + "\n"


def parse_baseline(text: str, source: str) -> dict[str, set[str]]:
    parsed = {section: set() for section in SECTIONS}
    current: str | None = None
    ordered: dict[str, list[str]] = {section: [] for section in SECTIONS}
    for lineno, raw in enumerate(text.splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("[") and line.endswith("]"):
            current = line[1:-1]
            if current not in parsed:
                raise ValueError(f"unknown baseline section {line}: {source}:{lineno}")
            continue
        if current is None:
            raise ValueError(f"baseline entry outside section: {source}:{lineno}")
        ordered[current].append(line)
    for section, entries in ordered.items():
        if entries != sorted(entries) or len(entries) != len(set(entries)):
            raise ValueError(f"baseline [{section}] entries must be sorted and unique: {source}")
        parsed[section] = set(entries)
    return parsed


def reference_baseline(repo_root: Path, ref: str) -> tuple[str, dict[str, set[str]] | None]:
    result = subprocess.run(["git", "rev-parse", "--verify", f"{ref}^{{commit}}"], cwd=repo_root, capture_output=True, text=True)
    if result.returncode:
        raise ValueError(f"cannot resolve baseline reference {ref!r}: {result.stderr.strip()}")
    sha = result.stdout.strip()
    blob = subprocess.run(["git", "show", f"{sha}:{BASELINE_REL.as_posix()}"], cwd=repo_root, capture_output=True, text=True)
    if blob.returncode:
        return sha, None
    return sha, parse_baseline(blob.stdout, f"{sha}:{BASELINE_REL}")


def _configuration_errors(findings: Iterable[Finding]) -> tuple[Finding, ...]:
    return tuple(
        finding for finding in findings
        if finding.kind in CONFIGURATION_ERROR_FINDINGS
    )


def check_analysis(
    analysis: Analysis,
    baseline: dict[str, set[str]],
    reference: dict[str, set[str]] | None,
    actual_manifest: str,
    *,
    reference_label: str,
    allowlist_label: str,
) -> int:
    """Apply manifest and one-way baseline contracts to a supplied analysis."""
    configuration_errors = _configuration_errors(analysis.findings)
    failed = False
    for finding in analysis.findings:
        if finding.kind in CONFIGURATION_ERROR_FINDINGS:
            print(f"FAIL: [{finding.kind}] {finding.source}: {finding.detail}", file=sys.stderr)
        else:
            print(f"WARN: [{finding.kind}] {finding.source}: {finding.detail}", file=sys.stderr)
    if configuration_errors:
        return 2
    expected_manifest = render_manifest(analysis.inventory)
    if actual_manifest != expected_manifest:
        print("FAIL: PG test-lane manifest drift.", file=sys.stderr)
        print(
            "Run `python3 scripts/check_pg_test_lane_membership.py --write-snapshots` "
            "after intentional PG test inventory changes. This rewrites BOTH the manifest "
            "and baseline; inspect both diffs.",
            file=sys.stderr,
        )
        print(
            "If the change creates a new rule violation, snapshot regeneration will not "
            "excuse it: fix the test/module/workflow, or add a narrowly scoped entry with "
            f"an inline reason to {allowlist_label}.",
            file=sys.stderr,
        )
        failed = True
    if reference is not None:
        for section in SECTIONS:
            growth = sorted(baseline[section] - reference[section])
            if growth:
                print(f"FAIL: [{section}] baseline growth forbidden vs {reference_label}:", file=sys.stderr)
                for entry in growth:
                    print(f"  + {entry}", file=sys.stderr)
                print(
                    "Remove '+' entries; the candidate baseline may only preserve or "
                    "remove debt from its immutable reference snapshot. Fix the test, "
                    "module name, or workflow lane instead.",
                    file=sys.stderr,
                )
                print(
                    f"For a proven classifier false positive only, add `test:<path> # reason` "
                    f"or `file:<path> # reason` to {allowlist_label}.",
                    file=sys.stderr,
                )
                failed = True
    for section in SECTIONS:
        new = sorted(analysis.debts[section] - baseline[section])
        stale = sorted(baseline[section] - analysis.debts[section])
        if new or stale:
            level = "FAIL" if stale else "WARN"
            print(f"{level}: [{section}] baseline drift: {len(new)} new, {len(stale)} stale.", file=sys.stderr)
            for entry in new:
                print(f"  + {entry}", file=sys.stderr)
            for entry in stale:
                print(f"  - {entry}", file=sys.stderr)
            print(
                "Fix '+' violations in the test/module/workflow. Remove '-' entries from "
                "the baseline to lock in debt reduction; then regenerate the manifest.",
                file=sys.stderr,
            )
            print(
                f"For a proven classifier false positive only, use {allowlist_label}; "
                "every entry requires an inline reason comment.",
                file=sys.stderr,
            )
            if stale:
                failed = True
    counts = analysis.debts
    print(f"pg-lane debt: rule1={len(counts['rule1'])} rule2={len(counts['rule2'])} rule3={len(counts['rule3'])} (allowlist={analysis.allowlist_count})")
    if failed:
        return 1
    print(f"PG test-lane membership check passed: {len(analysis.inventory.tests)} tests, {len(analysis.inventory.files)} files; rule4={len(counts['rule4'])}")
    return 0


def check(repo_root: Path, baseline_path: Path, manifest_path: Path, baseline_ref: str, allowlist_path: Path | None = None) -> int:
    analysis = analyze(repo_root, allowlist_path)
    baseline = parse_baseline(baseline_path.read_text("utf-8"), str(baseline_path))
    sha, reference = reference_baseline(repo_root, baseline_ref)
    allowlist = allowlist_path or repo_root / ALLOWLIST_REL
    return check_analysis(
        analysis,
        baseline,
        reference,
        manifest_path.read_text("utf-8"),
        reference_label=f"commit {sha}" if reference is not None else "bootstrap snapshot",
        allowlist_label=str(allowlist),
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__.splitlines()[0],
        epilog=(
            "During T0, new live debt is warn-only. Manifest drift, candidate "
            "baseline growth, and stale baseline entries return rc=1; T1 promotes "
            "new debt to enforcement."
        ),
    )
    parser.add_argument("--repo-root", type=Path, default=REPO_ROOT)
    parser.add_argument("--baseline", type=Path)
    parser.add_argument("--manifest", type=Path)
    parser.add_argument("--allowlist", type=Path)
    parser.add_argument("--baseline-ref", default=os.environ.get("TEST_LANE_BASELINE_REF", "HEAD"))
    parser.add_argument("--write-snapshots", action="store_true", help="rewrite the manifest and baseline from the current tree")
    args = parser.parse_args(argv)
    root = args.repo_root.resolve()
    baseline = args.baseline.resolve() if args.baseline else root / BASELINE_REL
    manifest = args.manifest.resolve() if args.manifest else root / MANIFEST_REL
    allowlist = args.allowlist.resolve() if args.allowlist else None
    try:
        if args.write_snapshots:
            analysis = analyze(root, allowlist)
            configuration_errors = _configuration_errors(analysis.findings)
            for finding in configuration_errors:
                print(f"FAIL: [{finding.kind}] {finding.source}: {finding.detail}", file=sys.stderr)
            if configuration_errors:
                return 2
            manifest.write_text(render_manifest(analysis.inventory), "utf-8")
            baseline.write_text(render_baseline(analysis.debts), "utf-8")
            print(f"wrote {manifest} and {baseline}")
            return 0
        return check(root, baseline, manifest, args.baseline_ref, allowlist)
    except (OSError, ValueError) as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
