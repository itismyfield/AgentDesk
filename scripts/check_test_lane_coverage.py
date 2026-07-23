#!/usr/bin/env python3
"""Reject new Rust library test modules that no curated CI lane fully selects.

AgentDesk's ``test-non-pg`` recipe and PR test jobs intentionally use libtest
name filters instead of running every library test. This source-only guard finds
``#[cfg(test)] mod ...`` declarations, derives their logical Rust module paths
(including ``#[path = "..."]`` aliases), and compares them with each curated
``cargo test`` command's positive and ``--skip`` filters.

The existing uncovered set is locked twice: its sorted names live in the
baseline file and its entry count lives in this script. Any new module, stale
entry, or baseline growth fails. Reducing debt therefore requires an explicit,
reviewable edit to both locks.
"""

from __future__ import annotations

import argparse
import re
import shlex
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable

REPO_ROOT = Path(__file__).resolve().parent.parent
BASELINE_REL = Path("scripts/test_lane_coverage_baseline.txt")
BASELINE_ENTRY_COUNT = 689

# Attributes do not contain a closing square bracket in the forms used by this
# repository. Strings and comments are blanked without changing offsets, so the
# original attribute text can be recovered safely from the same span.
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
PATH_ATTR_RE = re.compile(r'#\s*\[\s*path\s*=\s*"(?P<path>[^"]+)"\s*\]')

_RAW_STRING_OPEN = re.compile(r'(?:r|br)(#*)"')
_CHAR_LITERAL = re.compile(r"'(?:\\.|[^'\\])'")

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
_LIBTEST_VALUE_OPTIONS = {"--test-threads", "--format", "--color"}
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


@dataclass(frozen=True)
class LaneFilter:
    """One cargo-test invocation's libtest selection contract."""

    positives: tuple[str, ...]
    skips: tuple[str, ...]
    exact: bool = False

    def fully_selects(self, module: str) -> bool:
        """Whether this command selects every test whose path starts at module."""
        if self.exact:
            return False
        positive_match = not self.positives or any(
            positive in module for positive in self.positives
        )
        if not positive_match:
            return False
        return not any(skip in module for skip in self.skips)


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
) -> tuple[set[str], list[tuple[tuple[str, ...], str]]]:
    """Return cfg(test) modules and path aliases declared in one source file."""
    clean = strip_rust(source)
    attributes: dict[int, str] = {}
    for match in ATTRIBUTED_MOD_RE.finditer(clean):
        attributes[match.start("name")] = source[
            match.start("attrs") : match.end("attrs")
        ]

    modules: set[str] = set()
    aliases: list[tuple[tuple[str, ...], str]] = []
    inline_stack: list[tuple[int, str]] = []
    depth = 0
    cursor = 0
    for match in MOD_RE.finditer(clean):
        between = clean[cursor : match.start()]
        for brace in re.finditer(r"[{}]", between):
            if brace.group() == "{":
                depth += 1
            else:
                depth -= 1
                while inline_stack and inline_stack[-1][0] > depth:
                    inline_stack.pop()

        name = match.group("name")
        logical = (*base, *(item[1] for item in inline_stack), name)
        attrs = attributes.get(match.start("name"), "")
        if CFG_TEST_RE.search(attrs):
            modules.add("::".join(logical))
        path_attr = PATH_ATTR_RE.search(attrs)
        if path_attr and match.group("term") == ";":
            aliases.append((logical, path_attr.group("path")))

        if match.group("term") == "{":
            depth += 1
            inline_stack.append((depth, name))
        cursor = match.end()
    return modules, aliases


def test_modules_in_source(source: str, base: tuple[str, ...]) -> set[str]:
    """Find cfg(test) module paths in one Rust source file."""
    modules, _ = _module_records(source, base)
    return modules


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


def discover_test_modules(repo_root: Path) -> set[str]:
    """Inventory logical Rust library cfg(test) modules without building."""
    src_root = (repo_root / "src").resolve()
    physical_modules: set[tuple[str, ...]] = set()
    raw_aliases: dict[tuple[str, ...], tuple[str, ...]] = {}

    for path in sorted(src_root.rglob("*.rs")):
        rel = path.relative_to(src_root)
        if rel.name == "main.rs" or (rel.parts and rel.parts[0] == "bin"):
            continue
        base = file_module_path(src_root, path)
        source = path.read_text(encoding="utf-8")
        modules, aliases = _module_records(source, base)
        physical_modules.update(tuple(module.split("::")) for module in modules)
        for logical, relative_target in aliases:
            target = (path.parent / relative_target).resolve()
            try:
                physical_target = file_module_path(src_root, target)
            except ValueError as exc:
                raise ValueError(
                    f"#[path] target escapes src/: {path.relative_to(repo_root)} -> "
                    f"{relative_target}"
                ) from exc
            previous = raw_aliases.get(physical_target)
            if previous is not None and previous != logical:
                raise ValueError(
                    f"conflicting #[path] aliases for {target.relative_to(repo_root)}: "
                    f"{'::'.join(previous)} vs {'::'.join(logical)}"
                )
            raw_aliases[physical_target] = logical

    aliases = dict(raw_aliases)
    for _ in range(len(aliases) + 1):
        updated = {
            physical: _normalize_alias_path(logical, aliases)
            for physical, logical in aliases.items()
        }
        if updated == aliases:
            break
        aliases = updated

    return {
        "::".join(_normalize_alias_path(module, aliases))
        for module in physical_modules
    }


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


