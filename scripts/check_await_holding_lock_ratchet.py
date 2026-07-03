#!/usr/bin/env python3
"""Ratchet guard for `#[allow(clippy::await_holding_lock)]` (#3034).

`await_holding_lock` is denied crate-wide in Cargo.toml `[lints.clippy]`, so any
NEW hold-lock-across-await site fails CI on its own. This guard covers the other
direction: the count of *existing* per-site `#[allow(clippy::await_holding_lock)]`
escape hatches may only ever go DOWN. It can never grow back, so the suppression
debt converges to zero without anyone having to remember to chase it.

To intentionally remove a suppression, delete the `#[allow]` (and fix the site),
then lower BASELINE to the new count. Raising BASELINE is a deliberate, reviewable
diff edit and should carry justification — it is not the normal path.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

# Monotonic ceiling: the number of `#[allow(clippy::await_holding_lock)]`
# attributes permitted across the workspace. Lower this as sites are removed;
# never raise it casually.
#
# 34 → 36 (#3982): the two backstop regression tests
# `backstop_orphan_reclaim_downgrades_then_claims` and
# `backstop_failed_reclaim_falls_back_to_bounded_abort`
# (src/services/discord/tui_direct_pending_start.rs) hold `worker_test_lock()`
# across `tokio::time::advance` awaits that drive `run_worker`. The guard
# serializes tests mutating the process-wide PRESENT index / durable-store root;
# releasing it before the awaits would let concurrent tests stomp the statics.
# Both are test-only and cannot deadlock a live task. Justified, reviewable raise.
BASELINE = 35

ALLOW_RE = re.compile(r"#\[allow\([^)]*\bclippy::await_holding_lock\b")


def count_allows(repo_root: Path) -> tuple[int, list[str]]:
    """Count real `#[allow(clippy::await_holding_lock)]` attributes.

    Best-effort source scan: full-line comments are skipped and trailing line
    comments are stripped so prose mentioning the lint (e.g. lib.rs guidance)
    is not counted. Assumes single-line attributes, which rustfmt (enforced by
    fmt-check in CI) normalizes to. The crate-wide `deny` in Cargo.toml is the
    primary gate; this ratchet only prevents the documented allow count from
    creeping back up.
    """
    total = 0
    locations: list[str] = []
    for path in sorted(repo_root.rglob("*.rs")):
        rel = path.relative_to(repo_root)
        if "target" in rel.parts:
            continue
        for lineno, line in enumerate(
            path.read_text(encoding="utf-8").splitlines(), start=1
        ):
            if line.lstrip().startswith("//"):
                continue
            code = line.split("//", 1)[0]
            for _ in ALLOW_RE.finditer(code):
                total += 1
                locations.append(f"{rel}:{lineno}")
    return total, locations


def main() -> int:
    repo_root = Path(__file__).resolve().parent.parent
    current, locations = count_allows(repo_root)

    if current > BASELINE:
        print(
            f"FAIL: {current} `#[allow(clippy::await_holding_lock)]` sites "
            f"exceed the ratchet baseline of {BASELINE}.",
            file=sys.stderr,
        )
        print(
            "      The await_holding_lock allow count may only decrease. "
            "Remove/fix a site instead of adding one.",
            file=sys.stderr,
        )
        for loc in locations:
            print(f"        {loc}", file=sys.stderr)
        return 1

    if current < BASELINE:
        print(
            f"NOTE: {current} allow sites is below the baseline of {BASELINE}. "
            f"Lower BASELINE to {current} in scripts/check_await_holding_lock_ratchet.py "
            "to lock in the win."
        )
        return 0

    print(f"OK: {current} `#[allow(clippy::await_holding_lock)]` sites (baseline {BASELINE}).")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
