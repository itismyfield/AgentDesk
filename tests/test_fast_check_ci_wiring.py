"""Static contracts for the PR fast-compile and retained test lanes."""

from __future__ import annotations

import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path, PurePosixPath

import yaml


REPO_ROOT = Path(__file__).resolve().parents[1]
PR_WORKFLOW = REPO_ROOT / ".github/workflows/ci-pr.yml"
# The gate pin as the workflow holds it; fixtures that edit the gate swap this value for the edited hash.
CI_RUNNER_HARDENING_SHA256 = re.search(
    r"pin scripts/check-ci-runner-hardening\.sh ([0-9a-f]{64})", PR_WORKFLOW.read_text(encoding="utf-8")
).group(1)
NESTED_CHECKER_STUB = "import sys\nassert sys.argv[1:] == ['check'], 'nested checker must receive check'\n"

MIRROR_IF = "${{ !cancelled() }}"
MIRROR_IF_LINE = f"    if: {MIRROR_IF}\n"
RUST_REQUIRED_IF = "${{ !cancelled() && needs.changes.outputs.rust_tests_skip != 'true' }}"
_LINT_FILTER = (
    "needs.changes.outputs.rust_or_policy == 'true' || "
    "needs.changes.outputs.relay_contract == 'true'"
)
FILTER_BLOCK_HEADER = re.compile(r"^            \w+:$", re.M)
CROSS_OS_CONSUMER_SCRIPT = REPO_ROOT / "scripts/cross_os_consumer_paths.py"
CFG_SHIM_CONSUMERS = (
    "src/services/discord/turn_bridge/mod.rs",
    "src/services/discord/health/watcher_respawn.rs",
    "src/services/discord/outbound/delivery_record.rs",
    "src/services/discord/recovery_engine/restore_inflight.rs",
    "src/services/discord/recovery_engine/completion_delivery.rs",
    "src/services/discord/recovery_engine/manual_rebind/mod.rs",
    "src/services/discord/recovery_paths/restart.rs",
    "src/services/discord/router/message_handler.rs",
    "src/services/discord/router/message_handler/watchdog.rs",
    "src/services/discord/router/intake_dispatch/tests.rs",
    "src/services/discord/runtime_bootstrap/recovery_flush.rs",
    "src/services/discord/turn_finalizer.rs",
    "src/services/discord/turn_finalizer/delivery_lease.rs",
    "src/services/discord/terminal_ui_obligation.rs",
    "src/services/discord/destructive_cancel_gate.rs",
    "src/services/discord/inflight/save_store/create_monotonic_observer.rs",
    "src/services/discord/placeholder_live_events/tests.rs",
    "src/services/discord/tui_prompt_relay/tests.rs",
    "src/services/discord/tui_prompt_relay/relay_ownership.rs",
    "src/services/discord/tui_prompt_relay/synthetic_start/claim.rs",
    "src/services/discord/voice_barge_in/tests/pcm_harness_tests.rs",
)
UNREACHABLE_RUST_FILES = (
    ("src/services/discord/tmux/monitor_auto_turn_inflight_tests.rs",
     "src/services/discord/tmux/monitor_auto_turn_inflight.rs"),
    ("src/services/discord/tmux/task_notification_kind_restart_roundtrip_tests.rs",
     "src/services/discord/tmux.rs"),
    ("src/services/discord/tmux_output_stream/provider_output_guard_tests.rs",
     "src/services/discord/tmux_output_stream.rs"),
    ("src/services/discord/tmux_watcher/terminal_direct_fallback_tests.rs",
     "src/services/discord/tmux_watcher/terminal_direct_fallback.rs"),
)
MAIN_WORKFLOW = REPO_ROOT / ".github/workflows/ci-main.yml"
NIGHTLY_WORKFLOW = REPO_ROOT / ".github/workflows/ci-nightly.yml"
MACOS_TRUSTED_WORKFLOW = REPO_ROOT / ".github/workflows/ci-macos-trusted.yml"
BUSY_RETRY_4888_TEST_COMMAND = (
    "env -u AGENTDESK_ROOT_DIR cargo test --lib _4888 -- --test-threads=1"
)

