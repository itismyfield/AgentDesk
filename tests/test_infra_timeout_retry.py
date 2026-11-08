"""Exercise the shipped retry entry point using only a local gh transport."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import yaml


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/ci/infra-failure-rerun.sh"
CANCEL = "##[error]The operation was canceled.\n"
TIMEOUT = "The job has exceeded the maximum execution time of 45m0s"
SHA = "a" * 40


class TimeoutRetry(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="infra-timeout-")
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.attempt = dict(name="CI PR", event="pull_request", status="completed",
                            conclusion="failure", run_attempt=1, head_sha=SHA,
                            pull_requests=[dict(number=42)])
        self.latest = dict(self.attempt)
        self.pull = dict(state="open", head=dict(sha=SHA))
        self.job = dict(id=901, name="Script checks runner (guards)", conclusion="cancelled",
                        check_run_url="https://api.github.com/repos/offline/repo/check-runs/77",
                        steps=[dict(name="Run script checks", conclusion="cancelled")])
        self.jobs = [self.job]
        self.annotations = [dict(path=".github", annotation_level="failure", message=TIMEOUT)]
        self.log = CANCEL
        self.write("gh", '''#!/usr/bin/env python3
import json, os, pathlib, sys
p = pathlib.Path(os.environ["FIXTURES"])
a = sys.argv[1:]
with (p / "calls.jsonl").open("a") as out:
    out.write(json.dumps(a) + "\\n")
sources = {
    "repos/offline/repo/actions/runs/7/attempts/1": "attempt.json",
    "repos/offline/repo/actions/runs/7/attempts/1/jobs?per_page=100": "jobs.json",
    "repos/offline/repo/actions/jobs/901/logs": "job.log",
    "repos/offline/repo/actions/jobs/902/logs": "worker.log",
    "repos/offline/repo/check-runs/77/annotations?per_page=100": "annotations.json",
    "repos/offline/repo/actions/runs/7": "latest.json",
    "repos/offline/repo/pulls/42": "pull.json",
    "repos/offline/repo/actions/runs/7/artifacts?per_page=100": "artifacts.json",
}
if a[:1] == ["api"] and len(a) == 2 and a[1] in sources:
    source = p / sources[a[1]]
    if not source.exists(): raise SystemExit(1)
    print(source.read_text(), end="")
elif a == ["run", "rerun", "7", "--repo", "offline/repo", "--job", "901"]:
    (p / "rerun").write_text("901")
else:
    (p / "refused").write_text(repr(a))
    raise SystemExit(1)
''')
        (self.directory / "gh").chmod(0o755)

    def write(self, name, value):
        (self.directory / name).write_text(value if isinstance(value, str) else json.dumps(value))

    def run_classifier(self, *, dry_run=True, attempt=1, annotation_api=True):
        for name, value in (("attempt.json", self.attempt), ("latest.json", self.latest),
                            ("pull.json", self.pull), ("job.log", self.log),
                            ("jobs.json", dict(total_count=len(self.jobs), jobs=self.jobs))):
            self.write(name, value)
        if annotation_api:
            self.write("annotations.json", self.annotations)
        summary = self.directory / "summary.md"
        env = {**os.environ, "PATH": str(self.directory) + os.pathsep + os.environ["PATH"],
               "FIXTURES": str(self.directory), "GITHUB_REPOSITORY": "offline/repo",
               "RUN_ID": "7", "RUN_ATTEMPT": str(attempt), "RERUN_DRY_RUN": str(int(dry_run)),
               "GITHUB_STEP_SUMMARY": str(summary)}
        result = subprocess.run(["bash", str(SCRIPT)], cwd=ROOT, env=env,
                                capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.directory / "refused").exists(), result.stdout + result.stderr)
        self.summary = summary.read_text()
        self.calls = [json.loads(line) for line in (self.directory / "calls.jsonl").read_text().splitlines()] if (self.directory / "calls.jsonl").exists() else []
        return result.stdout

    def test_cancelled_guard_annotation_is_infra_and_would_rerun(self):
        self.assertIn("decision=would-rerun:infra", self.run_classifier())
        self.assertIn("`infra-job-timeout`", self.summary)
        self.assertFalse((self.directory / "rerun").exists())

    def test_cancelled_run_reaches_the_timeout_classifier(self):
        self.attempt["conclusion"] = self.latest["conclusion"] = "cancelled"
        self.assertIn("decision=would-rerun:infra", self.run_classifier())

    def test_actual_retry_rechecks_attempt_and_current_pr_head(self):
        self.attempt["conclusion"] = self.latest["conclusion"] = "cancelled"
        self.assertIn("decision=rerun-requested:infra", self.run_classifier(dry_run=False))
        self.assertEqual((self.directory / "rerun").read_text(), "901")
        self.assertEqual(self.calls[-3:], [
            ["api", "repos/offline/repo/actions/runs/7"],
            ["api", "repos/offline/repo/pulls/42"],
            ["run", "rerun", "7", "--repo", "offline/repo", "--job", "901"]])

    def test_unittest_failure_before_summary_vetoes_timeout(self):
        self.log = "2026-11-08T06:00:00Z FAIL: test_guard (tests.example.Guard.test_guard)\n" + CANCEL
        self.assertIn("decision=no-op:regression", self.run_classifier(dry_run=False))
        self.assertIn("`regression`", self.summary)
        self.assertFalse((self.directory / "rerun").exists())

    def interrupted_unittest_log(self, verbosity, outcome, prefix):
        program = '''import sys, unittest
class Interrupted(unittest.TestCase):
    def test_a_pass(self):
        pass
    def test_b_failure(self):
        if sys.argv[2] == "FAIL": self.fail("guard failed")
        raise RuntimeError("guard errored")
    def test_c_interrupt(self):
        raise KeyboardInterrupt
try:
    unittest.main(argv=["fixture"], verbosity=int(sys.argv[1]))
except KeyboardInterrupt:
    raise SystemExit(130)
'''
        result = subprocess.run([sys.executable, "-c", program, str(verbosity), outcome],
                                capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 130, result.stderr)
        self.assertIn("." + outcome[0] if verbosity == 1 else "... " + outcome, result.stderr)
        self.assertNotRegex(result.stderr, r"FAIL:|ERROR:|Ran \d+ tests|FAILED \(")
        return "".join(prefix + line + "\n" for line in result.stderr.splitlines()) + prefix + CANCEL

    def assert_interrupted_unittest_vetoes_timeout(self, verbosity):
        for outcome in ("FAIL", "ERROR"):
            for prefix in ("", "2026-11-08T06:00:00.0000000Z ",
                           "Script checks runner (guards)\tRun script checks\t2026-11-08T06:00:00Z "):
                with self.subTest(outcome=outcome, prefix=prefix):
                    self.log = self.interrupted_unittest_log(verbosity, outcome, prefix)
                    self.write("calls.jsonl", "")
                    output = self.run_classifier(dry_run=False)
                    reruns = [call for call in self.calls if call[:2] == ["run", "rerun"]]
                    self.assertEqual(reruns, [], output)
                    self.assertIn("decision=no-op:regression", output)
                    self.assertIn("`regression`", self.summary)

    def test_interrupted_compact_unittest_vetoes_timeout(self):
        self.assert_interrupted_unittest_vetoes_timeout(1)

    def test_interrupted_verbose_unittest_vetoes_timeout(self):
        self.assert_interrupted_unittest_vetoes_timeout(2)

    def assert_progress_is_not_a_failure(self, progress):
        self.log = "".join(prefix + line + "\n" for prefix in (
            "", "2026-11-08T06:00:00Z ",
            "Script checks runner (guards)\tRun script checks\t2026-11-08T06:00:00.0000000Z ",
        ) for line in progress.splitlines()) + CANCEL
        self.assertIn("decision=would-rerun:infra", self.run_classifier())
        self.assertIn("`infra-job-timeout`", self.summary)

    def test_passing_compact_progress_does_not_veto_timeout(self):
        self.assert_progress_is_not_a_failure("....")

    def test_prose_failure_letters_do_not_veto_timeout(self):
        self.assert_progress_is_not_a_failure("cache key F\nF is a flag\nINFO: .F\nprofile E\nprogress: ... FAIL")

    def test_skipped_success_summary_does_not_veto_timeout(self):
        self.assert_progress_is_not_a_failure("...s\n----------------------------------------------------------------------\nRan 4 tests in 0.001s\n\nOK (skipped=1)")

    def test_unittest_error_before_summary_vetoes_timeout(self):
        self.log = "ERROR: test_guard (tests.example.Guard.test_guard)\n" + CANCEL
        self.assertIn("decision=no-op:regression", self.run_classifier())

    def test_unittest_class_setup_error_vetoes_timeout(self):
        self.log = "ERROR: setUpClass (tests.example.Guard)\n" + CANCEL
        self.assertIn("decision=no-op:regression", self.run_classifier())

    def test_completed_failure_summary_vetoes_timeout(self):
        self.log = "FAILED (failures=1)\n" + CANCEL
        self.assertIn("decision=no-op:regression", self.run_classifier())

    def test_repo_config_fault_vetoes_timeout(self):
        self.log = "The workflow is not valid.\n" + CANCEL
        self.assertNotIn("decision=would-rerun", self.run_classifier())

    def test_manual_cancel_and_log_only_timeout_are_not_retried(self):
        self.annotations = []
        self.log = CANCEL + TIMEOUT + "\n"
        self.assertNotIn("decision=would-rerun", self.run_classifier())

    def test_cancelled_worker_is_not_newly_retryable(self):
        self.job["name"] = "Rust tests (worker 0)"
        self.assertNotIn("decision=would-rerun", self.run_classifier())

    def test_other_cancelled_jobs_block_timeout_retry(self):
        self.jobs.append(dict(id=902, name="Rust tests (worker 0)", conclusion="cancelled"))
        self.write("worker.log", CANCEL)
        self.assertIn("decision=no-op:unknown", self.run_classifier())

    def test_failed_worker_still_requires_artifacts_beside_timeout(self):
        self.jobs.append(dict(id=902, name="Rust tests (worker 0)", conclusion="failure"))
        self.write("worker.log", "The runner has received a shutdown signal.\n")
        self.write("artifacts.json", dict(total_count=0, artifacts=[]))
        self.assertIn("decision=no-op:worker-artifact-veto", self.run_classifier())

    def test_failure_job_cannot_use_cancelled_timeout_path(self):
        self.job["conclusion"] = "failure"
        self.assertNotIn("decision=would-rerun", self.run_classifier())

    def test_annotation_api_failure_is_closed(self):
        self.assertNotIn("decision=would-rerun", self.run_classifier(annotation_api=False))

    def test_wrong_annotation_level_is_closed(self):
        self.annotations[0]["annotation_level"] = "warning"
        self.assertNotIn("decision=would-rerun", self.run_classifier())

    def test_other_annotation_timeout_text_is_closed(self):
        self.annotations[0]["message"] = "test exceeded the maximum execution time"
        self.assertNotIn("decision=would-rerun", self.run_classifier())

    def test_full_annotation_page_is_closed(self):
        self.annotations *= 100
        self.assertNotIn("decision=would-rerun", self.run_classifier())

    def test_malformed_annotations_are_closed(self):
        self.annotations = {"message": TIMEOUT}
        self.assertNotIn("decision=would-rerun", self.run_classifier())

    def test_foreign_check_run_url_is_closed(self):
        self.job["check_run_url"] = "https://api.github.com/repos/another/repo/check-runs/77"
        self.assertNotIn("decision=would-rerun", self.run_classifier())

    def test_extra_check_run_url_segments_are_closed(self):
        self.job["check_run_url"] += "/other/77"
        self.assertNotIn("decision=would-rerun", self.run_classifier())

    def test_duplicate_timeout_guard_rows_are_closed(self):
        self.jobs.append(dict(self.job))
        self.assertIn("decision=no-op:unknown", self.run_classifier())

    def test_missing_check_run_url_is_closed(self):
        del self.job["check_run_url"]
        self.assertNotIn("decision=would-rerun", self.run_classifier())

    def test_second_attempt_never_retries(self):
        self.assertIn("decision=no-op:attempt-cap", self.run_classifier(attempt=2))
        self.assertEqual(self.calls, [])

    def test_new_attempt_blocks_live_retry(self):
        self.latest["run_attempt"] = 2
        self.assertIn("decision=no-op:stale-attempt", self.run_classifier(dry_run=False))
        self.assertFalse((self.directory / "rerun").exists())

    def test_in_progress_attempt_blocks_live_retry(self):
        self.latest["status"] = "in_progress"
        self.assertIn("decision=no-op:stale-attempt", self.run_classifier(dry_run=False))

    def test_new_pr_head_blocks_live_retry(self):
        self.pull["head"]["sha"] = "b" * 40
        self.assertIn("decision=no-op:stale-head", self.run_classifier(dry_run=False))
        self.assertFalse((self.directory / "rerun").exists())

    def test_workflow_allows_only_failed_or_cancelled_initial_runs(self):
        workflow = yaml.safe_load((ROOT / ".github/workflows/ci-pr-infra-retry.yml").read_text())
        self.assertEqual(workflow["permissions"].get("checks"), "read")
        condition = workflow["jobs"]["retry"]["if"]
        for conclusion in ("failure", "cancelled", "success", "skipped"):
            for attempt in (1, 2):
                expression = condition.replace("github.event.workflow_run.conclusion", repr(conclusion))
                expression = expression.replace("github.event.workflow_run.run_attempt", str(attempt))
                actual = eval(expression.replace("&&", "and").replace("||", "or"), {"__builtins__": {}})
                self.assertEqual(actual, conclusion in ("failure", "cancelled") and attempt == 1)


class QuietFixtureCleanup(unittest.TestCase):
    def test_body_interrupt_cleanup_has_no_unraisable_error(self):
        result = subprocess.run([sys.executable, "-m", "unittest", "-v",
            "scripts.e2e.tui_relay.test_driver_health.HarnessOutcomeContract.test_http_body_interrupts_still_propagate"],
            cwd=ROOT, capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("AttributeError", result.stderr)
        self.assertNotIn("Exception ignored", result.stderr)

    def test_expired_read_fixture_has_no_server_thread_error(self):
        program = '''import threading, unittest
from scripts.e2e.tui_relay.test_post_deploy_turn_smoke import GatewayReadyTest
suite = unittest.TestSuite([GatewayReadyTest("test_a_channel_read_that_outlasts_the_deadline_is_not_ready")])
result = unittest.TextTestRunner(verbosity=2).run(suite)
for thread in threading.enumerate():
    if thread is not threading.current_thread() and "process_request_thread" in thread.name:
        thread.join(3)
raise SystemExit(not result.wasSuccessful())
'''
        result = subprocess.run([sys.executable, "-c", program], cwd=ROOT,
                                capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("BrokenPipeError", result.stderr)
        self.assertNotIn("Exception occurred", result.stderr)


if __name__ == "__main__":
    unittest.main()
