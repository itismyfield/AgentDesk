#!/usr/bin/env python3
"""Reject new Rust library test modules that no curated CI lane selects.

AgentDesk's ``test-non-pg`` recipe and PR test jobs intentionally use libtest
name filters instead of running every library test. This source-only guard finds
``#[cfg(test)] mod ...`` declarations, derives their Rust module paths, and
checks whether at least one positive ``cargo test`` filter is a substring of the
module path (the same matching direction libtest uses for every test in that
module).

Existing uncovered modules are recorded in a baseline. The baseline is a debt
inventory, not an allowlist: a newly uncovered module fails, while a module that
becomes covered prints a reminder to remove its stale baseline entry.
"""

from __future__ import annotations

import argparse
import re
import shlex
import sys
from pathlib import Path
from typing import Iterable

REPO_ROOT = Path(__file__).resolve().parent.parent
JUSTFILE = REPO_ROOT / "justfile"
PR_WORKFLOW = REPO_ROOT / ".github" / "workflows" / "ci-pr.yml"
BASELINE = REPO_ROOT / "scripts" / "test_lane_coverage_baseline.txt"

# Attributes do not contain a closing square bracket in the forms used by this
# repository. Keeping this deliberately small makes the scanner independent of
# a Rust build and parser toolchain.
ATTRIBUTED_MOD_RE = re.compile(
    r"(?P<attrs>(?:#\s*\[[^\]]*\]\s*)+)"
    r"(?:(?:pub(?:\s*\([^)]*\))?)\s+)?"
    r"mod\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*(?P<term>[{;])",
    re.MULTILINE,
)
MOD_RE = re.compile(
    r"\bmod\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*(?P<term>[{;])",
    re.MULTILINE,
)
CFG_TEST_RE = re.compile(r"#\s*\[\s*cfg\s*\([^\]]*\btest\b[^\]]*\)\s*\]")

_RAW_STRING_OPEN = re.compile(r'(?:r|br)(#*)"')
_CHAR_LITERAL = re.compile(r"'(?:\\.|[^'\\])'")

# Cargo options whose following token is an option value, not a test filter.
_CARGO_VALUE_OPTIONS = {
    "--package",
    "-p",
    "--exclude",
    "--jobs",
    "-j",
    "--features",
    "--target",
    "--target-dir",
    "--manifest-path",
    "--color",
    "--config",
}
_LIBTEST_VALUE_OPTIONS = {"--skip", "--test-threads", "--format", "--color"}
_NON_LIB_TARGET_OPTIONS = {
    "--bin",
    "--bins",
    "--test",
    "--tests",
    "--example",
    "--examples",
    "--bench",
    "--benches",
    "--doc",
}


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
    """Return the conventional library module path for a Rust source file."""
    rel = path.relative_to(src_root)
    if rel.name == "lib.rs":
        return ()
    if rel.name == "mod.rs":
        return rel.parent.parts
    return (*rel.parent.parts, rel.stem)


def test_modules_in_source(source: str, base: tuple[str, ...]) -> set[str]:
    """Find cfg(test) module paths in one stripped Rust source file."""
    clean = strip_rust(source)
    test_mod_offsets = {
        match.start("name")
        for match in ATTRIBUTED_MOD_RE.finditer(clean)
        if CFG_TEST_RE.search(match.group("attrs"))
    }

    modules: set[str] = set()
    inline_stack: list[tuple[int, str]] = []
    depth = 0
    cursor = 0
    for match in MOD_RE.finditer(clean):
        # Account for ordinary scopes between module declarations.
        between = clean[cursor : match.start()]
        for brace in re.finditer(r"[{}]", between):
            if brace.group() == "{":
                depth += 1
            else:
                depth -= 1
                while inline_stack and inline_stack[-1][0] > depth:
                    inline_stack.pop()

        name = match.group("name")
        path = (*base, *(item[1] for item in inline_stack), name)
        if match.start("name") in test_mod_offsets:
            modules.add("::".join(path))

        if match.group("term") == "{":
            depth += 1
            inline_stack.append((depth, name))
        cursor = match.end()

    return modules


def discover_test_modules(repo_root: Path) -> set[str]:
    """Inventory conventional Rust library cfg(test) modules without building."""
    src_root = repo_root / "src"
    modules: set[str] = set()
    for path in sorted(src_root.rglob("*.rs")):
        rel = path.relative_to(src_root)
        if rel.name == "main.rs" or (rel.parts and rel.parts[0] == "bin"):
            continue
        modules.update(
            test_modules_in_source(
                path.read_text(encoding="utf-8"), file_module_path(src_root, path)
            )
        )
    return modules


def just_recipe_commands(justfile: str, recipe_name: str) -> tuple[str, ...]:
    """Extract command lines from one simple just recipe."""
    marker = re.compile(rf"^{re.escape(recipe_name)}:[ \t]*.*$", re.MULTILINE)
    match = marker.search(justfile)
    if match is None:
        raise ValueError(f"missing just recipe: {recipe_name}")
    commands: list[str] = []
    for line in justfile[match.end() :].splitlines():
        if line and not line[0].isspace():
            break
        stripped = line.strip()
        if stripped and not stripped.startswith("#"):
            commands.append(" ".join(stripped.split()))
    return tuple(commands)


