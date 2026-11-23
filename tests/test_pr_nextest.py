"""Exercise PR phase planning and evidence through the nextest CI CLI."""

import json
import os
import sys
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_nextest_ci import BIN, BUILD, SHA, Fixture, nextest_list, nci, source_losses  # noqa: E402
from nextest_scope import ScopeSelection  # noqa: E402


NAMES = ["m::core", "m::selected", "m::remainder1", "m::remainder2"]
CORE = {(BIN, NAMES[0])}
SCOPE = CORE | {(BIN, NAMES[1])}


class PrNextestTest(Fixture):
    def setUp(self):
        super().setUp()
        self.enterContext(mock.patch.dict(os.environ, {
            "GITHUB_HEAD_REF": "topic", "GITHUB_RUN_ID": "7", "GITHUB_RUN_ATTEMPT": "1",
            "PR_HEAD_SHA": SHA, "GITHUB_STEP_SUMMARY": str(self.dir / "job-summary.md"),
        }))
        self.base = self.enterContext(mock.patch(
            "nextest_required.comparison_base", return_value=("merge", SHA)))
        self.core = self.enterContext(mock.patch("pr_scope_core.select_core_ids", return_value=CORE))
        self.selector = self.enterContext(mock.patch(
            "nextest_scope.select_scope", return_value=ScopeSelection("scope", "fixture", frozenset(SCOPE))))

    def pr_plan(self):
        rc, err = self.run_cli(
            "plan", "--universe", self.write("list.json", nextest_list(NAMES)), "--archive", self.archive,
            "--workers", 2, "--sha", SHA, "--scope", "linux-lib", "--base-config", self.dir / "base.toml",
            "--retry-policy", self.dir / "empty.toml", "--output", self.dir / "plan",
            "--pr-scope", self.dir / "matrix", *[value for pair in BUILD.items() for value in pair],
        )
        self.assertEqual(rc, 0, err)
        return nci.load_plan(self.dir / "plan/plan.json")

    def observe(self, scope_result="success", full_result="failure"):
        report = self.dir / "scope-observation.json"
        rc, err = self.run_cli(
            "audit", "--plan", self.dir / "plan/plan.json", "--results", self.dir / "results",
            "--json-out", self.dir / "verdict.json", "--summary-out", self.dir / "summary.md",
            "--pr-scope-out", report, "--pr-scope-result", scope_result, "--pr-full-result", full_result,
        )
        self.assertTrue(report.is_file(), err)
        return rc, json.loads(report.read_text())

    def test_scope_keeps_core_and_partition_covers_original_base_inventory(self):
        self.plan_ok(NAMES, out="base")
        plan = self.pr_plan()
        count = plan["pr_scope"]["scope_workers"]
        phase = [{tuple(t) for w in workers for t in w["tests"]}
                 for workers in (plan["workers"][:count], plan["workers"][count:])]
        self.assertEqual(phase, [SCOPE, {(BIN, name) for name in NAMES} - SCOPE])
        self.assertEqual((self.dir / "matrix").read_text(), "scope_workers=[0,1]\nfull_workers=[2,3]\n")
        self.assertEqual(self.selector.call_args.kwargs["core_ids"], CORE)
        diff_args = (
            "base-diff", "--plan", self.dir / "plan/plan.json", "--baseline", self.dir / "base/plan.json",
            "--comparison-base-sha", SHA, "--head-sha", SHA, "--head-run-id", "7",
            "--justifications", self.write("justify.txt", ""),
            "--source-removed", self.write("removed.json", source_losses()), "--json-out", self.dir / "diff.json",
        )
        rc, err = self.run_cli(*diff_args, "--head-universe", self.dir / "list.json")
        self.assertEqual(rc, 0, err)
        self.assertEqual(json.loads((self.dir / "diff.json").read_text())["removed"], [])
        rc, err = self.run_cli(*diff_args, "--head-universe", self.write("partial.json", nextest_list(NAMES[:2])))
        self.assertEqual(rc, 1, err)
        self.assertIn("does not match the full plan", err)

    def test_landing_batch_bypasses_git_and_selector_and_gets_full_sweep(self):
        with mock.patch.dict(os.environ, {"GITHUB_HEAD_REF": "land/batch-example"}):
            plan = self.pr_plan()
        self.assertEqual(plan["pr_scope"], {"mode": "full", "reason": "landing-batch", "scope_workers": 0})
        for ignored in (self.base, self.core, self.selector):
            ignored.assert_not_called()
        self.worker_artifacts(plan)
        rc, record = self.observe("skipped", "success")
        self.assertEqual((rc, record["scope_state"], record["full_state"]), (0, "not_run", "complete"))

    def test_missing_scope_results_fail_the_scope_worker(self):
        plan = self.pr_plan()
        self.worker_artifacts(plan, overrides={0: []})
        rc, err = self.run_cli("audit-scope-worker", "--plan", self.dir / "plan/plan.json",
                               "--worker", 0, "--record", self.dir / "results/worker-0")
        self.assertEqual(rc, 1, err)
        self.assertIn("planned but not executed", self.out.getvalue())

    def test_scope_failure_censors_the_skipped_remainder(self):
        plan = self.pr_plan()
        failed = self.worker_tests(plan, 0)[0]
        self.worker_artifacts(plan, overrides={0: [(failed, "<failure/>", 0.5)]})
        self.write("results/worker-0/exit-code", "100\n")
        for worker in plan["workers"][plan["pr_scope"]["scope_workers"]:]:
            name = f"worker-{worker['index']}"
            (self.dir / "results" / name).rename(self.dir / name)
        rc, record = self.observe("failure", "skipped")
        self.assertEqual((rc, record["scope_state"], record["full_state"]), (1, "failed", "not_run"))
        self.assertEqual(record["scope_failure_ids"], [[BIN, failed]])
        self.assertEqual(record["full_failure_ids"], [])
        self.assertEqual(record["scope_miss_ids"], [])

    def test_full_final_failure_is_a_scope_miss_and_keeps_audit_red(self):
        plan = self.pr_plan()
        failed = self.worker_tests(plan, 2)[0]
        self.worker_artifacts(plan, overrides={2: [(failed, "<failure/>", 0.5)]})
        self.write("results/worker-2/exit-code", "100\n")
        self.write("results/worker-0/scope-time", "100 104\n")
        self.write("results/worker-1/scope-time", "102 106\n")
        rc, record = self.observe()
        self.assertEqual((rc, record["scope_state"], record["full_state"]), (1, "passed", "complete"))
        self.assertEqual(record["scope_miss_ids"], [[BIN, failed]])
        self.assertEqual(record["scope_seconds"], 6)
        self.assertIn("SCOPE_MISS=1", (self.dir / "job-summary.md").read_text())
        with mock.patch.dict(os.environ, {"GITHUB_RUN_ATTEMPT": "2"}):
            _, rerun = self.observe()
        self.assertIsNone(rerun["scope_seconds"])
        self.assertEqual(rerun["scope_miss_ids"], record["scope_miss_ids"])

    def test_exit_test_failure_without_failed_case_is_incomplete(self):
        plan = self.pr_plan()
        self.worker_artifacts(plan)
        self.write("results/worker-2/exit-code", "100\n")
        rc, record = self.observe()
        self.assertEqual((rc, record["full_state"], record["scope_miss_ids"]), (1, "incomplete", []))
