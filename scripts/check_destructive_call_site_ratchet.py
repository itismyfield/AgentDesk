#!/usr/bin/env python3
"""Ratchet destructive Rust call sites by category and exact per-file count.

This is a bounded lexical inventory, not a Rust/type/data-flow analysis and not
proof that any listed destruction is safe.  It scans stripped source text for:
tmux kill wrappers; watcher AtomicBool ``store(true, ...)`` calls; process kill
calls; and watcher-registry removal calls.  The first two categories include
test call sites, matching the #5071 map.  Process and registry categories reuse
the repository's existing lexical ``cfg(test)`` classifier and exclude whole
test modules.  Aliases, re-exports, macros that construct names, indirection,
and semantically equivalent spellings can remain unseen.

``--check`` rejects growth in an existing file and every UNLISTED file.  A
decrease is allowed: this is a no-growth ratchet.  For an intentional change,
run ``--write-baseline`` and review the JSON diff in the same commit.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Mapping, Sequence


REPO_ROOT = Path(__file__).resolve().parents[1]
BASELINE_PATH = Path("scripts/destructive_call_site_baseline.json")
CATEGORIES = ("tmux_kill", "watcher_cancel", "process_kill", "registry_remove")
WARNING = "These counts are a growth-blocking baseline, not proof of safety."
REPIN = (
    "Intentional change: run scripts/check_destructive_call_site_ratchet.py "
    "--write-baseline and commit the reviewed JSON diff."
)

ALL_SOURCE_PATTERNS = {
    "tmux_kill": re.compile(
        r"\b(?:crate\s*::\s*services\s*::\s*)?platform\s*::\s*tmux\s*::\s*"
        r"kill_session(?:_output_timeout|_output|_checked)?\s*\("
    ),
    "watcher_cancel": re.compile(
        r"\b(?:cancel|cancel_for_commit|expected_cancel|watcher_cancel)\s*\.\s*"
        r"store\s*\(\s*true\b"
    ),
}
PROCESS_PATTERN = re.compile(
    r"(?:\b(?:kill_pid_tree|terminate_process_handle)\s*\(|\.\s*kill\s*\(\s*\))"
)
REGISTRY_PATTERNS = {
    "direct_channel_remove": re.compile(
        r"\btmux_watchers\s*\.\s*(?:remove|remove_locked)\s*\("
    ),
    "remove_if_current": re.compile(r"\bremove_tmux_session_if_current\s*\("),
    "cancel_and_remove_if_current": re.compile(
        r"\bcancel_and_remove_channel_if_current\s*\("
    ),
    "remove_locked_helper": re.compile(r"\bremove_tmux_session_locked\s*\("),
}
REGISTRY_OWNER = "src/services/discord/tmux_watcher_registry.rs"


class RatchetError(RuntimeError):
    pass


def _load_rust_lexer():
    """Reuse the landed call-site gate's comment/string/cfg(test) machinery."""
    path = REPO_ROOT / "scripts/check_durable_frontier_writer_call_sites.py"
    spec = importlib.util.spec_from_file_location("destructive_ratchet_rust_lexer", path)
    if spec is None or spec.loader is None:
        raise RatchetError(f"cannot load Rust lexer: {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


RUST_LEXER = _load_rust_lexer()


def _stripped_text(path: Path) -> str:
    state = RUST_LEXER.StripState()
    return "\n".join(
        RUST_LEXER.strip_line(line, state)
        for line in path.read_text(encoding="utf-8").splitlines()
    )


def _is_whole_test_file(path: Path, rel: str) -> bool:
    return (
        RUST_LEXER.is_test_file(path.name)
        or rel in RUST_LEXER.PINNED_TEST_ONLY_MODULE_FILES
    )


def scan(repo_root: Path) -> tuple[dict[str, dict[str, int]], dict[str, int]]:
    root = Path(repo_root).resolve()
    source_root = root / "src"
    if not source_root.is_dir():
        raise RatchetError("scan root src/ is missing")
    counts: dict[str, dict[str, int]] = {category: {} for category in CATEGORIES}
    registry_subcounts = {name: 0 for name in REGISTRY_PATTERNS}
    paths = sorted(source_root.rglob("*.rs"))
    if not paths:
        raise RatchetError("scan root src/ contains no Rust files")
    for path in paths:
        if path.is_symlink():
            raise RatchetError(f"source symlink is outside the lexical model: {path}")
        rel = path.relative_to(root).as_posix()
        stripped = _stripped_text(path)
        for category, pattern in ALL_SOURCE_PATTERNS.items():
            found = len(pattern.findall(stripped))
            if found:
                counts[category][rel] = found

        if not rel.startswith("src/services/discord/") or _is_whole_test_file(path, rel):
            continue
        production = RUST_LEXER._production_text(path)
        process_found = len(PROCESS_PATTERN.findall(production))
        if process_found:
            counts["process_kill"][rel] = process_found
        # Definitions and helper-to-helper composition live in this owner file;
        # the #5071 map deliberately pins external removal consumers only.
        if rel == REGISTRY_OWNER:
            continue
        registry_found = 0
        for name, pattern in REGISTRY_PATTERNS.items():
            found = len(pattern.findall(production))
            registry_subcounts[name] += found
            registry_found += found
        if registry_found:
            counts["registry_remove"][rel] = registry_found
    return counts, registry_subcounts


def _category_files(payload: Mapping[str, object], category: str) -> dict[str, int]:
    categories = payload.get("categories")
    if not isinstance(categories, dict) or category not in categories:
        raise RatchetError(f"baseline missing category: {category}")
    entry = categories[category]
    if not isinstance(entry, dict) or not isinstance(entry.get("files"), dict):
        raise RatchetError(f"baseline category {category} has no files map")
    files = entry["files"]
    if any(not isinstance(path, str) or not isinstance(count, int) or count <= 0
           for path, count in files.items()):
        raise RatchetError(f"baseline category {category} has invalid per-file count")
    return dict(files)


def load_baseline(path: Path) -> tuple[dict[str, dict[str, int]], dict[str, object]]:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise RatchetError(f"cannot load baseline {path}: {exc}") from exc
    if payload.get("schema_version") != 1:
        raise RatchetError("baseline schema_version must be 1")
    counts = {category: _category_files(payload, category) for category in CATEGORIES}
    return counts, payload


def growth_errors(
    actual: Mapping[str, Mapping[str, int]],
    baseline: Mapping[str, Mapping[str, int]],
) -> list[str]:
    errors: list[str] = []
    for category in CATEGORIES:
        expected = baseline.get(category, {})
        for path, found in sorted(actual.get(category, {}).items()):
            pinned = expected.get(path, 0)
            if found <= pinned:
                continue
            if pinned == 0:
                errors.append(f"{category}: UNLISTED call site in {path} ({found}x)")
            else:
                errors.append(
                    f"{category}: GROWTH in {path}: found {found}x, baseline {pinned}x"
                )
    return errors


def _snapshot(
    counts: Mapping[str, Mapping[str, int]],
    registry_subcounts: Mapping[str, int],
    measured_sha: str,
) -> dict[str, object]:
    direct = registry_subcounts["direct_channel_remove"]
    if_current = registry_subcounts["remove_if_current"]
    cancel_current = registry_subcounts["cancel_and_remove_if_current"]
    locked = registry_subcounts["remove_locked_helper"]
    return {
        "schema_version": 1,
        "measured_at_sha": measured_sha,
        "comment": WARNING,
        "categories": {
            "tmux_kill": {"files": dict(sorted(counts["tmux_kill"].items()))},
            "watcher_cancel": {"files": dict(sorted(counts["watcher_cancel"].items()))},
            "process_kill": {"files": dict(sorted(counts["process_kill"].items()))},
            "registry_remove": {
                "comment": (
                    f"Measured production external removals are {direct}/{if_current}/"
                    f"{cancel_current}/{locked} (total {direct + if_current + cancel_current + locked}). "
                    "The design's 10/2/3/2=17 counted turn_finalizer/watcher_backstop.rs:252, "
                    "which is inside #[cfg(test)] at this SHA. health/recovery.rs remove_locked "
                    "is classified as direct channel remove, not remove_tmux_session_locked."
                ),
                "files": dict(sorted(counts["registry_remove"].items())),
            },
        },
    }


def write_baseline(
    path: Path,
    counts: Mapping[str, Mapping[str, int]],
    registry_subcounts: Mapping[str, int],
    measured_sha: str,
) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(_snapshot(counts, registry_subcounts, measured_sha), indent=2) + "\n",
        encoding="utf-8",
    )


