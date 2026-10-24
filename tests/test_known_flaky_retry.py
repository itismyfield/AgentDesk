"""Exercise the workflow entry point with a local gh transport double."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import yaml


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/ci/known-flaky-rerun.py"
sys.path.insert(0, str(SCRIPT.parent))
spec = importlib.util.spec_from_file_location("flaky_retry", SCRIPT)
retry = importlib.util.module_from_spec(spec)
spec.loader.exec_module(retry)
import nextest_required  # noqa: E402  (loaded from scripts/ci by the retry module)
sys.path.pop(0)
KNOWN = "suite::known_failure"
UNKNOWN = "suite::new_regression"
HEAD_SHA = "a" * 40


def judge_log(context, failed):
    """The judge step's real output for `context` when the jobs in `failed` fail and the rest pass."""
    jobs = [job for group, _ in nextest_required.CONTEXTS[context] for job in group]
    outputs = {name: "true" for name in ("rust_tests", "rust_or_policy", "relay_contract", "high_risk_recovery",
                                         "dashboard", "rust_tests_skip", "cross_os_rust")}
    outputs["rust_tests_skip"] = "false"
    needs = {"changes": {"result": "success", "outputs": outputs},
             **{job: {"result": "failure" if job in failed else "success"} for job in jobs}}
    ok, lines = nextest_required.context_verdict(context, needs)
    assert not ok
    return "".join(f"2026-10-22T08:57:43.9533985Z ##[error]{context}: {line}\n" for line in lines) + (
        f"{context}: FAIL\n2026-10-22T08:57:43.9544388Z ##[error]Process completed with exit code 1.\n")


def rust_log(*names):
    return "\n".join([
        *(f"test {name} ... FAILED" for name in names),
        f"test result: FAILED. 12 passed; {len(names)} failed; 0 ignored;",
        *(f"ERROR unlisted test failure: {name}. Fix it, or add it to the ledger." for name in names),
    ])


