"""Static contracts for the PR fast-compile and retained test lanes."""

from __future__ import annotations

import re
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
PR_WORKFLOW = REPO_ROOT / ".github/workflows/ci-pr.yml"
MAIN_WORKFLOW = REPO_ROOT / ".github/workflows/ci-main.yml"
NIGHTLY_WORKFLOW = REPO_ROOT / ".github/workflows/ci-nightly.yml"

# This manifest is intentionally exact: changing the retained test recipe must also
# update this test deliberately. The duplication is a drift-prevention gate, not an
# attempt to derive the expected coverage from the justfile under test.
EXPECTED_TEST_NON_PG_COMMANDS = (
    "cargo test --lib source_registry -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib task_notification -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib tui_task_card::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib server::routes::message_outbox::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib discord_thread_create -- --test-threads=1",
    "cargo test --lib reaction_control::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib intake_queue_transaction::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib pending_reaction_failure_adapter_tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib intake_dispatch_invariant_queued_entrypoints_promote_markers -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib mailbox_reaction_tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib queue_marker::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib queue_status_presentation::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::discord::outbound::serenity_reference::tests::lifecycle_notice_nonce_is_stable_and_semantic_event_scoped -- --exact",
    "cargo test --lib services::discord::outbound::delivery::tests::v3_referenced_send_preserves_reference_and_dedupes -- --exact",
    "cargo test --lib cli::args::tests::legacy_queue_help_directs_users_to_query_without_changing_compatibility_contract",
    "cargo test --all-targets transition -- --skip _pg --skip pg_ --skip postgres --test-threads=1",
    "cargo test --all-targets auto_queue -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --all-targets cancel -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --all-targets review_decision -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --all-targets stall_recovery -- --skip _pg --skip pg_ --skip postgres",
    "python3 scripts/ci-timeout.py 900 env -u AGENTDESK_ROOT_DIR cargo test --lib health -- --skip _pg --skip pg_ --skip postgres",
    "env -u AGENTDESK_ROOT_DIR cargo test --lib relay_recovery -- --skip _pg --skip pg_ --skip postgres",
    "cargo test invariant --all-targets -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --doc ClaudeBinary",
)


def job_block(workflow: str, job_name: str) -> str:
    marker = re.compile(rf"^  {re.escape(job_name)}:\n", re.MULTILINE)
    match = marker.search(workflow)
    if match is None:
        raise AssertionError(f"missing workflow job: {job_name}")
    next_job = re.compile(r"^  [A-Za-z0-9_-]+:\n", re.MULTILINE).search(
        workflow, match.end()
    )
    return workflow[match.start() : next_job.start() if next_job else len(workflow)]


def just_recipe_commands(justfile: str, recipe_name: str) -> tuple[str, ...]:
    marker = re.compile(rf"^{re.escape(recipe_name)}:[ \t]*.*$", re.MULTILINE)
    match = marker.search(justfile)
    if match is None:
        raise AssertionError(f"missing just recipe: {recipe_name}")

    commands: list[str] = []
    for line in justfile[match.end() :].splitlines():
        if line and not line[0].isspace():
            break
        stripped = line.strip()
        if stripped and not stripped.startswith("#"):
            commands.append(" ".join(stripped.split()))
    return tuple(commands)


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

    def test_required_targeted_context_mirrors_test_fast_pg_db_gate(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        job = job_block(workflow, "fast_targeted_tests_required_context")

        self.assertIn("name: Fast targeted tests (ubuntu-latest)", job)
        self.assertRegex(
            job,
            r"(?m)^    needs:\n      - changes\n      - test_fast\n    if: always\(\)$",
        )
        self.assertEqual(job.count("FILTER_NAME: pg_db"), 1)
        self.assertEqual(
            job.count("FILTER_OUTPUT: ${{ needs.changes.outputs.pg_db }}"), 1
        )
        self.assertEqual(job.count("UPSTREAM_JOB_NAME: test_fast"), 1)
        self.assertEqual(
            job.count("UPSTREAM_RESULT: ${{ needs.test_fast.result }}"), 1
        )

        test_job = job_block(workflow, "test_fast")
        self.assertRegex(
            test_job,
            r"(?m)^    if: needs\.changes\.outputs\.pg_db == 'true'$",
        )

    def test_main_and_nightly_retain_non_pg_test_coverage(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        self.assertIn("check: fmt-check lint cargo-check test", justfile)
        self.assertIn("test: test-non-pg", justfile)
        self.assertEqual(
            just_recipe_commands(justfile, "test-non-pg"),
            EXPECTED_TEST_NON_PG_COMMANDS,
        )

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
