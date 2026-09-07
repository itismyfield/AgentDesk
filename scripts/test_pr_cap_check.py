"""Exercise the real cap helper against isolated, local bare Git remotes."""

from __future__ import annotations

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


HELPER = Path(__file__).resolve().with_name("pr_cap_check.sh")


class PrCapCheckTest(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="adk-pr-cap-test-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.remote = self.root / "remote.git"
        self.producer = self.root / "producer"
        self.repo = self.root / "consumer"
        self.env = {
            key: value for key, value in os.environ.items()
            if not key.startswith("GIT_")
        }
        self.env.update({
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_ALLOW_PROTOCOL": "file",
            "GIT_TERMINAL_PROMPT": "0",
        })
        self.git(self.root, "init", "--bare", "--initial-branch=main", str(self.remote))
        self.git(self.root, "init", "--initial-branch=main", str(self.producer))
        (self.producer / "existing.txt").write_text(
            "".join(f"old {line}\n" for line in range(1000)), encoding="utf-8"
        )
        self.commit(self.producer)
        self.git(self.producer, "remote", "add", "origin", str(self.remote))
        self.git(self.producer, "push", "origin", "main")
        self.git(self.root, "clone", str(self.remote), str(self.repo))
        self.git(self.repo, "checkout", "-b", "feature")
        self.initial = self.git(self.repo, "rev-parse", "main")

    def git(self, repo: Path, *args: str) -> str:
        result = subprocess.run(
            ["git", "-C", str(repo), *args], env=self.env,
            capture_output=True, text=True, check=True, timeout=30,
        )
        return result.stdout.strip()

    def commit(self, repo: Path | None = None) -> None:
        repo = repo or self.repo
        self.git(repo, "add", "--all")
        self.git(repo, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                 "-c", "commit.gpgsign=false", "commit", "-m", "fixture")

    def add_lines(self, count: int, name: str = "added.txt") -> None:
        (self.repo / name).write_text("new\n" * count, encoding="utf-8")

    def check_cap(self, *args: str, cwd: Path | None = None) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(HELPER), *args], cwd=cwd or self.repo, env=self.env,
            capture_output=True, text=True, timeout=30,
        )

    def assert_pass(self, result: subprocess.CompletedProcess[str], totals: str) -> None:
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(totals, result.stdout)
        self.assertIn("CAP: PASS", result.stdout)

    def assert_fail(self, result: subprocess.CompletedProcess[str]) -> None:
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertNotIn("CAP: PASS", result.stdout + result.stderr)

    def test_stale_main_and_tracking_ref_refresh_before_measurement(self) -> None:
        for index in range(21):
            (self.producer / f"upstream-{index}").write_text("upstream\n" * 40)
        self.commit(self.producer)
        self.git(self.producer, "push", "origin", "main")
        self.git(self.repo, "fetch", "origin", "main")
        fresh = self.git(self.repo, "rev-parse", "origin/main")
        self.git(self.repo, "checkout", "-B", "feature", "origin/main")
        self.add_lines(1)
        self.commit()
        # Simulate an outdated tracking ref and a custom fetch mapping as well
        # as local main. The helper must fetch/update the explicit main ref.
        self.git(self.repo, "update-ref", "refs/remotes/origin/main", self.initial)
        self.git(self.repo, "config", "remote.origin.fetch",
                 "+refs/heads/unused:refs/remotes/origin/unused")
        self.assertEqual(len(self.git(self.repo, "diff", "--numstat", "main", "HEAD").splitlines()), 22)
        result = self.check_cap()
        self.assert_pass(result, "1 files +1/-0")
        self.assertIn("remaining 19 files/+799", result.stdout)
        self.assertIn(f"base={fresh}", result.stdout)
        self.assertEqual(self.git(self.repo, "rev-parse", "origin/main"), fresh)
        self.assertEqual(self.git(self.repo, "rev-parse", "main"), self.initial)

    def test_older_branch_uses_merge_base_not_upstream_tip(self) -> None:
        self.add_lines(2)
        self.commit()
        (self.producer / "existing.txt").write_text("upstream replacement\n")
        self.commit(self.producer)
        self.git(self.producer, "push", "origin", "main")
        self.assert_pass(self.check_cap(), "1 files +2/-0")

    def test_exact_caps_pass(self) -> None:
        for index in range(20):
            self.add_lines(40, f"file-{index}")
        self.commit()
        result = self.check_cap()
        self.assert_pass(result, "20 files +800/-0")
        self.assertIn("remaining 0 files/+0", result.stdout)

    def test_801_additions_fail(self) -> None:
        self.add_lines(801)
        self.commit()
        result = self.check_cap()
        self.assert_fail(result)
        self.assertIn("1 files +801/-0", result.stdout)

    def test_21_files_fail(self) -> None:
        for index in range(21):
            self.add_lines(1, f"file-{index}")
        self.commit()
        result = self.check_cap()
        self.assert_fail(result)
        self.assertIn("21 files +21/-0", result.stdout)

    def test_deletions_do_not_offset_additions(self) -> None:
        (self.repo / "existing.txt").unlink()
        self.add_lines(801)
        self.commit()
        result = self.check_cap()
        self.assert_fail(result)
        self.assertIn("2 files +801/-1000", result.stdout)

    def test_fetch_failure_cannot_use_existing_local_refs(self) -> None:
        self.git(self.repo, "remote", "set-url", "origin", str(self.root / "missing.git"))
        result = self.check_cap()
        self.assert_fail(result)
        self.assertIn("cannot fetch origin main", result.stderr)

    def test_missing_remote_main_cannot_use_stale_tracking_ref(self) -> None:
        self.git(self.remote, "update-ref", "-d", "refs/heads/main")
        self.assert_fail(self.check_cap())

    def test_invalid_and_non_commit_targets_fail(self) -> None:
        tree = self.git(self.repo, "rev-parse", "HEAD^{tree}")
        for target in ("", "--help", "absent", "HEAD..main", "HEAD main", tree):
            with self.subTest(target=target):
                self.assert_fail(self.check_cap(target))
        self.assert_fail(self.check_cap("HEAD", "main"))

    def test_unrelated_history_fails(self) -> None:
        self.git(self.repo, "checkout", "--orphan", "unrelated")
        self.add_lines(1)
        self.commit()
        result = self.check_cap()
        self.assert_fail(result)
        self.assertIn("no usable merge-base", result.stderr)

    def test_ambiguous_ref_does_not_select_a_smaller_target(self) -> None:
        self.add_lines(801)
        self.commit()
        self.git(self.repo, "tag", "feature", "main")
        self.git(self.repo, "config", "core.warnAmbiguousRefs", "false")
        result = self.check_cap("feature")
        self.assert_fail(result)
        self.assertIn("ambiguity", result.stderr)
        self.assert_pass(self.check_cap("refs/tags/feature"), "0 files +0/-0")

    def test_absolute_helper_path_measures_callers_worktree_and_target(self) -> None:
        self.add_lines(3)
        self.commit()
        worktree = self.root / "another worktree"
        self.git(self.repo, "worktree", "add", "--detach", str(worktree), "feature")
        subdirectory = worktree / "nested directory"
        subdirectory.mkdir()
        (worktree / "untracked").write_text("ignored\n" * 900)
        self.assert_pass(self.check_cap(cwd=subdirectory), "1 files +3/-0")
        self.assert_pass(self.check_cap("main", cwd=subdirectory), "0 files +0/-0")

    def test_whitespace_paths_and_detected_rename(self) -> None:
        for name in ("space name", "tab\tname", "newline\nname"):
            self.add_lines(1, name)
        renamed = self.repo / "renamed\tfile\n.txt"
        (self.repo / "existing.txt").rename(renamed)
        renamed.write_text(renamed.read_text().replace("old 0\n", "changed\n", 1))
        self.commit()
        self.git(self.repo, "config", "diff.renames", "false")
        self.assert_pass(self.check_cap(), "4 files +4/-1")

    def test_binary_cannot_certify_addition_cap(self) -> None:
        (self.repo / "binary.bin").write_bytes(b"\x00\x01\x02")
        self.commit()
        result = self.check_cap()
        self.assert_fail(result)
        self.assertIn("1 files +0/-0 (binary files: 1", result.stdout)
        self.assertIn("binary line counts unavailable", result.stdout)

    def test_diff_failure_cannot_print_pass(self) -> None:
        self.add_lines(1)
        self.commit()
        # A real Git repository error after refs and merge-base resolve.
        self.git(self.repo, "config", "diff.algorithm", "not-a-diff-algorithm")
        result = self.check_cap()
        self.assert_fail(result)
        self.assertIn("cannot compute numstat", result.stderr)


if __name__ == "__main__":
    unittest.main()
