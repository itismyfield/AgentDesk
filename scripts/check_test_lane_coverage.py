#!/usr/bin/env python3
"""Reject new Rust library test modules that no curated CI lane fully selects.

AgentDesk's main-push ``test-non-pg`` recipe intentionally uses libtest name
filters instead of running every library test. This source-only guard finds
``#[cfg(test)] mod ...`` declarations, derives their logical Rust module paths
(including ``#[path = "..."]`` aliases), and compares them with each curated
``cargo test`` command's positive and ``--skip`` filters.

The existing uncovered set is recorded as sorted names in the baseline file.
The checked-out candidate baseline must be a subset of an immutable reference
snapshot, and any newly uncovered module or stale entry also fails. Baseline
debt can therefore only shrink without a redundant scalar lock.
"""

from __future__ import annotations

import argparse
import fnmatch
import hashlib
import importlib.util
import re
import shlex
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Iterable

_PROVENANCE_SPEC = importlib.util.spec_from_file_location(
    "test_lane_candidate_provenance",
    Path(__file__).with_name("test_lane_candidate_provenance.py"),
)
assert _PROVENANCE_SPEC and _PROVENANCE_SPEC.loader
provenance = importlib.util.module_from_spec(_PROVENANCE_SPEC)
sys.modules[_PROVENANCE_SPEC.name] = provenance
_PROVENANCE_SPEC.loader.exec_module(provenance)

REPO_ROOT = Path(__file__).resolve().parent.parent
BASELINE_REL = Path("scripts/test_lane_coverage_baseline.txt")
PR_LANE_MANIFEST_REL = provenance.PR_LANE_MANIFEST_REL
DEFAULT_PR_TEST_PATHS = ("src/**",)

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
ATTRIBUTED_FN_RE = re.compile(
    r"(?P<attrs>(?:#\s*\[[^\]]*\]\s*)+)"
    r"(?:(?:pub(?:\s*\([^)]*\))?)\s+)?"
    r"(?:async\s+)?(?:unsafe\s+)?fn\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)",
    re.MULTILINE,
)
TEST_ATTR_RE = re.compile(
    r"#\s*\[\s*(?:(?:tokio|async_std|actix_rt)::)?test\b(?:\([^\]]*\))?\s*\]"
)

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
    "--bin",
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
    command: str = field(default="", compare=False)
    provenance: str = field(default="", compare=False)
    changed_paths: tuple[str, ...] = field(
        default=DEFAULT_PR_TEST_PATHS, compare=False
    )
    target: str = field(default="lib", compare=False)

    def selects(self, test_name: str) -> bool:
        """Model libtest's union of positives, exactness, and skip vetoes."""
        if self.target.startswith("bin:"):
            return False
        positive_match = not self.positives or any(
            test_name == positive if self.exact else positive in test_name
            for positive in self.positives
        )
        return positive_match and not any(skip in test_name for skip in self.skips)

    def fully_selects(self, module: str, test_names: Iterable[str]) -> bool:
        """Whether this command selects every discovered test in the module.

        Module debt retains its historical conservative contract: exact pins do
        not exempt a whole module, positive filters must match the module path,
        and any matching skip makes the module only partially covered.
        Changed-test provenance uses ``selects`` directly at full-name level.
        """
        if self.target.startswith("bin:") or self.exact:
            return False
        positive_match = not self.positives or any(
            positive in module for positive in self.positives
        )
        if not positive_match or any(skip in module for skip in self.skips):
            return False
        return not any(
            skip in test_name
            for test_name in test_names
            for skip in self.skips
        )

    def is_applicable_to(self, changed_paths: Iterable[str]) -> bool:
        return any(
            fnmatch.fnmatch(path, pattern)
            for path in changed_paths
            for pattern in self.changed_paths
        )


@dataclass(frozen=True)
class SourceInventory:
    tests_by_module: dict[str, set[str]]
    test_fingerprints: dict[str, str]
    unsupported_tests: tuple[str, ...] = ()
    test_modules: dict[str, str] = field(default_factory=dict)


ChangedTest = provenance.ChangedTest
PrLaneManifest = provenance.PrLaneManifest
CheckResult = provenance.CheckResult


