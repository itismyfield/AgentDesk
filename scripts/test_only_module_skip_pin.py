"""Single lexical pin for whole-file skips in the two #5071 writer gates.

Ordinary basename skips follow exactly ``tests.rs`` / ``*_tests.rs``.
Production module mounts of those files are rejected; higher-risk resolver
skips must match the immutable repo-relative pin before either gate scans.

The reused resolver is deliberately unchanged.  It does not guarantee at
least these seven measured Rust forms: a comment between ``#[path]`` and
``mod``; a macro-generated production ``mod``; ``cfg(not(test))`` plus
``include!``; ``cfg(any(test, feature))`` plus ``include!``;
``cfg_attr(path = ...)``; a raw-string ``#[path]``; or an ungated
``include!``.

This pin guarantees membership, not compiler-backed reachability.  In
particular, a content change can make an already pinned file production-
reachable without changing the pinned path set.  Compiler-backed reachability
is follow-up slice work.  The lexical scan root is ``src/``; call sites in files
reached by ``#[path]``/``include!`` targets resolving outside ``src/`` are not
seen.  Fail-closed handling for that boundary is follow-up slice work.  Symlink
rejection is lexical rather than atomic: a post-enumeration filesystem
replacement is outside the guarantee; CI assumes a static checkout while a
gate runs.
"""

from __future__ import annotations

import importlib.util
import os
import re
import sys
from collections.abc import Callable, Iterable
from pathlib import Path

try:
    from scripts.rust_lex import StripState, strip_line
except ModuleNotFoundError:
    from rust_lex import StripState, strip_line


def _load_sibling(name):
    if name in sys.modules:
        return sys.modules[name]
    spec = importlib.util.spec_from_file_location(name, Path(__file__).resolve().with_name(name + ".py"))
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load scripts/{name}.py")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


PINNED_RESOLVER_TEST_ONLY_FILES = _load_sibling(
    "skip_pin_registry"
).load(Path(__file__).resolve().parents[1]).frozensets()[1]
# Basenames are classified by rule; only resolver exclusions need review pins.
PINNED_TEST_ONLY_MODULE_FILES = PINNED_RESOLVER_TEST_ONLY_FILES

_UPDATE_HINT = (
    "Review basename and resolver classification in scripts/test_only_module_skip_pin.py, "
    "then register resolver drift in "
    "scripts/test_only_module_skip_pin.d/; "
    "tests.rs / *_tests.rs need no path pin."
)

_SYMLINK_HINT = (
    "Remove the symlink or replace it with a regular in-tree file; do not add "
    "it to the writer-gate skip pin."
)


_INVENTORY = _load_sibling("generate_inventory_docs")


def is_basename_test_file(name: str) -> bool:
    return name == "tests.rs" or name.endswith("_tests.rs")


def _has_production_items(text: str) -> bool:
    # Some mounted fixtures contain only imports and test-gated inline modules.
    # Mask exact spans so a production item on the same line stays visible.
    spans = []
    for match in _INVENTORY._CFG_MOD_RE.finditer(text):
        if _INVENTORY.cfg_requires_test(match.group("predicate")):
            try:
                _, end = _INVENTORY.scan_balanced(text, match.end() - 1, "{", "}")
            except _INVENTORY.ParseError:
                continue
            spans.append((match.start(), end))
    for start, end in reversed(spans):
        text = text[:start] + " " * (end - start) + text[end:]
    state = StripState()
    code = "\n".join(strip_line(line, state) for line in text.splitlines())
    return re.fullmatch(r"(?:\s*(?:pub(?:\([^)]*\))?\s+)?use\b[^;]*;)*\s*", code) is None


def validate_basename_mounts(
    all_files: Iterable[Path],
    basename_skips: set[Path],
    resolver_skips: set[Path],
    *,
    read_text_fn: Callable[[Path], str] = lambda path: path.read_text(encoding="utf-8"),
) -> None:
    """Reject lexical production module declarations targeting basename skips."""
    targets = {path.resolve() for path in basename_skips}
    for parent in all_files:
        if parent in basename_skips or parent in resolver_skips:
            continue
        for declaration in _INVENTORY._module_file_declarations(parent, read_text_fn(parent)):
            if declaration.requires_test:
                continue
            for child in declaration.child_paths:
                if child in targets and _has_production_items(read_text_fn(child)):
                    raise RuntimeError(
                        "FAIL: production module mounts basename test file: "
                        f"{parent.as_posix()} -> {child.as_posix()}. "
                        "Require a test-only cfg on the module or use a production basename."
                    )


def skip_pin_drift(
    excluded_paths: Iterable[str],
    pinned_paths: Iterable[str] = PINNED_TEST_ONLY_MODULE_FILES,
) -> str | None:
    """Accept basename rules and compare resolver paths in both directions."""

    actual = frozenset(
        Path(path).as_posix() for path in excluded_paths
        if not is_basename_test_file(Path(path).name)
    )
    expected = frozenset(Path(path).as_posix() for path in pinned_paths)
    if actual == expected:
        return None
    lines = ["FAIL: writer-gate whole-file skip set drift."]
    scan_only = sorted(actual - expected)
    pin_only = sorted(expected - actual)
    if scan_only:
        lines.append("  scan-only (newly skipped): " + ", ".join(scan_only))
    if pin_only:
        lines.append("  pin-only (no longer skipped): " + ", ".join(pin_only))
    lines.append("  " + _UPDATE_HINT)
    return "\n".join(lines)