def cargo_test_filters(command: str) -> set[str]:
    """Return positive libtest filters from a cargo-test shell command."""
    cargo = command.find("cargo test")
    if cargo < 0:
        return set()
    try:
        words = shlex.split(command[cargo:], comments=True)
    except ValueError as exc:
        raise ValueError(f"cannot parse cargo test command: {command!r}: {exc}") from exc
    if words[:2] != ["cargo", "test"]:
        return set()

    args = words[2:]
    before, separator, after = args, [], []
    if "--" in args:
        split = args.index("--")
        before, separator, after = args[:split], ["--"], args[split + 1 :]

    if any(option in before for option in _NON_LIB_TARGET_OPTIONS) and "--all-targets" not in before:
        return set()

    filters: set[str] = set()
    skip_next = False
    for token in before:
        if skip_next:
            skip_next = False
            continue
        if token in _CARGO_VALUE_OPTIONS:
            skip_next = True
            continue
        if token.startswith("-") or "=" in token and token.startswith("-"):
            continue
        filters.add(token)

    skip_next = False
    for token in after if separator else ():
        if skip_next:
            skip_next = False
            continue
        if token in _LIBTEST_VALUE_OPTIONS:
            skip_next = True
            continue
        if token.startswith("--skip=") or token.startswith("--test-threads="):
            continue
        if token.startswith("-"):
            continue
        filters.add(token)
    return filters


def discover_lane_filters(repo_root: Path) -> set[str]:
    """Parse positive filters from test-non-pg and PR-targeted test commands."""
    just_text = (repo_root / "justfile").read_text(encoding="utf-8")
    workflow = (repo_root / ".github/workflows/ci-pr.yml").read_text(encoding="utf-8")

    commands = list(just_recipe_commands(just_text, "test-non-pg"))
    for line in workflow.splitlines():
        if "cargo test" not in line:
            continue
        command = line.strip()
        if command.startswith("run:"):
            command = command.removeprefix("run:").strip()
            if len(command) >= 2 and command[0] == command[-1] and command[0] in "\"'":
                command = command[1:-1]
        commands.append(command)

    # PR jobs may delegate their targeted suite to a just recipe (test_fast does
    # this for test-postgres). Expand every referenced recipe that exists.
    for recipe in sorted(set(re.findall(r"\bjust\s+([A-Za-z0-9_-]+)", workflow))):
        try:
            commands.extend(just_recipe_commands(just_text, recipe))
        except ValueError:
            continue

    filters: set[str] = set()
    for command in commands:
        filters.update(cargo_test_filters(command))
    return filters


def uncovered_modules(modules: Iterable[str], filters: Iterable[str]) -> set[str]:
    """Return modules not wholly selected by any positive libtest filter."""
    active = tuple(filter_name for filter_name in filters if filter_name)
    return {
        module
        for module in modules
        if not any(f in module or f.startswith(f"{module}::") for f in active)
    }


def load_baseline(path: Path) -> set[str]:
    """Read the sorted one-module-per-line debt baseline."""
    entries = [
        line.strip()
        for line in path.read_text(encoding="utf-8").splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]
    if entries != sorted(entries):
        raise ValueError(f"baseline entries must be sorted: {path}")
    if len(entries) != len(set(entries)):
        raise ValueError(f"baseline contains duplicate entries: {path}")
    return set(entries)


def check(repo_root: Path, baseline_path: Path) -> int:
    modules = discover_test_modules(repo_root)
    filters = discover_lane_filters(repo_root)
    current = uncovered_modules(modules, filters)
    baseline = load_baseline(baseline_path)

    new = sorted(current - baseline)
    resolved = sorted(baseline - current)
    if new:
        print(
            f"FAIL: {len(new)} newly uncovered Rust test module(s); "
            f"{len(current)} currently uncovered (baseline {len(baseline)}).",
            file=sys.stderr,
        )
        print(
            "Add a module-level cargo test filter to test-non-pg or a PR targeted "
            "lane. Do not grow the baseline for new debt.",
            file=sys.stderr,
        )
        for module in new:
            print(f"  + {module}", file=sys.stderr)
        return 1

    if resolved:
        print(
            f"NOTE: {len(resolved)} baseline module(s) are now covered or removed; "
            f"delete them from {baseline_path.relative_to(repo_root)}."
        )
        for module in resolved:
            print(f"  - {module}")

    print(
        f"OK: {len(modules)} Rust cfg(test) modules inventoried; "
        f"{len(current)} uncovered module(s) match baseline; "
        f"{len(filters)} positive lane filter(s)."
    )
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo-root", type=Path, default=REPO_ROOT)
    parser.add_argument("--baseline", type=Path, default=None)
    args = parser.parse_args(argv)
    repo_root = args.repo_root.resolve()
    baseline = args.baseline.resolve() if args.baseline else repo_root / "scripts/test_lane_coverage_baseline.txt"
    try:
        return check(repo_root, baseline)
    except (OSError, ValueError) as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