@dataclass(frozen=True)
class SourceMount:
    logical_prefix: tuple[str, ...]
    physical_file: str


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
    for match in ATTRIBUTED_MOD_RE.finditer(clean):
        attributes[match.start("name")] = source[
            match.start("attrs") : match.end("attrs")
        ]

    declarations: list[tuple[int, int, str, str, str]] = []
    for match in MOD_RE.finditer(clean):
        attrs = attributes.get(match.start("name"), "")
        declarations.append(
            (match.start(), match.end(), match.group("name"), match.group("term"), attrs)
        )

    test_functions = {
        match.start("name"): match.group("name")
        for match in ATTRIBUTED_FN_RE.finditer(clean)
        if TEST_ATTR_RE.search(source[match.start("attrs") : match.end("attrs")])
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
        is_test_module = bool(CFG_TEST_RE.search(attrs))
        if is_test_module:
            modules.add("::".join(logical))
            tests_by_module.setdefault("::".join(logical), set())
        path_attr = PATH_ATTR_RE.search(attrs)
        if path_attr and term == ";":
            aliases.append((logical, path_attr.group("path"), parent_names))

        if term == "{":
            depth += 1
            inline_stack.append((depth, name, is_test_module))
        cursor = end
    return modules, tests_by_module, aliases


def test_modules_in_source(source: str, base: tuple[str, ...]) -> set[str]:
    """Find cfg(test) module paths in one Rust source file."""
    modules, _, _ = _module_records(source, base)
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


def _test_item_end(clean: str, fn_start: int) -> int:
    """Return one test function item's end, including a braced body."""
    opening = clean.find("{", fn_start)
    semicolon = clean.find(";", fn_start)
    if opening < 0 or (semicolon >= 0 and semicolon < opening):
        return semicolon + 1 if semicolon >= 0 else len(clean)
    depth = 0
    for index in range(opening, len(clean)):
        if clean[index] == "{":
            depth += 1
        elif clean[index] == "}":
            depth -= 1
            if depth == 0:
                return index + 1
    raise ValueError("unterminated test function body")


def _without_rust_comments(source: str) -> str:
    """Remove Rust comments while preserving literals and token spelling."""
    out: list[str] = []
    index = 0
    block_depth = 0
    while index < len(source):
        if block_depth:
            if source.startswith("/*", index):
                block_depth += 1
                index += 2
            elif source.startswith("*/", index):
                block_depth -= 1
                index += 2
            else:
                index += 1
            continue
        if source.startswith("//", index):
            newline = source.find("\n", index)
            if newline < 0:
                break
            out.append("\n")
            index = newline + 1
            continue
        if source.startswith("/*", index):
            block_depth = 1
            index += 2
            continue
        raw = _RAW_STRING_OPEN.match(source, index)
        if raw:
            closer = '"' + "#" * len(raw.group(1))
            end = source.find(closer, raw.end())
            end = len(source) if end < 0 else end + len(closer)
            out.append(source[index:end])
            index = end
            continue
        string_width = 2 if source.startswith('b"', index) else 1
        if source[index] == '"' or string_width == 2:
            end = index + string_width
            while end < len(source):
                if source[end] == "\\" and end + 1 < len(source):
                    end += 2
                elif source[end] == '"':
                    end += 1
                    break
                else:
                    end += 1
            out.append(source[index:end])
            index = end
            continue
        if source[index] == "'":
            char = _CHAR_LITERAL.match(source, index)
            if char:
                out.append(char.group())
                index = char.end()
                continue
        out.append(source[index])
        index += 1
    return "".join(out)


def _material_test_item(source: str, start: int, end: int) -> str:
    """Drop comments and insignificant whitespace while retaining literals."""
    without_comments = _without_rust_comments(source[start:end])
    return re.sub(r"\s+", "", without_comments)


def _source_test_fingerprints(source: str, base: tuple[str, ...]) -> dict[str, str]:
    clean = strip_rust(source)
    attributed_tests = [
        match
        for match in ATTRIBUTED_FN_RE.finditer(clean)
        if TEST_ATTR_RE.search(source[match.start("attrs") : match.end("attrs")])
    ]
    if not attributed_tests:
        return {}

    declarations: list[tuple[int, int, str, str]] = []
    for match in MOD_RE.finditer(clean):
        declarations.append(
            (match.start(), match.end(), match.group("name"), match.group("term"))
        )
    events = sorted(
        [(start, "mod", (end, name, term)) for start, end, name, term in declarations]
        + [(match.start("name"), "test_fn", match) for match in attributed_tests],
        key=lambda event: event[0],
    )
    inline_stack: list[tuple[int, str]] = []
    depth = 0
    cursor = 0
    fingerprints: dict[str, str] = {}
    for offset, kind, payload in events:
        for brace in re.finditer(r"[{}]", clean[cursor:offset]):
            if brace.group() == "{":
                depth += 1
            else:
                depth -= 1
                while inline_stack and inline_stack[-1][0] > depth:
                    inline_stack.pop()
        if kind == "test_fn":
            match = payload
            name = "::".join((*base, *(item[1] for item in inline_stack), match.group("name")))
            item_start = match.start("attrs")
            item_end = _test_item_end(clean, match.end())
            item = _material_test_item(source, item_start, item_end)
            fingerprints[name] = hashlib.sha256(item.encode("utf-8")).hexdigest()
            cursor = offset
            continue
        end, name, term = payload
        if term == "{":
            depth += 1
            inline_stack.append((depth, name))
        cursor = end
    return fingerprints


def _include_mounts(
    repo_root: Path, src_root: Path
) -> tuple[dict[Path, SourceMount], tuple[str, ...]]:
    mounts: dict[Path, SourceMount] = {}
    unsupported: list[str] = []
    include_re = re.compile(r'\binclude!\s*\(\s*"(?P<path>[^"]+)"\s*\)')
    for parent in sorted(src_root.rglob("*.rs")):
        rel = parent.relative_to(src_root)
        if rel.name == "main.rs" or (rel.parts and rel.parts[0] == "bin"):
            continue
        source = parent.read_text(encoding="utf-8")
        clean = strip_rust(source)
        base = file_module_path(src_root, parent)
        declarations = [
            (match.start(), match.end(), match.group("name"), match.group("term"))
            for match in MOD_RE.finditer(clean)
        ]
        include_matches = list(include_re.finditer(source))
        events = sorted(
            [(start, "mod", (end, name, term)) for start, end, name, term in declarations]
            + [(match.start(), "include", match) for match in include_matches],
            key=lambda event: event[0],
        )
        stack: list[tuple[int, str]] = []
        depth = 0
        cursor = 0
        for offset, kind, payload in events:
            for brace in re.finditer(r"[{}]", clean[cursor:offset]):
                if brace.group() == "{":
                    depth += 1
                else:
                    depth -= 1
                    while stack and stack[-1][0] > depth:
                        stack.pop()
            if kind == "mod":
                end, name, term = payload
                if term == "{":
                    depth += 1
                    stack.append((depth, name))
                cursor = end
                continue
            match = payload
            target = (parent.parent / match.group("path")).resolve()
            try:
                target.relative_to(src_root)
            except ValueError:
                unsupported.append(
                    f"{parent.relative_to(repo_root)}: include! target escapes src/"
                )
            else:
                mount = SourceMount(
                    (*base, *(item[1] for item in stack)),
                    str(parent.relative_to(repo_root)),
                )
                previous = mounts.get(target)
                if previous is not None and previous != mount:
                    unsupported.append(
                        f"{target.relative_to(repo_root)}: include! has multiple logical mounts"
                    )
                mounts[target] = mount
            cursor = match.end()
    return mounts, tuple(unsupported)


def discover_source_inventory(repo_root: Path) -> SourceInventory:
    """Inventory logical tests and material-body fingerprints without building."""
    repo_root = repo_root.resolve()
    src_root = (repo_root / "src").resolve()
    include_mounts, include_failures = _include_mounts(repo_root, src_root)
    physical_inventory: dict[tuple[str, ...], set[tuple[str, ...]]] = {}
    physical_fingerprints: dict[tuple[str, ...], str] = {}
    raw_aliases: dict[tuple[str, ...], tuple[str, ...]] = {}
    unsupported: list[str] = list(include_failures)

    for path in sorted(src_root.rglob("*.rs")):
        rel = path.relative_to(src_root)
        if rel.name == "main.rs" or (rel.parts and rel.parts[0] == "bin"):
            continue
        mount = include_mounts.get(path.resolve())
        base = mount.logical_prefix if mount else file_module_path(src_root, path)
        source = path.read_text(encoding="utf-8")
        modules, tests_by_module, aliases = _module_records(source, base)
        fingerprints = _source_test_fingerprints(source, base)
        for module in modules:
            physical = tuple(module.split("::"))
            physical_inventory.setdefault(physical, set()).update(
                tuple(test.split("::"))
                for test in tests_by_module.get(module, set())
            )
        if mount and fingerprints:
            owner_module = (
                "::".join(base)
                if base and base[-1] == "tests"
                else "::".join(base[:-1] or base)
            )
            owner_path = tuple(owner_module.split("::"))
            physical_inventory.setdefault(owner_path, set()).update(
                tuple(test.split("::")) for test in fingerprints
            )
        for test_name, fingerprint in fingerprints.items():
            physical_fingerprints[tuple(test_name.split("::"))] = fingerprint
        for logical, relative_target, inline_parents in aliases:
            target = (path.parent.joinpath(*inline_parents) / relative_target).resolve()
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

    inventory: dict[str, set[str]] = {}
    fingerprints: dict[str, str] = {}
    for physical_module, physical_tests in physical_inventory.items():
        module = "::".join(_normalize_alias_path(physical_module, aliases))
        inventory.setdefault(module, set()).update(
            "::".join(_normalize_alias_path(test, aliases))
            for test in physical_tests
        )
    for physical_test, fingerprint in physical_fingerprints.items():
        fingerprints["::".join(_normalize_alias_path(physical_test, aliases))] = fingerprint
    test_modules = {
        test_name: module
        for module, test_names in inventory.items()
        for test_name in test_names
    }
    for test_name in fingerprints:
        if test_name in test_modules:
            continue
        candidates = [
            module
            for module in inventory
            if test_name.startswith(f"{module}::")
        ]
        if candidates:
            test_modules[test_name] = max(candidates, key=len)
    return SourceInventory(
        inventory, fingerprints, tuple(unsupported), test_modules
    )


def discover_test_inventory(repo_root: Path) -> dict[str, set[str]]:
    """Inventory logical test modules and test paths without building."""
    return discover_source_inventory(repo_root).tests_by_module


def discover_test_modules(repo_root: Path) -> set[str]:
    """Inventory logical Rust library cfg(test) modules without building."""
    return set(discover_test_inventory(repo_root))


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


def cargo_test_filter(
    command: str,
    *,
    provenance: str = "",
    changed_paths: tuple[str, ...] = DEFAULT_PR_TEST_PATHS,
) -> LaneFilter | None:
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
    # The package's `agentdesk` binary has no unit-test modules of its own; its
    # historical high-risk command therefore selected zero tests. Model that target so
    # the manifest can prove the real PR lane is non-vacuous; reject unrelated
    # target-specific commands because this inventory only describes lib.rs.
    target = "all" if "--all-targets" in before else "lib"
    target_options = [option for option in before if option in _NON_LIB_TARGET_OPTIONS]
    if target_options and "--all-targets" not in before:
        if "--bin" not in before:
            return None
        binary_index = before.index("--bin")
        binary = before[binary_index + 1 : binary_index + 2]
        if binary != ["agentdesk"]:
            return None
        target = "bin:agentdesk"

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

    return LaneFilter(
        tuple(positives),
        tuple(skips),
        exact,
        command,
        provenance,
        changed_paths,
        target,
    )


def load_pr_lane_manifest(path: Path) -> PrLaneManifest:
    return provenance.load_pr_lane_manifest(path, cargo_test_filter)


def discover_lane_filters(repo_root: Path) -> tuple[LaneFilter, ...]:
    """Parse selection contracts from positive main-push and PR test lanes."""
    just_text = (repo_root / "justfile").read_text(encoding="utf-8")
    workflows = (
        (repo_root / ".github/workflows/ci-main.yml").read_text(encoding="utf-8"),
        (repo_root / ".github/workflows/ci-pr.yml").read_text(encoding="utf-8"),
    )

    commands = list(just_recipe_commands(just_text, "test-non-pg"))
    for workflow in workflows:
        for line in workflow.splitlines():
            command = line.strip()
            if "cargo test" not in command or command.startswith("#"):
                continue
            if command.startswith("run:"):
                command = command.removeprefix("run:").strip()
                if (
                    len(command) >= 2
                    and command[0] == command[-1]
                    and command[0] in "\"'"
                ):
                    command = command[1:-1]
            commands.append(command)

        for recipe in sorted(
            set(re.findall(r"\bjust\s+([A-Za-z0-9_-]+)", workflow))
        ):
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
    modules: Iterable[str] | dict[str, set[str]], lanes: Iterable[LaneFilter]
) -> set[str]:
    """Return modules not fully selected by any single curated invocation."""
    inventory = modules if isinstance(modules, dict) else {module: set() for module in modules}
    active = tuple(lanes)
    return {
        module
        for module, test_names in inventory.items()
        if not any(lane.fully_selects(module, test_names) for lane in active)
    }