def _lexical_rust_files(root: Path, scan_root: Path) -> list[Path]:
    """Enumerate every regular file and reject symlinks/non-``.rs`` files."""

    source = root / scan_root
    symlinks: list[str] = []
    non_rust: list[str] = []
    if source.is_symlink():
        symlinks.append(scan_root.as_posix())
    files: list[Path] = []
    if source.is_dir() and not symlinks:
        for directory, dirnames, filenames in os.walk(source, followlinks=False):
            directory_path = Path(directory)
            for name in sorted((*dirnames, *filenames)):
                path = directory_path / name
                if path.is_symlink():
                    symlinks.append(path.relative_to(root).as_posix())
            for name in sorted(filenames):
                path = directory_path / name
                if path.is_symlink() or not path.is_file():
                    # The ``not path.is_file()`` half is this enumerator's sole
                    # non-fail-closed branch: git checkouts cannot carry a FIFO,
                    # while the old read-text path would block on one. Symlinks
                    # were recorded above and still fail closed; only this
                    # non-regular case is silently omitted here.
                    continue
                files.append(path)
                if not name.endswith(".rs"):
                    non_rust.append(path.relative_to(root).as_posix())
    if symlinks:
        raise RuntimeError(
            "FAIL: writer gates reject file or directory symlinks under src/: "
            + ", ".join(sorted(symlinks))
            + ". "
            + _SYMLINK_HINT
        )
    if non_rust:
        raise RuntimeError(
            "FAIL: writer gates reject non-.rs regular files under src/: "
            + ", ".join(sorted(non_rust))
            + ". The writer gate cannot classify non-.rs files under src/; "
            "remove them or extend the gate policy."
        )
    return sorted(files)


def validated_scan_files(
    root: Path,
    scan_root: Path,
    is_test_file: Callable[[str], bool],
    *,
    pinned_paths: Iterable[str] = PINNED_TEST_ONLY_MODULE_FILES,
) -> tuple[list[Path], frozenset[Path]]:
    """Enumerate, classify, pin-check, and return all files plus whole skips."""

    pinned = frozenset(Path(path).as_posix() for path in pinned_paths)
    all_files = _lexical_rust_files(root, scan_root)
    basename_skips = {path for path in all_files if is_test_file(path.name)}
    rule_skips = {path for path in all_files if is_basename_test_file(path.name)}
    if basename_skips != rule_skips:
        raise RuntimeError("FAIL: writer-gate basename classifier differs from tests.rs / *_tests.rs rule")
    production_files = [path for path in all_files if path not in basename_skips]
    if not production_files:
        if all_files:
            detail = (
                "all enumerated regular .rs files are basename-classified "
                "test-only files"
            )
        else:
            detail = "the lexical src/ enumeration found no regular .rs files"
        raise RuntimeError(
            "FAIL: writer-gate production file list is empty; the shared "
            "test-only resolver was not invoked because its empty-input "
            "fallback would scan the repository instead of this lexical src/ "
            "enumeration. "
            + detail
            + ". Restore a production .rs file under src/ or "
            "extend the gate policy."
        )
    resolved_to_lexical = {path.resolve(): path for path in all_files}
    texts: dict[Path, str] = {}

    def read_text(path: Path) -> str:
        if path not in texts:
            texts[path] = path.read_text(encoding="utf-8")
        return texts[path]

    resolver_results = _INVENTORY.test_only_module_files(
        production_files=production_files,
        all_files=all_files,
        read_text_fn=read_text,
    )
    resolver_skips: set[Path] = set()
    unmapped: list[str] = []
    for result in resolver_results:
        lexical = resolved_to_lexical.get(result.resolve())
        if lexical is None:
            unmapped.append(result.as_posix())
        else:
            resolver_skips.add(lexical)
    if unmapped:
        raise RuntimeError(
            "FAIL: test-only resolver returned paths outside lexical src/ enumeration: "
            + ", ".join(sorted(unmapped))
            + ". "
            + _UPDATE_HINT
        )

    validate_basename_mounts(
        all_files, basename_skips, resolver_skips, read_text_fn=read_text
    )
    whole_skips = frozenset(basename_skips | resolver_skips)
    lexical_skips = {
        path.relative_to(root).as_posix() for path in whole_skips
    }
    drift = skip_pin_drift(lexical_skips, pinned)
    pinned_count = len(basename_skips) + len(pinned)
    if len(whole_skips) != pinned_count:
        census = (
            "FAIL: writer-gate skipped census differs from pin count "
            f"({len(whole_skips)} skipped, {len(basename_skips)} basename + "
            f"{len(pinned)} resolver pinned).\n  {_UPDATE_HINT}"
        )
        raise RuntimeError(f"{drift}\n{census}" if drift else census)
    if drift:
        raise RuntimeError(drift)
    return all_files, whole_skips
