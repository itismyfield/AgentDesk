"""Exercise the real cap helper against isolated, local bare Git remotes."""

from __future__ import annotations

import os
import json
import hashlib
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
            if not key.startswith(("GIT_", "GITHUB_", "PR_CAP_"))
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
        self.assert_pass(result, "1 files net +1 code")
        self.assertIn("remaining 29 files/+799", result.stdout)
        self.assertIn(f"base={fresh}", result.stdout)
        self.assertEqual(self.git(self.repo, "rev-parse", "origin/main"), fresh)
        self.assertEqual(self.git(self.repo, "rev-parse", "main"), self.initial)

    def test_older_branch_uses_merge_base_not_upstream_tip(self) -> None:
        self.add_lines(2)
        self.commit()
        (self.producer / "existing.txt").write_text("upstream replacement\n")
        self.commit(self.producer)
        self.git(self.producer, "push", "origin", "main")
        self.assert_pass(self.check_cap(), "1 files net +2 code")

    def test_exact_caps_pass(self) -> None:
        for index in range(30):
            self.add_lines(1 if index else 771, f"file-{index}")
        self.commit()
        result = self.check_cap()
        self.assert_pass(result, "30 files net +800 code")
        self.assertIn("remaining 0 files/+0", result.stdout)

    def test_801_additions_fail(self) -> None:
        self.add_lines(801)
        self.commit()
        result = self.check_cap()
        self.assert_fail(result)
        self.assertIn("1 files net +801 code", result.stdout)

    def test_31_files_fail(self) -> None:
        for index in range(31):
            self.add_lines(1, f"file-{index}")
        self.commit()
        result = self.check_cap()
        self.assert_fail(result)
        self.assertIn("31 files net +31 code", result.stdout)

    def test_deletions_offset_additions(self) -> None:
        (self.repo / "existing.txt").unlink()
        self.add_lines(801)
        self.commit()
        result = self.check_cap()
        self.assert_pass(result, "2 files net -199 code")

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
        self.assert_pass(self.check_cap("refs/tags/feature"), "0 files net +0 code")

    def test_absolute_helper_path_measures_callers_worktree_and_target(self) -> None:
        self.add_lines(3)
        self.commit()
        worktree = self.root / "another worktree"
        self.git(self.repo, "worktree", "add", "--detach", str(worktree), "feature")
        subdirectory = worktree / "nested directory"
        subdirectory.mkdir()
        (worktree / "untracked").write_text("ignored\n" * 900)
        self.assert_pass(self.check_cap(cwd=subdirectory), "1 files net +3 code")
        self.assert_pass(self.check_cap("main", cwd=subdirectory), "0 files net +0 code")

    def test_space_path_and_canonical_no_rename(self) -> None:
        for name in ("space name",):
            self.add_lines(1, name)
        renamed = self.repo / "renamed.txt"
        (self.repo / "existing.txt").rename(renamed)
        renamed.write_text(renamed.read_text().replace("old 0\n", "changed\n", 1))
        self.commit()
        self.git(self.repo, "config", "diff.renames", "false")
        self.assert_pass(self.check_cap(), "3 files net +1 code")

    def test_binary_cannot_certify_addition_cap(self) -> None:
        (self.repo / "binary.bin").write_bytes(b"\x00\x01\x02")
        self.commit()
        result = self.check_cap()
        self.assert_fail(result)
        self.assertIn("binary prod files: binary.bin", result.stdout)

    def test_diff_failure_cannot_print_pass(self) -> None:
        self.add_lines(1)
        self.commit()
        # A real Git repository error after refs and merge-base resolve.
        self.git(self.repo, "config", "diff.algorithm", "not-a-diff-algorithm")
        result = self.check_cap()
        self.assert_fail(result)
        self.assertIn("production measurement failed", result.stderr)


    def event(self, body="", base=None):
        path = self.root / "event.json"
        path.write_text(json.dumps({"pull_request": {"head": {"sha": self.git(self.repo, "rev-parse", "HEAD")}, "base": {"sha": base or self.initial}, "body": body}}))
        self.env.update(PR_CAP_CI="1", GITHUB_EVENT_PATH=str(path))

    def test_modes_and_exception(self):
        self.add_lines(801)
        self.commit()
        self.assert_fail(self.check_cap())
        self.env["PR_CAP_MODE"] = "report-only"
        self.assertEqual(self.check_cap().returncode, 0)
        self.env["PR_CAP_MODE"] = "enforce"
        self.event("PR-CAP-EXEMPT: generated compatibility migration")
        self.assertIn("CAP: EXEMPT", self.check_cap().stdout)
        self.event("PR-CAP-EXEMPT:   \nnot a reason")
        self.assert_fail(self.check_cap())
        self.event("PR-CAP-EXEMPT: one\nPR-CAP-EXEMPT: two")
        self.assert_fail(self.check_cap())
        self.env["PR_CAP_MODE"] = "off"
        self.assertIn("CAP: DISABLED", self.check_cap().stdout)
        self.env["PR_CAP_MODE"] = "bogus"
        self.assert_fail(self.check_cap())

    def test_exemption_accepts_crlf_and_cr_body_lines(self):
        self.add_lines(801)
        self.commit()
        for ending in ("\r\n", "\r"):
            with self.subTest(ending=repr(ending)):
                self.event(ending.join(("Summary", "PR-CAP-EXEMPT: compatibility migration", "End")))
                result = self.check_cap()
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn("CAP: EXEMPT (compatibility migration)", result.stdout)

    def test_exemption_examples_do_not_authorize_a_violation(self):
        self.add_lines(801)
        self.commit()
        examples = (
            "```text\nPR-CAP-EXEMPT: example\n```",
            "````text\n```\nPR-CAP-EXEMPT: example\n````",
            "~~~text\nPR-CAP-EXEMPT: example\n~~~",
            "<!--\nPR-CAP-EXEMPT: example\n-->",
            "<!-- PR-CAP-EXEMPT: example -->",
            "> PR-CAP-EXEMPT: example",
            "  > PR-CAP-EXEMPT: example",
        )
        for body in examples:
            with self.subTest(body=body):
                self.event(body)
                result = self.check_cap()
                self.assert_fail(result)
                self.assertIn("CAP: FAIL", result.stdout)
                self.event(body + "\n\nPR-CAP-EXEMPT: reviewed migration")
                result = self.check_cap()
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn("CAP: EXEMPT (reviewed migration)", result.stdout)

    def test_lazy_quote_continuation_cannot_authorize_exemption(self):
        self.add_lines(801)
        self.commit()
        self.event("> Quoted example\nPR-CAP-EXEMPT: quoted reason")
        result = self.check_cap()
        self.assert_fail(result)
        self.assertIn("CAP: FAIL", result.stdout)
        self.event("> Quoted example\n\nPR-CAP-EXEMPT: reviewed migration")
        result = self.check_cap()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("CAP: EXEMPT (reviewed migration)", result.stdout)

    def test_multiple_exemptions_error_only_after_measured_violation(self):
        self.event("PR-CAP-EXEMPT: one\nPR-CAP-EXEMPT: two")
        self.assert_pass(self.check_cap(), "0 files net +0 code")
        self.add_lines(801)
        self.commit()
        self.event("PR-CAP-EXEMPT: one\nPR-CAP-EXEMPT: two")
        result = self.check_cap()
        self.assert_fail(result)
        self.assertIn("CAP: FAIL", result.stdout)
        self.assertIn("multiple exemption reasons", result.stderr)

    def test_advisory_modes_and_exemption_annotate_github_checks(self):
        self.add_lines(801)
        self.commit()
        summary = self.root / "step-summary.md"
        self.env.update(GITHUB_ACTIONS="true", GITHUB_STEP_SUMMARY=str(summary))
        for mode, body, status in (
            ("off", "", "CAP: DISABLED (PR_CAP_MODE=off)"),
            ("report-only", "", "CAP: REPORT-ONLY (measured violation; enforcement disabled)"),
            ("enforce", "PR-CAP-EXEMPT: reviewed migration", "CAP: EXEMPT (reviewed migration)"),
        ):
            with self.subTest(mode=mode):
                summary.write_text("Earlier step\n")
                self.env["PR_CAP_MODE"] = mode
                self.event(body)
                result = self.check_cap()
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn("::warning title=PR cap::" + status, result.stdout)
                self.assertEqual(summary.read_text(), "Earlier step\n" + status + "\n")

    def test_github_annotation_escapes_exemption_and_summary_is_optional(self):
        self.add_lines(801)
        self.commit()
        self.env["GITHUB_ACTIONS"] = "true"
        self.env.pop("GITHUB_STEP_SUMMARY", None)
        self.event("PR-CAP-EXEMPT: reviewed 100%0A::error:: migration")
        result = self.check_cap()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("::warning title=PR cap::CAP: EXEMPT (reviewed 100%250A::error:: migration)", result.stdout)
        self.assertNotIn("\n::error::", result.stdout)

    def test_report_only_within_limits_annotates_disabled_enforcement(self):
        self.add_lines(3)
        self.commit()
        summary = self.root / "step-summary.md"
        summary.write_text("Earlier step\n")
        self.env.update(PR_CAP_MODE="report-only", GITHUB_ACTIONS="true", GITHUB_STEP_SUMMARY=str(summary))
        result = self.check_cap()
        self.assert_pass(result, "1 files net +3 code")
        status = "CAP: REPORT-ONLY (within limits; enforcement disabled)"
        self.assertIn("::warning title=PR cap::" + status, result.stdout)
        self.assertEqual(summary.read_text(), "Earlier step\n" + status + "\n")

    def test_local_advisory_mode_does_not_emit_github_notice(self):
        summary = self.root / "step-summary.md"
        self.env.update(PR_CAP_MODE="off", GITHUB_ACTIONS="false", GITHUB_STEP_SUMMARY=str(summary))
        result = self.check_cap()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn("::warning", result.stdout)
        self.assertFalse(summary.exists())

    def test_report_only_and_exemption_cannot_hide_errors(self):
        self.add_lines(801)
        self.commit()
        self.event("PR-CAP-EXEMPT: migration")
        self.env["PR_CAP_MODE"] = "report-only"
        self.git(self.repo, "config", "diff.algorithm", "invalid")
        self.assert_fail(self.check_cap())

    def test_ci_declared_stack_base_and_event_head(self):
        self.add_lines(801)
        self.commit()
        parent = self.git(self.repo, "rev-parse", "HEAD")
        self.add_lines(1, "child")
        self.commit()
        self.event(base=parent)
        self.git(self.repo, "checkout", "main")
        self.assert_pass(self.check_cap(), "1 files net +1 code")

    def test_ci_event_commits_measure_without_fetching_main(self):
        self.add_lines(3)
        self.commit()
        head = self.git(self.repo, "rev-parse", "HEAD")
        self.event()
        self.git(self.repo, "checkout", "main")
        self.git(self.repo, "remote", "set-url", "origin", str(self.root / "missing.git"))
        result = self.check_cap()
        self.assert_pass(result, "1 files net +3 code")
        self.assertIn(f"base={self.initial} target={head}", result.stdout)

    def test_ci_missing_event_commit_cannot_fall_back_to_main(self):
        self.event("PR-CAP-EXEMPT: migration", base="a" * 40)
        self.env["PR_CAP_MODE"] = "report-only"
        self.git(self.repo, "remote", "set-url", "origin", str(self.root / "missing.git"))
        result = self.check_cap()
        self.assert_fail(result)
        self.assertIn("cannot resolve base", result.stderr)
        self.assertNotIn("cannot fetch", result.stderr)

    def test_exclusions_comments_and_inline_tests(self):
        for name in ("docs/a.md", "tests/a.rs", "src/generated/a.rs", "src/fixtures/a.txt", "scripts/test_a.py"):
            path = self.repo / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("test\n" * 900)
        (self.repo / "src/prod.rs").write_text("// comment\n\nfn prod() {}\n#[cfg(test)]\nmod tests {\n" + "    fn t() {}\n" * 900 + "}\n")
        self.commit()
        self.assert_pass(self.check_cap(), "1 files net +1 code")

    def test_canonical_snapshot(self):
        digest = hashlib.sha256(HELPER.with_name("pr_cap_prod.py").read_bytes()).hexdigest()
        self.assertEqual(digest, "3a64a9eb82a7ceef9a243aa71181610b0ba831ad488006389e0839fbe9b3af76")

if __name__ == "__main__":
    unittest.main()