class Retry(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="known-flaky-")
        self.addCleanup(self.tmp.cleanup)
        self.path = Path(self.tmp.name)
        self.script = self.path / SCRIPT.name
        self.script.write_bytes(SCRIPT.read_bytes())
        for name in ("pr_head_current.py", "nextest_required.py"):
            (self.path / name).write_bytes((SCRIPT.parent / name).read_bytes())
        self.write("known-flaky-tests.json", [{
            "test": KNOWN, "run": "https://github.com/itismyfield/AgentDesk/actions/runs/39703194502",
            "reason": "Reproduced scheduler-dependent fixture race.",
        }])
        self.attempt = dict(name="CI PR", event="pull_request", status="completed", conclusion="failure", run_attempt=1,
                            head_sha=HEAD_SHA, pull_requests=[dict(number=42)])
        self.write("attempt.json", self.attempt)
        self.write("latest.json", self.attempt)
        self.write("pull.json", dict(state="open", head=dict(sha=HEAD_SHA)))
        self.jobs(self.job(901))
        (self.path / "901.log").write_text(rust_log(KNOWN))
        stub = self.path / "gh"
        stub.write_text("""#!/usr/bin/env python3
import json, os, pathlib, sys
p = pathlib.Path(os.environ['FIXTURES'])
args = sys.argv[1:]
with (p / 'calls.jsonl').open('a') as f: f.write(json.dumps(args) + '\\n')
if args[:2] == ['run', 'rerun']: sys.exit(int(os.environ.get('FAIL_RERUN', '0')))
if args[:2] == ['run', 'download']:
    import shutil
    shutil.copytree(p / 'artifacts' / args[args.index('--name') + 1], args[args.index('--dir') + 1])
    sys.exit(0)
if args[0] != 'api': sys.exit(99)
url = args[1]
if '/pulls/' in url: file = 'pull.json'
elif '/jobs?' in url: file = 'jobs.json'
elif '/attempts/' in url: file = 'attempt.json'
elif url.endswith('/logs'): file = url.split('/')[-2] + '.log'
elif '/artifacts?' in url: file = 'artifacts.json'
else: file = 'latest.json'
sys.stdout.write((p / file).read_text())
""")
        stub.chmod(0o755)

    def write(self, name, data):
        (self.path / name).write_text(json.dumps(data))

    @staticmethod
    def job(job_id, conclusion="failure"):
        return dict(id=job_id, conclusion=conclusion, steps=[dict(name="Test", conclusion=conclusion)])

    def jobs(self, *jobs):
        self.write("jobs.json", dict(total_count=len(jobs), jobs=jobs))

    def mirrored_failure(self, **overrides):
        source = {**self.job(901), "name": "Script checks runner (guards)"}
        judge = {**self.job(902), "name": "Script checks", "steps": [
            dict(name="Judge required context", conclusion="failure"),
        ], **overrides}
        self.jobs(source, judge, {**self.job(903, "success"), "name": "Changed paths"})
        (self.path / "902.log").write_text(judge_log("Script checks", {"scripts_guards"}))

    def run_retry(self, attempt=1, command=None, **extra):
        for name in ("calls.jsonl", "output", "summary"):
            (self.path / name).unlink(missing_ok=True)
        result = subprocess.run(command or [sys.executable, str(self.script)], text=True, capture_output=True, check=True, env={
            **os.environ, "PATH": f"{self.path}{os.pathsep}{os.environ['PATH']}",
            "FIXTURES": str(self.path), "GITHUB_REPOSITORY": "itismyfield/AgentDesk",
            "RUN_ID": "123", "RUN_ATTEMPT": str(attempt), "GITHUB_OUTPUT": str(self.path / "output"),
            "GITHUB_STEP_SUMMARY": str(self.path / "summary"), "RERUN_DRY_RUN": "0", **extra,
        })
        calls = self.path / "calls.jsonl"
        return result.stdout, [json.loads(row) for row in calls.read_text().splitlines()] if calls.exists() else []

    def infra_command(self):
        scripts = self.path / "repo/scripts"
        (scripts / "ci").mkdir(parents=True, exist_ok=True)
        for name in ("ci/infra-failure-rerun.sh", "ci/real-failure-predicate.sh", "ci/pr_head_current.py",
                     "ci/nextest_ci.py", "ci/nextest_required.py"):
            (scripts / name).write_bytes((ROOT / "scripts" / name).read_bytes())
        # The worker died before uploading; the run's plan artifact still lets classify-worker decide.
        self.write("list.json", {"test-count": 1, "rust-suites": {"agentdesk": {
            "binary-id": "agentdesk", "kind": "lib", "status": "listed", "testcases": {
                "m::a": {"ignored": False, "filter-match": {"status": "matches"}}}}}})
        (self.path / "lib-tests.tar.zst").write_bytes(b"archive")
        (self.path / "policy.toml").write_text("version = 1\n")
        subprocess.run([sys.executable, str(ROOT / "scripts/ci/nextest_ci.py"), "plan", "--universe", self.path / "list.json",
                        "--archive", self.path / "lib-tests.tar.zst", "--workers", "1", "--sha", HEAD_SHA,
                        "--scope", "linux-lib", "--run-id", "7", "--base-config", ROOT / ".config/nextest.toml",
                        "--retry-policy", self.path / "policy.toml", "--today", "2026-10-24",
                        "--output", self.path / "artifacts/nextest-plan/plan", "--rust-version", "1.94.1",
                        "--nextest-version", "0.9.149", "--target", "x86_64-unknown-linux-gnu"],
                       check=True, capture_output=True)
        self.write("artifacts.json", {"total_count": 1, "artifacts": [{"name": "nextest-plan", "expired": False}]})
        self.jobs({**self.job(901), "name": "Rust tests (worker 0)"})
        (self.path / "901.log").write_text("Failed to resolve action download info. Error: Service Unavailable\n")
        return ["bash", str(scripts / "ci/infra-failure-rerun.sh")]

    def test_infra_advanced_head_makes_zero_rerun_requests(self):
        command = self.infra_command()
        self.write("pull.json", dict(state="open", head=dict(sha="b" * 40)))
        out, calls = self.run_retry(command=command)
        self.assertIn("no-op:stale-head", out)
        self.assertFalse(any(c[:2] == ["run", "rerun"] for c in calls), calls)

    def test_infra_current_head_retries_once_and_second_attempt_cannot_retry(self):
        command = self.infra_command()
        out, calls = self.run_retry(command=command)
        self.assertIn("rerun-requested:infra", out)
        self.assertEqual([c for c in calls if c[:2] == ["run", "rerun"]], [
            ["run", "rerun", "123", "--repo", "itismyfield/AgentDesk", "--job", "901"],
        ])
        out, calls = self.run_retry(command=command, attempt=2)
        self.assertIn("attempt-cap", out)
        self.assertEqual(calls, [])

    def test_flaky_exception_still_runs_workflow_infra_fallback_once(self):
        command = self.infra_command()
        self.script.write_text("raise RuntimeError('classifier fixture failure')\n")
        flaky = subprocess.run([sys.executable, str(self.script)], capture_output=True)
        self.assertNotEqual(flaky.returncode, 0)
        workflow = yaml.safe_load((ROOT / ".github/workflows/ci-pr-infra-retry.yml").read_text())
        step = next(s for s in workflow["jobs"]["retry"]["steps"]
                    if s.get("run") == "./scripts/ci/infra-failure-rerun.sh")
        condition = step["if"].removeprefix("${{").removesuffix("}}").strip()
        # GitHub adds success() unless the condition explicitly checks status.
        has_status = any(f"{name}(" in condition for name in ("cancelled", "always", "failure", "success"))
        for cancelled, handled, expected in ((False, "", 1), (False, "true", 0), (True, "", 0)):
            with self.subTest(cancelled=cancelled, handled=handled):
                expression = condition.replace("!cancelled()", str(not cancelled)).replace("&&", "and")
                expression = expression.replace("steps.flaky.outputs.handled", repr(handled))
                should_run = has_status and eval(expression, {"__builtins__": {}})
                calls = self.run_retry(command=command)[1] if should_run else []
                self.assertEqual(sum(c[:2] == ["run", "rerun"] for c in calls), expected)

    def assert_no_retry(self, **extra):
        out, calls = self.run_retry(**extra)
        self.assertNotIn("rerun-requested:", out)
        self.assertFalse(any(call[:2] == ["run", "rerun"] for call in calls), calls)
        self.assertEqual((self.path / "output").read_text(), "handled=false\n")
        return out

    def test_only_failed_jobs_are_retried_exactly_once(self):
        self.jobs(self.job(901), self.job(902), self.job(903, "success"))
        (self.path / "902.log").write_text(rust_log(KNOWN))
        out, calls = self.run_retry()
        self.assertIn("rerun-requested:known-flaky", out)
        self.assertEqual([c for c in calls if c[:2] == ["run", "rerun"]], [
            ["run", "rerun", "123", "--repo", "itismyfield/AgentDesk", "--failed"],
        ])
        self.assertEqual((self.path / "output").read_text(), "handled=true\n")
        self.assertTrue(all("--allow-escape-sequences" in call for call in calls
                            if call[0] == "api" and call[1].endswith("/logs")))

    def test_second_attempt_cannot_retry_even_when_it_failed_again(self):
        self.attempt["run_attempt"] = 2
        self.write("attempt.json", self.attempt)
        self.write("latest.json", self.attempt)
        self.assertIn("attempt-cap", self.assert_no_retry(attempt=2))

    def test_mixed_known_and_unlisted_failure_does_not_retry(self):
        (self.path / "901.log").write_text(rust_log(KNOWN, UNKNOWN))
        self.assertIn("unlisted-failure", self.assert_no_retry())

    def test_removed_entries_leave_an_empty_allowlist_with_no_retry(self):
        self.write("known-flaky-tests.json", [])
        self.assertIn("empty-allowlist", self.assert_no_retry())

    def test_unlisted_failure_in_another_job_blocks_retry(self):
        self.jobs(self.job(901), self.job(902))
        (self.path / "902.log").write_text(rust_log(UNKNOWN))
        self.assert_no_retry()

    def test_stale_attempt_is_not_retried(self):
        self.write("latest.json", {**self.attempt, "run_attempt": 2})
        self.assertIn("stale-attempt", self.assert_no_retry())

    def test_advanced_pr_head_is_not_retried(self):
        self.write("pull.json", dict(state="open", head=dict(sha="b" * 40)))
        self.assertIn("stale-head", self.assert_no_retry())

    def test_missing_closed_or_unavailable_pr_head_cannot_retry(self):
        for pull in ({}, dict(state="closed", head=dict(sha=HEAD_SHA)), dict(state="open", head=None)):
            with self.subTest(pull=pull):
                self.write("pull.json", pull)
                self.assert_no_retry()
        (self.path / "pull.json").unlink()
        self.assert_no_retry()

    def test_run_without_associated_pr_cannot_retry(self):
        self.write("attempt.json", {**self.attempt, "pull_requests": []})
        self.assert_no_retry()

    def test_truncated_log_is_not_retried(self):
        (self.path / "901.log").write_text(f"test {KNOWN} ... FAILED\n")
        self.assert_no_retry()

    def test_cancelled_job_is_not_a_known_failure(self):
        self.jobs(self.job(901), self.job(902, "cancelled"))
        self.assert_no_retry()

    def test_api_rejection_cannot_report_a_requested_retry(self):
        out, _ = self.run_retry(FAIL_RERUN="1")
        self.assertIn("unavailable-evidence", out)
        self.assertEqual((self.path / "output").read_text(), "handled=false\n")

    def test_missing_steps_cannot_be_retried(self):
        self.jobs(dict(id=901, conclusion="failure"))
        self.assert_no_retry()

    def test_known_source_and_its_required_context_mirror_retry(self):
        self.mirrored_failure()
        out, calls = self.run_retry()
        self.assertIn("rerun-requested:known-flaky", out)
        self.assertEqual(sum(call[:2] == ["run", "rerun"] for call in calls), 1)

    def test_judge_links_each_failed_job_to_its_exact_source(self):
        for context, failed, source in (("Lint", {"lint"}, "Lint runner"),
                                        ("relay-authority-contract", {"relay_authority_mutations"},
                                         "Relay authority mutations (2)")):
            with self.subTest(context=context):
                self.mirrored_failure(name=context)
                jobs = json.loads((self.path / "jobs.json").read_text())["jobs"]
                jobs[0]["name"] = source
                self.jobs(*jobs)
                (self.path / "902.log").write_text(judge_log(context, failed))
                self.assertIn("rerun-requested:known-flaky", self.run_retry()[0])

    def test_mirror_requires_its_own_classified_failed_source(self):
        self.mirrored_failure()
        jobs = json.loads((self.path / "jobs.json").read_text())["jobs"]
        for name in ("Script checks runner (contracts)", "Rust tests (worker 1)"):
            with self.subTest(source=name):
                jobs[0]["name"] = name
                self.jobs(*jobs)
                self.assertIn("unlisted-mirror", self.assert_no_retry())

    def test_mirror_rejects_pin_and_unknown_step_failure(self):
        for name in ("Verify pinned judge and CI structure", "Unrelated independent check"):
            with self.subTest(name=name):
                self.mirrored_failure(steps=[dict(name=name, conclusion="failure")])
                self.assertIn("unlisted-mirror", self.assert_no_retry())

    def test_mirror_rejects_mismatched_or_additional_errors(self):
        self.mirrored_failure()
        original = (self.path / "902.log").read_text()
        for log in (
            original.replace("scripts_guards: 'failure'", "scripts_contracts: 'failure'"),
            original.replace("expected to run: true\n2026-10-22T08:57:43.9533985Z ##[error]Script checks: scripts_contracts",
                             "expected to run: false\n2026-10-22T08:57:43.9533985Z ##[error]Script checks: scripts_contracts"),
            original.replace("'failure' but", "'cancelled' but"),
            original.replace("Script checks: rust_tests: success", "Lint: rust_tests: success"),
            original.replace("##[error]Script checks: test_evidence: success (expected to run: true)\n", ""),
            original + "##[error]content hash mismatch: expected trusted, found changed\n",
            original + "ERROR an independent check failed\n",
            original.replace("##[error]Script checks: scripts_guards", "Script checks: scripts_guards"),
            judge_log("Script checks", {"scripts_guards", "rust_tests"}),
            rust_log(KNOWN),
        ):
            with self.subTest(log=log):
                (self.path / "902.log").write_text(log)
                self.assertIn("unlisted-mirror", self.assert_no_retry())

    def test_mirror_requires_a_successful_changes_job(self):
        self.mirrored_failure()
        jobs = json.loads((self.path / "jobs.json").read_text())["jobs"]
        for changes in (None, "skipped", "failure"):
            with self.subTest(changes=changes):
                self.jobs(*jobs[:2], *([] if changes is None else [
                    {**jobs[2], "conclusion": changes}]))
                if changes == "failure":
                    (self.path / "903.log").write_text("##[error]Changed paths failed\n")
                self.assert_no_retry()

    def test_mirror_cannot_hide_an_unlisted_source_failure(self):
        self.mirrored_failure()
        (self.path / "901.log").write_text(rust_log(UNKNOWN))
        self.assertIn("unlisted-failure", self.assert_no_retry())


