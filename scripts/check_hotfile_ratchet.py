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
"""

from __future__ import annotations

import sys
import tomllib
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
MANIFEST = REPO_ROOT / "scripts" / "hotfile_ratchet.toml"


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

    failed = False
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