def cargo_test_filter(command: str) -> LaneFilter | None:
    """Parse one library cargo-test command's positive and skip filters."""
    cargo = command.find("cargo test")
    if cargo < 0:
        return None
    try:
        words = shlex.split(command[cargo:], comments=True)
    except ValueError as exc:
        raise ValueError(f"cannot parse cargo test command: {command!r}: {exc}") from exc
    if words[:2] != ["cargo", "test"]:
        return None

    args = words[2:]
    before, after = args, []
    if "--" in args:
        split = args.index("--")
        before, after = args[:split], args[split + 1 :]
    if any(option in before for option in _NON_LIB_TARGET_OPTIONS) and "--all-targets" not in before:
        return None

    positives: list[str] = []
    skip_next = False
    for token in before:
        if skip_next:
            skip_next = False
            continue
        if token in _CARGO_VALUE_OPTIONS:
            skip_next = True
            continue
        if token.startswith("-"):
            continue
        positives.append(token)

    skips: list[str] = []
    exact = False
    index = 0
    while index < len(after):
        token = after[index]
        if token == "--exact":
            exact = True
        elif token == "--skip":
            if index + 1 >= len(after):
                raise ValueError(f"--skip has no value: {command!r}")
            skips.append(after[index + 1])
            index += 1
        elif token.startswith("--skip="):
            skips.append(token.partition("=")[2])
        elif token in _LIBTEST_VALUE_OPTIONS:
            index += 1
        elif token.startswith("--test-threads="):
            pass
        elif not token.startswith("-"):
            positives.append(token)
        index += 1

    return LaneFilter(tuple(positives), tuple(skips), exact)


def discover_lane_filters(repo_root: Path) -> tuple[LaneFilter, ...]:
    """Parse selection contracts from test-non-pg and PR targeted commands."""
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

    for recipe in sorted(set(re.findall(r"\bjust\s+([A-Za-z0-9_-]+)", workflow))):
        try:
            commands.extend(just_recipe_commands(just_text, recipe))
        except ValueError:
            continue

    lanes: list[LaneFilter] = []
    for command in commands:
        lane = cargo_test_filter(command)
        if lane is not None:
            lanes.append(lane)
    return tuple(dict.fromkeys(lanes))


def uncovered_modules(
    modules: Iterable[str], lanes: Iterable[LaneFilter]
) -> set[str]:
    """Return modules not fully selected by any single curated invocation."""
    active = tuple(lanes)
    return {
        module
        for module in modules
        if not any(lane.fully_selects(module) for lane in active)
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


def check(
    repo_root: Path,
    baseline_path: Path,
    expected_baseline_count: int = BASELINE_ENTRY_COUNT,
    *,
    emit_success: bool = True,
) -> int:
    modules = discover_test_modules(repo_root)
    lanes = discover_lane_filters(repo_root)
    current = uncovered_modules(modules, lanes)
    baseline = load_baseline(baseline_path)

    if len(baseline) != expected_baseline_count:
        direction = "growth" if len(baseline) > expected_baseline_count else "shrinkage"
        print(
            f"FAIL: baseline {direction}: {len(baseline)} entries, but the locked "
            f"count is {expected_baseline_count}.",
            file=sys.stderr,
        )
        print(
            "Update BASELINE_ENTRY_COUNT only when review intentionally accepts a "
            "smaller corrected debt set; baseline growth is forbidden.",
            file=sys.stderr,
        )
        return 1

    new = sorted(current - baseline)
    stale = sorted(baseline - current)
    if new or stale:
        print(
            f"FAIL: coverage baseline drift: {len(new)} newly uncovered, "
            f"{len(stale)} stale/covered, {len(current)} currently uncovered "
            f"(locked baseline {len(baseline)}).",
            file=sys.stderr,
        )
        for module in new:
            print(f"  + {module}", file=sys.stderr)
        for module in stale:
            print(f"  - {module}", file=sys.stderr)
        print(
            "Add broad module coverage for '+' entries. Remove '-' entries and "
            "lower BASELINE_ENTRY_COUNT to lock in debt reduction.",
            file=sys.stderr,
        )
        return 1

    if emit_success:
        print(
            f"OK: {len(modules)} logical Rust cfg(test) modules inventoried; "
            f"{len(current)} uncovered module(s) exactly match the locked baseline; "
            f"{len(lanes)} curated cargo-test invocation(s)."
        )
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo-root", type=Path, default=REPO_ROOT)
    parser.add_argument("--baseline", type=Path, default=None)
    args = parser.parse_args(argv)
    repo_root = args.repo_root.resolve()
    baseline = args.baseline.resolve() if args.baseline else repo_root / BASELINE_REL
    try:
        return check(repo_root, baseline)
    except (OSError, ValueError) as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