def _totals(counts: Mapping[str, Mapping[str, int]]) -> str:
    return ", ".join(
        f"{category}={sum(counts[category].values())}/{len(counts[category])}files"
        for category in CATEGORIES
    )


def main(argv: Sequence[str] | None = None, repo_root: Path = REPO_ROOT) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_mutually_exclusive_group(required=True)
    modes.add_argument("--check", action="store_true")
    modes.add_argument("--write-baseline", action="store_true")
    args = parser.parse_args(argv)
    root = Path(repo_root).resolve()
    try:
        counts, registry_subcounts = scan(root)
        if args.write_baseline:
            sha = subprocess.run(
                ["git", "rev-parse", "HEAD"], cwd=root, text=True,
                capture_output=True, check=True,
            ).stdout.strip()
            write_baseline(root / BASELINE_PATH, counts, registry_subcounts, sha)
            print(f"WROTE destructive call-site baseline at {sha}: {_totals(counts)}")
            return 0
        baseline, _payload = load_baseline(root / BASELINE_PATH)
        errors = growth_errors(counts, baseline)
    except Exception as exc:
        print(f"FAIL: destructive call-site ratchet: {type(exc).__name__}: {exc}", file=sys.stderr)
        return 1
    if errors:
        print("FAIL: destructive call-site growth detected", file=sys.stderr)
        print("\n".join(f"  - {error}" for error in errors), file=sys.stderr)
        print(REPIN, file=sys.stderr)
        return 1
    print(f"OK: destructive call-site ratchet: {_totals(counts)}. {WARNING}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
