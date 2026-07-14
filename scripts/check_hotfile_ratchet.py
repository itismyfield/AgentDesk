#!/usr/bin/env python3
"""Raw-LOC ratchet guard for the oversized Discord-relay hot files (#3565).

`tmux_watcher.rs`, `tui_prompt_relay.rs`, and `turn_bridge/mod.rs` are already
far too large and a regression risk: any further growth makes an eventual
decomposition harder. This guard freezes each file's RAW line count (`wc -l`:
comments, blank lines, and test code all COUNTED) at the ceiling recorded in
`scripts/hotfile_ratchet.toml`. A file may shrink (lower the ceiling to lock in
the win) but may never exceed its ceiling.

Metric: raw physical line count. This is a deliberately different, complementary
metric from the production-LoC ratchet in
`scripts/audit_maintainability_giant_baseline.toml` (which excludes `#[cfg(test)]`
code). Both gates are intended to stay in force.

Counting uses ``len(text.splitlines())`` rather than shelling out to ``wc`` so
the check is deterministic and dependency-free. For the repo's LF-terminated
source files this is exactly the ``wc -l`` value (CRLF is not used here).

A missing hot file or a missing manifest is a HARD error (exit 1): a rename or
move must not silently pass the gate.

The three relay hot files in ``REQUIRED_HOTFILES`` MUST each have a ceiling
entry in the manifest. Dropping a key from the manifest would otherwise stop
that file from being checked while the script still passed (the manifest table
is non-empty); ``REQUIRED_HOTFILES`` closes that bypass by hard-failing (exit 1)
on any missing required entry.
"""

from __future__ import annotations

import sys
from pathlib import Path

MIN_PYTHON = (3, 11)

# Keep this before importing tomllib so unsupported interpreters fail with the
# repository policy message instead of a raw ModuleNotFoundError.
if sys.version_info < MIN_PYTHON:
    version = (
        f"{sys.version_info.major}.{sys.version_info.minor}.{sys.version_info.micro}"
    )
    print(
        "ERROR: scripts/check_hotfile_ratchet.py requires Python 3.11+ "
        "for stdlib tomllib; "
        f"{sys.executable} is Python {version}.",
        file=sys.stderr,
    )
    print(
        "Run with python3.11+ or set PYTHON=/path/to/python3.11+ when using "
        "scripts/ci-script-checks.sh.",
        file=sys.stderr,
    )
    raise SystemExit(1)

import tomllib

from ratchet_admission import audit_repository_admissions

REPO_ROOT = Path(__file__).resolve().parent.parent
MANIFEST = REPO_ROOT / "scripts" / "hotfile_ratchet.toml"

# The relay hot files that MUST always be ratcheted. Every path here has to have
# a ceiling entry in scripts/hotfile_ratchet.toml; if a future edit deletes one
# of these keys (which would silently stop checking that file), the gate fails
# closed instead of passing on the remaining non-empty table.
REQUIRED_HOTFILES = (
    "src/services/discord/tmux_watcher.rs",
    "src/services/discord/tui_prompt_relay.rs",
    "src/services/discord/turn_bridge/mod.rs",
)


def line_count(path: Path) -> int:
    """Return the raw line count of ``path`` (``wc -l`` equivalent for LF files)."""
    text = path.read_text(encoding="utf-8", errors="replace")
    return len(text.splitlines())


def main() -> int:
    if not MANIFEST.is_file():
        print(
            f"FAIL: hotfile ratchet manifest not found: {MANIFEST}",
            file=sys.stderr,
        )
        return 1

    with MANIFEST.open("rb") as fh:
        manifest = tomllib.load(fh)

    ceilings = manifest.get("hotfile_ratchet", {})
    if not ceilings:
        print(
            f"FAIL: no [hotfile_ratchet] entries in {MANIFEST}.",
            file=sys.stderr,
        )
        return 1

    missing_required = [rel for rel in REQUIRED_HOTFILES if rel not in ceilings]
    if missing_required:
        for rel in missing_required:
            print(
                f"FAIL: required relay hot file '{rel}' has no ceiling entry in "
                f"{MANIFEST}. Every path in REQUIRED_HOTFILES must stay ratcheted; "
                "do not delete its entry (that would silently stop checking it).",
                file=sys.stderr,
            )
        return 1

    failed = False
    admission_audit = audit_repository_admissions(
        repo_root=REPO_ROOT,
        ratchet="hotfile_ratchet",
        config_rel_path="scripts/hotfile_ratchet.toml",
        table_name="hotfile_ratchet",
    )
    for warning in admission_audit.warnings:
        print(warning, file=sys.stderr)
    for error in admission_audit.errors:
        print(f"FAIL: {error}", file=sys.stderr)
        failed = True

    for rel, ceiling in sorted(ceilings.items()):
        path = REPO_ROOT / rel
        if not path.is_file():
            print(
                f"FAIL: ratcheted hot file is missing: {rel}. If it was moved or "
                "renamed, update scripts/hotfile_ratchet.toml.",
                file=sys.stderr,
            )
            failed = True
            continue

        current = line_count(path)
        if current > ceiling:
            print(
                f"FAIL: {rel} has {current} lines, exceeding the ratchet ceiling "
                f"of {ceiling}.",
                file=sys.stderr,
            )
            print(
                "      Hot-file line counts may only decrease. Shrink the file "
                "(prefer decomposition) instead of raising the ceiling.",
                file=sys.stderr,
            )
            failed = True
        elif current < ceiling:
            print(
                f"NOTE: {rel} has {current} lines, below its ceiling of {ceiling}. "
                f"Lower ceiling to {current} in scripts/hotfile_ratchet.toml to "
                "lock in the win."
            )
        else:
            print(f"OK: {rel} = {current} lines (ceiling {ceiling}).")

    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
