"""Static contracts for the PR fast-compile and retained test lanes."""

from __future__ import annotations

import re
import subprocess
import tempfile
import unittest
from pathlib import Path

import yaml


REPO_ROOT = Path(__file__).resolve().parents[1]
PR_WORKFLOW = REPO_ROOT / ".github/workflows/ci-pr.yml"
MAIN_WORKFLOW = REPO_ROOT / ".github/workflows/ci-main.yml"
NIGHTLY_WORKFLOW = REPO_ROOT / ".github/workflows/ci-nightly.yml"
MACOS_TRUSTED_WORKFLOW = REPO_ROOT / ".github/workflows/ci-macos-trusted.yml"
BUSY_RETRY_4888_TEST_COMMAND = (
    "env -u AGENTDESK_ROOT_DIR cargo test --lib _4888 -- --test-threads=1"
)

# This manifest is intentionally exact: changing the retained test recipe must also
# update this test deliberately. The duplication is a drift-prevention gate, not an
# attempt to derive the expected coverage from the justfile under test.
EXPECTED_TEST_NON_PG_COMMANDS = (
    "cargo test --lib engine::ops::kv_ops::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib server::routes::docs::inventory::endpoints::part_0 -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::task_completion_v1::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib source_registry -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib task_notification -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib delivery_lease_key -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::discord::session_relay_sink::delivery_orchestration_tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::discord::e2e_control::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib server::routes::e2e_control::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib formatting -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib delivery_record -- --skip _pg --skip pg_ --skip postgres",
    (
        "cargo test --lib services::discord::recovery_known_ids::recovery_known_message_ids_tests"
        " -- --skip _pg --skip pg_ --skip postgres"
    ),
    (
        "cargo test --lib services::discord::tmux::placeholder_suppression::evidence::tests"
        " -- --skip _pg --skip pg_ --skip postgres"
    ),
    "env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::tmux::watcher_lifecycle::tests::tests::turn_starts_reuse_healthy_runtime_path_incumbent_after_handoff -- --exact",
    "cargo test --lib server::claude_oauth_usage_tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib tui_task_card::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib server::routes::message_outbox::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::dispatches::outbox_claiming::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::dispatches::discord_delivery::guard::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib discord_thread_create -- --test-threads=1",
    "cargo test --lib reaction_control::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib intake_queue_transaction::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib pending_reaction_failure_adapter_tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib intake_dispatch_invariant_queued_entrypoints_promote_markers -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::discord::router::intake_dispatch::tests::telemetry_only_unopted -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib attachment -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib mailbox_reaction_tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::discord::zombie_foreground_release::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib queue_marker::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::discord::placeholder_controller::queued_card_gate::tests"
    " -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib queue_status_presentation::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib status_panel_singleton_store -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib busy_followup_retry_store -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::claude_tui::input::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::tmux_common::sentinel_tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::discord::turn_bridge::followup_requeue::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::discord::turn_bridge::terminal_outcome_delivery::busy_followup_retry::tests -- --skip _pg --skip pg_ --skip postgres",
    "env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::inflight::destructive_commit::tests -- --test-threads=1",
    "env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::inflight::save_store::bridge_entry_guard_tests -- --test-threads=1",
    "env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::inflight::save_store::identity_gate::bridge_entry::tests -- --test-threads=1",
    "env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::inflight::save_store::identity_gate::claude_e_stamp::tests -- --test-threads=1",
    "env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::turn_bridge::bridge_entry_persist::tests -- --test-threads=1",
    "env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::turn_bridge::current_message_anchor::tests -- --test-threads=1",
    "env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::turn_bridge::guards::tests -- --test-threads=1",
    "env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::turn_bridge::stream_loop::tool_arms::authority_tests -- --test-threads=1",
    "cargo test --lib services::discord::gateway::tests -- --skip _pg --skip pg_ --skip postgres",
    "env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::gateway::outbound_messages::classified_edit_tests -- --skip _pg --skip pg_ --skip postgres --test-threads=1",
    "env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::router::intake_dispatch::queued::tests -- --skip _pg --skip pg_ --skip postgres --test-threads=1",
    "env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::router::message_handler::intake_turn::placeholder_handoff::tests -- --skip _pg --skip pg_ --skip postgres --test-threads=1",
    "env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::turn_finalizer::completion_admission::tests -- --skip _pg --skip pg_ --skip postgres --test-threads=1",
    "env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::turn_finalizer::completion_admission_actor::tests -- --skip _pg --skip pg_ --skip postgres --test-threads=1",
    "env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::turn_finalizer::cleanup::tests::late_already_finalized_cleanup_releases_mailbox_and_rearms_once_4906 -- --exact --test-threads=1",
    "env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::turn_finalizer::cleanup::tests::mailbox_release_backstop_coalesces_duplicate_arms_and_eventually_fires_4906 -- --exact --test-threads=1",
    "env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::tmux::tmux_watcher::placeholder_reclaim::redrive_reclaim_e2e_tests::live_tmux_redrive_reclaim_cycle_terminates_4299 -- --exact --test-threads=1",
    "env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::tmux::tmux_watcher::terminal_relay_plan::soft_terminal_direct_send_authority_tests -- --skip _pg --skip pg_ --skip postgres --test-threads=1",
    "env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::recovery_engine::runtime::reregister_ledger_reseed_tests -- --test-threads=1",
    "env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::placeholder_sweeper::abandon_guard::tests -- --test-threads=1",
    "env -u AGENTDESK_ROOT_DIR cargo test --lib placeholder_live_events -- --skip _pg --skip pg_ --skip postgres",
    "env -u AGENTDESK_ROOT_DIR cargo test --lib single_message_panel::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::discord::outbound::serenity_reference::tests::lifecycle_notice_nonce_is_stable_and_semantic_event_scoped -- --exact",
    "cargo test --lib services::discord::outbound::delivery::tests::v3_referenced_send_preserves_reference_and_dedupes -- --exact",
    "cargo test --lib canonical_identity::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib session_canonical_identity::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::observability::metrics::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::observability::turn_lifecycle::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::observability::recovery_audit::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib cli::args::tests::legacy_queue_help_directs_users_to_query_without_changing_compatibility_contract",
    "cargo test --all-targets transition -- --skip _pg --skip pg_ --skip postgres --test-threads=1",
    "cargo test --all-targets auto_queue -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --all-targets cancel -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --all-targets review_decision -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --all-targets stall_recovery -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --all-targets routines -- --skip _pg --skip pg_ --skip postgres",
    "python3 scripts/ci-timeout.py 900 env -u AGENTDESK_ROOT_DIR cargo test --lib health -- --skip _pg --skip pg_ --skip postgres",
    "env -u AGENTDESK_ROOT_DIR cargo test --lib relay_recovery -- --skip _pg --skip pg_ --skip postgres",
    "env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::tui_prompt_relay::local_model_queue_wake_e2e -- --skip _pg --skip pg_ --skip postgres --test-threads=1",
    "cargo test --lib services::discord::model_catalog -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::discord::commands::model_ui::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::discord::runtime_bootstrap::shutdown::lifecycle_tests -- --skip _pg --skip pg_ --skip postgres",
    # #5188: Claude session-rotation (`/clear`) delivery-propagation contracts.
    "cargo test --lib services::discord::tui_prompt_relay::session_rotation_settle::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::discord::tui_prompt_relay::injected_prompt_policy::session_resetting_lifecycle_tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::tui_prompt_dedupe::session_rotation::tests -- --skip _pg --skip pg_ --skip postgres",
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


def workflow_paths(root: Path = REPO_ROOT) -> tuple[Path, ...]:
    workflows = root / ".github/workflows"
    return tuple(
        sorted((*workflows.glob("*.yml"), *workflows.glob("*.yaml")))
    )


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
        command = (
            "env -u AGENTDESK_ROOT_DIR cargo test --lib "
            'services::session_forwarding -- "${NON_PG_SKIP_ARGS[@]}"'
        )
        self.assertEqual(test_job.count("- name: Trusted session forwarding tests"), 1)
        self.assertIn("source scripts/ci/non-pg-test-filter.sh", test_job)
        self.assertEqual(test_job.count(command), 1)
        self.assertNotIn(command, job_block(workflow, "scripts"))

    def test_telemetry_only_intake_regressions_run_in_required_lane(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        changes = job_block(workflow, "changes")
        test_job = job_block(workflow, "test_fast")
        mirror = job_block(workflow, "fast_targeted_tests_required_context")
        command = (
            "env -u AGENTDESK_ROOT_DIR cargo test --lib "
            "services::discord::router::intake_dispatch::tests::telemetry_only_unopted "
            '-- "${NON_PG_SKIP_ARGS[@]}"'
        )

        self.assertEqual(test_job.count("- name: Telemetry-only intake authority regressions"), 1)
        self.assertEqual(test_job.count(command), 1)
        self.assertIn("- 'src/services/discord/router/intake_dispatch.rs'", changes)
        self.assertIn("- 'src/services/discord/router/intake_dispatch/**'", changes)
        self.assertIn("- test_fast", mirror)
        self.assertIn("FILTER_NAME: pg_db", mirror)
        self.assertIn("UPSTREAM_JOB_NAME: test_fast", mirror)

    def test_terminal_delivery_evidence_regressions_flow_through_registered_required_context(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        changes_job = job_block(workflow, "changes")
        test_job = job_block(workflow, "test_fast")
        mirror_job = job_block(workflow, "fast_targeted_tests_required_context")
        registered_required_contexts = {
            "Lint",
            "Script checks",
            "Fast check (ubuntu-latest)",
            "High-risk recovery",
            "Dashboard (Node 22)",
            "Fast targeted tests (ubuntu-latest)",
        }

        self.assertNotIn("terminal_delivery_evidence_tests:", workflow)
        self.assertNotIn("terminal_delivery_evidence_required_context:", workflow)
        for path in (
            "src/services/discord/inflight.rs",
            "src/services/discord/inflight/**",
            "src/services/discord/tmux_watcher.rs",
            "src/services/discord/tmux_watcher/**",
            "src/services/discord/turn_bridge/terminal_outcome_delivery.rs",
            "src/services/discord/turn_bridge/terminal_outcome_delivery/**",
        ):
            self.assertIn(f"- '{path}'", changes_job)
        for command in (
            "cargo test --lib inflight::terminal_delivery_evidence_loss::tests",
            "cargo test --lib services::discord::turn_bridge::terminal_outcome_delivery::delivery_epilogue_tests",
            "cargo test --lib watcher_terminal_commit_identity_mismatch_skips_without_clobbering_newer_row",
            "cargo test --lib identity_guarded_save_rejects_stale_write_against_newer_turn",
        ):
            self.assertIn(command, test_job)
        self.assertIn("name: Fast targeted tests (ubuntu-latest)", mirror_job)
        self.assertIn("Fast targeted tests (ubuntu-latest)", registered_required_contexts)
        self.assertIn("- test_fast", mirror_job)
        self.assertIn("FILTER_NAME: pg_db", mirror_job)
        self.assertIn("FILTER_OUTPUT: ${{ needs.changes.outputs.pg_db }}", mirror_job)
        self.assertIn("UPSTREAM_JOB_NAME: test_fast", mirror_job)
        self.assertIn("UPSTREAM_RESULT: ${{ needs.test_fast.result }}", mirror_job)

    def test_footer_marker_regressions_run_in_required_test_fast_lane(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        test_job = job_block(workflow, "test_fast")
        self.assertEqual(test_job.count("- name: Footer-only marker regressions"), 1)
        for command in (
            'cargo test --lib task_notification -- "${NON_PG_SKIP_ARGS[@]}"',
            'cargo test --lib services::discord::tmux::tmux_watcher::discrete_trigger_marker::tests -- "${NON_PG_SKIP_ARGS[@]}"',
        ):
            self.assertEqual(test_job.count(command), 1)

        changes = job_block(workflow, "changes")
        for path in (
            # Glob, not a file list: a per-file enumeration silently excludes
            # modules added later (see the matching comment in ci-pr.yml).
            "src/services/discord/task_notification_delivery/**",
            "src/services/discord/tmux.rs",
            "src/services/discord/tmux_watcher/discrete_trigger_marker.rs",
            "src/services/discord/tui_prompt_relay/task_notification_prompt.rs",
        ):
            self.assertIn(f"- '{path}'", changes)

    def test_pr_cross_os_lane_is_compile_only(self) -> None:
        job = job_block(PR_WORKFLOW.read_text(encoding="utf-8"), "check_fast_cross_os")

        self.assertIn("name: Fast check + non-PG tests (${{ matrix.os }})", job)
        self.assertIn("os: [windows-latest]", job)
        self.assertIn("- name: cargo check", job)
        self.assertNotRegex(job, r"(?m)^\s*cargo test\b")
        self.assertNotIn("- name: cargo test", job)
        self.assertNotIn("Discord thread-create cross-process lock", job)

    def test_macos_pr_lane_runs_single_message_panel_tests(self) -> None:
        workflow = MACOS_TRUSTED_WORKFLOW.read_text(encoding="utf-8")
        command = (
            "env -u AGENTDESK_ROOT_DIR cargo test --lib "
            "single_message_panel::tests -- --skip _pg --skip pg_ --skip postgres"
        )
        self.assertEqual(workflow.count(command), 2)

    def test_macos_pr_lane_runs_placeholder_live_events_tests(self) -> None:
        workflow = MACOS_TRUSTED_WORKFLOW.read_text(encoding="utf-8")
        command = (
            "env -u AGENTDESK_ROOT_DIR cargo test --lib "
            "placeholder_live_events -- --skip _pg --skip pg_ --skip postgres"
        )
        self.assertEqual(workflow.count(command), 2)

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
                self.assertIn("source scripts/ci/non-pg-test-filter.sh", job)
                self.assertIn(
                    'cargo test --all-targets -- "${NON_PG_SKIP_ARGS[@]}"', job
                )
                self.assertIn("run_non_pg_filter_false_positives", job)
        self.assertIn(
            "cargo test --lib discord_thread_create -- --test-threads=1",
            job_block(nightly, "full_windows"),
        )
        postgres = job_block(nightly, "postgres_full")
        self.assertIn("source scripts/ci/non-pg-test-filter.sh", postgres)
        self.assertIn(
            'cargo test --all-targets -- "${PG_INCLUDE_ARGS[@]}" '
            "--nocapture --test-threads=1",
            postgres,
        )

    def test_relay_authority_contract_job_uses_pinned_recipe(self) -> None:
        job = job_block(
            PR_WORKFLOW.read_text(encoding="utf-8"), "relay-authority-contract"
        )
        self.assertRegex(
            job,
            r"(?m)^  relay-authority-contract:\n"
            r"    name: relay-authority-contract\n"
            r"    runs-on: ubuntu-latest\n"
            r"    timeout-minutes: 30\n"
            r"    env:\n"
            r'      CARGO_PROFILE_DEV_DEBUG: "0"\n'
            r'      CARGO_PROFILE_TEST_DEBUG: "0"\n'
            r"    steps:\n"
            r"      - uses: actions/checkout@v4\n\n"
            r"      - name: Install Rust toolchain\n"
            r"        uses: dtolnay/rust-toolchain@master\n"
            r"        with:\n"
            r'          toolchain: "1\.94\.1"$',
        )
        self.assertRegex(
            job,
            r"(?m)^      - name: Run named relay-authority contract targets\n"
            r"        env:\n"
            r"          BASH_ENV: /dev/null\n"
            r'          CARGO_PROFILE_DEV_DEBUG: "0"\n'
            r'          CARGO_PROFILE_TEST_DEBUG: "0"\n'
            r"        shell: bash\n"
            r"        timeout-minutes: 30\n"
            r"        run: \|\n"
            r"          env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::session_relay_sink -- --test-threads=1\n"
            r"          env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::relay_recovery::tests -- --test-threads=1\n"
            r"          env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::tui_prompt_relay::local_model_queue_wake_e2e -- --test-threads=1$",
        )
        self.assertRegex(
            job,
            r"(?m)^      - name: Require relay-authority mutations to be killed\n"
            r"        env:\n"
            r"          BASH_ENV: /dev/null\n"
            r'          CARGO_PROFILE_DEV_DEBUG: "0"\n'
            r'          CARGO_PROFILE_TEST_DEBUG: "0"\n'
            r"        shell: bash\n"
            r"        timeout-minutes: 30\n"
            r"        run: bash scripts/run_relay_authority_mutations\.sh$",
        )

    def test_script_checks_aggregate_is_exactly_one_step(self) -> None:
        hardening = (
            REPO_ROOT / "scripts/check-ci-runner-hardening.sh"
        ).read_text(encoding="utf-8")
        self.assertIn(
            "unless script_check_steps.length == 1",
            hardening,
        )
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        scripts_job = job_block(workflow, "scripts")
        aggregate = (
            "      - name: Run script checks\n"
            "        run: ./scripts/ci-script-checks.sh\n"
            "        env:\n"
            "          # Required Script checks only run on protected PR candidates. Their\n"
            "          # synthetic merge first parent is the exact immutable base snapshot.\n"
            "          TEST_LANE_BASELINE_REF: HEAD^1\n"
        )
        self.assertIn(aggregate, scripts_job)
        mutated_job = scripts_job.replace(aggregate, aggregate + aggregate, 1)
        mutated = workflow.replace(scripts_job, mutated_job, 1)
        result = self.run_hardening_fixture(mutated)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            'must retain exactly one "Run script checks" step', result.stderr
        )

    def test_script_checks_aggregate_must_not_define_if(self) -> None:
        hardening = (
            REPO_ROOT / "scripts/check-ci-runner-hardening.sh"
        ).read_text(encoding="utf-8")
        self.assertIn(
            'if script_check_step.key?("if")',
            hardening,
        )
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        mutated = workflow.replace(
            "      - name: Run script checks\n",
            "      - name: Run script checks\n        if: ${{ false }}\n",
            1,
        )
        result = self.run_hardening_fixture(mutated)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            '"Run script checks" step must not define if', result.stderr
        )

    def test_script_checks_aggregate_must_run_exact_command(self) -> None:
        hardening = (
            REPO_ROOT / "scripts/check-ci-runner-hardening.sh"
        ).read_text(encoding="utf-8")
        self.assertIn(
            'unless script_check_commands == ["./scripts/ci-script-checks.sh"]',
            hardening,
        )
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        mutated = workflow.replace(
            "        run: ./scripts/ci-script-checks.sh\n",
            "        run: ./scripts/ci-script-checks.sh --changed\n",
            1,
        )
        result = self.run_hardening_fixture(mutated)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "must run exactly ./scripts/ci-script-checks.sh", result.stderr
        )

    def test_required_pr_steps_cannot_be_silently_disabled(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        jobs = yaml.safe_load(workflow)["jobs"]
        protected_steps = {
            "scripts": (
                ("Run script checks", "must not continue on error"),
            ),
            "relay-authority-contract": (
                (
                    "Verify named relay-authority targets and selection floors",
                    "must retain exact continue-on-error policy",
                ),
                (
                    "Run named relay-authority contract targets",
                    "must retain exact continue-on-error policy",
                ),
                (
                    "Require relay-authority mutations to be killed",
                    "must retain exact continue-on-error policy",
                ),
            ),
        }

        for job_name, step_specs in protected_steps.items():
            for step_name, expected_error in step_specs:
                with self.subTest(job=job_name, step=step_name):
                    step = next(
                        candidate
                        for candidate in jobs[job_name]["steps"]
                        if candidate.get("name") == step_name
                    )
                    self.assertFalse(
                        step.get("continue-on-error", False),
                        f"required PR job {job_name!r} step {step_name!r} defines "
                        "truthy key 'continue-on-error'",
                    )

                    mutated = workflow.replace(
                        f"      - name: {step_name}\n",
                        f"      - name: {step_name}\n        continue-on-error: true\n",
                        1,
                    )
                    self.assertNotEqual(mutated, workflow)
                    result = self.run_hardening_fixture(mutated)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn(expected_error, result.stderr)

    def test_registered_step_continue_policy_is_typed_and_exact(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        step_name = "Run named relay-authority contract targets"
        for label, yaml_value in (
            ("string-false", '"false"'),
            ("boolean-true", "true"),
            ("string-true", '"true"'),
        ):
            with self.subTest(value=label):
                mutated = workflow.replace(
                    f"      - name: {step_name}\n",
                    f"      - name: {step_name}\n"
                    f"        continue-on-error: {yaml_value}\n",
                    1,
                )
                self.assertNotEqual(mutated, workflow)
                result = self.run_hardening_fixture(mutated)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(
                    "must retain exact continue-on-error policy", result.stderr
                )

    def test_registered_step_continue_policy_accepts_absent_and_boolean_false(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        step_name = "Run named relay-authority contract targets"
        for label, insertion in (
            ("absent", ""),
            ("boolean-false", "        continue-on-error: false\n"),
        ):
            with self.subTest(value=label):
                mutated = workflow.replace(
                    f"      - name: {step_name}\n",
                    f"      - name: {step_name}\n{insertion}",
                    1,
                )
                result = self.run_hardening_fixture(mutated)
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_script_checks_run_accepts_equivalent_scalar_and_block_forms(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        scripts_job = job_block(workflow, "scripts")
        cases = {
            "scalar": "        run: ./scripts/ci-script-checks.sh\n",
            "block": "        run: |\n          ./scripts/ci-script-checks.sh\n",
        }
        for form, replacement in cases.items():
            with self.subTest(form=form):
                mutated_job = scripts_job.replace(
                    "        run: ./scripts/ci-script-checks.sh\n", replacement, 1
                )
                mutated = workflow.replace(scripts_job, mutated_job, 1)
                result = self.run_hardening_fixture(mutated)
                self.assertEqual(result.returncode, 0, result.stderr)

        mutated_job = scripts_job.replace(
            "        run: ./scripts/ci-script-checks.sh\n",
            "        run: |\n          ./scripts/ci-script-checks.sh --changed\n",
            1,
        )
        mutated = workflow.replace(scripts_job, mutated_job, 1)
        result = self.run_hardening_fixture(mutated)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "must run exactly ./scripts/ci-script-checks.sh", result.stderr
        )

    def test_script_checks_needs_accepts_equivalent_scalar_and_list_forms(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        scripts_job = job_block(workflow, "scripts")
        cases = {
            "scalar": "    needs: changes\n",
            "single-element-list": "    needs: [changes]\n",
        }
        for form, replacement in cases.items():
            with self.subTest(form=form):
                mutated_job = scripts_job.replace(
                    "    needs: changes\n", replacement, 1
                )
                if form != "scalar":
                    self.assertNotEqual(mutated_job, scripts_job)
                mutated = workflow.replace(scripts_job, mutated_job, 1)
                result = self.run_hardening_fixture(mutated)
                self.assertEqual(result.returncode, 0, result.stderr)

        mutated_job = scripts_job.replace(
            "    needs: changes\n", "    needs: [changes, other]\n", 1
        )
        self.assertNotEqual(mutated_job, scripts_job)
        mutated = workflow.replace(scripts_job, mutated_job, 1)
        result = self.run_hardening_fixture(mutated)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must retain exact needs: changes", result.stderr)

    def test_trusted_macos_runs_busy_retry_regressions_on_both_runner_paths(self) -> None:
        workflow = MACOS_TRUSTED_WORKFLOW.read_text(encoding="utf-8")
        hosted = job_block(workflow, "macos_hosted")
        self_hosted = job_block(workflow, "macos_self_hosted")

        self.assertEqual(hosted.count(BUSY_RETRY_4888_TEST_COMMAND), 1)
        self.assertEqual(
            self_hosted.count(f"nice -n 10 {BUSY_RETRY_4888_TEST_COMMAND}"), 1
        )

    def test_test_lane_baseline_uses_candidate_snapshot_refs(self) -> None:
        pr_workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        main_workflow = MAIN_WORKFLOW.read_text(encoding="utf-8")
        pr_job = job_block(pr_workflow, "scripts")
        main_job = job_block(main_workflow, "scripts")

        for job in (pr_job, main_job):
            self.assertIn("fetch-depth: 0", job)
            self.assertNotIn("origin/main", job)
            self.assertNotIn("github.event.pull_request.base.sha", job)
        self.assertNotIn("workflow_dispatch:", pr_workflow)
        self.assertRegex(
            pr_job, r"(?m)^          TEST_LANE_BASELINE_REF: HEAD\^1$"
        )
        self.assertNotIn("github.event_name", pr_job)
        self.assertNotIn("inputs.", pr_job)
        self.assertRegex(
            main_job, r"(?m)^          TEST_LANE_BASELINE_REF: HEAD$"
        )
        self.assertNotRegex(
            main_job, r"(?m)^          TEST_LANE_BASELINE_REF: HEAD\^1$"
        )

    def test_required_script_context_is_pr_only(self) -> None:
        pr_workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        self.assertIn("pull_request:", pr_workflow)
        self.assertNotIn("workflow_dispatch:", pr_workflow)
        self.assertNotRegex(pr_workflow, r"(?m)^  push:")
        self.assertEqual(pr_workflow.count("name: Script checks"), 1)
        self.assertRegex(
            job_block(pr_workflow, "scripts"),
            r"(?m)^          TEST_LANE_BASELINE_REF: HEAD\^1$",
        )
        for workflow_path in workflow_paths():
            workflow = workflow_path.read_text(encoding="utf-8")
            with self.subTest(workflow=workflow_path.name):
                if workflow_path != PR_WORKFLOW:
                    self.assertNotRegex(workflow, r"(?m)^    name: Script checks$")
        main_job = job_block(
            MAIN_WORKFLOW.read_text(encoding="utf-8"), "scripts"
        )
        self.assertIn("name: Main script checks", main_job)
        self.assertNotRegex(main_job, r"(?m)^    name: Script checks$")
        self.assertFalse(
            (REPO_ROOT / ".github/workflows/test-lane-baseline-main.yml").exists()
        )

    def run_hardening_fixture(
        self,
        pr_workflow: str,
        extra_workflows: dict[str, str] | None = None,
        workflow_symlinks: dict[str, str] | None = None,
    ) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            workflows = root / ".github/workflows"
            workflows.mkdir(parents=True)
            (root / "scripts").mkdir()
            (workflows / "ci-pr.yml").write_text(pr_workflow, encoding="utf-8")
            trusted = (REPO_ROOT / ".github/workflows/ci-macos-trusted.yml").read_text(
                encoding="utf-8"
            )
            (workflows / "ci-macos-trusted.yml").write_text(
                trusted, encoding="utf-8"
            )
            for name, content in (extra_workflows or {}).items():
                (workflows / name).write_text(content, encoding="utf-8")
            for name, target in (workflow_symlinks or {}).items():
                (workflows / name).symlink_to(target)
            script = (REPO_ROOT / "scripts/check-ci-runner-hardening.sh").read_text(
                encoding="utf-8"
            )
            (root / "scripts/check-ci-runner-hardening.sh").write_text(
                script, encoding="utf-8"
            )
            return subprocess.run(
                ["bash", "scripts/check-ci-runner-hardening.sh"],
                cwd=root,
                text=True,
                capture_output=True,
                check=False,
            )

    def test_hardening_rejects_flow_sequence_manual_trigger(self) -> None:
        source = PR_WORKFLOW.read_text(encoding="utf-8")
        mutated = re.sub(
            r"(?ms)^on:\n.*?^concurrency:\n",
            "on: [pull_request, workflow_dispatch]\n\nconcurrency:\n",
            source,
            count=1,
        )
        self.assertNotEqual(mutated, source)
        result = self.run_hardening_fixture(mutated)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("triggered only by pull_request", result.stderr)

    def test_hardening_rejects_yaml_manual_duplicate_script_context(self) -> None:
        duplicate = """\
name: Duplicate required context
on: [push, workflow_dispatch]
jobs:
  bypass:
    name: "Script checks "
    runs-on: ubuntu-latest
    steps:
      - run: true
"""
        result = self.run_hardening_fixture(
            PR_WORKFLOW.read_text(encoding="utf-8"),
            {"manual-bypass.yaml": duplicate},
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must not publish required Script checks context", result.stderr)
        self.assertIn("manual-bypass.yaml", result.stderr)

    def test_hardening_accepts_clean_yaml_workflow(self) -> None:
        workflow = """\
name: Clean workflow
on: push
jobs:
  clean:
    name: Documentation check
    runs-on: ubuntu-latest
    steps:
      - run: true
"""
        result = self.run_hardening_fixture(
            PR_WORKFLOW.read_text(encoding="utf-8"),
            {"clean.yaml": workflow},
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_hardening_accepts_unrelated_matrix_job_name(self) -> None:
        workflow = """\
name: Matrix workflow
on: push
jobs:
  matrix:
    name: Build (${{ matrix.os }})
    runs-on: ${{ matrix.os }}
    strategy:
      matrix:
        os: [ubuntu-latest]
    steps:
      - run: true
"""
        result = self.run_hardening_fixture(
            PR_WORKFLOW.read_text(encoding="utf-8"),
            {"matrix.yaml": workflow},
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_hardening_rejects_full_expression_script_context(self) -> None:
        workflow = """\
name: Dynamic bypass
on: workflow_dispatch
jobs:
  bypass:
    name: ${{ 'Script checks' }}
    runs-on: ubuntu-latest
    steps:
      - run: true
"""
        result = self.run_hardening_fixture(
            PR_WORKFLOW.read_text(encoding="utf-8"),
            {"dynamic-bypass.yaml": workflow},
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("dynamic job names must not be able to publish", result.stderr)
        self.assertIn("dynamic-bypass.yaml", result.stderr)

    def test_hardening_rejects_split_expression_script_context(self) -> None:
        workflow = """\
name: Split dynamic bypass
on: workflow_dispatch
jobs:
  bypass:
    name: Script check${{ 's' }}
    runs-on: ubuntu-latest
    steps:
      - run: true
"""
        result = self.run_hardening_fixture(
            PR_WORKFLOW.read_text(encoding="utf-8"),
            {"split-bypass.yml": workflow},
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("dynamic job names must not be able to publish", result.stderr)
        self.assertIn("split-bypass.yml", result.stderr)

    def test_hardening_rejects_matrix_name_with_required_static_context(self) -> None:
        workflow = """\
name: Matrix suffix bypass
on: workflow_dispatch
jobs:
  bypass:
    name: Script checks (${{ matrix.os }})
    runs-on: ${{ matrix.os }}
    strategy:
      matrix:
        os: [ubuntu-latest]
    steps:
      - run: true
"""
        result = self.run_hardening_fixture(
            PR_WORKFLOW.read_text(encoding="utf-8"),
            {"matrix-bypass.yml": workflow},
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("dynamic job names must not be able to publish", result.stderr)
        self.assertIn("matrix-bypass.yml", result.stderr)

    def test_hardening_rejects_multiple_job_name_expressions(self) -> None:
        names = (
            "${{ matrix.a }}${{ matrix.b }}",
            "${{ matrix.a }} ${{ matrix.b }}",
            "${{ 'Script' }} ${{ matrix.b }}",
            "${{ matrix.a }} ${{ github.event_name }}",
        )
        for index, name in enumerate(names):
            with self.subTest(name=name):
                workflow = f"""\
name: Multiple expression bypass
on: workflow_dispatch
jobs:
  bypass:
    name: {name}
    runs-on: ubuntu-latest
    steps:
      - run: true
"""
                filename = f"multiple-expression-{index}.yaml"
                result = self.run_hardening_fixture(
                    PR_WORKFLOW.read_text(encoding="utf-8"),
                    {filename: workflow},
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(
                    "dynamic job names must not be able to publish", result.stderr
                )
                self.assertIn(filename, result.stderr)

    def test_hardening_rejects_yaml_aliases_by_policy(self) -> None:
        workflow = """\
name: Aliased workflow
on: push
jobs:
  first: &shared_job
    name: Documentation check
    runs-on: ubuntu-latest
    steps:
      - run: true
  second: *shared_job
"""
        result = self.run_hardening_fixture(
            PR_WORKFLOW.read_text(encoding="utf-8"),
            {"aliased.yaml": workflow},
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cannot parse YAML", result.stderr)
        self.assertIn("aliased.yaml", result.stderr)

    def test_hardening_rejects_workflow_symlink(self) -> None:
        result = self.run_hardening_fixture(
            PR_WORKFLOW.read_text(encoding="utf-8"),
            workflow_symlinks={"linked.yaml": "ci-pr.yml"},
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("linked.yaml must not be a symlink", result.stderr)

    def test_ci_script_checks_runs_this_contract(self) -> None:
        script = (REPO_ROOT / "scripts/ci-script-checks.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn(
            '"$PYTHON" -m unittest tests.test_fast_check_ci_wiring', script
        )
        self.assertIn(
            'scripts/check_test_lane_coverage.py --baseline-ref "$TEST_LANE_BASELINE_REF"',
            script,
        )
        self.assertNotIn("TEST_LANE_BASELINE_REF:-HEAD", script)
        self.assertIn(
            '"$PYTHON" -m unittest tests.test_test_lane_coverage', script
        )


if __name__ == "__main__":
    unittest.main()