resolve_candidate_commit = provenance.resolve_commit
changed_paths = provenance.changed_paths
source_tree_at_commit = provenance.source_tree_at_commit
changed_tests_between = provenance.changed_tests_between
ensure_non_vacuous_filters = provenance.ensure_non_vacuous_filters


def verify_pr_lane_manifest(
    repo_root: Path, manifest_path: Path
) -> PrLaneManifest:
    return provenance.verify_pr_lane_manifest(
        repo_root, manifest_path, cargo_test_filter, just_recipe_commands
    )


def check_pr_candidate(
    repo_root: Path,
    base_sha: str,
    *,
    manifest_path: Path | None = None,
) -> CheckResult:
    return provenance.check_pr_candidate(
        repo_root,
        base_sha,
        discover_source_inventory=discover_source_inventory,
        cargo_test_filter=cargo_test_filter,
        just_recipe_commands=just_recipe_commands,
        manifest_path=manifest_path,
    )


def parse_baseline(text: str, source: str) -> set[str]:
    """Parse a sorted one-module-per-line debt baseline."""
    entries = [
        line.strip()
        for line in text.splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]
    if entries != sorted(entries):
        raise ValueError(f"baseline entries must be sorted: {source}")
    if len(entries) != len(set(entries)):
        raise ValueError(f"baseline contains duplicate entries: {source}")
    return set(entries)