EXPECTED_TEST_NON_PG_COMMANDS = (
    "cargo test --lib engine::ops::kv_ops::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib server::routes::docs::inventory::endpoints::part_0 -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::task_completion_v1::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::session_host:: -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib source_registry -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib task_notification -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib delivery_lease_key -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::discord::session_relay_sink::delivery_orchestration_tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::discord::health::reachability::verdict::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::discord::health::reachability::discovery::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::discord::health::reachability::tail::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::discord::health::reachability::ledger::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::discord::health::reachability::observation::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::discord::health::reachability::obligation::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::discord::health::reachability::divergence::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::discord::e2e_control::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib server::routes::e2e_control::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib formatting -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib delivery_record -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::discord::outbound::receipt_index::tests"
    " -- --skip _pg --skip pg_ --skip postgres",
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
    "cargo test --lib services::discord::turn_bridge::headless_delivery",
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
    "env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::placeholder_sweeper::abandon_guard::tests -- --skip _pg --skip pg_ --skip postgres --test-threads=1",
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


def step_block(job: str, step_name: str) -> str:
    marker = re.compile(rf"^      - name: {re.escape(step_name)}\n", re.MULTILINE)
    match = marker.search(job)
    if match is None:
        raise AssertionError(f"missing workflow step: {step_name}")
    next_step = re.compile(r"^      - (?:name:|uses:)", re.MULTILINE).search(
        job, match.end()
    )
    return job[match.start() : next_step.start() if next_step else len(job)]


_STEP_IF_TOKEN = re.compile(
    r"\s*(?:('(?:[^']|'')*')|(&&|\|\||==|!=|!|\(|\))|([A-Za-z_][\w.-]*(?:\(\))?))"
)


def eval_step_if(condition: object, context: dict[str, str]) -> bool:
    """Evaluate a step `if:` built from literals, ==/!=, !/&&/|| and `context` paths.

    Status functions are true because the steps before the gate succeeded.
    """
    if condition is None or isinstance(condition, bool):
        return condition is not False
    expr = str(condition).strip()
    if expr.startswith("${{") and expr.endswith("}}"):
        expr = expr[3:-2].strip()
    python, pos = [], 0
    while pos < len(expr):
        match = _STEP_IF_TOKEN.match(expr, pos)
        if match is None:
            raise AssertionError(f"unsupported step condition: {condition!r}")
        literal, op, name = match.groups()
        if literal is not None:
            python.append(repr(literal[1:-1].replace("''", "'")))
        elif op is not None:
            python.append({"&&": " and ", "||": " or ", "!": " not "}.get(op, op))
        elif name in ("true", "false"):
            python.append(str(name == "true"))
        elif name in ("always()", "success()"):
            python.append("True")
        elif name in context:
            python.append(repr(context[name]))
        else:
            raise AssertionError(f"unknown name {name!r} in step condition: {condition!r}")
        pos = match.end()
    return bool(eval("".join(python), {"__builtins__": {}}))


MACOS_FILTER_SCENARIOS = (
    ("success", "false", False),
    ("success", "true", True),
    ("success", "", True),
    ("failure", "false", True),
    ("failure", "", True),
)


def replace_last(source: str, old: str, new: str) -> str:
    head, separator, tail = source.rpartition(old)
    if not separator:
        raise AssertionError(f"missing text for final replacement: {old!r}")
    return head + new + tail


def comment_out_in_filter(workflow: str, block: str, selector: str) -> str:
    """Restrict edits to the named filter so repeated selectors in other jobs
    cannot make a missing-path regression test pass without changing its input.
    """
    head, separator, rest = workflow.partition(f"            {block}:\n")
    if not separator:
        raise AssertionError(f"missing filter block: {block!r}")
    following = FILTER_BLOCK_HEADER.search(rest)
    cut = following.start() if following else len(rest)
    body, tail = rest[:cut], rest[cut:]
    line = f"              - '{selector}'"
    if line not in body:
        raise AssertionError(f"{block!r} does not list {selector!r}")
    return head + separator + body.replace(line, f"              # - '{selector}'", 1) + tail


def glob_matcher(pattern: str) -> re.Pattern[str]:
    """Reproduce picomatch `{dot: true}` for the shapes dorny/paths-filter@v3 resolves.

    Cross-checked against picomatch 2.3.2 over every tracked file and every
    ci-pr.yml filter pattern: identical selection for all 191 non-negated
    patterns. Negation (`!pat`) is unsupported and asserted absent below; a
    second matching oracle (pathspec/gitwildmatch) is deliberately not used,
    because two oracles for one rule means one of them is always wrong.
    """
    parts, index = [], 0
    while index < len(pattern):
        if pattern[index : index + 3] == "/**":
            parts.append("/.*" if index + 3 == len(pattern) else "(?:/.*)?")
            index += 3
        elif pattern[index : index + 3] == "**/":
            parts.append("(?:.*/)?")
            index += 3
        elif pattern[index : index + 2] == "**":
            parts.append(".*")
            index += 2
        elif pattern[index] == "*":
            parts.append("[^/]*")
            index += 1
        elif pattern[index] == "?":
            parts.append("[^/]")
            index += 1
        else:
            parts.append(re.escape(pattern[index]))
            index += 1
    return re.compile("".join(parts) + r"\Z")


def selects(patterns: list[str], path: str) -> bool:
    return any(glob_matcher(pattern).match(path) for pattern in patterns)


def derived_cross_os_consumers(
    output_format: str = "paths", scope: str = "src/services/discord"
) -> tuple[str, ...]:
    completed = subprocess.run(
        [sys.executable, str(CROSS_OS_CONSUMER_SCRIPT), "--format", output_format, "--scope", scope],
        capture_output=True,
        text=True,
        check=True,
        cwd=REPO_ROOT,
    )
    return tuple(completed.stdout.split())


def unreachable_rust_files() -> tuple[str, ...]:
    completed = subprocess.run(
        [sys.executable, str(CROSS_OS_CONSUMER_SCRIPT), "--format", "unreachable", "--scope", "src"],
        capture_output=True,
        text=True,
        check=True,
        cwd=REPO_ROOT,
    )
    return tuple(completed.stdout.split())


def workflow_paths(root: Path = REPO_ROOT) -> tuple[Path, ...]:
    workflows = root / ".github/workflows"
    return tuple(
        sorted((*workflows.glob("*.yml"), *workflows.glob("*.yaml")))
    )


def paths_filter_definitions(workflow: str) -> dict[str, list[str]]:
    parsed = yaml.safe_load(workflow)
    steps = parsed["jobs"]["changes"]["steps"]
    filter_step = next(
        step for step in steps if step.get("uses") == "dorny/paths-filter@v3"
    )
    return yaml.safe_load(filter_step["with"]["filters"])


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
    def test_large_file_guard_uses_nul_delimited_tracked_paths(self) -> None:
        scripts_job = job_block(PR_WORKFLOW.read_text(encoding="utf-8"), "scripts")
        guard = scripts_job.split("- name: Large file guard", 1)[1].split(
            "- name: Install shellcheck", 1
        )[0]

        self.assertIn("shell: bash", guard)
        self.assertIn("while IFS= read -r -d '' path; do", guard)
        self.assertIn("done < <(git ls-files -z)", guard)
        self.assertNotIn("while IFS= read -r path; do", guard)
        self.assertNotIn("done < <(git ls-files)\n", guard)

    def test_verified_skip_is_shared_by_independent_rust_jobs_and_mirrors(self) -> None:
        jobs = yaml.safe_load(PR_WORKFLOW.read_text(encoding="utf-8"))["jobs"]
        verified = ("steps.comment_only.outcome == 'success' && "
                    "steps.comment_only.outputs.comment_only == 'true' && "
                    "steps.comment_only.outputs.rust_tests_skip == 'true'")
        outputs = jobs["changes"]["outputs"]
        self.assertEqual(outputs["rust_tests_skip"], "${{ " + verified + " && 'true' || 'false' }}")
        lint_verified = verified.replace("outputs.rust_tests_skip", "outputs.rust_lint_skip")
        for name in ("rust_or_policy", "rust_compile"):
            self.assertEqual(outputs[name], "${{ " + lint_verified + " && 'false' || steps.filter.outputs." + name + " }}")
        for name, needs in (("relay_authority_mutations", "changes"),):
            with self.subTest(job=name):
                self.assertEqual(jobs[name]["needs"], needs)
                self.assertEqual(jobs[name]["if"], RUST_REQUIRED_IF)
                for skip in ("true", "false", "", None):
                    self.assertEqual(eval_step_if(jobs[name]["if"], {
                        "cancelled()": False, "needs.changes.outputs.rust_tests_skip": skip,
                    }), skip != "true")

    def test_rust_comment_skip_retains_compile_and_lint_checks(self) -> None:
        outputs = yaml.safe_load(PR_WORKFLOW.read_text(encoding="utf-8"))["jobs"]["changes"]["outputs"]
        cases = (("rust comments", "false", "success", "true", True),
                 ("missing lint proof", None, "success", "true", True),
                 ("empty lint proof", "", "success", "true", True),
                 ("failed gate", "true", "failure", "true", True),
                 ("not comment only", "true", "success", "false", True),
                 ("docs and YAML", "true", "success", "true", False))
        for name in ("rust_or_policy", "rust_compile"):
            expression = "(" + outputs[name][3:-2] + ") == 'true'"
            for case, lint_skip, outcome, comment_only, selected in cases:
                with self.subTest(output=name, case=case):
                    self.assertEqual(eval_step_if(expression, {
                        "steps.comment_only.outcome": outcome,
                        "steps.comment_only.outputs.comment_only": comment_only,
                        "steps.comment_only.outputs.rust_tests_skip": "true",
                        "steps.comment_only.outputs.rust_lint_skip": lint_skip,
                        "steps.filter.outputs." + name: "true",
                    }), selected)

    def test_pr_lint_runner_is_the_ubuntu_compile_and_policy_lane(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        jobs = yaml.safe_load(workflow)["jobs"]
        self.assertNotIn("check_fast", jobs)
        job = jobs["lint"]

        self.assertEqual(job["name"], "Lint runner")
        self.assertEqual(job["runs-on"], "ubuntu-latest")
        self.assertEqual(job["if"], _LINT_FILTER)
        # Clippy over every target and feature is the compile `cargo check` used to repeat.
        self.assertEqual(
            [line.strip() for step in job["steps"] for line in str(step.get("run", "")).splitlines() if line.strip()],
            [
                "just fmt-check",
                "npm run test:policies",
                "set -o pipefail",
                "mkdir -p target/clippy-observation",
                "cargo clippy --workspace --all-targets --all-features --message-format=json -- -W clippy::all | tee target/clippy-observation/diagnostics.jsonl",
                'python3 scripts/check_clippy_warning_count.py --input target/clippy-observation/diagnostics.jsonl --output target/clippy-observation/report.json --baseline scripts/clippy_warning_baseline.json --base-ref "$CLIPPY_BASE_REF"',
                "sccache --show-stats || true",
            ],
        )
        self.assertNotIn("continue-on-error", job_block(workflow, "lint"))

    def test_nextest_jobs_reject_fail_open_and_artifact_mutants_after_repin(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        hardening = (REPO_ROOT / "scripts/check-ci-runner-hardening.sh").read_text()
        baseline = self.run_hardening_fixture(workflow)
        self.assertEqual(baseline.returncode, 0, baseline.stderr)
        mutants = (
            ("rust_build", "if: ${{ !cancelled() && needs.changes.outputs.rust_tests == 'true' }}", "if: ${{ !cancelled() }}", "required needs closure"),
            ("rust_build", "          python3 scripts/ci/nextest_ci.py selector-audit --plan \"$NEXTEST_DIR/plan/plan.json\"\n", "", "exact cargo/test command list"),
            ("rust_build", "python3 scripts/ci/rust_test_sources.py compare --platform linux", "true", "exact cargo/test command list"),
            ("rust_build", "          name: nextest-plan\n", "          name: nextest-plan-copy\n", "artifact uploads"),
            ("rust_tests", "if: ${{ !cancelled() && needs.rust_build.result == 'success' }}", "if: success()", "required needs closure"),
            ("rust_tests", " --profile ci --no-tests fail", " --profile ci", "exact cargo/test command list"),
            ("rust_tests", "fail-fast: false", "fail-fast: true", "matrix"),
            ("rust_tests", "worker: [0, 1, 2, 3, 4, 5]", "worker: [0, 1, 2, 3, 4]", "matrix"),
            ("rust_tests", "-attempt-${{ github.run_attempt }}", "", "artifact uploads"),
            ("test_evidence", "python3 scripts/ci/nextest_ci.py collect-results", "python3 scripts/ci/nextest_ci.py --help", "exact cargo/test command list"),
            ("test_evidence", "    needs: [changes, rust_build, rust_tests]\n", "    needs: [changes, rust_build]\n", "exact needs"),
            ("rust_build", "          python3 scripts/check_relay_authority_contract.py --nextest-list \"$NEXTEST_DIR/universe.json\"\n", "", "exact cargo/test command list"),
            ("rust_build", 'nextest-base-diff.sh "$RUNNER_TEMP/nextest"', 'nextest-base-diff.sh "$RUNNER_TEMP/nextest" --report-only', "exact cargo/test command list"),
            ("rust_build", step_block(job_block(workflow, "rust_build"), "Compare tests with the comparison base"), "", 'exactly one "Compare tests with the comparison base"'),
        )
        for job_id, old, new, reason in mutants:
            with self.subTest(job=job_id, mutation=old):
                original = job_block(workflow, job_id)
                changed = original.replace(old, new, 1)
                self.assertNotEqual(changed, original)
                mutated = workflow.replace(original, changed, 1)
                gate = self._repin_job_hash(hardening, mutated, job_id)
                result = self.run_hardening_fixture(mutated.replace(CI_RUNNER_HARDENING_SHA256, hashlib.sha256(gate.encode()).hexdigest()), hardening_script=gate)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(reason, result.stderr)
        # Main runs the PR's N: any drift from the PR jobs, or a lost or shortened baseline, is red.
        main = MAIN_WORKFLOW.read_text(encoding="utf-8")
        main_mutants = (
            ("rust_tests", "worker: [0, 1, 2, 3, 4, 5]", "worker: [0, 1, 2, 3, 4]", "main rust_tests must equal the PR job"),
            ("test_evidence", "    needs: [rust_build, rust_tests]\n", "    needs: rust_build\n", "main test_evidence must keep needs"),
            ("rust_build", "          retention-days: 14\n", "          retention-days: 3\n", "nextest-baseline for 14 days"),
            ("rust_build", "          name: nextest-baseline\n", "          name: nextest-plan-copy\n", "nextest-baseline for 14 days"),
        )
        for job_id, old, new, reason in main_mutants:
            with self.subTest(main_job=job_id, mutation=old):
                original = job_block(main, job_id)
                changed = original.replace(old, new, 1)
                self.assertNotEqual(changed, original)
                result = self.run_hardening_fixture(workflow, extra_workflows={"ci-main.yml": main.replace(original, changed, 1)})
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(reason, result.stderr)
        # Nightly runs the same N, and no other nightly job may run lib tests outside it.
        nightly = NIGHTLY_WORKFLOW.read_text(encoding="utf-8")
        nightly_mutants = (
            ("rust_tests", "worker: [0, 1, 2, 3, 4, 5]", "worker: [0, 1]", "nightly rust_tests must equal the main job"),
            ("postgres_full", "cargo test --test e2e -- --test-threads=1", "cargo test --lib high_risk_recovery:: -- --test-threads=1",
             "nightly lib tests run only in"),
        )
        for job_id, old, new, reason in nightly_mutants:
            with self.subTest(nightly_job=job_id, mutation=old):
                original = job_block(nightly, job_id)
                changed = original.replace(old, new, 1)
                self.assertNotEqual(changed, original)
                result = self.run_hardening_fixture(workflow, extra_workflows={"ci-nightly.yml": nightly.replace(original, changed, 1)})
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(reason, result.stderr)

    def test_retired_targeted_lane_regressions_stay_in_the_audited_selectors(self) -> None:
        # The old test_fast and high-risk steps ran these by name; the selector audit now fails the run if one stops matching.
        selectors = (REPO_ROOT / "scripts/ci/targeted-test-selectors.txt").read_text(encoding="utf-8").split()
        for selector in (
            "services::session_forwarding",
            "task_notification",
            "services::discord::tmux::tmux_watcher::discrete_trigger_marker::tests",
            "inflight::terminal_delivery_evidence_loss::tests",
            "services::discord::turn_bridge::terminal_outcome_delivery::delivery_epilogue_tests",
            "watcher_terminal_commit_identity_mismatch_skips_without_clobbering_newer_row",
            "identity_guarded_save_rejects_stale_write_against_newer_turn",
        ):
            self.assertIn(selector, selectors)
        jobs = yaml.safe_load(PR_WORKFLOW.read_text(encoding="utf-8"))["jobs"]
        for job_id in ("rust_build", "test_evidence"):
            runs = " ".join(str(step.get("run", "")) for step in jobs[job_id]["steps"])
            self.assertIn("nextest_ci.py selector-audit --plan", runs, job_id)

    def test_pg15_owner_proof_runs_whenever_the_rust_tests_run(self) -> None:
        jobs = yaml.safe_load(PR_WORKFLOW.read_text(encoding="utf-8"))["jobs"]
        self.assertEqual(jobs["pg15_owner_proof"]["needs"], "changes")
        for rust_tests in ("true", "false"):
            for job_id in ("pg15_owner_proof", "rust_build"):
                with self.subTest(job=job_id, rust_tests=rust_tests):
                    condition = jobs[job_id]["if"].replace("!cancelled()", "true")
                    self.assertEqual(eval_step_if(condition, {"needs.changes.outputs.rust_tests": rust_tests}), rust_tests == "true")

    def test_fast_targeted_context_fails_closed_on_each_pg15_outcome_its_gate_forbids(self) -> None:
        sys.path.insert(0, str(REPO_ROOT / "scripts/ci"))
        import nextest_required
        judge = yaml.safe_load(PR_WORKFLOW.read_text(encoding="utf-8"))["jobs"]["required_contexts"]
        self.assertIn("pg15_owner_proof", judge["needs"])
        red = {
            ("true", "success"): False, ("true", "skipped"): True,
            ("true", "failure"): True, ("true", "cancelled"): True,
            ("false", "success"): False, ("false", "skipped"): False,
            ("false", "failure"): True, ("false", "cancelled"): True,
        }
        for (rust_tests, outcome), expected_red in red.items():
            n_result = "success" if rust_tests == "true" else "skipped"
            needs = {job: {"result": n_result} for job in ("rust_build", "rust_tests", "test_evidence")}
            needs.update(changes={"result": "success", "outputs": {"rust_tests": rust_tests}}, pg15_owner_proof={"result": outcome})
            with self.subTest(rust_tests=rust_tests, outcome=outcome):
                ok, lines = nextest_required.context_verdict("Fast targeted tests (ubuntu-latest)", needs)
                self.assertEqual(not ok, expected_red, lines)

    def test_pg15_owner_proof_runs_the_exact_ids_on_a_postgres_15_server(self) -> None:
        job = yaml.safe_load(PR_WORKFLOW.read_text(encoding="utf-8"))["jobs"]["pg15_owner_proof"]
        self.assertEqual(job["env"]["POSTGRES_SERVICE_IMAGE"], "postgres:15")
        names = [step.get("name") for step in job["steps"]]
        order = ["Start PostgreSQL service", "Require PostgreSQL 15 server",
                 "PostgreSQL 15 exact owner proof", "Stop PostgreSQL service"]
        self.assertEqual([name for name in names if name in order], order)

        version_step = job["steps"][names.index("Require PostgreSQL 15 server")]
        with tempfile.TemporaryDirectory() as temp:
            docker = Path(temp) / "docker"
            docker.write_text('#!/bin/sh\nprintf "%s\\n" "$FAKE_SERVER_VERSION"\n', encoding="utf-8")
            docker.chmod(0o755)
            for version, accepted in (("150019", True), ("170004", False), ("160010", False),
                                      ("1500", False), ("", False)):
                with self.subTest(server_version_num=version):
                    result = subprocess.run(
                        ["bash", "-c", version_step["run"]], capture_output=True, check=False,
                        env={**os.environ, "PATH": f"{temp}:{os.environ['PATH']}",
                             "FAKE_SERVER_VERSION": version},
                    )
                    self.assertEqual(result.returncode == 0, accepted)

        proof = job["steps"][names.index("PostgreSQL 15 exact owner proof")]
        self.assertEqual(proof["env"], {
            "ADK_LIB_TEST_ARTIFACT": "${{ runner.temp }}/lib-test-artifact",
            "BASH_ENV": "/dev/null",
        })
        words = proof["run"].split()
        lane, command = words[: words.index("--")], words[words.index("--") + 1 :]
        self.assertEqual(lane[:2], ["python3", "scripts/run_test_lane.py"])
        self.assertEqual(lane[lane.index("--expected-ids") + 1], "scripts/ci/pg15_required_ids.txt")
        self.assertEqual(command[-1], "--test-threads=1")
        exact = command[command.index("--exact") + 1 : -1]
        ids = (REPO_ROOT / "scripts/ci/pg15_required_ids.txt").read_text(encoding="utf-8").split()
        self.assertEqual(exact, ids)

    def test_pg15_required_ids_are_inventoried_and_never_ledgered(self) -> None:
        ids = (REPO_ROOT / "scripts/ci/pg15_required_ids.txt").read_text(encoding="utf-8").split()
        self.assertTrue(ids)
        self.assertEqual(len(ids), len(set(ids)))
        manifest = (REPO_ROOT / "scripts/lib_test_inventory_manifest.txt").read_text(encoding="utf-8")
        inventoried = set(manifest.split("[tests]\n", 1)[1].split("\n[", 1)[0].split())
        self.assertEqual([test_id for test_id in ids if test_id not in inventoried], [])
        ledger = (REPO_ROOT / "scripts/known_test_failures.txt").read_text(encoding="utf-8")
        patterns = [line.split("|")[1].strip() for line in ledger.splitlines()
                    if line.strip() and not line.lstrip().startswith("#") and "|" in line]
        self.assertTrue(patterns)
        self.assertEqual([(test_id, pattern) for test_id in ids for pattern in patterns
                          if test_id.startswith(pattern)], [])

    def test_pr_cross_os_lane_allows_only_targeted_writer_runtime(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        jobs = yaml.safe_load(workflow)["jobs"]
        check, targets = jobs["check_fast_cross_os"], jobs["check_fast_cross_os_targets"]
        python_step_name = (
            "Provision Python 3.11 (tomllib owner for "
            "check_test_target_integrity, a2a-C PR-1)"
        )
        self.assertEqual(check["name"], "Fast check + non-PG tests (${{ matrix.os }})")
        self.assertEqual(targets["name"], "Windows exact targets (${{ matrix.os }})")
        for key in ("needs", "if", "runs-on", "env", "strategy"):
            self.assertEqual(targets[key], check[key], key)
        self.assertEqual(check["strategy"], {"fail-fast": False, "matrix": {"os": ["windows-latest"]}})
        self.assertEqual(
            check["if"],
            "needs.changes.outputs.rust_compile == 'true' && needs.changes.outputs.cross_os_rust == 'true'",
        )
        setup = [step.get("name", step.get("uses")) for step in check["steps"]]
        setup = setup[: setup.index("Cache Cargo dependencies") + 1]
        self.assertEqual(check["steps"][: len(setup)], targets["steps"][: len(setup)])
        self.assertLess(setup.index(python_step_name), setup.index("Install Rust toolchain"))
        python_step = check["steps"][setup.index(python_step_name)]
        self.assertEqual(python_step["uses"], "actions/setup-python@v5")
        self.assertEqual(python_step["with"]["python-version"], "3.11")

        runs = [
            (job_id, step.get("run", step.get("uses")))
            for job_id in ("check_fast_cross_os", "check_fast_cross_os_targets")
            for step in jobs[job_id]["steps"][len(setup) :]
            if step.get("name") != "sccache stats"
        ]
        self.assertEqual(
            runs,
            [
                ("check_fast_cross_os", "cargo check --workspace --all-targets"),
                ("check_fast_cross_os_targets", "./scripts/ci/run-writer-namespace-windows-targets.sh"),
                ("check_fast_cross_os_targets", "taiki-e/install-action@v2"),
                ("check_fast_cross_os_targets",
                 "set -euo pipefail\n"
                 "cargo nextest list --lib --ignore-default-filter --message-format json > target/universe-windows.json\n"
                 "python scripts/ci/rust_test_sources.py compare --platform windows --list target/universe-windows.json --json-out target/compare-windows.json\n"),
            ],
        )
        # The Windows list must come from the Windows runner, with the pinned nextest.
        install, compare = targets["steps"][len(setup) + 1 : len(setup) + 3]
        self.assertEqual(install["with"], {"tool": "cargo-nextest@0.9.149"})
        self.assertEqual((install["if"], compare["if"]), ("runner.os == 'Windows'", "runner.os == 'Windows'"))
        self.assertNotIn("continue-on-error", compare)
        writer = targets["steps"][len(setup)]
        self.assertEqual(writer["name"], "Writer namespace exact Windows targets")
        self.assertEqual(writer["if"], "runner.os == 'Windows'")
        self.assertEqual(writer["timeout-minutes"], 60)
        self.assertEqual(writer["shell"], "bash")
        for job_id in ("check_fast_cross_os", "check_fast_cross_os_targets"):
            self.assertNotIn("continue-on-error", jobs[job_id])
            job = job_block(workflow, job_id)
            self.assertNotRegex(job, r"(?m)^\s*cargo test\b")
            self.assertNotIn("- name: cargo test", job)
        filters = paths_filter_definitions(workflow)
        proof_paths = (
            "scripts/ci/run-writer-namespace-windows-targets.sh",
            "scripts/exact_rust_test_proof.py",
        )
        for selector in ("rust_compile", "cross_os_rust"):
            for proof_path in proof_paths:
                self.assertEqual(filters[selector].count(proof_path), 1)
        self.assertNotIn(proof_paths[1], job_block(workflow, "check_fast_cross_os_targets"))
        for proof_path in proof_paths:
            filter_line = f"              - '{proof_path}'"
            mutations = (
                workflow.replace(filter_line, "", 1),
                replace_last(workflow, filter_line, ""),
                workflow.replace(filter_line, f"{filter_line}\n{filter_line}", 1),
                replace_last(workflow, filter_line, f"{filter_line}\n{filter_line}"),
            )
            for mutated in mutations:
                self.assertNotEqual(self.run_hardening_fixture(mutated).returncode, 0)

    def test_cross_os_derived_selectors_match_source(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        selectors = paths_filter_definitions(workflow)["cross_os_rust"]
        self.assertEqual(
            tuple(path for path in selectors if path.startswith("src/services/discord/")),
            derived_cross_os_consumers("globs"),
        )

    def test_cfg_consumers_across_src_select_windows(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        filters = paths_filter_definitions(workflow)
        consumers = derived_cross_os_consumers(scope="src")
        self.assertTrue(consumers)
        for selector in ("rust_compile", "cross_os_rust"):
            with self.subTest(filter=selector):
                self.assertEqual([path for path in consumers if not selects(filters[selector], path)], [])

    def test_turn_bridge_and_historical_diff_select_windows(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        filters = paths_filter_definitions(workflow)
        fixture = json.loads(
            (REPO_ROOT / "tests/fixtures/ci_cross_os_5813.json").read_text(encoding="utf-8")
        )
        self.assertEqual(fixture["commit"], "d91d59d1d5fb4bcdd1602e29b7e2c980f0cece25")
        turn_bridge = REPO_ROOT / "src/services/discord/turn_bridge"
        paths = [path.relative_to(REPO_ROOT).as_posix() for path in turn_bridge.rglob("*.rs")]
        self.assertTrue(paths)
        cases = [(path, [path]) for path in paths]
        cases.append(("new turn_bridge module", ["src/services/discord/turn_bridge/new/module.rs"]))
        cases.append(("historical diff", fixture["paths"]))
        self.assertTrue(fixture["paths"])
        for name, changed_paths in cases:
            with self.subTest(diff=name):
                selected = {
                    key: any(selects(filters[key], path) for path in changed_paths)
                    for key in ("rust_compile", "cross_os_rust")
                }
                self.assertEqual(selected, {"rust_compile": True, "cross_os_rust": True})

    def test_cfg_gated_relay_consumers_select_windows(self) -> None:
        """cross_os_rust must select every derived cfg-shim consumer (#5832).

        The predecessor of this test pinned seven literal glob strings, which
        proved only that someone had typed them. This binds the workflow to the
        source instead: `scripts/cross_os_consumer_paths.py` recomputes the file
        class from the module tree, and a consumer that no glob matches fails
        here rather than on main's required Windows lane.
        """
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        paths = paths_filter_definitions(workflow)["cross_os_rust"]
        self.assertNotIn("src/services/discord/**", paths)
        self.assertEqual([pattern for pattern in paths if pattern.startswith("!")], [])

        consumers = derived_cross_os_consumers()
        self.assertGreater(len(consumers), 100)
        self.assertEqual([path for path in consumers if not selects(paths, path)], [])
        for path in CFG_SHIM_CONSUMERS:
            with self.subTest(consumer=path):
                self.assertIn(path, consumers)
                self.assertTrue(selects(paths, path))

        derived = [path for path in paths if path.startswith("src/services/discord/")]
        self.assertGreater(len(derived), 30)
        for selector in derived:
            with self.subTest(selector=selector):
                survivors = paths_filter_definitions(
                    comment_out_in_filter(workflow, "cross_os_rust", selector)
                )["cross_os_rust"]
                self.assertNotIn(selector, survivors)
                self.assertTrue(
                    [path for path in consumers if not selects(survivors, path)]
                )

    def test_module_walk_has_no_unaudited_blind_spots(self) -> None:
        """A file the walk never reaches cannot be derived (#5834 r3 P1-1).

        `voice_barge_in/tests/pcm_harness_tests.rs` was exactly that: Windows
        compiles it, it carries the #5828 cfg(unix)/not(unix) pair, no glob
        matched it -- and the coverage test above still passed, because the walk
        is its own oracle. Pinning the unreached set turns the next resolver gap
        into a failure here rather than a green lane that proves nothing.
        """
        unreached = unreachable_rust_files()
        self.assertEqual(unreached, tuple(path for path, _ in UNREACHABLE_RUST_FILES))
        for path, includer in UNREACHABLE_RUST_FILES:
            with self.subTest(unreachable=path):
                self.assertTrue((REPO_ROOT / path).is_file())
                self.assertNotIn(includer, unreached)
                spliced = PurePosixPath(path).relative_to(PurePosixPath(includer).parent)
                self.assertIn(
                    f'include!("{spliced}")',
                    (REPO_ROOT / includer).read_text(encoding="utf-8"),
                )

    def test_inflight_lock_primitive_triggers_required_native_windows_lane(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        jobs = yaml.safe_load(workflow)["jobs"]
        cross_os = jobs["check_fast_cross_os"]
        owner_paths = (
            "src/services/discord/inflight/store.rs",
            "src/services/discord/inflight/save_store.rs",
        )

        cross_os_paths = paths_filter_definitions(workflow)["cross_os_rust"]
        owner_selector = "src/services/discord/inflight/**"
        self.assertEqual(cross_os_paths.count(owner_selector), 1)
        for owner_path in owner_paths:
            self.assertTrue(selects(cross_os_paths, owner_path))
        self.assertNotIn("src/services/discord/**", cross_os_paths)

        commented = paths_filter_definitions(
            replace_last(
                workflow,
                f"              - '{owner_selector}'",
                f"              # - '{owner_selector}'",
            )
        )["cross_os_rust"]
        self.assertNotIn(owner_selector, commented)
        for owner_path in owner_paths:
            with self.subTest(missing_owner=owner_path):
                self.assertFalse(selects(commented, owner_path))
        self.assertEqual(
            cross_os["if"],
            "needs.changes.outputs.rust_compile == 'true' && "
            "needs.changes.outputs.cross_os_rust == 'true'",
        )
        self.assertEqual(cross_os["strategy"]["matrix"]["os"], ["windows-latest"])
        # The judge reads both Windows runners for the cross-OS context.
        self.assertLessEqual({"check_fast_cross_os", "check_fast_cross_os_targets"}, set(jobs["required_contexts"]["needs"]))

    def test_provider_and_session_host_trees_select_native_windows_lane(self) -> None:
        """Hand-listed cfg-shim trees outside the derived discord block.

        `src/services/*` reaches only the parent `.rs` files, so each subtree
        glob is load-bearing: commenting it out must unselect its sample file.
        """
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        cross_os_paths = paths_filter_definitions(workflow)["cross_os_rust"]
        owners = {
            "src/services/claude/**": "src/services/claude/tui_session_launch.rs",
            "src/services/qwen/**": "src/services/qwen/session_lifecycle.rs",
            "src/services/session_host/**": "src/services/session_host/tmux_host.rs",
        }
        self.assertTrue(selects(cross_os_paths, "src/services/session_host.rs"))
        for selector, sample in owners.items():
            with self.subTest(selector=selector):
                self.assertEqual(cross_os_paths.count(selector), 1)
                self.assertTrue(selects(cross_os_paths, sample))
                survivors = paths_filter_definitions(
                    comment_out_in_filter(workflow, "cross_os_rust", selector)
                )["cross_os_rust"]
                self.assertFalse(selects(survivors, sample))

    def test_trusted_macos_hosted_lane_runs_single_message_panel_tests(self) -> None:
        workflow = MACOS_TRUSTED_WORKFLOW.read_text(encoding="utf-8")
        command = (
            "env -u AGENTDESK_ROOT_DIR cargo test --lib "
            "single_message_panel::tests -- --skip _pg --skip pg_ --skip postgres"
        )
        self.assertEqual(job_block(workflow, "macos_hosted").count(command), 1)

    def test_trusted_macos_hosted_lane_runs_placeholder_live_events_tests(self) -> None:
        workflow = MACOS_TRUSTED_WORKFLOW.read_text(encoding="utf-8")
        command = (
            "env -u AGENTDESK_ROOT_DIR cargo test --lib "
            "placeholder_live_events -- --skip _pg --skip pg_ --skip postgres"
        )
        self.assertEqual(job_block(workflow, "macos_hosted").count(command), 1)

    def test_main_and_nightly_retain_non_pg_test_coverage(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        self.assertIn("check: fmt-check lint cargo-check test", justfile)
        self.assertIn("test: test-non-pg", justfile)
        self.assertEqual(
            just_recipe_commands(justfile, "test-non-pg"),
            EXPECTED_TEST_NON_PG_COMMANDS,
        )

        main_jobs = yaml.safe_load(MAIN_WORKFLOW.read_text(encoding="utf-8"))["jobs"]
        recipe = just_recipe_commands(justfile, "test-non-pg")
        non_lib = [
            command.replace(" --all-targets", " --bins --test '*'").replace(
                "-- --skip _pg --skip pg_ --skip postgres",
                '-- "${NON_PG_SKIP_ARGS[@]}"',
            )
            for command in recipe
            if "cargo test --lib " not in command
        ]
        self.assertIn("cargo test --doc ClaudeBinary", non_lib)
        lint_run = next(step["run"] for step in main_jobs["lint"]["steps"] if step.get("name") == "just lint")
        observed = next(line.strip().split(" | tee ")[0] for line in lint_run.splitlines() if line.strip().startswith("cargo clippy "))
        self.assertEqual(observed.replace(" --message-format=json", ""), just_recipe_commands(justfile, "lint")[0])
        self.assertIn("set -o pipefail", lint_run)
        self.assertNotIn("cargo clippy --workspace --all-targets --all-features -- -D warnings", lint_run)
        self.assertEqual(
            [
                line.strip()
                for step in main_jobs["lint"]["steps"]
                for line in str(step.get("run", "")).splitlines()
                if line.strip()
            ],
            [
                "npm run test:policies",
                "just fmt-check",
                "set -o pipefail",
                "mkdir -p target/clippy-observation",
                "cargo clippy --workspace --all-targets --all-features --message-format=json -- -W clippy::all | tee target/clippy-observation/diagnostics.jsonl",
                "python3 scripts/check_clippy_warning_count.py --input target/clippy-observation/diagnostics.jsonl --output target/clippy-observation/report.json --baseline scripts/clippy_warning_baseline.json --base-ref HEAD^1",
                "source scripts/ci/non-pg-test-filter.sh",
                *non_lib,
                "sccache --show-stats || true",
            ],
        )

        nightly = NIGHTLY_WORKFLOW.read_text(encoding="utf-8")
        for job_name in ("full_macos", "full_windows"):
            with self.subTest(job=job_name):
                job = job_block(nightly, job_name)
                # Lib tests run through the audited native path; the other targets keep the skip filter.
                self.assertIn("pg-classifier", job)
                self.assertIn("source scripts/ci/non-pg-test-filter.sh", job)
                self.assertIn(
                    'cargo test --bins --examples --benches --test \'*\' -- "${NON_PG_SKIP_ARGS[@]}"', job
                )
        self.assertIn("cargo test --test e2e -- --test-threads=1", job_block(nightly, "postgres_full"))

    def test_relay_authority_contract_job_uses_pinned_recipe(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        mutation = yaml.safe_load(workflow)["jobs"]["relay_authority_mutations"]
        self.assertEqual(mutation["if"], RUST_REQUIRED_IF)
        self.assertEqual(mutation["needs"], "changes")
        self.assertEqual(mutation["runs-on"], "ubuntu-latest")
        self.assertEqual(mutation["timeout-minutes"], 45)
        self.assertEqual(mutation["env"]["CARGO_PROFILE_DEV_DEBUG"], "0")
        self.assertEqual(mutation["env"]["CARGO_PROFILE_TEST_DEBUG"], "0")
        setup = [step for step in mutation["steps"] if "uses" in step and step.get("id") != "mutation_paths"]
        self.assertEqual(setup[0], {"uses": "actions/checkout@v4", "with": {"fetch-depth": 2}})
        self.assertEqual(setup[1]["with"]["toolchain"], "1.94.1")
        self.assertEqual(mutation["strategy"], {"fail-fast": False, "matrix": {"shard": [0, 1, 2]}})
        self.assertEqual(mutation["env"]["RELAY_AUTHORITY_MUTATION_SHARD_INDEX"], "${{ matrix.shard }}")
        self.assertEqual(mutation["env"]["RELAY_AUTHORITY_MUTATION_SHARD_TOTAL"], "3")
        self.assertRegex(
            job_block(workflow, "relay_authority_mutations"),
            r"(?m)^      - name: Require relay-authority mutations to be killed\n"
            r"        if: steps\.mutation_paths\.outputs\.mutation_sources != 'false'"
            r" \|\| steps\.mutation_wiring\.outputs\.wiring_changed != 'false'\n"
            r"        env:\n"
            r"          BASH_ENV: /dev/null\n"
            r'          CARGO_PROFILE_DEV_DEBUG: "0"\n'
            r'          CARGO_PROFILE_TEST_DEBUG: "0"\n'
            r"        shell: bash\n"
            r"        timeout-minutes: 45\n"
            r"        run: bash scripts/run_relay_authority_mutations\.sh$",
        )

    def assert_mutation_dependency_preparation(self, job: dict) -> None:
        condition = (
            "steps.mutation_paths.outputs.mutation_sources != 'false'"
            " || steps.mutation_wiring.outputs.wiring_changed != 'false'"
        )
        self.assertEqual(job["if"], RUST_REQUIRED_IF)
        self.assertEqual(job["needs"], "changes")
        self.assertNotIn("continue-on-error", job)
        steps = job["steps"]
        self.assertEqual(steps[0], {"uses": "actions/checkout@v4", "with": {"fetch-depth": 2}})
        path_filter = steps[1]
        self.assertEqual(path_filter.get("id"), "mutation_paths")
        self.assertEqual(path_filter.get("uses"), "dorny/paths-filter@v3")
        self.assertNotIn("if", path_filter)
        self.assertNotIn("continue-on-error", path_filter)
        names = [step.get("name") for step in steps]
        mutation_index = names.index("Require relay-authority mutations to be killed")
        self.assertEqual(steps[mutation_index].get("if"), condition)
        preparation = (
            "Install Rust toolchain",
            "Setup sccache",
            "Cache Cargo dependencies",
            "Fetch Cargo dependencies",
        )
        for name in preparation:
            self.assertIn(name, names, f"missing mutation preparation step: {name}")
            index = names.index(name)
            self.assertGreater(index, 1, f"{name} must follow the path filter")
            self.assertLess(index, mutation_index, f"{name} must precede mutations")
            self.assertEqual(steps[index].get("if"), condition, name)
            self.assertNotIn("continue-on-error", steps[index], name)
        fetch = steps[names.index("Fetch Cargo dependencies")]
        self.assertEqual(fetch.get("run", "").strip(), "cargo fetch --locked")
        self.assertEqual(fetch.get("shell"), "bash")
        self.assertEqual(fetch.get("env", {}).get("BASH_ENV"), "/dev/null")
        self.assertEqual(fetch.get("timeout-minutes"), 10)

    def test_mutation_job_prepares_online_dependencies_before_offline_mutations(self) -> None:
        job = yaml.safe_load(PR_WORKFLOW.read_text(encoding="utf-8"))["jobs"][
            "relay_authority_mutations"
        ]
        self.assert_mutation_dependency_preparation(job)

    def test_required_judge_backstops_judge_and_gate_content_hashes(self) -> None:
        jobs = yaml.safe_load(PR_WORKFLOW.read_text(encoding="utf-8"))["jobs"]
        gate_sha = hashlib.sha256((REPO_ROOT / "scripts/check-ci-runner-hardening.sh").read_bytes()).hexdigest()
        judge_sha = hashlib.sha256((REPO_ROOT / "scripts/ci/nextest_required.py").read_bytes()).hexdigest()
        self.assertEqual(gate_sha, CI_RUNNER_HARDENING_SHA256)
        judge = jobs["required_contexts"]
        self.assertEqual(judge["if"], MIRROR_IF)
        pin = next(step for step in judge["steps"] if step.get("name") == "Verify pinned judge and CI structure")
        self.assertEqual(pin["if"], "${{ matrix.context == 'Script checks' || matrix.context == 'relay-authority-contract' }}")
        self.assertEqual((pin["shell"], pin["env"]), ("bash", {"BASH_ENV": "/dev/null"}))
        self.assertIn(f"pin scripts/ci/nextest_required.py {judge_sha}\n", pin["run"])
        self.assertIn(f"pin scripts/check-ci-runner-hardening.sh {gate_sha}\n", pin["run"])
        self.assertTrue(pin["run"].endswith("scripts/check-ci-runner-hardening.sh\npython3 scripts/check_writer_gate_ci_wiring.py\n"))

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
        aggregate = step_block(scripts_job, "Run script checks")
        parsed_steps = yaml.safe_load(workflow)["jobs"]["scripts"]["steps"]
        parsed_aggregate = [
            step for step in parsed_steps if step.get("name") == "Run script checks"
        ]
        self.assertEqual(len(parsed_aggregate), 1)
        self.assertEqual(parsed_aggregate[0]["shell"], "bash")
        self.assertEqual(parsed_aggregate[0]["run"], "./scripts/ci-script-checks.sh")
        self.assertEqual(
            parsed_aggregate[0]["env"],
            {
                "BASH_ENV": "/dev/null",
                "PYTHON": "python3",
                "GFP_EVENT_NAME": "${{ github.event_name }}",
                "GFP_REPOSITORY": "${{ github.repository }}",
                "GFP_HEAD_REPOSITORY": "${{ github.event.pull_request.head.repo.full_name }}",
                "GFP_CANDIDATE_SHA": "${{ github.sha }}",
                "GFP_BASE_SHA": "${{ github.event.pull_request.base.sha }}",
                "GFP_HEAD_SHA": "${{ github.event.pull_request.head.sha }}",
                "TEST_LANE_BASELINE_REF": "HEAD^1",
                "SCRIPT_CHECK_SHARD": "cargo",
                "ADK_LIB_TEST_ARTIFACT": "${{ runner.temp }}/lib-test-artifact",
                "CARGO_PROFILE_DEV_DEBUG": "0",
                "CARGO_PROFILE_TEST_DEBUG": "0",
            },
        )
        mutated_job = scripts_job.replace(aggregate, aggregate + aggregate, 1)
        mutated = workflow.replace(scripts_job, mutated_job, 1)
        result = self.run_hardening_fixture(mutated)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            'must retain exactly one "Run script checks" step', result.stderr
        )

        deleted_job = scripts_job.replace(aggregate, "", 1)
        self.assertNotEqual(deleted_job, scripts_job)
        deleted = workflow.replace(scripts_job, deleted_job, 1)
        result = self.run_hardening_fixture(deleted)
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

    def test_giant_file_progress_selector_wiring(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        aggregate = (REPO_ROOT / "scripts/ci-script-checks.sh").read_text(encoding="utf-8")
        self.assertIn('GFP_REFRESH_DOCS=1 "$PYTHON" scripts/giant_file_progress.py', aggregate)
        self.assertNotIn('"$PYTHON" scripts/generate_inventory_docs.py\n', aggregate)
        self.assertIn("tests.test_giant_file_progress tests.test_inventory_giant_split", aggregate)
        parsed_pr = yaml.safe_load(workflow)
        run_step = next(step for step in parsed_pr["jobs"]["scripts"]["steps"]
                        if step.get("name") == "Run script checks")
        expected_pr_env = {"GFP_EVENT_NAME": "${{ github.event_name }}",
            "GFP_REPOSITORY": "${{ github.repository }}",
            "GFP_HEAD_REPOSITORY": "${{ github.event.pull_request.head.repo.full_name }}",
            "GFP_CANDIDATE_SHA": "${{ github.sha }}",
            "GFP_BASE_SHA": "${{ github.event.pull_request.base.sha }}",
            "GFP_HEAD_SHA": "${{ github.event.pull_request.head.sha }}"}
        self.assertLessEqual(expected_pr_env.items(), run_step["env"].items())
        upload = next(step for step in parsed_pr["jobs"]["scripts"]["steps"] if step.get("name") == "Upload giant-file progress evidence")
        self.assertEqual(upload["if"], "always()")
        self.assertEqual(upload["with"]["path"], "target/giant-file-progress/evidence.json")
        main_steps = yaml.safe_load(MAIN_WORKFLOW.read_text(encoding="utf-8"))["jobs"]["scripts"]["steps"]
        main_run = next(step for step in main_steps if step.get("name") == "Run script checks")
        self.assertEqual(main_run["env"]["GFP_CANDIDATE_SHA"], "${{ github.sha }}")
        self.assertEqual(main_run["env"]["GFP_REF"], "${{ github.ref }}")
        self.assertEqual(main_run["env"]["GFP_REF_PROTECTED"], "${{ github.ref_protected }}")
        self.assertTrue(any(step.get("name") == "Upload giant-file progress evidence" for step in main_steps))
        for key in ("GFP_BASE_SHA", "GFP_HEAD_REPOSITORY"):
            mutated = workflow.replace(f"          {key}: {expected_pr_env[key]}\n", "", 1)
            rejected = self.run_hardening_fixture(mutated)
            self.assertNotEqual(rejected.returncode, 0, key)
        main = MAIN_WORKFLOW.read_text()
        for job_id in ("scripts", "scripts_guards", "scripts_contracts"):
            block = job_block(main, job_id)
            for key in ("GFP_REF", "GFP_REF_PROTECTED"):
                with self.subTest(main_shard=job_id, key=key):
                    mutated = main.replace(block, re.sub(rf"(?m)^          {key}: .*\n", "", block), 1)
                    self.assertNotEqual(mutated, main)
                    rejected = self.run_hardening_fixture(workflow, {"ci-main.yml": mutated})
                    self.assertNotEqual(rejected.returncode, 0)
                    self.assertIn("Run script checks env must equal", rejected.stderr)

    def test_script_checks_effective_execution_contract_covers_all_yaml_scopes(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        root_env = "env:\n  CARGO_TERM_COLOR: always\n"
        scripts_marker = "  scripts:\n    name: Script checks runner\n"

        mutations = {
            "step shell": workflow.replace(
                "      - name: Run script checks\n        shell: bash\n",
                "      - name: Run script checks\n        shell: bash -n {0}\n",
                1,
            ),
            "step working-directory": workflow.replace(
                "      - name: Run script checks\n        shell: bash\n",
                "      - name: Run script checks\n"
                "        working-directory: /tmp\n"
                "        shell: bash\n",
                1,
            ),
            "step env": workflow.replace(
                "          PYTHON: python3\n",
                "          PYTHON: /bin/true\n",
                1,
            ),
            "job defaults shell": workflow.replace(
                scripts_marker,
                scripts_marker + "    defaults:\n      run:\n        shell: bash -n {0}\n",
                1,
            ),
            "job defaults working-directory": workflow.replace(
                scripts_marker,
                scripts_marker
                + "    defaults:\n      run:\n        working-directory: /tmp\n",
                1,
            ),
            "job env": workflow.replace(
                scripts_marker,
                scripts_marker + "    env:\n      PYTHON: /bin/true\n",
                1,
            ),
            "workflow defaults shell": workflow.replace(
                root_env,
                "defaults:\n  run:\n    shell: bash -n {0}\n\n" + root_env,
                1,
            ),
            "workflow defaults working-directory": workflow.replace(
                root_env,
                "defaults:\n  run:\n    working-directory: /tmp\n\n" + root_env,
                1,
            ),
            "workflow env": workflow.replace(
                root_env,
                "env:\n  PYTHON: /bin/true\n  CARGO_TERM_COLOR: always\n",
                1,
            ),
            "previous GITHUB_ENV write": workflow.replace(
                "      - name: Install shellcheck\n"
                "        run: sudo apt-get install -y shellcheck zsh\n",
                "      - name: Install shellcheck\n"
                "        run: echo \"PYTHON=/bin/true\" >> \"$GITHUB_ENV\"\n",
                1,
            ),
            "previous GITHUB_PATH write": workflow.replace(
                "      - name: Install shellcheck\n"
                "        run: sudo apt-get install -y shellcheck zsh\n",
                "      - name: Install shellcheck\n"
                "        run: echo \"/tmp/injected\" >> \"$GITHUB_PATH\"\n",
                1,
            ),
            "runs-on": workflow.replace(
                "  scripts:\n    name: Script checks runner\n    needs: changes\n    if: ${{ !cancelled() }}\n    runs-on: ubuntu-latest\n",
                "  scripts:\n    name: Script checks runner\n    needs: changes\n    if: ${{ !cancelled() }}\n    runs-on: macos-latest\n",
                1,
            ),
        }

        for label, mutated in mutations.items():
            with self.subTest(mutation=label):
                self.assertNotEqual(mutated, workflow)
                result = self.run_hardening_fixture(mutated)
                self.assertNotEqual(result.returncode, 0, result.stderr)
                self.assertIn("effective execution changed", result.stderr)
                if label == "previous GITHUB_ENV write":
                    self.assertIn('"key":"PYTHON"', result.stderr)
                elif label == "previous GITHUB_PATH write":
                    self.assertIn('"path":"/tmp/injected"', result.stderr)

        passing = self.run_hardening_fixture(workflow)
        self.assertEqual(passing.returncode, 0, passing.stderr)

    def test_script_checks_protected_step_inventory_is_pinned(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        hardening = (
            REPO_ROOT / "scripts/check-ci-runner-hardening.sh"
        ).read_text(encoding="utf-8")
        self.assertIn(
            '"protected_step_inventory" => protected_step_inventory(steps)',
            hardening,
        )
        insertion = (
            "      - name: Run script checks\n"
            "        shell: bash\n"
            "        run: ./scripts/ci-script-checks.sh\n"
        )
        scripts = job_block(workflow, "scripts")
        cases = {
            "interstitial step": workflow.replace(
                insertion,
                "      - name: Unregistered interstitial check\n"
                "        run: true\n\n"
                + insertion,
                1,
            ),
            "pre-pair aggregate overwrite": workflow.replace(
                scripts,
                scripts.replace(
                    "      - name: Protect writer gate aggregate wiring (#5308)\n",
                    "      - name: Protect writer gate aggregate wiring (#5308)\n",
                    1,
                ),
                1,
            ),
        }
        cases["pre-pair aggregate overwrite"] = workflow.replace(
            scripts,
            scripts.replace(
                "      - name: Protect writer gate aggregate wiring (#5308)\n",
                "      - name: Replace aggregate before protection\n"
                "        run: printf '#!/usr/bin/env bash\\nexit 0\\n' > scripts/ci-script-checks.sh\n\n"
                "      - name: Protect writer gate aggregate wiring (#5308)\n",
                1,
            ),
            1,
        )
        for label, mutated in cases.items():
            with self.subTest(mutation=label):
                self.assertNotEqual(mutated, workflow)
                result = self.run_hardening_fixture(mutated)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(
                    "protected step inventory changed; expected indices [9, 10]",
                    result.stderr,
                )

    def test_required_judge_job_is_pinned_fail_closed(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        judge = job_block(workflow, "required_contexts")
        closure_if = 'required needs closure job "required_contexts" must exist with its exact if'
        mutations = {
            "job deleted": ("", None),
            "changes dependency deleted": (judge.replace("      - changes\n", "", 1), None),
            "evidence dependency deleted": (judge.replace("      - test_evidence\n", "", 1), None),
            "job if deleted": (judge.replace(MIRROR_IF_LINE, "", 1), closure_if),
            "job if success": (judge.replace(MIRROR_IF_LINE, "    if: success()\n", 1), closure_if),
            "job if event": (judge.replace(MIRROR_IF_LINE, "    if: ${{ github.event_name == 'push' }}\n", 1), closure_if),
            "job continue-on-error injected": (judge.replace("    runs-on: ubuntu-latest\n", "    continue-on-error: true\n    runs-on: ubuntu-latest\n", 1), closure_if),
            "checkout provenance": (judge.replace("      - uses: actions/checkout@v4\n", "      - uses: actions/checkout@v4\n        with:\n          repository: attacker/green-judge\n", 1), None),
            "extra step": (judge.replace("      - uses: actions/checkout@v4\n", "      - uses: actions/checkout@v4\n      - run: printf 'exit 0\\n' > scripts/ci/nextest_required.py\n", 1), None),
            "judge weakened": (judge.replace('        run: python3 scripts/ci/nextest_ci.py required-context "${{ matrix.context }}"\n', "        run: 'true'\n", 1), None),
            "context dropped": (judge.replace("          - Library test sweep (ubuntu-latest)\n", "", 1), None),
            "context added by include": (judge.replace("          - Lint\n", "          - Lint\n        include:\n          - context: Lint\n", 1), None),
            "backstop scoped away": (judge.replace("matrix.context == 'Script checks' || ", "", 1), None),
            "stale judge pin": (re.sub(r"(pin scripts/ci/nextest_required\.py )[0-9a-f]{64}", r"\g<1>" + "0" * 64, judge), "required_contexts must retain its exact judge, pins and needs"),
        }
        for field, value in {
            "defaults": "defaults:\n      run:\n        shell: bash -n {0}",
            "env": "env:\n      PYTHON: /bin/true",
            "environment": "environment: never-approved",
            "container": "container: ubuntu:latest",
        }.items():
            mutations[field] = (judge.replace("    runs-on: ubuntu-latest\n", "    runs-on: ubuntu-latest\n" + "\n".join(f"    {line}" for line in value.splitlines()) + "\n", 1), None)
        for label, (mutated_judge, diagnostic) in mutations.items():
            with self.subTest(mutation=label):
                self.assertNotEqual(mutated_judge, judge)
                result = self.run_hardening_fixture(workflow.replace(judge, mutated_judge, 1))
                self.assertNotEqual(result.returncode, 0, result.stderr)
                if diagnostic:
                    self.assertIn(diagnostic, result.stderr)

    def test_required_needs_closure_rejects_fail_open_mutations(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        mutations = []
        for job_id in ("rust_build", "rust_tests", "test_evidence", "pg15_owner_proof",
                       "relay_authority_mutations", "lint", "dashboard", "check_fast_cross_os_targets"):
            runner = job_block(workflow, job_id)
            mutations.append((job_id, runner, re.sub(r"(?m)^    if: .*\n", "    if: always()\n", runner, count=1)))
            mutations.append((job_id, runner, runner.replace("    runs-on:", "    continue-on-error: true\n    runs-on:", 1)))
        for job_id, original, changed in mutations:
            with self.subTest(job=job_id, mutation=changed.splitlines()[2:4]):
                self.assertNotEqual(original, changed)
                result = self.run_hardening_fixture(workflow.replace(original, changed, 1))
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn(f'required needs closure job "{job_id}" must exist with its exact if', result.stderr)

    def test_cross_os_required_context_fails_closed_on_either_runner(self) -> None:
        sys.path.insert(0, str(REPO_ROOT / "scripts/ci"))
        import nextest_required
        context = "Fast check cross OS required context (ubuntu-latest)"
        outcomes = ("success", "failure", "cancelled", "skipped")
        for cross_os_rust in ("true", "false"):
            for check in outcomes:
                for targets in outcomes:
                    needs = {job: {"result": "success"} for job in ("rust_build", "rust_tests", "test_evidence")}
                    needs.update(changes={"result": "success", "outputs": {"rust_tests": "true", "cross_os_rust": cross_os_rust}},
                                 check_fast_cross_os={"result": check}, check_fast_cross_os_targets={"result": targets})
                    with self.subTest(cross_os_rust=cross_os_rust, check=check, targets=targets):
                        allowed = {"success"} if cross_os_rust == "true" else {"success", "skipped"}
                        self.assertEqual(nextest_required.context_verdict(context, needs)[0], check in allowed and targets in allowed)

    def test_cross_os_split_rejects_fail_open_mutations(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        judge = job_block(workflow, "required_contexts")
        targets = job_block(workflow, "check_fast_cross_os_targets")
        writer = step_block(targets, "Writer namespace exact Windows targets")
        mutations = (
            (judge, judge.replace("      - check_fast_cross_os_targets\n", "", 1), "required needs closure changed"),
            (targets, targets.replace("    runs-on:", "    continue-on-error: true\n    runs-on:", 1), 'required needs closure job "check_fast_cross_os_targets" must exist with its exact if'),
            (targets, targets.replace("    if: needs.changes.outputs.rust_compile == 'true' && ", "    if: ", 1), 'required needs closure job "check_fast_cross_os_targets" must exist with its exact if'),
            (targets, targets.replace(writer, "", 1), 'must retain exactly one "Writer namespace exact Windows targets" step'),
        )
        for original, changed, diagnostic in mutations:
            with self.subTest(diagnostic=diagnostic):
                self.assertNotEqual(original, changed)
                result = self.run_hardening_fixture(workflow.replace(original, changed, 1))
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn(diagnostic, result.stderr)

    def test_required_job_needs_closure_has_role_specific_scheduling_policy(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        jobs = yaml.safe_load(workflow)["jobs"]
        n_gate = "${{ !cancelled() && needs.rust_build.result == 'success' }}"
        rust_gate = "${{ !cancelled() && needs.changes.outputs.rust_tests == 'true' }}"
        cross_os = "needs.changes.outputs.rust_compile == 'true' && needs.changes.outputs.cross_os_rust == 'true'"
        expected_if = {
            "changes": None, "required_contexts": MIRROR_IF, "scripts": MIRROR_IF, "scripts_guards": None, "scripts_contracts": None,
            "rust_build": rust_gate, "pg15_owner_proof": rust_gate, "rust_tests": n_gate, "test_evidence": n_gate,
            "relay_authority_mutations": RUST_REQUIRED_IF,
            "lint": _LINT_FILTER, "dashboard": "needs.changes.outputs.dashboard == 'true'",
            "check_fast_cross_os": cross_os, "check_fast_cross_os_targets": cross_os,
        }
        closure: set[str] = set()
        frontier = ["required_contexts"]
        while frontier:
            job_id = frontier.pop()
            if job_id in closure:
                continue
            closure.add(job_id)
            needs = jobs[job_id].get("needs", [])
            frontier.extend([needs] if isinstance(needs, str) else needs)
        self.assertEqual(closure, set(expected_if))
        for job_id, condition in expected_if.items():
            self.assertEqual(jobs[job_id].get("if"), condition, job_id)
        for job_id in sorted(expected_if):
            job = job_block(workflow, job_id)
            marker = f"    name: {jobs[job_id]['name']}\n"
            self.assertIn(marker, job)
            injected_if = "    if: ${{ github.event_name == 'push' }}\n"
            for key, mutated_job in (
                ("continue-on-error", job.replace(marker, f"{marker}    continue-on-error: true\n", 1)),
                ("if", re.sub(r"(?m)^    if: .*\n", injected_if, job, count=1) if "if" in jobs[job_id] else job.replace(marker, marker + injected_if, 1)),
            ):
                with self.subTest(job=job_id, key=key):
                    result = self.run_hardening_fixture(workflow.replace(job, mutated_job, 1))
                    self.assertNotEqual(result.returncode, 0, result.stderr)

    def test_change_surfaces_wiring_table_is_the_exact_publisher_closure(self) -> None:
        jobs = yaml.safe_load(PR_WORKFLOW.read_text(encoding="utf-8"))["jobs"]
        doc = (REPO_ROOT / "docs/agent-maintenance/change-surfaces.md").read_text(encoding="utf-8")
        blocks = re.findall(r"<!-- required-publisher-wiring -->\n(.*?)\n *<!-- /required-publisher-wiring -->", doc, re.S)
        self.assertEqual(len(blocks), 1, "change-surfaces.md must hold exactly one publisher wiring table")
        rows, frontier = {}, ["required_contexts"]
        while frontier:
            job_id = frontier.pop()
            if job_id not in rows:
                needs = jobs[job_id].get("needs", [])
                rows[job_id] = [needs] if isinstance(needs, str) else needs
                frontier.extend(rows[job_id])

        def cell(values: list[str]) -> str:
            return ", ".join(f"`{value}`" for value in values) or "none"

        expected = ["| job | needs | job-level `if` |", "|---|---|---|"] + [
            f"| `{job_id}` | {cell(needs)} | {cell([jobs[job_id]['if']] if 'if' in jobs[job_id] else [])} |"
            for job_id, needs in sorted(rows.items())
        ]
        self.assertEqual([line.strip() for line in blocks[0].splitlines()], expected)

    def test_duplicate_required_job_id_is_rejected_before_last_wins_resolution(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        judge = job_block(workflow, "required_contexts")
        malicious_last = judge.replace("          - Lint\n", "", 1)
        mutated = workflow.replace("  lint:\n", malicious_last + "  lint:\n", 1)
        self.assertNotEqual(mutated, workflow)
        result = self.run_hardening_fixture(mutated)
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertIn("duplicate job IDs are forbidden: required_contexts", result.stderr)

    def test_script_checks_mirror_fails_closed_on_skipped_upstream_results(self) -> None:
        mirror_script = REPO_ROOT / "scripts/required-check-mirror.sh"
        base_env = {
            **os.environ,
            "GITHUB_ACTIONS": "true",
            "FILTER_NAME": "scripts",
            "FILTER_OUTPUT": "true",
            "UPSTREAM_JOB_NAME": "scripts",
        }
        for changed_paths_result, upstream_result in (
            ("skipped", "skipped"),
            ("success", "skipped"),
            ("success", "failure"),
            ("success", "cancelled"),
        ):
            with self.subTest(
                changed_paths_result=changed_paths_result,
                upstream_result=upstream_result,
            ):
                env = {
                    **base_env,
                    "CHANGED_PATHS_RESULT": changed_paths_result,
                    "UPSTREAM_RESULT": upstream_result,
                }
                result = subprocess.run(
                    ["bash", str(mirror_script)],
                    env=env,
                    text=True,
                    capture_output=True,
                    check=False,
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("::error::", result.stderr)

        success = subprocess.run(
            ["bash", str(mirror_script)],
            env={
                **base_env,
                "CHANGED_PATHS_RESULT": "success",
                "UPSTREAM_RESULT": "success",
            },
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(success.returncode, 0, success.stderr)

    def test_required_contexts_are_published_only_by_the_judge(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        baseline = self.run_hardening_fixture(workflow)
        self.assertEqual(baseline.returncode, 0, baseline.stderr)
        rogue = "  rogue:\n    name: {name}\n    runs-on: ubuntu-latest\n{extra}    steps:\n      - run: 'true'\n\n"
        matrix = "    strategy:\n      matrix:\n        context: [x]\n"
        main = MAIN_WORKFLOW.read_text(encoding="utf-8")
        cases = (
            ("PR literal Lint", workflow.replace("  lint:\n", rogue.format(name="Lint", extra="") + "  lint:\n", 1), {}),
            ("PR literal High-risk recovery", workflow.replace("  lint:\n", rogue.format(name="High-risk recovery", extra="") + "  lint:\n", 1), {}),
            ("PR matrix expression", workflow.replace("  lint:\n", rogue.format(name="'${{ matrix.context }}'", extra=matrix) + "  lint:\n", 1), {}),
            ("PR unnamed matrix suffix", workflow.replace("  lint:\n", rogue.format(name="Dashboard", extra=matrix).replace("    name: Dashboard\n", "") .replace("  rogue:", "  Dashboard:") + "  lint:\n", 1), {}),
            ("main publishes Library test sweep", workflow, {"ci-main.yml": main.replace("\njobs:\n", "\njobs:\n" + rogue.format(name="Library test sweep (ubuntu-latest)", extra=""), 1)}),
            ("other workflow publishes relay contract", workflow, {"ci-extra.yml": "name: extra\non: push\njobs:\n" + rogue.format(name="relay-authority-contract", extra="")}),
        )
        for label, pr, extra in cases:
            with self.subTest(case=label):
                result = self.run_hardening_fixture(pr, extra_workflows=extra)
                self.assertNotEqual(result.returncode, 0, result.stderr)
                self.assertIn("must publish every required context only from the required_contexts judge", result.stderr)

    def test_helper_content_pin_kills_all_prior_mutation_classes(self) -> None:
        helper = (REPO_ROOT / "scripts/required-check-mirror.sh").read_text(
            encoding="utf-8"
        )
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        mutations = {
            "environment conditional": helper.replace(
                "set -euo pipefail\n",
                "set -euo pipefail\n\n"
                'if [ "${GITHUB_ACTIONS:-}" = "true" ] && '
                '[ "${UPSTREAM_JOB_NAME:-}" = "scripts" ]; then exit 0; fi\n',
                1,
            ),
            "step-instance conditional": helper.replace(
                "set -euo pipefail\n",
                'set -euo pipefail\n[ "${GITHUB_ACTION:-}" = "__run_2" ] && exit 0\n',
                1,
            ),
            "argv0 conditional": helper.replace(
                "set -euo pipefail\n",
                'set -euo pipefail\ncase "$0" in ./scripts/*) exit 0;; esac\n',
                1,
            ),
            "unconditional exit zero": helper.replace(
                "set -euo pipefail\n", "set -euo pipefail\nexit 0\n", 1
            ),
        }
        for label, mutated_helper in mutations.items():
            with self.subTest(mutation=label):
                result = self.run_hardening_fixture(
                    workflow, mirror_helper=mutated_helper
                )
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn("content hash mismatch", result.stderr)

    def test_required_jobs_hash_backstops_reject_judge_and_gate_mutations(self) -> None:
        jobs = yaml.safe_load(PR_WORKFLOW.read_text(encoding="utf-8"))["jobs"]
        run = next(step["run"] for step in jobs["required_contexts"]["steps"] if step.get("name") == "Verify pinned judge and CI structure")
        pin_only = run.rsplit("\nscripts/check-ci-runner-hardening.sh\n", 1)[0]
        judge = (REPO_ROOT / "scripts/ci/nextest_required.py").read_text(encoding="utf-8")
        gate = (REPO_ROOT / "scripts/check-ci-runner-hardening.sh").read_text(encoding="utf-8")
        judge_sha = hashlib.sha256(judge.encode()).hexdigest()
        cases = (
            ("matching bytes", judge, gate, pin_only, 0),
            ("one-byte judge mutation", judge + "#", gate, pin_only, 1),
            ("judge pin mutation", judge, gate, pin_only.replace(judge_sha, "0" * 64, 1), 1),
            ("one-byte gate mutation", judge, gate + "#", pin_only, 1),
            ("gate pin mutation", judge, gate, pin_only.replace(CI_RUNNER_HARDENING_SHA256, "0" * 64, 1), 1),
            ("gate repin roundtrip", judge, gate + "# reviewed\n", pin_only.replace(CI_RUNNER_HARDENING_SHA256, hashlib.sha256((gate + "# reviewed\n").encode()).hexdigest(), 1), 0),
        )
        for label, judge_candidate, gate_candidate, script, code in cases:
            with self.subTest(case=label), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                (root / "scripts/ci").mkdir(parents=True)
                (root / "scripts/ci/nextest_required.py").write_text(judge_candidate, encoding="utf-8")
                (root / "scripts/check-ci-runner-hardening.sh").write_text(gate_candidate, encoding="utf-8")
                result = subprocess.run(["bash", "-c", script], cwd=root, text=True, capture_output=True, check=False)
                self.assertEqual(result.returncode, code, result.stdout + result.stderr)
                if code:
                    self.assertIn("content hash mismatch", result.stdout + result.stderr)

    def test_warning_ceiling_and_cap_adjacency_reject_bypass(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        main = MAIN_WORKFLOW.read_text(encoding="utf-8")
        command = 'python3 scripts/check_clippy_warning_count.py --input target/clippy-observation/diagnostics.jsonl --output target/clippy-observation/report.json --baseline scripts/clippy_warning_baseline.json --base-ref '
        variants = (
            (workflow.replace("      - name: Production PR cap\n", '      - run: echo PR_CAP_CI=0 >> "$GITHUB_ENV"\n\n      - name: Production PR cap\n', 1), main, "production PR cap"),
            (workflow.replace(command + '"$CLIPPY_BASE_REF"', 'true', 1), main, "PR warning ceiling"),
            (workflow, main.replace(command + "HEAD^1", "true", 1), "main Clippy observation"),
        )
        for pr, altered_main, expected in variants:
            with self.subTest(expected=expected):
                result = self.run_hardening_fixture(pr, {"ci-main.yml": altered_main})
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn(expected, result.stderr)

    def test_documented_harmless_surface_edits_are_not_overblocked(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        scripts = job_block(workflow, "scripts")
        cases = {
            "step after protected pair": workflow.replace(
                scripts,
                scripts.replace(
                    "      - name: Hotfile LOC ratchet (always, #3565)\n",
                    "      - name: Harmless post-protection setup\n"
                    "        run: true\n\n"
                    "      - name: Hotfile LOC ratchet (always, #3565)\n",
                    1,
                ),
                1,
            ),
            "GITHUB_ENV prose without redirection": workflow.replace(
                "      - name: Install shellcheck\n"
                "        run: sudo apt-get install -y shellcheck zsh\n",
                "      - name: Install shellcheck\n"
                "        # This prose mentions GITHUB_ENV but performs no write.\n"
                "        run: sudo apt-get install -y shellcheck zsh\n",
                1,
            ),
            "aggregate timeout": workflow.replace(
                "      - name: Run script checks\n        shell: bash\n",
                "      - name: Run script checks\n"
                "        timeout-minutes: 30\n"
                "        shell: bash\n",
                1,
            ),
        }
        for label, mutated in cases.items():
            with self.subTest(edit=label):
                result = self.run_hardening_fixture(mutated)
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_unregistered_target_step_forbidden_runtime_env_is_rejected(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        relay_job = job_block(workflow, "relay_authority_mutations")
        mutated_relay_job = relay_job.replace(
            "          fetch-depth: 2\n\n",
            "          fetch-depth: 2\n\n"
            "      - name: Unregistered forbidden env\n"
            "        env:\n"
            '          CARGO_PROFILE_DEV_DEBUG: "1"\n'
            "        run: true\n\n",
            1,
        )
        mutated_workflow = workflow.replace(relay_job, mutated_relay_job, 1)
        self.assertNotEqual(mutated_workflow, workflow)

        hardening = (
            REPO_ROOT / "scripts/check-ci-runner-hardening.sh"
        ).read_text(encoding="utf-8")
        repinned_hardening = self._repin_job_hash(
            hardening, mutated_workflow, "relay_authority_mutations"
        )
        repinned_gate_sha256 = hashlib.sha256(repinned_hardening.encode()).hexdigest()
        mutated_workflow = mutated_workflow.replace(
            CI_RUNNER_HARDENING_SHA256,
            repinned_gate_sha256,
        )
        result = self.run_hardening_fixture(
            mutated_workflow, hardening_script=repinned_hardening
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must not set CARGO_PROFILE_DEV_DEBUG", result.stderr)

    def test_required_pr_steps_cannot_be_silently_disabled(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        jobs = yaml.safe_load(workflow)["jobs"]
        protected_steps = {
            "scripts": (
                (
                    "Protect writer gate aggregate wiring (#5308)",
                    "must not continue on error",
                ),
                ("Run script checks", "must not continue on error"),
            ),
            "rust_build": (
                ("Archive, list, compare and plan", "must retain exact continue-on-error policy"),
            ),
            "relay_authority_mutations": (
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

    def test_writer_gate_wiring_step_is_direct_and_exact(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        step = (
            "      - name: Protect writer gate aggregate wiring (#5308)\n"
            "        timeout-minutes: 10\n"
            "        shell: bash\n"
            "        run: |\n"
            "          python3 scripts/check_writer_gate_ci_wiring.py\n"
            "          python3 -m unittest tests.test_writer_gate_ci_wiring\n"
            "          scripts/check-ci-runner-hardening.sh\n"
        )
        self.assertEqual(workflow.count(step), 1)

        for label, mutated_step, expected_error in (
            (
                "deleted",
                "",
                "must retain exactly one writer gate aggregate wiring step",
            ),
            (
                "conditional",
                step.replace(
                    "        run: |\n", "        if: ${{ false }}\n        run: |\n"
                ),
                "writer gate aggregate wiring step must not define if",
            ),
            (
                "command drift",
                step.replace(
                    "python3 scripts/check_writer_gate_ci_wiring.py",
                    "python3 scripts/check_writer_gate_ci_wiring.py --help",
                ),
                "must retain the exact external protection command list",
            ),
            (
                "hardening deleted",
                step.replace("          scripts/check-ci-runner-hardening.sh\n", ""),
                "must retain the exact external protection command list",
            ),
        ):
            with self.subTest(mutation=label):
                mutated = workflow.replace(step, mutated_step, 1)
                self.assertNotEqual(mutated, workflow)
                result = self.run_hardening_fixture(mutated)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(expected_error, result.stderr)

    def test_registered_step_continue_policy_is_typed_and_exact(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        step_name = "Archive, list, compare and plan"
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
        step_name = "Archive, list, compare and plan"
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
                # Main must carry the same step policy, since its nextest jobs equal the PR's.
                main = MAIN_WORKFLOW.read_text(encoding="utf-8")
                main = main.replace(f"      - name: {step_name}\n", f"      - name: {step_name}\n{insertion}", 1)
                result = self.run_hardening_fixture(mutated, {"ci-main.yml": main})
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

    def test_script_checks_needs_preserves_producer_and_changes(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        scripts_job = job_block(workflow, "scripts")
        for needs in ("[changes, lib_test_prepare]", "[changes, other]", "other"):
            with self.subTest(needs=needs):
                mutated = scripts_job.replace("needs: changes", f"needs: {needs}", 1)
                self.assertNotEqual(mutated, scripts_job)
                result = self.run_hardening_fixture(workflow.replace(scripts_job, mutated, 1))
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("must retain exact needs: changes", result.stderr)

    def test_trusted_macos_runs_busy_retry_regressions_on_hosted_runner(self) -> None:
        workflow = MACOS_TRUSTED_WORKFLOW.read_text(encoding="utf-8")
        hosted = job_block(workflow, "macos_hosted")
        self.assertEqual(hosted.count(BUSY_RETRY_4888_TEST_COMMAND), 1)

    def test_trusted_macos_has_only_an_unconditional_hosted_job(self) -> None:
        workflow = yaml.safe_load(MACOS_TRUSTED_WORKFLOW.read_text(encoding="utf-8"))
        jobs = workflow["jobs"]
        self.assertNotIn("macos_self_hosted", jobs)
        self.assertNotIn("resolve_macos_runner", jobs)
        self.assertEqual(set(jobs), {"macos_hosted"})
        hosted = jobs["macos_hosted"]
        self.assertEqual(hosted["name"], "Trusted macOS check (hosted)")
        self.assertEqual(hosted["runs-on"], "macos-15")
        self.assertNotIn("needs", hosted)
        self.assertNotIn("if", hosted)

    def assert_filter_gates_exactly(self, job_name: str, gated: set[str]) -> None:
        """`gated` skips only on a successful run=false; other steps never skip."""
        workflow = yaml.safe_load(MACOS_TRUSTED_WORKFLOW.read_text(encoding="utf-8"))
        steps = workflow["jobs"][job_name]["steps"]
        start = next(i for i, step in enumerate(steps) if step.get("id") == "rust_filter")
        conditions = {step["name"]: step.get("if") for step in steps[start + 1 :]}
        self.assertLessEqual(gated, set(conditions))
        for outcome, run, gated_runs in MACOS_FILTER_SCENARIOS:
            context = {
                "steps.rust_filter.outcome": outcome,
                "steps.rust_filter.outputs.run": run,
            }
            for name, condition in conditions.items():
                with self.subTest(job=job_name, step=name, outcome=outcome, run=run):
                    self.assertEqual(
                        eval_step_if(condition, context), gated_runs or name not in gated
                    )

    def test_trusted_macos_hosted_job_gates_only_heavy_steps(self) -> None:
        workflow = MACOS_TRUSTED_WORKFLOW.read_text(encoding="utf-8")
        hosted = job_block(workflow, "macos_hosted")
        header, steps = hosted.split("    steps:\n", 1)
        self.assertNotIn("rust_filter", header)
        checkout = steps.index("      - uses: actions/checkout@v4\n")
        filter_step = step_block(hosted, "Decide whether heavy steps are needed")
        self.assertLess(checkout, steps.index(filter_step))
        self.assertIn("fetch-depth: 0", steps[checkout : steps.index(filter_step)])
        parsed = yaml.safe_load(workflow)["jobs"]["macos_hosted"]
        filter_config = next(step for step in parsed["steps"] if step.get("id") == "rust_filter")
        self.assertEqual(filter_config["continue-on-error"], True)
        self.assertEqual(filter_config["shell"], "bash")
        self.assertEqual(filter_config["env"], {"EVENT_NAME": "${{ github.event_name }}"})
        self.assertEqual(
            filter_config["run"],
            'python3 scripts/ci/macos-trusted-rust-filter.py --event "$EVENT_NAME" '
            '--base-ref origin/main >> "$GITHUB_OUTPUT"',
        )
        self.assert_filter_gates_exactly(
            "macos_hosted",
            {
                "Install Rust toolchain",
                "Install Opus on macOS",
                "Cache Cargo dependencies",
                "cargo check",
                "H2 tmux boundary measurement (macos, inert)",
                "H2 module map (macos, inert)",
                "cargo test (non-PG, targeted subset)",
                "Fresh user portable smoke",
            },
        )
        self.assertIn("Disable sccache on hosted macOS", hosted)
        self.assertNotIn("Configure local sccache", hosted)

    def assert_hosted_sccache_reset(self, run: str) -> None:
        with tempfile.TemporaryDirectory() as temp:
            github_env = Path(temp) / "github-env"
            github_env.touch()
            result = subprocess.run(
                ["bash", "--noprofile", "--norc", "-e", "-o", "pipefail", "-c", run],
                cwd=temp,
                env={
                    "PATH": os.environ["PATH"],
                    "GITHUB_ENV": str(github_env),
                    "RUSTC_WRAPPER": "sccache",
                    "SCCACHE_GHA_ENABLED": "true",
                },
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            entries = dict(line.split("=", 1) for line in github_env.read_text().splitlines())
        self.assertEqual(entries.get("RUSTC_WRAPPER"), "")
        self.assertEqual(entries.get("SCCACHE_GHA_ENABLED"), "")

    def test_trusted_macos_hosted_sccache_reset_writes_both_empty_values(self) -> None:
        workflow = yaml.safe_load(MACOS_TRUSTED_WORKFLOW.read_text(encoding="utf-8"))
        disable = next(
            step for step in workflow["jobs"]["macos_hosted"]["steps"]
            if step.get("name") == "Disable sccache on hosted macOS"
        )
        self.assertEqual(disable["shell"], "bash")
        self.assertNotIn("if", disable)
        self.assert_hosted_sccache_reset(disable["run"])
        for variable in ("RUSTC_WRAPPER", "SCCACHE_GHA_ENABLED"):
            with self.subTest(deleted=variable):
                line = f'  echo "{variable}="\n'
                self.assertIn(line, disable["run"])
                with self.assertRaises(AssertionError):
                    self.assert_hosted_sccache_reset(disable["run"].replace(line, "", 1))

    def test_test_lane_baseline_uses_candidate_snapshot_refs(self) -> None:
        pr_workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        main_workflow = MAIN_WORKFLOW.read_text(encoding="utf-8")
        pr_job = job_block(pr_workflow, "scripts")
        main_job = job_block(main_workflow, "scripts")

        for job in (pr_job, main_job):
            self.assertIn("fetch-depth: 0", job)
            self.assertNotIn("origin/main", job)
        self.assertNotIn("workflow_dispatch:", pr_workflow)
        self.assertRegex(
            pr_job, r"(?m)^          TEST_LANE_BASELINE_REF: HEAD\^1$"
        )
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
        judge = yaml.safe_load(pr_workflow)["jobs"]["required_contexts"]
        self.assertEqual(judge["strategy"]["matrix"]["context"].count("Script checks"), 1)
        self.assertNotRegex(pr_workflow, r"(?m)^    name: Script checks$")
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

    def _repin_job_hash(
        self, hardening: str, workflow: str, job_id: str
    ) -> str:
        ruby = r"""
require "yaml"; require "json"; require "digest"
def canonical_yaml(v)
  case v
  when Hash then v.keys.sort_by(&:to_s).each_with_object({}) { |k, h| h[k.to_s] = canonical_yaml(v[k]) unless k.to_s == "continue-on-error" && (v[k].nil? || v[k] == false) }
  when Array then v.map { |x| canonical_yaml(x) }
  else v
  end
end
workflow, job_id = ARGV
canonical = canonical_yaml(YAML.load_file(workflow).fetch("jobs").fetch(job_id))
puts Digest::SHA256.hexdigest(JSON.generate(canonical))
"""
        with tempfile.TemporaryDirectory() as temp:
            workflow_path = Path(temp) / "ci-pr.yml"
            workflow_path.write_text(workflow, encoding="utf-8")
            digest = subprocess.run(
                ["ruby", "-e", ruby, str(workflow_path), job_id],
                text=True,
                capture_output=True,
                check=False,
            )
        self.assertEqual(digest.returncode, 0, digest.stderr)
        job_match = re.search(
            rf'("{re.escape(job_id)}" => \{{.*?"job_sha256" => ")'
            r"[0-9a-f]{64}",
            hardening,
            re.DOTALL,
        )
        self.assertIsNotNone(job_match)
        assert job_match is not None
        return (
            hardening[: job_match.start()]
            + job_match.group(1)
            + digest.stdout.strip()
            + hardening[job_match.end() :]
        )

    def test_nested_libtest_checker_is_required_and_fails_closed(self) -> None:
        hardening = (REPO_ROOT / "scripts/check-ci-runner-hardening.sh").read_text()
        pr = PR_WORKFLOW.read_text().replace(CI_RUNNER_HARDENING_SHA256, hashlib.sha256(hardening.encode()).hexdigest())
        baseline = self.run_hardening_fixture(pr)
        self.assertEqual(baseline.returncode, 0, baseline.stderr)
        for label, checker in (("missing", None), ("invalid declarations", "raise SystemExit(79)\n")):
            with self.subTest(checker=label):
                rejected = self.run_hardening_fixture(pr, nested_checker=checker)
                self.assertNotEqual(rejected.returncode, 0, "nested declaration checker must run and fail closed")
                self.assertIn("nested libtest declarations must pass validation", rejected.stderr)

    def run_hardening_fixture(
        self,
        pr_workflow: str,
        extra_workflows: dict[str, str] | None = None,
        workflow_symlinks: dict[str, str] | None = None,
        hardening_script: str | None = None,
        mirror_helper: str | None = None,
        nested_checker: str | None = NESTED_CHECKER_STUB,
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
            (workflows / "ci-main.yml").write_text(MAIN_WORKFLOW.read_text(encoding="utf-8"), encoding="utf-8")
            (workflows / "ci-nightly.yml").write_text(NIGHTLY_WORKFLOW.read_text(encoding="utf-8"), encoding="utf-8")
            for name, content in (extra_workflows or {}).items():
                (workflows / name).write_text(content, encoding="utf-8")
            for name, target in (workflow_symlinks or {}).items():
                (workflows / name).symlink_to(target)
            script = hardening_script or (
                REPO_ROOT / "scripts/check-ci-runner-hardening.sh"
            ).read_text(encoding="utf-8")
            (root / "scripts/check-ci-runner-hardening.sh").write_text(
                script, encoding="utf-8"
            )
            helper = mirror_helper or (
                REPO_ROOT / "scripts/required-check-mirror.sh"
            ).read_text(encoding="utf-8")
            (root / "scripts/required-check-mirror.sh").write_text(
                helper, encoding="utf-8"
            )
            (root / "scripts/ci").mkdir()
            judge = REPO_ROOT / "scripts/ci/nextest_required.py"
            (root / "scripts/ci/nextest_required.py").write_bytes(judge.read_bytes())
            # Workflow fixtures isolate wiring; the real checker has its own declaration tests.
            if nested_checker is not None:
                (root / "scripts/ci/nested_libtest.py").write_text(nested_checker, encoding="utf-8")
            return subprocess.run(
                ["bash", "scripts/check-ci-runner-hardening.sh"],
                cwd=root,
                text=True,
                capture_output=True,
                check=False,
            )

    def test_hardening_rejects_routing_outside_repository_hosted_runner_policy(self) -> None:
        pr_workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        trusted = MACOS_TRUSTED_WORKFLOW.read_text(encoding="utf-8")
        variants = {
            "scalar": "    runs-on: self-hosted\n",
            "list": "    runs-on: [self-hosted, macOS]\n",
            "block-list": "    runs-on:\n      - self-hosted\n      - macOS\n",
            "group-labels": "    runs-on: {group: macs, labels: self-hosted}\n",
            "escaped": '    "runs-on": "self-\\u0068osted"\n',
            "case": "    runs-on: SELF-HOSTED\n",
            "matrix": (
                "    runs-on: ${{ matrix.os }}\n"
                "    strategy:\n      matrix:\n        os: [self-hosted]\n"
            ),
            "variable": "    runs-on: ${{ vars.MACOS_RUNNER }}\n",
            "group-variable": (
                "    runs-on: {group: '${{ vars.MACOS_RUNNER_GROUP }}', labels: macOS}\n"
            ),
            "format": "    runs-on: ${{ format('self-{0}', 'hosted') }}\n",
            "custom-label": "    runs-on: agentdesk-macos\n",
            "custom-list": "    runs-on: [macOS, ARM64]\n",
            "mixed-hosted-custom-list": "    runs-on: [ubuntu-latest, agentdesk-macos]\n",
            "multiple-hosted-labels": "    runs-on: [ubuntu-latest, macos-15]\n",
            "multiple-hosted-labels-mapping": "    runs-on: {labels: [ubuntu-latest, macos-15]}\n",
            "duplicate-hosted-labels": "    runs-on: [ubuntu-latest, ubuntu-latest]\n",
            "duplicate-hosted-labels-mapping": "    runs-on: {labels: [ubuntu-latest, ubuntu-latest]}\n",
            "nested-label-mapping": "    runs-on: {labels: {labels: macos-15}}\n",
            "unknown-variable": "    runs-on: ${{ vars.CI_RUNNER }}\n",
            "group-hosted-label": "    runs-on: {group: macs, labels: macos-15}\n",
            "literal-expression": "    runs-on: ${{ 'ubuntu-latest' }}\n",
            "matrix-expression": "    runs-on: ${{ matrix.os || 'ubuntu-latest' }}\n",
            "missing-matrix": "    runs-on: ${{ matrix.os }}\n",
            "dynamic-matrix": (
                "    runs-on: ${{ matrix.os }}\n"
                "    strategy:\n      matrix: ${{ fromJSON(vars.MATRIX) }}\n"
            ),
            "dynamic-axis": (
                "    runs-on: ${{ matrix.os }}\n"
                "    strategy:\n      matrix:\n        os: ${{ fromJSON(vars.RUNNERS) }}\n"
            ),
            "matrix-custom-candidate": (
                "    runs-on: ${{ matrix.os }}\n"
                "    strategy:\n      matrix:\n        os: [ubuntu-latest, agentdesk-macos]\n"
            ),
            "matrix-variable-candidate": (
                "    runs-on: ${{ matrix.os }}\n"
                "    strategy:\n      matrix:\n        os: ['${{ vars.CI_RUNNER }}']\n"
            ),
            "matrix-list-candidate": (
                "    runs-on: ${{ matrix.os }}\n"
                "    strategy:\n      matrix:\n        os: [[ubuntu-latest]]\n"
            ),
            "matrix-mapping-candidate": (
                "    runs-on: ${{ matrix.os }}\n"
                "    strategy:\n      matrix:\n        os: [{labels: ubuntu-latest}]\n"
            ),
            "matrix-include-list-candidate": (
                "    runs-on: ${{ matrix.os }}\n"
                "    strategy:\n      matrix:\n        include: [{os: [ubuntu-latest]}]\n"
            ),
            "matrix-include-mapping-candidate": (
                "    runs-on: ${{ matrix.os }}\n"
                "    strategy:\n      matrix:\n        include: [{os: {labels: ubuntu-latest}}]\n"
            ),
            "matrix-include-custom": (
                "    runs-on: ${{ matrix.os }}\n"
                "    strategy:\n      matrix:\n        os: [ubuntu-latest]\n"
                "        include: [{os: agentdesk-macos}]\n"
            ),
            "matrix-dynamic-include": (
                "    runs-on: ${{ matrix.os }}\n"
                "    strategy:\n      matrix:\n        os: [ubuntu-latest]\n"
                "        include: ${{ fromJSON(vars.EXTRA) }}\n"
            ),
            "matrix-policy-include-missing-runner": (
                "    runs-on: ${{ matrix.runner }}\n"
                "    strategy:\n      matrix:\n"
                "        include: [{runner: macos-15}, {target: custom}]\n"
            ),
            "matrix-base-missing-runner": (
                "    runs-on: ${{ matrix.runner }}\n"
                "    strategy:\n      matrix:\n        target: [linux]\n"
                "        include: [{runner: macos-15}]\n"
            ),
            "matrix-policy-include-inherited-runner": (
                "    runs-on: ${{ matrix.os }}\n"
                "    strategy:\n      matrix:\n        os: [ubuntu-latest]\n"
                "        include: [{feature: extra}]\n"
            ),
            "matrix-policy-excluded-custom": (
                "    runs-on: ${{ matrix.os }}\n"
                "    strategy:\n      matrix:\n        os: [ubuntu-latest, agentdesk-macos]\n"
                "        exclude: [{os: agentdesk-macos}]\n"
            ),
            "empty-runner-list": "    runs-on: []\n",
        }
        for name, runner in variants.items():
            for path in ("ci-macos-trusted.yml", "extra.yaml"):
                with self.subTest(runner=name, workflow=path):
                    mutated = trusted.replace("    runs-on: macos-15\n", runner, 1)
                    self.assertNotEqual(mutated, trusted)
                    result = self.run_hardening_fixture(
                        pr_workflow, extra_workflows={path: mutated}
                    )
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("hosted runner policy", result.stderr)
                    if name.startswith("matrix-policy-"):
                        self.assertIn("explicitly enumerated static matrix runner axis", result.stderr)
                        self.assertIn("every include row must specify that axis", result.stderr)
                        self.assertIn("exclude cannot approve forbidden candidates", result.stderr)

    def test_hardening_accepts_hosted_labels_and_repository_policy_static_matrices(self) -> None:
        variants = {
            label: f"    runs-on: {label}\n"
            for label in ("ubuntu-latest", "ubuntu-22.04", "macos-15", "macos-latest", "windows-latest")
        }
        variants.update({
            "label-list": "    runs-on: [ubuntu-latest]\n",
            "label-mapping": "    runs-on: {labels: macos-15}\n",
            "label-mapping-singleton-list": "    runs-on: {labels: [ubuntu-latest]}\n",
            "matrix-axis": (
                "    runs-on: ${{ matrix.os }}\n"
                "    strategy:\n      matrix:\n        os: [ubuntu-latest, windows-latest]\n"
            ),
            "matrix-include-only": (
                "    runs-on: ${{ matrix.runner }}\n"
                "    strategy:\n      matrix:\n"
                "        include: [{runner: macos-15}, {runner: ubuntu-22.04}]\n"
            ),
            "matrix-axis-include-exclude": (
                "    runs-on: ${{ matrix.os }}\n"
                "    strategy:\n      matrix:\n        os: [ubuntu-latest, macos-latest]\n"
                "        include: [{os: windows-latest}]\n"
                "        exclude: [{os: macos-latest}]\n"
            ),
        })
        for name, runner in variants.items():
            with self.subTest(runner=name):
                workflow = "name: Runner probe\non: push\njobs:\n  probe:\n" + runner
                workflow += "    steps:\n      - run: echo ok\n"
                result = self.run_hardening_fixture(
                    PR_WORKFLOW.read_text(encoding="utf-8"), {"runner-probe.yaml": workflow}
                )
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_hardening_keeps_trusted_macos_scheduled_and_uncancelled(self) -> None:
        pr_workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        trusted = MACOS_TRUSTED_WORKFLOW.read_text(encoding="utf-8")
        mutations = {
            "push": ("  workflow_dispatch:\n", "  push:\n    branches: [main]\n  workflow_dispatch:\n"),
            "no-cron": ("    - cron: ", "    - not-cron: "),
            "cancel": ("  cancel-in-progress: false\n", "  cancel-in-progress: true\n"),
            "shared-group": ("${{ github.event_name }}-", ""),
        }
        for name, (old, new) in mutations.items():
            with self.subTest(mutation=name):
                mutated = trusted.replace(old, new, 1)
                self.assertNotEqual(mutated, trusted)
                result = self.run_hardening_fixture(
                    pr_workflow, extra_workflows={"ci-macos-trusted.yml": mutated}
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("no run may cancel another", result.stderr)

    def test_hardening_distinguishes_retired_variable_reads_from_prose(self) -> None:
        variants = {
            "step-name": ("name", "Verify self-hosted routing is absent", True),
            "defensive-if": ("if", "runner.environment != 'self-hosted'", True),
            "defensive-expression": ("if", "${{ runner.environment != 'self-hosted' }}", True),
            "variable-prose": ("name", "Explain vars.MACOS_RUNNER retirement", True),
            "quoted-literal": ("name", "${{ 'vars.MACOS_RUNNER' }}", True),
            "quoted-if-literal": ("if", "contains('vars.MACOS_RUNNER', 'MACOS_RUNNER')", True),
            "env-if-prose": ("env", {"if": "vars.MACOS_RUNNER"}, True),
            "different-variable": ("env", {"RUNNER": "${{ vars.MACOS_RUNNER_GROUP }}"}, True),
            "actual-dot-read": ("env", {"RUNNER": "${{ vars.MACOS_RUNNER }}"}, False),
            "actual-bracket-read": ("env", {"RUNNER": "${{ vars['MACOS_RUNNER'] }}"}, False),
            "actual-case-read": ("env", {"RUNNER": "${{ VARS.macos_runner }}"}, False),
            "actual-implicit-if": ("if", "vars.MACOS_RUNNER != ''", False),
            "quoted-delimiter": ("name", "${{ format('}}', vars.MACOS_RUNNER) }}", False),
        }
        for name, (key, value, accepted) in variants.items():
            with self.subTest(case=name):
                workflow = {"name": "Runner probe", "on": "push", "jobs": {"probe": {
                    "runs-on": "ubuntu-latest", "steps": [{"run": "echo ok", key: value}],
                }}}
                result = self.run_hardening_fixture(
                    PR_WORKFLOW.read_text(encoding="utf-8"),
                    {"runner-probe.yaml": yaml.safe_dump(workflow)},
                )
                self.assertEqual(result.returncode, 0 if accepted else 1, result.stderr)
                if not accepted:
                    self.assertIn("forbids vars.MACOS_RUNNER references", result.stderr)

    def test_hardening_accepts_every_repository_workflow(self) -> None:
        result = self.run_hardening_fixture(
            PR_WORKFLOW.read_text(encoding="utf-8"),
            {path.name: path.read_text(encoding="utf-8") for path in workflow_paths()},
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_main_windows_warm_times_out_steps_before_advisory_job(self) -> None:
        job = yaml.safe_load(MAIN_WORKFLOW.read_text(encoding="utf-8"))["jobs"]["windows_cache_warm"]
        self.assertIs(job["continue-on-error"], True)
        compile_steps = [step for step in job["steps"] if "cargo" in step.get("run", "")]
        self.assertEqual(len(compile_steps), 2)
        for step in compile_steps:
            timeout = step.get("timeout-minutes")
            self.assertIs(type(timeout), int, f"warm {step['name']} needs a native step deadline")
            self.assertGreater(timeout, 0)
        budget = sum(step["timeout-minutes"] for step in compile_steps)
        self.assertGreaterEqual(job["timeout-minutes"], budget + 30)

    def test_hardening_rejects_main_windows_warm_that_misses_pr_cache_key(self) -> None:
        source = MAIN_WORKFLOW.read_text(encoding="utf-8")
        warm = source.index("\n  # Warms the shared cargo registry cache")
        variants = {
            "missing": (source[:warm] + "\n", "windows_cache_warm is missing"),
            "save-if": (
                source[:warm] + source[warm:].replace(
                    "${{ github.ref == 'refs/heads/main' }}",
                    "${{ github.event_name == 'pull_request' }}",
                ),
                "rust-cache must save the shared key from main only",
            ),
            "env drift": (
                source[:warm] + source[warm:].replace(
                    '      CARGO_PROFILE_TEST_DEBUG: "0"\n    steps:', "    steps:", 1
                ),
                "env must equal check_fast_cross_os",
            ),
            "runs tests": (
                source.replace("cargo test --lib --no-run", "cargo test --lib"),
                "without running tests",
            ),
        }
        for name, (mutated, reason) in variants.items():
            with self.subTest(name):
                self.assertNotEqual(mutated, source)
                result = self.run_hardening_fixture(
                    PR_WORKFLOW.read_text(encoding="utf-8"), {"ci-main.yml": mutated}
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(reason, result.stderr)

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

    def test_hardening_rejects_non_string_yaml_job_keys(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        collision = (
            "  yes:\n"
            "    name: Script checks\n"
            "    runs-on: ubuntu-latest\n"
            "    steps:\n"
            "      - run: true\n\n"
            "  on:\n"
            "    name: harmless schema-collision decoy\n"
            "    runs-on: ubuntu-latest\n"
            "    steps:\n"
            "      - run: true\n\n"
        )
        mutated = workflow.replace(
            "  required_contexts:\n",
            collision + "  required_contexts:\n",
            1,
        )
        self.assertNotEqual(mutated, workflow)
        result = self.run_hardening_fixture(mutated)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("job IDs must be strings", result.stderr)
        self.assertIn("non-string YAML job keys", result.stderr)

    def test_hardening_rejects_yaml_11_booleanish_plain_job_keys(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        for key in ("yes", "no", "on", "off", "true", "false", "y", "n"):
            with self.subTest(key=key):
                probe = (
                    f"  {key}:\n"
                    "    name: harmless key probe\n"
                    "    runs-on: ubuntu-latest\n"
                    "    steps:\n"
                    "      - run: true\n\n"
                )
                mutated = workflow.replace(
                    "  required_contexts:\n",
                    probe + "  required_contexts:\n",
                    1,
                )
                result = self.run_hardening_fixture(mutated)
                self.assertNotEqual(result.returncode, 0)
                self.assertRegex(
                    result.stderr,
                    r"(?:non-string YAML job keys|ambiguous YAML plain job keys)",
                )
        quoted = workflow.replace(
            "  required_contexts:\n",
            '  "yes":\n    name: quoted-key probe\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n\n  required_contexts:\n',
            1,
        )
        self.assertEqual(self.run_hardening_fixture(quoted).returncode, 0)

    def test_fixed_surfaces_use_raw_github_scalar_values(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        judge = job_block(workflow, "required_contexts")
        mutations = job_block(workflow, "relay_authority_mutations")
        cases = (
            ("plain no is not the false boolean", judge, "      fail-fast: false\n", "      fail-fast: no\n"),
            ("quoted context changes the pinned scalar style", judge, "          - Lint\n", '          - "Lint"\n'),
            ("explicit str tag is not discarded", judge, "          BASH_ENV: /dev/null\n", "          BASH_ENV: !!str /dev/null\n"),
            ("judge timeout leading zero is not decimal 10", judge, "    timeout-minutes: 10\n", "    timeout-minutes: 010\n"),
            ("job timeout leading zero is not decimal 45", mutations, "    timeout-minutes: 45\n", "    timeout-minutes: 055\n"),
        )
        for label, job, old, new in cases:
            with self.subTest(case=label):
                mutated = workflow.replace(job, job.replace(old, new, 1), 1)
                self.assertNotEqual(mutated, workflow)
                result = self.run_hardening_fixture(mutated)
                self.assertEqual(result.returncode, 1, result.stderr)

    def test_hardening_rejects_workflow_symlink(self) -> None:
        result = self.run_hardening_fixture(
            PR_WORKFLOW.read_text(encoding="utf-8"),
            workflow_symlinks={"linked.yaml": "ci-pr.yml"},
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("linked.yaml must not be a symlink", result.stderr)

    def test_nightly_notification_suite_is_executable_and_failure_is_fatal(self) -> None:
        aggregate = (REPO_ROOT / "scripts/ci-script-checks.sh").read_text()
        start = aggregate.index("# Nightly notification contract (#6006).")
        end = aggregate.index("# End nightly notification contract.", start)
        block = aggregate[start:end]
        with tempfile.TemporaryDirectory() as tmp:
            probe = Path(tmp) / "python-probe"
            journal = Path(tmp) / "argv"
            probe.write_text('#!/bin/bash\nprintf "%s\\n" "$@" > "$JOURNAL"\nexit "$PROBE_RC"\n')
            probe.chmod(0o755)
            for rc in (0, 17):
                result = subprocess.run(["bash", "-c", "set -euo pipefail\n" + block],
                    env={"PATH": os.environ["PATH"], "PYTHON": str(probe),
                         "JOURNAL": str(journal), "PROBE_RC": str(rc)},
                    capture_output=True, text=True)
                self.assertEqual(result.returncode, rc, result.stderr)
                self.assertEqual(journal.read_text().splitlines(),
                                 ["-m", "unittest", "tests.test_nightly_ci_triage"])

    def test_rust_test_source_checks_run_fatally_in_the_contracts_shard(self) -> None:
        title = "Rust test source wiring and PG classification"
        script = REPO_ROOT / "scripts/ci-script-checks.sh"
        env = {k: v for k, v in os.environ.items() if not k.startswith("SCRIPT_CHECK_")}
        listing = subprocess.run(
            ["bash", str(script)], cwd=REPO_ROOT, capture_output=True, text=True,
            env={**env, "SCRIPT_CHECK_LIST": "1", "SCRIPT_CHECK_SHARD": "contracts"},
        )
        self.assertIn(f"contracts\t{title}", listing.stdout.splitlines(), listing.stderr)
        lines = script.read_text(encoding="utf-8").splitlines()
        start = lines.index(f'if run_check contracts "{title}"; then') + 1
        after = next(i for i in range(start, len(lines)) if lines[i].startswith("if run_check "))
        block = "\n".join(lines[start:max(i for i in range(start, after) if lines[i] == "fi")])
        with tempfile.TemporaryDirectory() as tmp:
            probe, journal = Path(tmp) / "python-probe", Path(tmp) / "argv"
            probe.write_text('#!/bin/bash\nprintf "%s\\n" "$1" >> "$JOURNAL"\n[ "$1" != "$FAIL_ON" ]\n')
            probe.chmod(0o755)

            def run(fail_on: str = "") -> tuple[int, list[str]]:
                journal.write_text("")
                done = subprocess.run(
                    ["bash", "--noprofile", "--norc", "-c", "set -euo pipefail\n" + block],
                    env={"PATH": os.environ["PATH"], "PYTHON": str(probe), "JOURNAL": str(journal), "FAIL_ON": fail_on},
                    capture_output=True, text=True,
                )
                return done.returncode, journal.read_text().splitlines()

            calls = ["scripts/ci/rust_test_sources.py", "scripts/ci/pg_test_classifier.py"]
            self.assertEqual(run(), (0, calls))
            for index, call in enumerate(calls):
                with self.subTest(failing=call):
                    self.assertEqual(run(call), (1, calls[:index + 1]))


if __name__ == "__main__":
    unittest.main()
