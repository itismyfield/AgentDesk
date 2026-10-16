"""Single lexical pin for whole-file skips in the two #5071 writer gates.

The ordinary basename skips and the higher-risk resolver skips are listed
separately for review, then exposed as one immutable set.  Every exclusion is
compared with this repo-relative lexical set before either gate scans files.

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
import sys
from collections.abc import Callable, Iterable
from pathlib import Path


_paths_spec = importlib.util.spec_from_file_location(
    "test_only_module_skip_pin_paths", Path(__file__).with_name("test_only_module_skip_pin_paths.py")
)
assert _paths_spec is not None and _paths_spec.loader is not None
_paths = importlib.util.module_from_spec(_paths_spec)
_paths_spec.loader.exec_module(_paths)
PINNED_BASENAME_TEST_FILES = _paths.PINNED_BASENAME_TEST_FILES
PINNED_RESOLVER_TEST_ONLY_FILES = _paths.PINNED_RESOLVER_TEST_ONLY_FILES

PINNED_TEST_ONLY_MODULE_FILES = (
    PINNED_BASENAME_TEST_FILES | PINNED_RESOLVER_TEST_ONLY_FILES
)

_UPDATE_HINT = (
    "Review basename and resolver classification in scripts/test_only_module_skip_pin.py, then update "
    "scripts/test_only_module_skip_pin_paths.py; both gates and tests derive their "
    "path/count expectation from that single file."
)

_SYMLINK_HINT = (
    "Remove the symlink or replace it with a regular in-tree file; do not add "
    "it to the writer-gate skip pin."
)


def _load_inventory_generator():
    name = "generate_inventory_docs"
    if name in sys.modules:
        return sys.modules[name]
    spec = importlib.util.spec_from_file_location(
        name, Path(__file__).resolve().parent / "generate_inventory_docs.py"
    )
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load scripts/generate_inventory_docs.py")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


_INVENTORY = _load_inventory_generator()


def skip_pin_drift(
    excluded_paths: Iterable[str],
    pinned_paths: Iterable[str] = PINNED_TEST_ONLY_MODULE_FILES,
) -> str | None:
    """Compare lexical repo-relative paths in both directions."""

    actual = frozenset(Path(path).as_posix() for path in excluded_paths)
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
    resolver_results = _INVENTORY.test_only_module_files(
        production_files=production_files,
        all_files=all_files,
        read_text_fn=lambda path: path.read_text(encoding="utf-8"),
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

    whole_skips = frozenset(basename_skips | resolver_skips)
    lexical_skips = {
        path.relative_to(root).as_posix() for path in whole_skips
    }
    drift = skip_pin_drift(lexical_skips, pinned)
    pinned_count = len(pinned)
    if len(whole_skips) != pinned_count:
        census = (
            "FAIL: writer-gate skipped census differs from pin count "
            f"({len(whole_skips)} skipped, {pinned_count} pinned).\n  {_UPDATE_HINT}"
        )
        raise RuntimeError(f"{drift}\n{census}" if drift else census)
    if drift:
        raise RuntimeError(drift)
    return all_files, whole_skips
