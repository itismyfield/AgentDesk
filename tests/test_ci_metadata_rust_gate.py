"""Exercise the changed-paths gate, real PR workflow expressions and required judge together."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

from tests import test_comment_only_ci_gate as harness
from tests.test_fast_check_ci_wiring import job_block
from tests.test_high_risk_recovery_path_filter import load_filters, pattern_to_regex


WORKFLOW = ".github/workflows/ci-pr.yml"
WORKERS = ("base_compare", "rust_scope", "rust_tests", "pg15_owner_proof")
EXECUTION_FILES = (
    "scripts/ci/nextest_ci.py",
    "scripts/ci/nextest-pr-worker.sh",
    "scripts/ci/nextest-base-diff.sh",
    "scripts/ci/lib-test-artifact.py",
    "scripts/ci/rust_test_sources.py",
    "scripts/ci/pg15_required_ids.txt",
    "scripts/check_relay_authority_contract.py",
    "tests/fixtures/herdr_t14_fake_provider.py",
    "scripts/ci/new-worker-helper.py",
)
SRC_FILTER = "              - 'src/**'"
BASE = """on: pull_request
jobs:
  rust_build:
    steps: [{run: build}]
  rust_scope: &worker
    steps: [{run: cargo nextest run}]
  rust_tests: *worker
  base_compare: {timeout-minutes: 60}
  pg15_owner_proof: {timeout-minutes: 80}
  test_evidence: {timeout-minutes: 15}
  lint:
    steps: [{run: lint}]