class Parser(unittest.TestCase):
    def test_independent_actions_error_beside_known_failure_blocks_retry(self):
        for error in ("##[error]independent audit failed", "::error::independent audit failed"):
            with self.subTest(error=error):
                self.assertIsNone(retry.failed_tests(rust_log(KNOWN) + "\n" + error))
        for status in (1, 101):
            self.assertEqual(retry.failed_tests(rust_log(KNOWN) +
                             f"\n##[error]Process completed with exit code {status}."), {KNOWN})

    def test_mirror_contracts_match_the_trusted_workflow(self):
        jobs = yaml.safe_load((ROOT / ".github/workflows/ci-pr.yml").read_text())["jobs"]
        judge = jobs["required_contexts"]
        self.assertEqual(set(judge["strategy"]["matrix"]["context"]), set(nextest_required.CONTEXTS))
        self.assertEqual([s["run"] for s in judge["steps"] if s.get("name") == retry.JUDGE_STEP],
                         ['python3 scripts/ci/nextest_ci.py required-context "${{ matrix.context }}"'])
        judged = {job for groups in nextest_required.CONTEXTS.values() for group, _ in groups for job in group}
        self.assertEqual(judged, set(retry.JOB_NAMES))
        for job_id in judged:
            rendered = jobs[job_id]["name"].replace("${{ matrix.worker }}", "1").replace(
                "${{ matrix.shard }}", "1").replace("${{ matrix.os }}", "windows-latest")
            self.assertRegex(rendered, "^" + retry.JOB_NAMES[job_id] + "$")
        contracts = set()
        for job_id, job in jobs.items():
            for step in job.get("steps", []):
                if step.get("run", "").strip() != "./scripts/required-check-mirror.sh":
                    continue
                key = (job.get("name", job_id), step.get("name", "Run ./scripts/required-check-mirror.sh"))
                contracts.add(key)
                filter_name, upstream, source_pattern = retry.MIRRORS[key]
                self.assertEqual(step["env"]["FILTER_NAME"], filter_name)
                self.assertEqual(step["env"]["UPSTREAM_JOB_NAME"], upstream)
                self.assertEqual(step["env"]["UPSTREAM_RESULT"], "${{ needs." + upstream + ".result }}")
                source_name = jobs[upstream].get("name", upstream)
                source_name = source_name.replace("${{ matrix.shard }}", "1").replace("${{ matrix.os }}", "windows-latest")
                self.assertRegex(source_name, "^" + source_pattern + "$")
        self.assertEqual(contracts, set(retry.MIRRORS))

    def test_nested_child_and_outer_failures_match_occurrence_counts(self):
        # Isolated libtests report the child failure and then its outer failure.
        log = (f"test {KNOWN} ... FAILED\n"
               f"test {KNOWN} ... FAILED\n"
               "test result: FAILED. 0 passed; 1 failed; 0 ignored;\n"
               "test result: FAILED. 775 passed; 1 failed; 2 ignored;\n")
        self.assertEqual(retry.failed_tests(log), {KNOWN})
        self.assertIsNone(retry.failed_tests(log.rsplit("test result:", 1)[0]))

    def test_python_summary_supplemental_counts_are_not_failures(self):
        heading = "ERROR: test_mapping (tests.test_h2_session.Fence.test_mapping)\n"
        for summary in ("errors=1, skipped=2", "errors=1, expected failures=3",
                        "errors=1, skipped=2, expected failures=3"):
            with self.subTest(summary=summary):
                self.assertEqual(retry.failed_tests(heading + f"FAILED ({summary})\n"),
                                 {"tests.test_h2_session.Fence.test_mapping"})
        self.assertIsNone(retry.failed_tests(
            heading + "FAILED (errors=1, skipped=2, unexpected successes=1)\n"))

    def test_lane_missing_selection_gate_beside_known_failure_blocks_retry(self):
        # Actual run_test_lane emits both count-prefixed and prose gate errors.
        for diagnostic in (
            "ERROR 1 lane-missing (expected but never reported) id(s), named above.",
            "ERROR 1 not-executed id(s), named above.",
            "ERROR libtest reported 1 selected tests (0 passed + 1 failed + 0 ignored) "
            "but the manifest derives 2 for this lane.",
        ):
            with self.subTest(diagnostic=diagnostic):
                self.assertIsNone(retry.failed_tests(rust_log(KNOWN) + "\n" + diagnostic))

    def test_lane_status_gate_beside_known_failure_blocks_retry(self):
        self.assertIsNone(retry.failed_tests(rust_log(KNOWN) +
                          "\nERROR command exited 0 while libtest reported failures\n"))

    def test_rust_timestamps_and_lane_diagnostics(self):
        log = "\n".join("job\tstep\t2026-10-22T01:02:03.000Z " + line for line in rust_log(KNOWN).splitlines())
        self.assertEqual(retry.failed_tests(log), {KNOWN})

    def test_python_versions_use_the_same_exact_name(self):
        for suffix in ("", ".test_mapping"):
            self.assertEqual(retry.failed_tests(
                f"ERROR: test_mapping (tests.test_h2_session.Fence{suffix})\nFAILED (errors=1)\n"
            ), {"tests.test_h2_session.Fence.test_mapping"})

    def test_mixed_compile_failure_and_incomplete_summary_fail_closed(self):
        for log in (rust_log(KNOWN) + "\nerror[E0001]: compile error", rust_log(KNOWN).replace("1 failed", "2 failed"),
                    rust_log(KNOWN) + "\nFAIL: unknown shell assertion", "test result: FAILED. 0 passed; 1 failed;"):
            self.assertIsNone(retry.failed_tests(log))

    def test_manifest_and_required_workflow_wiring(self):
        retry.load_known(retry.MANIFEST)
        workflow = yaml.safe_load((ROOT / ".github/workflows/ci-pr-infra-retry.yml").read_text())
        self.assertEqual(workflow["run-name"],
                         "CI PR retry ${{ github.event.workflow_run.id }} "
                         "${{ github.event.workflow_run.head_sha }} attempt ${{ github.event.workflow_run.run_attempt }}")
        job = workflow["jobs"]["retry"]
        self.assertEqual(workflow["permissions"]["pull-requests"], "read")
        self.assertIn("run_attempt == 1", job["if"])
        steps = job["steps"]
        self.assertEqual(steps[0]["with"]["ref"], "${{ github.event.repository.default_branch }}")
        flaky = next(step for step in steps if step.get("id") == "flaky")
        self.assertEqual(flaky["run"], "python3 scripts/ci/known-flaky-rerun.py")
        infra = next(step for step in steps if step.get("run") == "./scripts/ci/infra-failure-rerun.sh")
        self.assertEqual(infra["if"], "${{ !cancelled() && steps.flaky.outputs.handled != 'true' }}")
        self.assertIn("tests.test_known_flaky_retry", (ROOT / "scripts/ci-script-checks.sh").read_text())


if __name__ == "__main__":
    unittest.main()
