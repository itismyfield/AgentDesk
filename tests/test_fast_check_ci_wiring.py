"""Static contracts for the PR fast-compile and retained test lanes."""

from __future__ import annotations

import re
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
PR_WORKFLOW = REPO_ROOT / ".github/workflows/ci-pr.yml"
MAIN_WORKFLOW = REPO_ROOT / ".github/workflows/ci-main.yml"
NIGHTLY_WORKFLOW = REPO_ROOT / ".github/workflows/ci-nightly.yml"


def job_block(workflow: str, job_name: str) -> str:
    marker = re.compile(rf"^  {re.escape(job_name)}:\n", re.MULTILINE)
    match = marker.search(workflow)
    if match is None:
        raise AssertionError(f"missing workflow job: {job_name}")
    next_job = re.compile(r"^  [A-Za-z0-9_-]+:\n", re.MULTILINE).search(
        workflow, match.end()
    )
    return workflow[match.start() : next_job.start() if next_job else len(workflow)]


class FastCheckCiWiringTests(unittest.TestCase):
    def test_pr_fast_check_is_compile_and_policy_only(self) -> None:
        job = job_block(PR_WORKFLOW.read_text(encoding="utf-8"), "check_fast")

        self.assertIn("name: Fast compile check (${{ matrix.os }})", job)
        self.assertIn(
            "if: needs.changes.outputs.rust_or_policy == 'true' || "
            "needs.changes.outputs.relay_contract == 'true'",
            job,
        )
        self.assertIn("os: [ubuntu-latest]", job)
        self.assertIn("- name: Policy JS unit tests", job)
        self.assertIn("- name: cargo check\n        run: just cargo-check", job)
        self.assertNotIn("just test-non-pg", job)
        self.assertNotRegex(job, r"(?m)^\s*cargo test\b")

    def test_required_fast_check_context_mirrors_the_same_upstream_job(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        job = job_block(workflow, "fast_check_required_context")

        self.assertIn("name: Fast check (ubuntu-latest)", job)
        self.assertIn("- check_fast", job)
        self.assertIn("if: always()", job)
        self.assertEqual(job.count("UPSTREAM_JOB_NAME: check_fast"), 2)
        self.assertIn(
            "if: ${{ needs.changes.outputs.relay_contract != 'true' }}", job
        )
        self.assertIn(
            "if: ${{ needs.changes.outputs.relay_contract == 'true' }}", job
        )

        lint_job = job_block(workflow, "lint")
        self.assertIn(
            "if: needs.changes.outputs.rust_or_policy == 'true' || "
            "needs.changes.outputs.relay_contract == 'true'",
            lint_job,
        )

    def test_main_and_nightly_retain_non_pg_test_coverage(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        self.assertIn("check: fmt-check lint cargo-check test", justfile)
        self.assertIn("test: test-non-pg", justfile)

        main_job = job_block(MAIN_WORKFLOW.read_text(encoding="utf-8"), "full_non_pg")
        self.assertIn("- name: just check\n        run: just check", main_job)

        nightly = NIGHTLY_WORKFLOW.read_text(encoding="utf-8")
        for job_name in ("full_macos", "full_windows"):
            with self.subTest(job=job_name):
                job = job_block(nightly, job_name)
                self.assertIn("- name: cargo test (non-PG)", job)
                self.assertIn(
                    "cargo test --all-targets -- --skip _pg_ --skip postgres_", job
                )

    def test_ci_script_checks_runs_this_contract(self) -> None:
        script = (REPO_ROOT / "scripts/ci-script-checks.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn(
            '"$PYTHON" -m unittest tests.test_fast_check_ci_wiring', script
        )


if __name__ == "__main__":
    unittest.main()
