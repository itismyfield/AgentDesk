#!/usr/bin/env python3
"""Enforce PostgreSQL test-lane membership and workflow wiring (#4979 S1).

The gate identifies PG-dependent Rust tests only inside test regions, checks four
lane contracts, and ratchets existing violations through a sectioned baseline.
It is enforced from day one: unlike #5006's warn-only rollout over live
unbaselined offenders, this gate records all pre-existing debt, leaving no
initial false-positive surface for warnings to hide.
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
_JOBS_KEY = re.compile(r"^(?P<indent>[^\S\n]*)jobs:[^\S\n]*$", re.MULTILINE)
_JOB = re.compile(
    r"^(?P<indent>[^\S\n]+)(?P<name>[A-Za-z0-9_-]+):[^\S\n]*$",
    re.MULTILINE,
)


def _matching_brace(clean: str, opening: int) -> int:
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


def _module_ranges(source: str, clean: str) -> list[ModuleRange]:
    attrs = {match.start("name"): match.group("attrs") for match in _ATTR_MOD.finditer(clean)}
    ranges: list[ModuleRange] = []
    for match in _MOD.finditer(clean):
        if match.group("term") != "{":
            continue
        opening = match.end() - 1
        ranges.append(ModuleRange(
            opening, _matching_brace(clean, opening), match.group("name"),
            bool(_CFG_TEST.search(attrs.get(match.start("name"), ""))),
        ))
    return ranges


def _scope_at(offset: int, ranges: Iterable[ModuleRange]) -> tuple[str, ...]:
    containing = [item for item in ranges if item.start < offset < item.end]
    return tuple(item.name for item in sorted(containing, key=lambda item: item.start))


def _inside_test_region(offset: int, ranges: Iterable[ModuleRange], external: bool) -> bool:
    return external or any(item.is_test and item.start < offset < item.end for item in ranges)


def _external_test_files(repo_root: Path, coverage) -> set[Path]:
    src_root = (repo_root / "src").resolve()
    targets: set[Path] = set()
    for path in sorted(src_root.rglob("*.rs")):
        source = path.read_text("utf-8")
        clean = coverage.strip_rust(source)
        ranges = _module_ranges(source, clean)
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


def discover_pg_inventory(repo_root: Path) -> PgInventory:
    repo_root = repo_root.resolve()
    coverage = _load_coverage_module(repo_root)
    src_root = (repo_root / "src").resolve()
    aliases = _aliases(repo_root, coverage)
    external_tests = _external_test_files(repo_root, coverage)
    declared_tests = {
        test
        for test_names in coverage.discover_test_inventory(repo_root).values()
        for test in test_names
    }
    records: list[tuple[str, str, str, set[str]]] = []

    for path in sorted(src_root.rglob("*.rs")):
        rel = path.relative_to(src_root)
        if rel.name == "main.rs" or (rel.parts and rel.parts[0] == "bin"):
            continue
        source = path.read_text("utf-8")
        clean = coverage.strip_rust(source)
        ranges = _module_ranges(source, clean)
        external = path.resolve() in external_tests
        physical_base = coverage.file_module_path(src_root, path)

        helpers: set[str] = set()
        for match in _STRUCT.finditer(clean):
            if not _inside_test_region(match.start(), ranges, external):
                continue
            opening = _opening_brace(clean, match.end())
            if opening is None:
                continue
            body = clean[opening:_matching_brace(clean, opening) + 1]
            if any(pattern.search(body) for pattern in CONNECT_SEED_PATTERNS):
                helpers.add(match.group(1))
        for match in _IMPL.finditer(clean):
            if not _inside_test_region(match.start(), ranges, external):
                continue
            opening = match.end() - 1
            body = clean[opening:_matching_brace(clean, opening) + 1]
            if any(pattern.search(body) for pattern in CONNECT_SEED_PATTERNS):
                helpers.add(match.group(1))
        for match in re.finditer(r"\b(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)", clean):
            if not _inside_test_region(match.start(), ranges, external):
                continue
            opening = _opening_brace(clean, match.end())
            if opening is None:
                continue
            body = clean[opening:_matching_brace(clean, opening) + 1]
            if any(pattern.search(body) for pattern in CONNECT_SEED_PATTERNS):
                helpers.add(match.group(1))

        for match in _ATTR_FN.finditer(clean):
            if not _TEST_ATTR.search(match.group("attrs")):
                continue
            if not _inside_test_region(match.start(), ranges, external):
                continue
            opening = _opening_brace(clean, match.end())
            if opening is None:
                continue
            body = clean[opening:_matching_brace(clean, opening) + 1]
            physical = (*physical_base, *_scope_at(match.start(), ranges), match.group("name"))
            logical = coverage._normalize_alias_path(physical, aliases)
            name = "::".join(logical)
            if name in declared_tests:
                records.append((name, str(path.relative_to(repo_root)), body, helpers.copy()))

    tests: dict[str, str] = {}
    for name, path, body, helpers in records:
        direct = any(pattern.search(body) for pattern in SEED_PATTERNS)
        indirect = any(re.search(rf"\b{re.escape(helper)}\b", body) for helper in helpers)
        if direct or indirect:
            tests[name] = path
    return PgInventory(tests)


def parse_jobs(path: Path, repo_root: Path) -> list[Job]:
    """Parse top-level jobs without treating workflow trigger keys as jobs.

    This intentionally stays dependency-free instead of relying on PyYAML, which
    is not declared by AgentDesk's script-check environment. It supports the
    repository's block-style workflows and fails closed if ``jobs:`` is absent.
    """
    text = path.read_text("utf-8")
    jobs_key = _JOBS_KEY.search(text)
    if jobs_key is None:
        return []
    jobs_indent = len(jobs_key.group("indent"))
    candidates = [
        match for match in _JOB.finditer(text, jobs_key.end())
        if len(match.group("indent")) == jobs_indent + 2
    ]
    rel = str(path.relative_to(repo_root))
    return [
        Job(
            rel,
            match.group("name"),
            text[match.end():candidates[index + 1].start() if index + 1 < len(candidates) else len(text)],
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
    workflows = sorted(
        set((repo_root / ".github/workflows").glob("*.yml"))
        | set((repo_root / ".github/workflows").glob("*.yaml"))
    )
    jobs = [job for path in workflows for job in parse_jobs(path, repo_root)]
    inventory = discover_pg_inventory(repo_root)
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
    return Analysis(PgInventory(active_tests), debts, len(allowed_tests) + len(allowed_files))


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
    failed = False
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
            print(f"FAIL: [{section}] baseline drift: {len(new)} new, {len(stale)} stale.", file=sys.stderr)
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
        epilog=("This is a day-one enforced gate (violations return rc=1): existing debt is fully baselined, unlike #5006's warn-only rollout over unbaselined offenders."),
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