""" + job_block(harness.PR_WORKFLOW.read_text(encoding="utf-8"), "changes")


class CiMetadataRustGateTests(unittest.TestCase):
    def setUp(self):
        self.repo = harness.GateProcessTests()
        self.repo.setUp()
        self.addCleanup(self.repo.tearDown)
        self.repo.write(WORKFLOW, BASE)
        for path in EXECUTION_FILES[:-1]:
            self.repo.write(path, "# execution input\n")
        self.repo.base = self.repo.commit("workflow baseline")

    def run_case(self, changes, skip, gate_outcome="success", forced=None, alter_merge=None):
        for path, body in changes.items():
            self.repo.write(path, body)
        head = self.repo.commit("change")
        if alter_merge is not None:
            self.repo.git("checkout", "-q", "--detach", self.repo.base)
            self.repo.git("merge", "-q", "--no-ff", "--no-commit", head)
            self.repo.write(WORKFLOW, alter_merge)
            self.repo.commit("merge checkout")
        raw = harness.outputs(**{
            name: [path for path in changes if any(pattern_to_regex(p).match(path) for p in patterns)]
            for name, patterns in load_filters(self.repo.root / WORKFLOW).items()
        })
        result, raw_outputs = self.repo.run_gate(head, filters=json.dumps(raw))
        self.assertEqual(result.returncode, 0, result.stderr)
        outputs = dict(line.split("=", 1) for line in raw_outputs.splitlines())
        self.assertEqual(outputs.get("ci_metadata_only") == "true", skip, result.stdout)
        run = harness.WorkflowSimulation(raw, gate_outcome=gate_outcome, forced=forced,
                                         scope_workers="[0]", **outputs)
        return run

    def test_ci_metadata_with_unchanged_test_jobs_skips_workers_and_keeps_build(self):
        run = self.run_case({
            WORKFLOW: BASE.replace("run: build", "run: install-and-build").replace("run: lint", "run: lint-with-tool"),
            "scripts/ci/preinstalled-tool.sh": "#!/bin/bash\n",
            "scripts/check-ci-runner-hardening.sh": "#!/bin/bash\n",
            "tests/test_tool.py": "# Python test\n",
            "tests/test_tool.rb": "# Ruby test\n",
            "docs/tool.md": "# Notes\n",
        }, skip=True)
        self.assertEqual(run.results["rust_build"], "success")
        self.assertEqual(run.results["test_evidence"], "success")
        self.assertEqual({job: run.results[job] for job in WORKERS}, dict.fromkeys(WORKERS, "skipped"))
        self.assertEqual(run.results["lint"], "success")
        self.assertEqual(run.results["check_fast_cross_os"], "success")
        self.assertEqual(run.changes["high_risk_recovery"], "false")
        self.assertEqual(run.gate(), "success", run.verdicts())

    def test_changed_test_step_runs_workers(self):
        self.assert_full_run(self.run_case({WORKFLOW: BASE.replace("cargo nextest run", "cargo nextest run --no-fail-fast")}, skip=False))

    def test_rust_source_mixed_with_workflow_runs_workers(self):
        self.assert_full_run(self.run_case({WORKFLOW: BASE.replace("run: build", "run: build-new"),
                                           "src/lib.rs": "pub fn one() -> u32 { 2 }\n"}, skip=False))

    def test_ci_script_without_workflow_retains_full_run(self):
        self.assert_full_run(self.run_case({"scripts/ci/nextest-pr-worker.sh": "#!/bin/bash\n"}, skip=False))

    def test_execution_files_with_workflow_metadata_run_workers(self):
        for path in EXECUTION_FILES:
            with self.subTest(path=path):
                self.repo.git("checkout", "-q", "--detach", self.repo.base)
                self.assert_full_run(self.run_case({WORKFLOW: BASE + "# comment\n", path: "# new input\n"}, skip=False))

    def test_changes_path_filter_edit_runs_workers(self):
        self.assertIn(SRC_FILTER, BASE)
        self.assert_full_run(self.run_case({WORKFLOW: BASE.replace(SRC_FILTER, SRC_FILTER.replace("-", "# -", 1), 1)},
                                          skip=False))

    def test_changes_gate_command_edit_runs_workers(self):
        old_command = "python3 scripts/ci/comment_only_gate.py"
        self.assertIn(old_command, BASE)
        self.assert_full_run(self.run_case({WORKFLOW: BASE.replace(old_command, old_command + " --changed", 1)},
                                          skip=False))

    def test_rust_source_mixed_with_changed_filter_runs_workers(self):
        self.assertIn(SRC_FILTER, BASE)
        self.assert_full_run(self.run_case({WORKFLOW: BASE.replace(SRC_FILTER + "\n", "", 1),
                                           "src/lib.rs": "pub fn one() -> u32 { 2 }\n"}, skip=False))

    def test_rust_source_only_runs_workers(self):
        self.assert_full_run(self.run_case({"src/lib.rs": "pub fn one() -> u32 { 2 }\n"}, skip=False))

    def test_duplicate_yaml_job_falls_back_to_full_run(self):
        self.assert_full_run(self.run_case({WORKFLOW: BASE + "  rust_tests: {steps: []}\n"}, skip=False))

    def test_merge_checkout_with_changed_test_wiring_falls_back_to_full_run(self):
        head = BASE.replace("run: build", "run: build-new")
        self.assert_full_run(self.run_case({WORKFLOW: head}, skip=False,
                                         alter_merge=head.replace("timeout-minutes: 60", "timeout-minutes: 61")))

    def test_failed_gate_step_cannot_skip_workers_even_with_affirmative_output(self):
        self.assert_full_run(self.run_case({WORKFLOW: BASE + "# comment\n"}, skip=True, gate_outcome="failure"))

    def test_skipped_build_is_red_even_when_workers_are_waived(self):
        run = self.run_case({WORKFLOW: BASE + "# comment\n"}, skip=True, forced={"rust_build": "skipped"})
        self.assertEqual(run.gate(), "failure", run.verdicts())

    def test_evidence_records_skip_with_successful_build(self):
        run = self.run_case({WORKFLOW: BASE + "# comment\n"}, skip=True)
        steps = run.jobs["test_evidence"]["steps"]
        plan = next(step for step in steps if step.get("id") == "plan")
        self.assertFalse(harness.evaluate(plan["if"], run.lookup))
        record = next(step for step in steps if step.get("name") == "Record unavailable Rust evidence")
        env = {key: harness.render(value, run.lookup) for key, value in record["env"].items()}
        for build, tests, reason, mode in (("success", "false", "rust-path-filter", "skipped"),
                                           ("success", "true", "plan-unavailable", "full"),
                                           ("failure", "false", "rust-build-unavailable", "full")):
            with self.subTest(build=build, tests=tests), tempfile.TemporaryDirectory() as tmp:
                result = subprocess.run(["bash", "--noprofile", "--norc", "-c", record["run"]],
                                        cwd=harness.REPO_ROOT, capture_output=True, text=True,
                                        env={**os.environ, **env, "RUNNER_TEMP": tmp, "GITHUB_STEP_SUMMARY": "",
                                             "GITHUB_RUN_ID": "42", "GITHUB_RUN_ATTEMPT": "1",
                                             "BUILD_RESULT": build, "RUST_TESTS": tests})
                self.assertEqual(result.returncode, 0, result.stderr)
                observed = json.loads((Path(tmp) / "scope-observation.json").read_text())
                self.assertEqual((observed["reason"], observed["mode"]), (reason, mode))

    def assert_full_run(self, run):
        self.assertEqual(run.changes["rust_build"], "true")
        self.assertEqual(run.changes["rust_tests"], "true")
        self.assertEqual(run.results["rust_build"], "success")
        self.assertEqual({job: run.results[job] for job in WORKERS}, dict.fromkeys(WORKERS, "success"))
        self.assertEqual(run.gate(), "success", run.verdicts())
