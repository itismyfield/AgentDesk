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
import subprocess
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
BASELINE = 54  # +1 (#4795): busy-retry FIFO regression test holds shared_test_env_lock across await to serialize AGENTDESK_ROOT_DIR (RuntimeRootGuard tempdir) against parallel tests (test-only, SAFETY comment at site). # +16 (#4091 r7 batch-4): crate-wide test env-lock isolation sweep — async tests in queue_io/race_loss/terminal_controller_cutover/voice-pcm-harness/operator_connectors/worktree_orphan_sweep now hold shared_test_env_lock across await to serialize AGENTDESK_ROOT_DIR against parallel tests (all test-only, SAFETY-commented). # +1 (#4091 r5): tui_prompt_relay runtime-binding test holds tui_prompt_dedupe::TEST_LOCK across await to serialize shared dedupe-map resets against parallel tests (test-only, SAFETY comment at site). # +1 (#4068): mailbox snapshot peek-only test holds lock_test_env() guard across await to serialize AGENTDESK_ROOT_DIR

ALLOW_RE = re.compile(r"#\[allow\([^)]*\bclippy::await_holding_lock\b")


def _tracked_rs_files(repo_root: Path) -> list[Path]:
    """Return the repo's git-tracked `*.rs` files (as absolute paths).

    Enumerating with `git ls-files` instead of `repo_root.rglob("*.rs")` is what
    keeps the count independent of the developer's local checkout (#4376). A
    worktree-based dev environment leaves gitignored `.claude/worktrees/<id>/`
    clones — each a full `src/` copy — sitting in the repo root. A raw rglob
    walks into them and counts every suppression site once per nested worktree,
    so a developer with active worktrees is always locally red (e.g. 1027 vs
    baseline 53) while CI (a fresh, worktree-free checkout) stays green. Tracked
    files are exactly what CI sees; `target/`, `node_modules/`, and the worktree
    clones are gitignored/untracked and drop out automatically, so the old
    `"target" in rel.parts` special-case is no longer needed.
    """
    result = subprocess.run(
        ["git", "ls-files", "-z", "--", "*.rs"],
        cwd=repo_root,
        capture_output=True,
        text=True,
        check=True,
    )
    return [repo_root / rel for rel in result.stdout.split("\0") if rel]


def count_allows(repo_root: Path) -> tuple[int, list[str]]:
    """Count real `#[allow(clippy::await_holding_lock)]` attributes.

    Only git-tracked `*.rs` files are scanned (see `_tracked_rs_files`), so the
    result matches CI regardless of local worktree clones (#4376).

    Best-effort source scan: full-line comments are skipped and trailing line
    comments are stripped so prose mentioning the lint (e.g. lib.rs guidance)
    is not counted. Assumes single-line attributes, which rustfmt (enforced by
    fmt-check in CI) normalizes to. The crate-wide `deny` in Cargo.toml is the
    primary gate; this ratchet only prevents the documented allow count from
    creeping back up.
    """
    total = 0
    locations: list[str] = []
    for path in sorted(_tracked_rs_files(repo_root)):
        rel = path.relative_to(repo_root)
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
