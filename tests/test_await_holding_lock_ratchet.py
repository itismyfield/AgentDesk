"""Self-tests for scripts/check_await_holding_lock_ratchet.py (#4376).

Covers the counting contract after the #4376 fix that switched file enumeration
from `repo_root.rglob("*.rs")` to `git ls-files`:
 (a) only git-tracked `*.rs` files are counted — a gitignored worktree clone
     (`.claude/worktrees/<id>/src/…`) planted next to a tracked file must NOT
     inflate the count (the bug: a worktree dev's local run counted every
     suppression once per nested clone, e.g. 1027 vs baseline 53, so they were
     always locally red while CI — a fresh, clone-free checkout — stayed green);
 (b) full-line and trailing `//` comments mentioning the lint are stripped;
 (c) the FAIL/NOTE/OK exit branches;
 (d) the live repo agrees with the frozen BASELINE.

The (a) tests are the mutation guard: reverting `count_allows` to
`sorted(repo_root.rglob("*.rs"))` makes them count the gitignored clone and fail
by assertion (not by import/compile error).
"""

from __future__ import annotations

import importlib.util
import io
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest import mock

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts" / "check_await_holding_lock_ratchet.py"

_spec = importlib.util.spec_from_file_location("check_await_holding_lock_ratchet", SCRIPT)
ratchet = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(ratchet)


def _write(root: Path, rel: str, body: str) -> None:
    path = root / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body, encoding="utf-8")


def _git(root: Path, *args: str) -> None:
    subprocess.run(
        ["git", *args],
        cwd=root,
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


# One real suppression site. Indented as rustfmt would leave an attribute.
ALLOW = "    #[allow(clippy::await_holding_lock)]\n"

# A tracked production file with exactly ONE suppression.
TRACKED_RS = (
    "async fn holds() {\n"
    + ALLOW
    + "    let _g = LOCK.lock().unwrap();\n"
    + "    do_async().await;\n"
    + "}\n"
)


class TrackedFileScopeTests(unittest.TestCase):
    """git ls-files enumeration: only tracked files count (#4376)."""

    def _count_in_git_repo(self, builder) -> tuple[int, list[str]]:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _git(root, "init", "-q")
            # The worktree clones the bug is about live under `.claude/`, which
            # AgentDesk gitignores; mirror that so they are untracked here too.
            _write(root, ".gitignore", ".claude/\n")
            builder(root)
            _git(root, "add", "-A")
            return ratchet.count_allows(root)

    def test_gitignored_worktree_clone_is_not_counted(self):
        # The core #4376 regression: a tracked file with ONE suppression, plus a
        # gitignored `.claude/worktrees/<id>/src` clone of it. Only the tracked
        # copy may count — the clone is exactly what `git ls-files` (vs rglob)
        # drops. Reverting to rglob counts the clone and fails this assert.
        def build(root: Path) -> None:
            _write(root, "src/services/discord/a.rs", TRACKED_RS)
            _write(root, ".claude/worktrees/wt1/src/services/discord/a.rs", TRACKED_RS)

        total, locs = self._count_in_git_repo(build)
        self.assertEqual(total, 1, locs)
        self.assertEqual(len(locs), 1)
        self.assertTrue(all(".claude" not in loc for loc in locs), locs)
        self.assertTrue(locs[0].startswith("src/"), locs)

    def test_many_gitignored_clones_do_not_multiply_the_count(self):
        # Reproduces the "N worktrees -> Nx count" inflation shape directly:
        # five clones of a single tracked site must still count as one.
        def build(root: Path) -> None:
            _write(root, "src/a.rs", TRACKED_RS)
            for i in range(5):
                _write(root, f".claude/worktrees/wt{i}/src/a.rs", TRACKED_RS)

        total, _locs = self._count_in_git_repo(build)
        self.assertEqual(total, 1)

    def test_full_line_and_trailing_comments_not_counted(self):
        # (b) prose mentioning the lint (full-line `//`, doc `///`, and trailing
        # `//`) is stripped; only the real attribute counts.
        body = (
            "// #[allow(clippy::await_holding_lock)] in a comment must NOT count\n"
            "/// doc: #[allow(clippy::await_holding_lock)] must NOT count\n"
            "async fn f() {\n"
            "    let _ = 1; // #[allow(clippy::await_holding_lock)] trailing, NOT counted\n"
            + ALLOW
            + "    g().await;\n"
            "}\n"
        )
        total, locs = self._count_in_git_repo(lambda r: _write(r, "src/b.rs", body))
        self.assertEqual(total, 1, locs)


class ExitBranchTests(unittest.TestCase):
    """FAIL/NOTE/OK branches, isolated from the real tree via mocking."""

    def _run_main(self, current: int, baseline: int) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        with mock.patch.object(ratchet, "BASELINE", baseline), mock.patch.object(
            ratchet, "count_allows", return_value=(current, ["a.rs:1"] * current)
        ), redirect_stdout(out), redirect_stderr(err):
            code = ratchet.main()
        return code, out.getvalue(), err.getvalue()

    def test_over_baseline_fails(self):
        code, _out, err = self._run_main(current=54, baseline=53)
        self.assertEqual(code, 1)
        self.assertIn("FAIL", err)

    def test_below_baseline_notes_and_passes(self):
        code, out, _err = self._run_main(current=52, baseline=53)
        self.assertEqual(code, 0)
        self.assertIn("NOTE", out)
        self.assertIn("Lower BASELINE to 52", out)

    def test_at_baseline_ok(self):
        code, out, _err = self._run_main(current=53, baseline=53)
        self.assertEqual(code, 0)
        self.assertIn("OK", out)


class LiveRepoBaselineTests(unittest.TestCase):
    def test_live_repo_matches_baseline(self):
        # The real tree matches the frozen BASELINE via git ls-files enumeration,
        # independent of any local `.claude/worktrees` clones.
        result = subprocess.run(
            [sys.executable, str(SCRIPT)],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(f"OK: {ratchet.BASELINE}", result.stdout)


if __name__ == "__main__":
    unittest.main()