def load_baseline(path: Path) -> set[str]:
    """Read the working-tree debt baseline."""
    return parse_baseline(path.read_text(encoding="utf-8"), str(path))


def resolve_commit(repo_root: Path, ref: str) -> str:
    """Resolve a baseline reference once to an immutable commit object."""
    if not ref or set(ref) == {"0"}:
        raise ValueError(f"invalid baseline reference: {ref or '<empty>'}")
    try:
        result = subprocess.run(
            ["git", "rev-parse", "--verify", f"{ref}^{{commit}}"],
            cwd=repo_root,
            check=True,
            capture_output=True,
            text=True,
        )
    except (OSError, subprocess.CalledProcessError) as exc:
        detail = getattr(exc, "stderr", "") or str(exc)
        raise ValueError(
            f"cannot resolve baseline reference {ref!r}: {detail.strip()}"
        ) from exc
    sha = result.stdout.strip()
    if not re.fullmatch(r"[0-9a-fA-F]{40,64}", sha):
        raise ValueError(f"git returned an invalid commit id for {ref!r}: {sha!r}")
    return sha


def load_baseline_from_git(repo_root: Path, ref: str) -> tuple[str, set[str]]:
    """Read the baseline blob from one immutable commit snapshot."""
    sha = resolve_commit(repo_root, ref)
    source = f"{sha}:{BASELINE_REL.as_posix()}"
    try:
        result = subprocess.run(
            ["git", "show", source],
            cwd=repo_root,
            check=True,
            capture_output=True,
            text=True,
        )
    except (OSError, subprocess.CalledProcessError) as exc:
        detail = getattr(exc, "stderr", "") or str(exc)
        raise ValueError(
            f"cannot read reference baseline {source}: {detail.strip()}"
        ) from exc
    return sha, parse_baseline(result.stdout, source)


def baseline_growth(
    baseline: set[str], reference_baseline: set[str]
) -> list[str]:
    """Return candidate entries absent from the immutable reference."""
    return sorted(baseline - reference_baseline)


def check(
    repo_root: Path,
    baseline_path: Path,
    reference_baseline: set[str],
    *,
    reference_label: str = "reference snapshot",
    emit_success: bool = True,
    base_sha: str | None = None,
) -> int:
    source_inventory = discover_source_inventory(repo_root)
    inventory = source_inventory.tests_by_module
    lanes = discover_lane_filters(repo_root)
    current = uncovered_modules(inventory, lanes)
    baseline = load_baseline(baseline_path)

    vacuous = ensure_non_vacuous_filters(lanes, source_inventory)
    if vacuous:
        print(
            f"FAIL: {len(vacuous)} curated cargo-test filter(s) select zero tests.",
            file=sys.stderr,
        )
        for failure in vacuous:
            print(f"  - {failure}", file=sys.stderr)
        return 1

    growth = baseline_growth(baseline, reference_baseline)
    if growth:
        print(
            f"FAIL: baseline growth forbidden: {len(growth)} entr"
            f"{'y' if len(growth) == 1 else 'ies'} absent from {reference_label}.",
            file=sys.stderr,
        )
        for module in growth:
            print(f"  + {module}", file=sys.stderr)
        print(
            "Remove '+' entries; the candidate baseline may only preserve or "
            "remove debt from its immutable reference snapshot.",
            file=sys.stderr,
        )
        return 1

    new = sorted(current - baseline)
    stale = sorted(baseline - current)
    if new or stale:
        print(
            f"FAIL: coverage baseline drift: {len(new)} newly uncovered, "
            f"{len(stale)} stale/covered, {len(current)} currently uncovered "
            f"(candidate baseline {len(baseline)}).",
            file=sys.stderr,
        )
        for module in new:
            print(f"  + {module}", file=sys.stderr)
        for module in stale:
            print(f"  - {module}", file=sys.stderr)
        print(
            "Add broad module coverage for '+' entries. Remove '-' entries to "
            "lock in debt reduction.",
            file=sys.stderr,
        )
        return 1

    pr_result = None
    if base_sha is not None:
        pr_result = check_pr_candidate(repo_root, base_sha)
        if pr_result.failures:
            print(
                f"FAIL: PR test-lane provenance: {len(pr_result.failures)} violation(s).",
                file=sys.stderr,
            )
            for failure in pr_result.failures:
                print(f"  - {failure}", file=sys.stderr)
            return 1
        if emit_success:
            for changed in pr_result.changed_tests:
                owners = ", ".join(pr_result.coverage[changed.name])
                print(f"PR-COVERED: {changed.change} {changed.name} -> {owners}")

    if emit_success:
        removed = len(reference_baseline - baseline)
        pr_suffix = (
            f"; {len(pr_result.changed_tests)} changed test(s) have PR provenance"
            if pr_result is not None
            else ""
        )
        print(
            f"OK: {len(inventory)} logical Rust cfg(test) modules and "
            f"{sum(map(len, inventory.values()))} test function(s) inventoried; "
            f"{len(current)} uncovered module(s) exactly match the candidate "
            f"baseline, which removed {removed} debt entr"
            f"{'y' if removed == 1 else 'ies'} from {reference_label}; "
            f"{len(lanes)} curated cargo-test invocation(s){pr_suffix}."
        )
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo-root", type=Path, default=REPO_ROOT)
    parser.add_argument("--baseline", type=Path, default=None)
    parser.add_argument("--baseline-ref", required=True)
    parser.add_argument(
        "--base-sha",
        help="explicit immutable PR base commit for changed-test provenance",
    )
    args = parser.parse_args(argv)
    repo_root = args.repo_root.resolve()
    baseline = args.baseline.resolve() if args.baseline else repo_root / BASELINE_REL
    try:
        reference_sha, reference_baseline = load_baseline_from_git(
            repo_root, args.baseline_ref
        )
        return check(
            repo_root,
            baseline,
            reference_baseline,
            reference_label=f"commit {reference_sha}",
            base_sha=args.base_sha,
        )
    except (OSError, ValueError) as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
