"""Static contracts for the PR fast-compile and retained test lanes."""

from __future__ import annotations

import hashlib
import os
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path, PurePosixPath

import yaml


REPO_ROOT = Path(__file__).resolve().parents[1]
REQUIRED_CHECK_MIRROR_SHA256 = (
    "57c78a2ea1d5587ff1c74d5d25e2e32d25814198c5ee966e2297845c6230a30d"
)
CI_RUNNER_HARDENING_SHA256 = (
    "b47025553a859c1e369ab5f7c4b0853819a4b370738d4f68887e099d873ac7d6"
)
PR_WORKFLOW = REPO_ROOT / ".github/workflows/ci-pr.yml"
# Path-filtered required contexts: (mirror job, required name, runner job,
# runner name, runner `if`, FILTER_NAME, FILTER_OUTPUT).
_LINT_FILTER = (
    "needs.changes.outputs.rust_or_policy == 'true' || "
    "needs.changes.outputs.relay_contract == 'true'"
)
PATH_FILTER_REQUIRED_MIRRORS = (
    (
        "lint_required_context",
        "Lint",
        "lint",
        "Lint runner",
        _LINT_FILTER,
        "rust_or_policy_or_relay_contract",
        "${{ " + _LINT_FILTER + " }}",
    ),
    (
        "high_risk_recovery_required_context",
        "High-risk recovery",
        "high-risk-recovery",
        "High-risk recovery runner",
        "needs.changes.outputs.high_risk_recovery == 'true'",
        "high_risk_recovery",
        "${{ needs.changes.outputs.high_risk_recovery }}",
    ),
    (
        "dashboard_required_context",
        "Dashboard (Node 22)",
        "dashboard",
        "Dashboard (Node 22) runner",
        "needs.changes.outputs.dashboard == 'true'",
        "dashboard",
        "${{ needs.changes.outputs.dashboard }}",
    ),
)
PATH_FILTER_MIRROR_PIN_STEP = "Pin required-check mirror helper (#5321)"
FILTER_BLOCK_HEADER = re.compile(r"^            \w+:$", re.M)
CROSS_OS_CONSUMER_SCRIPT = REPO_ROOT / "scripts/cross_os_consumer_paths.py"
# #5828's own break (turn_bridge/mod.rs) plus the 22 files measured on PR #5834
# that carry the same shim and were left unselected by the hand-written list.
# Every one is compiled on Windows and reaches a `#[cfg(unix)]`-gated module, so
# dropping or mis-cfg-ing its shim reproduces #5828 on main.
CFG_SHIM_CONSUMERS = (
    "src/services/discord/turn_bridge/mod.rs",
    "src/services/discord/health/watcher_respawn.rs",
    "src/services/discord/outbound/delivery_record.rs",
    "src/services/discord/recovery_engine/restore_inflight.rs",
    "src/services/discord/recovery_engine/completion_delivery.rs",
    "src/services/discord/recovery_engine/unix_journal.rs",
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
    # r3: reached only once the walk resolves a `#[path]` inside an inline
    # `mod tests {` against the module's directory, per rustc directory
    # ownership. Windows compiles it and it carries a cfg(unix)/not(unix) pair.
    "src/services/discord/voice_barge_in/tests/pcm_harness_tests.rs",
)
# Files under the derived scope that the module walk cannot reach, each paired
# with the walked file that `include!`s it. `include!` is text substitution, not
# a module declaration, so the compiled unit is the including file -- which the
# walk does reach and the globs do select. An entry that is unreached for any
# other reason is a #5828-class blind spot the derivation would silently drop,
# so this table is exhaustive and the test below fails when it grows.
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

# This manifest is intentionally exact: changing the retained test recipe must also
# update this test deliberately. The duplication is a drift-prevention gate, not an
# attempt to derive the expected coverage from the justfile under test.
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
    # #5071 T4-B5 (4987 S6): the watchdog sidecar intake lane.
    "cargo test --lib services::discord::health::reachability::external_verdict::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::discord::health::reachability::obligation::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::discord::health::reachability::divergence::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib services::discord::e2e_control::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib server::routes::e2e_control::tests -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib formatting -- --skip _pg --skip pg_ --skip postgres",
    "cargo test --lib delivery_record -- --skip _pg --skip pg_ --skip postgres",
    # #5071 T4-B3 (4987 S2): the receipt projection index lane (union coverage
    # and the frontier operand); the `delivery_record` filter above does not
    # reach this module.
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


# (filter outcome, filter `run` output, whether gated steps run)
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


def derived_cross_os_consumers() -> tuple[str, ...]:
    completed = subprocess.run(
        [sys.executable, str(CROSS_OS_CONSUMER_SCRIPT), "--format", "paths"],
        capture_output=True,
        text=True,
        check=True,
        cwd=REPO_ROOT,
    )
    return tuple(completed.stdout.split())


def unreachable_rust_files() -> tuple[str, ...]:
    completed = subprocess.run(
        [sys.executable, str(CROSS_OS_CONSUMER_SCRIPT), "--format", "unreachable"],
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
        # Both runners share the setup prefix that ends at the dependency cache.
        setup = [step.get("name", step.get("uses")) for step in check["steps"]]
        setup = setup[: setup.index("Cache Cargo dependencies") + 1]
        self.assertEqual(check["steps"][: len(setup)], targets["steps"][: len(setup)])
        self.assertLess(setup.index(python_step_name), setup.index("Install Rust toolchain"))
        python_step = check["steps"][setup.index(python_step_name)]
        self.assertEqual(python_step["uses"], "actions/setup-python@v5")
        self.assertEqual(python_step["with"]["python-version"], "3.11")

        # The two runners together execute exactly the pre-split command set.
        runs = [
            (job_id, step["run"])
            for job_id in ("check_fast_cross_os", "check_fast_cross_os_targets")
            for step in jobs[job_id]["steps"][len(setup) :]
            if step.get("name") != "sccache stats"
        ]
        self.assertEqual(
            runs,
            [
                ("check_fast_cross_os", "cargo check --workspace --all-targets"),
                ("check_fast_cross_os_targets", "./scripts/ci/run-writer-namespace-windows-targets.sh"),
            ],
        )
        writer = targets["steps"][len(setup)]
        self.assertEqual(writer["name"], "Writer namespace exact Windows targets")
        self.assertEqual(writer["if"], "runner.os == 'Windows'")
        self.assertEqual(writer["timeout-minutes"], 30)
        self.assertEqual(writer["shell"], "bash")
        for job_id in ("check_fast_cross_os", "check_fast_cross_os_targets"):
            # A job-level continue-on-error reports failure to the mirror as success.
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
        self.assertEqual(filters["cross_os_rust"].count("src/services/writer_protocol/**"), 1)
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
        # The narrow positive list is the design; glob_matcher has no negation.
        self.assertNotIn("src/services/discord/**", paths)
        self.assertEqual([pattern for pattern in paths if pattern.startswith("!")], [])

        consumers = derived_cross_os_consumers()
        self.assertGreater(len(consumers), 100)
        self.assertEqual([path for path in consumers if not selects(paths, path)], [])
        for path in CFG_SHIM_CONSUMERS:
            with self.subTest(consumer=path):
                self.assertIn(path, consumers)
                self.assertTrue(selects(paths, path))

        # Every derived selector is load-bearing: commenting one out must leave
        # a consumer unmatched. This also forbids redundant spellings, because a
        # subsumed glob would delete cleanly with nothing uncovered.
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
                # The stated reason, checked rather than asserted in prose.
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
        mirror = jobs["check_fast_cross_os_required_context"]
        owner_paths = (
            "src/services/discord/inflight/store.rs",
            "src/services/discord/inflight/save_store.rs",
        )

        cross_os_paths = paths_filter_definitions(workflow)["cross_os_rust"]
        # #5832 replaced the two literal entries with the derived selector that
        # covers them; pinning the literals again would freeze a redundant glob.
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
        self.assertEqual(
            mirror["needs"],
            ["changes", "check_fast_cross_os", "check_fast_cross_os_targets"],
        )
        self.assertEqual(mirror["if"], "always()")
        mirror_steps = [
            step
            for step in mirror["steps"]
            if step.get("run") == "./scripts/required-check-mirror.sh"
        ]
        self.assertEqual(
            [step["env"] for step in mirror_steps],
            [
                {
                    "BASH_ENV": "/dev/null",
                    "CHANGED_PATHS_RESULT": "${{ needs.changes.result }}",
                    "FILTER_NAME": "cross_os_rust",
                    "FILTER_OUTPUT": "${{ needs.changes.outputs.cross_os_rust }}",
                    "UPSTREAM_JOB_NAME": runner,
                    "UPSTREAM_RESULT": "${{ needs.%s.result }}" % runner,
                }
                for runner in ("check_fast_cross_os", "check_fast_cross_os_targets")
            ],
        )

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
                self.assertIn("run_non_pg_filter_replay", job)
        self.assertIn(
            "cargo test --lib discord_thread_create -- --test-threads=1",
            job_block(nightly, "full_windows"),
        )
        postgres = job_block(nightly, "postgres_full")
        self.assertIn("source scripts/ci/non-pg-test-filter.sh", postgres)
        self.assertIn(
            'cargo test --lib -- "${PG_INCLUDE_ARGS[@]}" '
            "--nocapture --test-threads=1",
            postgres,
        )
        self.assertIn("run: cargo test --test e2e -- --test-threads=1", postgres)

    def test_relay_authority_contract_job_uses_pinned_recipe(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        jobs = yaml.safe_load(workflow)["jobs"]
        job = job_block(workflow, "relay_authority_targets")
        setup = []
        for job_id, cap in (("relay_authority_targets", 30), ("relay_authority_mutations", 45)):
            runner = jobs[job_id]
            self.assertNotIn("if", runner)
            self.assertNotIn("needs", runner)
            self.assertEqual(runner["runs-on"], "ubuntu-latest")
            self.assertEqual(runner["timeout-minutes"], cap)
            self.assertEqual(runner["env"]["CARGO_PROFILE_DEV_DEBUG"], "0")
            self.assertEqual(runner["env"]["CARGO_PROFILE_TEST_DEBUG"], "0")
            setup.append([step for step in runner["steps"] if "uses" in step and step.get("id") != "mutation_paths"])
        # The mutation job alone keeps HEAD^1 for its wiring digest; the rest of setup is shared.
        self.assertEqual(setup[1][0], {"uses": "actions/checkout@v4", "with": {"fetch-depth": 2}})
        setup[1][0] = {"uses": "actions/checkout@v4"}
        self.assertEqual(
            setup[0],
            [{key: value for key, value in step.items() if key != "if"} for step in setup[1]],
        )
        self.assertEqual(setup[0][0], {"uses": "actions/checkout@v4"})
        self.assertEqual(setup[0][1]["with"]["toolchain"], "1.94.1")
        mutation = jobs["relay_authority_mutations"]
        self.assertEqual(mutation["strategy"], {"fail-fast": False, "matrix": {"shard": [0, 1, 2]}})
        self.assertEqual(mutation["env"]["RELAY_AUTHORITY_MUTATION_SHARD_INDEX"], "${{ matrix.shard }}")
        self.assertEqual(mutation["env"]["RELAY_AUTHORITY_MUTATION_SHARD_TOTAL"], "3")
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
            r"          env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::turn_bridge::stream_tick::guarded_persist::tests::a_vanished_row_suppresses_without_ending_stream_lifecycle -- --test-threads=1\n"
            r"          env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::turn_bridge::stream_tick::guarded_persist::tests::same_authority_watcher_epoch_advance_keeps_bridge_lifecycle_authority -- --test-threads=1\n"
            r"          env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::turn_bridge::bridge_entry_persist::tests::entry_gate_matrix_over_outcome_and_anchor -- --test-threads=1\n"
            r"          env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::turn_bridge::bridge_entry_persist::tests::an_enforced_rowless_turn_without_an_anchor_sends_no_placeholder -- --test-threads=1\n"
            r"          env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::turn_bridge::bridge_entry_persist::tests::a_rowless_entry_patch_keeps_its_pre_persist_detached_locals -- --test-threads=1\n"
            r"          env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::tmux::tmux_watcher::terminal_relay_plan::soft_terminal_direct_send_authority_tests -- --test-threads=1\n"
            r"          env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::tmux::tmux_watcher::streaming_status_tick::committed_progress_tests::native_collector_tests::recovered_native_preview_terminal -- --test-threads=1\n"
            r"          env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::tui_prompt_relay::local_model_queue_wake_e2e -- --test-threads=1$",
        )
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
        self.assertNotIn("if", job)
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

    def test_required_relay_job_backstops_mirror_content_hash(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        self.assertEqual(
            hashlib.sha256(
                (REPO_ROOT / "scripts/check-ci-runner-hardening.sh").read_bytes()
            ).hexdigest(),
            CI_RUNNER_HARDENING_SHA256,
        )
        relay_job = yaml.safe_load(workflow)["jobs"]["relay-authority-contract"]
        self.assertEqual(relay_job["if"], "always()")
        self.assertEqual(relay_job["needs"], ["relay_authority_targets", "relay_authority_mutations"])
        pin_steps = {
            step["name"]: step
            for step in relay_job["steps"]
            if isinstance(step, dict) and step.get("name", "").endswith("(#5321)")
        }
        self.assertEqual(set(pin_steps), {"Pin required-check mirror content (#5321)"})
        pin = pin_steps["Pin required-check mirror content (#5321)"]
        self.assertEqual(pin["shell"], "bash")
        self.assertEqual(pin["timeout-minutes"], 10)
        self.assertEqual(pin["env"]["BASH_ENV"], "/dev/null")
        self.assertIn('sha256sum "$helper_path"', pin["run"])
        self.assertIn(REQUIRED_CHECK_MIRROR_SHA256, pin["run"])
        self.assertIn('sha256sum "$gate_path"', pin["run"])
        self.assertIn(CI_RUNNER_HARDENING_SHA256, pin["run"])
        self.assertTrue(pin["run"].endswith("scripts/check-ci-runner-hardening.sh\n"))

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
        self.assertTrue(any(step.get("name") == "Upload giant-file progress evidence" for step in main_steps))
        for key in ("GFP_BASE_SHA", "GFP_HEAD_REPOSITORY"):
            mutated = workflow.replace(f"          {key}: {expected_pr_env[key]}\n", "", 1)
            rejected = self.run_hardening_fixture(mutated)
            self.assertNotEqual(rejected.returncode, 0, key)

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
                "  scripts:\n    name: Script checks runner\n    needs: changes\n    runs-on: ubuntu-latest\n",
                "  scripts:\n    name: Script checks runner\n    needs: changes\n    runs-on: macos-latest\n",
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
                    "protected step inventory changed; expected indices [8, 9]",
                    result.stderr,
                )

    def test_script_checks_required_context_mirror_is_pinned_fail_closed(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        mirror = job_block(workflow, "scripts_required_context")
        job = yaml.safe_load(workflow)["jobs"]["scripts_required_context"]
        self.assertEqual(job["name"], "Script checks")
        self.assertEqual(
            job["needs"], ["changes", "scripts", "scripts_guards", "scripts_contracts"]
        )
        self.assertEqual(job["if"], "always()")
        self.assertNotIn("continue-on-error", job)
        self.assertEqual(job["runs-on"], "ubuntu-latest")
        self.assertEqual(len(job["steps"]), 5)
        self.assertEqual(job["steps"][0], {"uses": "actions/checkout@v4"})

        contract, result = job["steps"][1:3]
        self.assertEqual(contract["name"], "Verify Script checks mirror contract (#5321)")
        self.assertEqual(contract["env"], {"BASH_ENV": "/dev/null"})
        self.assertEqual(contract["shell"], "bash")
        self.assertEqual(contract["timeout-minutes"], 10)
        self.assertIn(f"expected={REQUIRED_CHECK_MIRROR_SHA256}", contract["run"])
        self.assertIn(f"expected={CI_RUNNER_HARDENING_SHA256}", contract["run"])
        self.assertIn("scripts/check-ci-runner-hardening.sh", contract["run"])
        self.assertEqual(result["name"], "Mirror script checks result for branch protection")
        self.assertEqual(result["run"], "./scripts/required-check-mirror.sh")
        self.assertEqual(result["env"]["UPSTREAM_JOB_NAME"], "scripts")
        self.assertEqual(result["env"]["UPSTREAM_RESULT"], "${{ needs.scripts.result }}")

        mutations = {
            "job deleted": "",
            "changes dependency deleted": mirror.replace("      - changes\n", "", 1),
            "job if weakened": mirror.replace(
                "    if: always()\n",
                "    if: ${{ github.event_name == 'push' }}\n",
                1,
            ),
            "job continue-on-error injected": mirror.replace(
                "    runs-on: ubuntu-latest\n",
                "    continue-on-error: true\n    runs-on: ubuntu-latest\n",
                1,
            ),
            "checkout provenance": mirror.replace(
                "      - uses: actions/checkout@v4\n",
                "      - uses: actions/checkout@v4\n"
                "        with:\n"
                "          repository: attacker/green-mirror\n",
                1,
            ),
            "extra step": mirror.replace(
                "      - uses: actions/checkout@v4\n",
                "      - uses: actions/checkout@v4\n"
                "      - run: printf 'exit 0\\n' > scripts/required-check-mirror.sh\n",
                1,
            ),
            "mirror weakened": mirror.replace(
                "        run: ./scripts/required-check-mirror.sh\n",
                "        run: 'true'\n",
                1,
            ),
        }
        for field, value in {
            "defaults": "defaults:\n      run:\n        shell: bash -n {0}",
            "env": "env:\n      PYTHON: /bin/true",
            "environment": "environment: never-approved",
            "strategy": "strategy:\n      matrix:\n        shard: [only]",
            "container": "container: ubuntu:latest",
        }.items():
            mutations[field] = mirror.replace(
                "    runs-on: ubuntu-latest\n",
                "    runs-on: ubuntu-latest\n"
                + "\n".join(f"    {line}" for line in value.splitlines())
                + "\n",
                1,
            )

        for label, mutated_mirror in mutations.items():
            with self.subTest(mutation=label):
                mutated = workflow.replace(mirror, mutated_mirror, 1)
                result = self.run_hardening_fixture(mutated)
                self.assertNotEqual(result.returncode, 0, result.stderr)

    def test_script_checks_publisher_rejects_missing_always_condition(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        mirror = job_block(workflow, "scripts_required_context")
        mutated_mirror = mirror.replace("    if: always()\n", "", 1)
        self.assertNotEqual(mutated_mirror, mirror)
        mutated = workflow.replace(mirror, mutated_mirror, 1)
        result = self.run_hardening_fixture(mutated)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn(
            "publisher must carry `if: always()` so upstream failure still "
            "runs the fail-closed mirror",
            result.stderr,
        )

    def test_script_checks_publisher_rejects_conditional_if(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        mirror = job_block(workflow, "scripts_required_context")
        mutations = {
            "success": mirror.replace(
                "    if: always()\n", "    if: success()\n", 1
            ),
            "event condition": mirror.replace(
                "    if: always()\n",
                "    if: ${{ github.event_name == 'push' }}\n",
                1,
            ),
        }
        for label, mutated_mirror in mutations.items():
            with self.subTest(condition=label):
                self.assertNotEqual(mutated_mirror, mirror)
                mutated = workflow.replace(mirror, mutated_mirror, 1)
                result = self.run_hardening_fixture(mutated)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn(
                    "publisher must carry `if: always()` so upstream failure "
                    "still runs the fail-closed mirror",
                    result.stderr,
                )

    def test_relay_authority_graph_rejects_fail_open_mutations(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        publisher = job_block(workflow, "relay-authority-contract")
        mutations = [
            (publisher, publisher.replace("    if: always()\n", "", 1), "publisher must carry `if: always()`"),
            (publisher, publisher.replace("    if: always()\n", "    if: success()\n", 1), "publisher must carry `if: always()`"),
            (publisher, publisher.replace("      - relay_authority_mutations\n", "", 1), "required unconditional needs closure changed"),
        ]
        for job_id in ("relay_authority_targets", "relay_authority_mutations"):
            runner = job_block(workflow, job_id)
            mutations.append((runner, runner.replace("    runs-on:", "    needs: changes\n    runs-on:", 1), "needs closure must not include changes"))
            mutations.append((runner, runner.replace("    runs-on:", "    if: always()\n    runs-on:", 1), "must not define an if key"))
        for original, changed, diagnostic in mutations:
            with self.subTest(mutation=changed.splitlines()[:6]):
                self.assertNotEqual(original, changed)
                result = self.run_hardening_fixture(workflow.replace(original, changed, 1))
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn(diagnostic, result.stderr)

    def test_relay_authority_mirror_rejects_unsuccessful_runner_results(self) -> None:
        jobs = yaml.safe_load(PR_WORKFLOW.read_text(encoding="utf-8"))["jobs"]
        mirrors = jobs["relay-authority-contract"]["steps"][2:]
        self.assertEqual(len(mirrors), 2)
        for step in mirrors:
            for result in ("success", "failure", "cancelled", "skipped", ""):
                with self.subTest(step=step["name"], result=result):
                    env = {**os.environ, **step["env"], "UPSTREAM_RESULT": result}
                    process = subprocess.run(["bash", "scripts/required-check-mirror.sh"], cwd=REPO_ROOT, env=env, text=True, capture_output=True, check=False)
                    self.assertEqual(process.returncode == 0, result == "success", process.stderr)

    def run_cross_os_mirror(self, mirror: dict, cross_os_rust: str, results: dict[str, str]) -> bool:
        """Evaluate the mirror steps as Actions would; unlisted needs read as ''."""
        context = {"changes": "success", **results}

        def expand(value: str) -> str:
            def lookup(match: re.Match[str]) -> str:
                job, field = match.groups()
                if job not in mirror["needs"]:
                    return ""
                return cross_os_rust if field == "outputs.cross_os_rust" else context[job]

            return re.sub(r"\$\{\{ needs\.([\w-]+)\.(result|outputs\.\w+) \}\}", lookup, value)

        for step in mirror["steps"]:
            if "run" not in step:
                continue
            env = {**os.environ, **{key: expand(value) for key, value in step["env"].items()}}
            process = subprocess.run(["bash", "-c", step["run"]], cwd=REPO_ROOT, env=env, text=True, capture_output=True, check=False)
            if process.returncode != 0:
                return False
        return True

    def test_cross_os_required_context_fails_closed_on_either_runner(self) -> None:
        mirror = yaml.safe_load(PR_WORKFLOW.read_text(encoding="utf-8"))["jobs"][
            "check_fast_cross_os_required_context"
        ]
        outcomes = ("success", "failure", "cancelled", "skipped")
        for cross_os_rust in ("true", "false"):
            for check in outcomes:
                for targets in outcomes:
                    with self.subTest(cross_os_rust=cross_os_rust, check=check, targets=targets):
                        allowed = {"success"} if cross_os_rust == "true" else {"success", "skipped"}
                        self.assertEqual(
                            self.run_cross_os_mirror(
                                mirror,
                                cross_os_rust,
                                {"check_fast_cross_os": check, "check_fast_cross_os_targets": targets},
                            ),
                            check in allowed and targets in allowed,
                        )

    def test_cross_os_split_rejects_fail_open_mutations(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        mirror = job_block(workflow, "check_fast_cross_os_required_context")
        targets = job_block(workflow, "check_fast_cross_os_targets")
        targets_mirror = step_block(mirror, "Mirror check_fast_cross_os_targets result for branch protection")
        writer = step_block(targets, "Writer namespace exact Windows targets")
        mutations = (
            (mirror, mirror.replace("      - check_fast_cross_os_targets\n", "", 1), "cross-OS required-context mirror must retain exact needs"),
            (mirror, mirror.replace("    if: always()\n", "", 1), "cross-OS required-context mirror must retain exact if"),
            (mirror, mirror.replace(targets_mirror, "", 1), 'must retain exactly one "Mirror check_fast_cross_os_targets result for branch protection" step'),
            (mirror, mirror.replace(targets_mirror, targets_mirror.replace("${{ needs.changes.outputs.cross_os_rust }}", "'false'"), 1), "must pin exact step env"),
            (targets, targets.replace("    runs-on:", "    continue-on-error: true\n    runs-on:", 1), "cross-OS exact targets job must not be allowed to continue on error"),
            (targets, targets.replace("    if: needs.changes.outputs.rust_compile == 'true' && ", "    if: ", 1), "cross-OS exact targets job must retain exact if"),
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
        expected_closure = {
            "changes",
            "scripts",
            "scripts_guards",
            "scripts_contracts",
            "scripts_required_context",
            "relay-authority-contract",
            "relay_authority_targets",
            "relay_authority_mutations",
        }
        closure: set[str] = set()
        frontier = ["scripts_required_context", "relay-authority-contract"]
        while frontier:
            job_id = frontier.pop()
            if job_id in closure:
                continue
            closure.add(job_id)
            needs = jobs[job_id].get("needs", [])
            frontier.extend([needs] if isinstance(needs, str) else needs)
        self.assertEqual(closure, expected_closure)

        self.assertEqual(jobs["scripts_required_context"]["if"], "always()")
        self.assertEqual(jobs["relay-authority-contract"]["if"], "always()")
        for job_id in (
            "relay_authority_targets",
            "relay_authority_mutations",
            "changes",
            "scripts",
            "scripts_guards",
            "scripts_contracts",
        ):
            self.assertNotIn("if", jobs[job_id])

        for job_id in sorted(expected_closure):
            job = job_block(workflow, job_id)
            marker = f"    name: {jobs[job_id]['name']}\n"
            self.assertIn(marker, job)
            with self.subTest(job=job_id, key="continue-on-error"):
                mutated_job = job.replace(
                    marker,
                    f"{marker}    continue-on-error: true\n",
                    1,
                )
                mutated = workflow.replace(job, mutated_job, 1)
                result = self.run_hardening_fixture(mutated)
                self.assertNotEqual(result.returncode, 0, result.stderr)

        for job_id in (
            "relay_authority_targets",
            "relay_authority_mutations",
            "changes",
            "scripts",
            "scripts_guards",
            "scripts_contracts",
        ):
            job = job_block(workflow, job_id)
            marker = f"    name: {jobs[job_id]['name']}\n"
            with self.subTest(job=job_id, key="if"):
                mutated_job = job.replace(
                    marker,
                    f"{marker}    if: ${{{{ github.event_name == 'push' }}}}\n",
                    1,
                )
                mutated = workflow.replace(job, mutated_job, 1)
                result = self.run_hardening_fixture(mutated)
                self.assertNotEqual(result.returncode, 0, result.stderr)

    def test_duplicate_required_job_id_is_rejected_before_last_wins_resolution(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        mirror = job_block(workflow, "scripts_required_context")
        malicious_last = mirror.replace(
            "FILTER_OUTPUT: true",
            "FILTER_OUTPUT: !!binary dHJ1ZQ==",
            1,
        )
        mutated = workflow.replace(
            "  relay-authority-contract:\n",
            malicious_last + "  relay-authority-contract:\n",
            1,
        )
        self.assertNotEqual(mutated, workflow)
        result = self.run_hardening_fixture(mutated)
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertIn(
            "duplicate job IDs are forbidden: scripts_required_context",
            result.stderr,
        )

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

    def test_path_filter_required_contexts_publish_from_always_mirrors(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        jobs = yaml.safe_load(workflow)["jobs"]
        for (
            mirror_id,
            context,
            runner_id,
            runner_name,
            runner_if,
            filter_name,
            filter_output,
        ) in PATH_FILTER_REQUIRED_MIRRORS:
            with self.subTest(context=context):
                job = jobs[mirror_id]
                self.assertEqual(job["name"], context)
                self.assertEqual(job["needs"], ["changes", runner_id])
                self.assertEqual(job["if"], "always()")
                self.assertNotIn("continue-on-error", job)
                self.assertEqual(job["runs-on"], "ubuntu-latest")
                self.assertEqual(len(job["steps"]), 3)
                self.assertEqual(job["steps"][0], {"uses": "actions/checkout@v4"})
                pin, result = job["steps"][1:]
                self.assertEqual(pin["name"], PATH_FILTER_MIRROR_PIN_STEP)
                self.assertEqual(pin["env"], {"BASH_ENV": "/dev/null"})
                self.assertEqual(pin["shell"], "bash")
                self.assertEqual(pin["timeout-minutes"], 10)
                self.assertIn(f"expected={REQUIRED_CHECK_MIRROR_SHA256}", pin["run"])
                self.assertEqual(result["run"], "./scripts/required-check-mirror.sh")
                self.assertEqual(
                    result["env"],
                    {
                        "BASH_ENV": "/dev/null",
                        "CHANGED_PATHS_RESULT": "${{ needs.changes.result }}",
                        "FILTER_NAME": filter_name,
                        "FILTER_OUTPUT": filter_output,
                        "UPSTREAM_JOB_NAME": runner_id,
                        "UPSTREAM_RESULT": f"${{{{ needs.{runner_id}.result }}}}",
                    },
                )

                runner = jobs[runner_id]
                self.assertEqual(runner["name"], runner_name)
                self.assertNotEqual(runner["name"], context)
                self.assertEqual(runner["needs"], "changes")
                self.assertEqual(runner["if"], runner_if)
                publishers = [
                    job_id
                    for job_id, candidate in jobs.items()
                    if isinstance(candidate, dict)
                    and str(candidate.get("name", job_id)).strip() == context
                ]
                self.assertEqual(publishers, [mirror_id])

    def test_path_filter_required_mirrors_reject_fail_open_mutations(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        baseline = self.run_hardening_fixture(workflow)
        self.assertEqual(baseline.returncode, 0, baseline.stderr)
        # label -> [(context, original block, mutated block, diagnostics)]; one
        # gate run per label mutates all three contexts and must name each.
        batches: dict[str, list[tuple[str, str, str, tuple[str, ...]]]] = {}
        for (
            mirror_id,
            context,
            runner_id,
            runner_name,
            _runner_if,
            _filter_name,
            filter_output,
        ) in PATH_FILTER_REQUIRED_MIRRORS:
            mirror = job_block(workflow, mirror_id)
            runner = job_block(workflow, runner_id)
            mirror_mutations = {
                "job deleted": "",
                "changes dependency deleted": mirror.replace("      - changes\n", "", 1),
                "job if deleted": mirror.replace("    if: always()\n", "", 1),
                "job if weakened": mirror.replace(
                    "    if: always()\n", "    if: success()\n", 1
                ),
                "job continue-on-error injected": mirror.replace(
                    "    runs-on: ubuntu-latest\n",
                    "    continue-on-error: true\n    runs-on: ubuntu-latest\n",
                    1,
                ),
                "helper pin deleted": mirror.replace(
                    step_block(mirror, PATH_FILTER_MIRROR_PIN_STEP), "", 1
                ),
                "helper pin corrupted": mirror.replace(
                    f"expected={REQUIRED_CHECK_MIRROR_SHA256}", "expected=" + "0" * 64, 1
                ),
                "mirror weakened": mirror.replace(
                    "        run: ./scripts/required-check-mirror.sh\n",
                    "        run: 'true'\n",
                    1,
                ),
                "mirror step if false": mirror.replace(
                    "        run: ./scripts/required-check-mirror.sh\n",
                    "        if: false\n        run: ./scripts/required-check-mirror.sh\n",
                    1,
                ),
                "mirror step or true": mirror.replace(
                    "        run: ./scripts/required-check-mirror.sh\n",
                    "        run: ./scripts/required-check-mirror.sh || true\n",
                    1,
                ),
                "helper pin step if false": mirror.replace(
                    f"      - name: {PATH_FILTER_MIRROR_PIN_STEP}\n",
                    f"      - name: {PATH_FILTER_MIRROR_PIN_STEP}\n        if: false\n",
                    1,
                ),
                "filter output forced false": mirror.replace(
                    f"FILTER_OUTPUT: {filter_output}\n", "FILTER_OUTPUT: false\n", 1
                ),
                "required name moved off mirror": mirror.replace(
                    f"    name: {context}\n", f"    name: {context} mirror\n", 1
                ),
            }
            runner_mutations = {
                "runner publishes required name": runner.replace(
                    f"    name: {runner_name}\n", f"    name: {context}\n", 1
                ),
            }
            diagnostics = (
                f"{context} required-context mirror {mirror_id} ",
                f"{context} runner job {runner_id} ",
                f"required {context} context must belong only to jobs.{mirror_id};",
            )
            for label, mutated in mirror_mutations.items():
                batches.setdefault(label, []).append((context, mirror, mutated, diagnostics))
            for label, mutated in runner_mutations.items():
                batches.setdefault(label, []).append((context, runner, mutated, diagnostics))
        for label, cases in batches.items():
            mutated = workflow
            for context, original, mutated_block, _diagnostics in cases:
                with self.subTest(context=context, mutation=label):
                    self.assertNotEqual(mutated_block, original)
                mutated = mutated.replace(original, mutated_block, 1)
            result = self.run_hardening_fixture(mutated)
            self.assertNotEqual(result.returncode, 0, result.stderr)
            for context, _original, _mutated_block, diagnostics in cases:
                with self.subTest(context=context, mutation=label):
                    self.assertTrue(
                        any(marker in result.stderr for marker in diagnostics),
                        result.stderr,
                    )

        # Effective check names: an unnamed job publishes its ID,
        # and a matrix job without a name expression gets a value suffix.
        unnamed_jobs = (
            "  Lint:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n"
            "  Dashboard:\n    strategy:\n      matrix:\n        runtime: [\"Node 22\"]\n"
            "    runs-on: ubuntu-latest\n    steps:\n      - run: true\n"
        )
        result = self.run_hardening_fixture(workflow.rstrip("\n") + "\n\n" + unnamed_jobs)
        with self.subTest(in_pr_workflow="gate rc"):
            self.assertNotEqual(result.returncode, 0, result.stderr)
        for context, mirror_id, job_id in (
            ("Lint", "lint_required_context", "Lint"),
            ("Dashboard (Node 22)", "dashboard_required_context", "Dashboard"),
        ):
            with self.subTest(in_pr_workflow=job_id):
                self.assertIn(
                    f"required {context} context must belong only to jobs.{mirror_id}; "
                    f"publishers: [\"{mirror_id}\", \"{job_id}\"]",
                    result.stderr,
                )

        # Other workflows, on any trigger, must not publish a
        # required name; workflow names do not namespace check names.
        probes = {
            "named-lint.yml": ("pull_request:", "probe", "    name: Lint\n", "Lint"),
            "unnamed-lint.yml": ("pull_request:", "Lint", "", "Lint"),
            "unnamed-matrix.yml": (
                "pull_request:",
                "Dashboard",
                "    strategy:\n      matrix:\n        runtime: [\"Node 22\"]\n",
                "Dashboard (Node 22)",
            ),
            "named-matrix.yml": (
                "schedule:\n    - cron: '0 0 * * *'",
                "probe",
                "    name: Dashboard\n    strategy:\n      matrix:\n        runtime: [\"Node 22\"]\n",
                "Dashboard (Node 22)",
            ),
            "dispatch.yml": ("workflow_dispatch:", "probe", "    name: High-risk recovery\n", "High-risk recovery"),
            "push.yml": (
                "push:\n    branches: [main]",
                "probe",
                "    name: Dashboard (Node 22)\n",
                "Dashboard (Node 22)",
            ),
        }
        extra_workflows = {
            file: (
                f"name: probe\non:\n  {trigger}\njobs:\n  {job_id}:\n{job_fields}"
                "    runs-on: ubuntu-latest\n    steps:\n      - run: true\n"
            )
            for file, (trigger, job_id, job_fields, _context) in probes.items()
        }
        result = self.run_hardening_fixture(workflow, extra_workflows=extra_workflows)
        with self.subTest(probe="gate rc"):
            self.assertNotEqual(result.returncode, 0, result.stderr)
        for file, (_trigger, job_id, _job_fields, context) in probes.items():
            with self.subTest(probe=file):
                self.assertIn(
                    f".github/workflows/{file}: workflow must not publish required "
                    f"{context} context (jobs: {job_id})",
                    result.stderr,
                )

    def test_path_filter_required_mirrors_fail_closed_on_upstream_results(self) -> None:
        jobs = yaml.safe_load(PR_WORKFLOW.read_text(encoding="utf-8"))["jobs"]
        mirror_script = REPO_ROOT / "scripts/required-check-mirror.sh"
        # (changes result, filter output, runner result) -> mirror passes?
        cases = (
            ("success", "false", "skipped", True),
            ("success", "false", "success", True),
            ("success", "true", "success", True),
            ("failure", "false", "skipped", False),
            ("cancelled", "false", "skipped", False),
            ("skipped", "false", "skipped", False),
            ("success", "", "skipped", False),
            ("success", "true", "skipped", False),
            ("success", "true", "failure", False),
            ("success", "true", "cancelled", False),
            ("success", "false", "failure", False),
            ("success", "false", "cancelled", False),
        )
        for mirror_id, context, *_rest in PATH_FILTER_REQUIRED_MIRRORS:
            step_env = jobs[mirror_id]["steps"][2]["env"]
            for changes_result, filter_value, runner_result, passes in cases:
                with self.subTest(
                    context=context,
                    changes=changes_result,
                    filter=filter_value,
                    runner=runner_result,
                ):
                    env = {
                        **os.environ,
                        **{key: str(value) for key, value in step_env.items()},
                        "CHANGED_PATHS_RESULT": changes_result,
                        "FILTER_OUTPUT": filter_value,
                        "UPSTREAM_RESULT": runner_result,
                    }
                    result = subprocess.run(
                        ["bash", str(mirror_script)],
                        env=env,
                        text=True,
                        capture_output=True,
                        check=False,
                    )
                    if passes:
                        self.assertEqual(result.returncode, 0, result.stderr)
                    else:
                        self.assertNotEqual(result.returncode, 0)
                        self.assertIn("::error::", result.stderr)

    def test_path_filter_mirror_helper_pins_reject_helper_mutations(self) -> None:
        jobs = yaml.safe_load(PR_WORKFLOW.read_text(encoding="utf-8"))["jobs"]
        helper = (REPO_ROOT / "scripts/required-check-mirror.sh").read_text(
            encoding="utf-8"
        )
        for mirror_id, context, *_rest in PATH_FILTER_REQUIRED_MIRRORS:
            run = jobs[mirror_id]["steps"][1]["run"]
            self.assertEqual(run.count(REQUIRED_CHECK_MIRROR_SHA256), 1)
            for label, helper_candidate, pin_run, passes in (
                ("reviewed helper", helper, run, True),
                ("one-byte helper mutation", helper + "#", run, False),
                (
                    "helper pin mutation",
                    helper,
                    run.replace(REQUIRED_CHECK_MIRROR_SHA256, "0" * 64, 1),
                    False,
                ),
            ):
                with self.subTest(context=context, case=label), tempfile.TemporaryDirectory() as temp:
                    root = Path(temp)
                    (root / "scripts").mkdir()
                    (root / "scripts/required-check-mirror.sh").write_text(
                        helper_candidate, encoding="utf-8"
                    )
                    result = subprocess.run(
                        ["bash", "-c", pin_run],
                        cwd=root,
                        text=True,
                        capture_output=True,
                        check=False,
                    )
                    if passes:
                        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                    else:
                        self.assertNotEqual(result.returncode, 0)
                        self.assertIn("content hash mismatch", result.stdout + result.stderr)

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

    def test_required_jobs_hash_backstops_reject_helper_and_gate_mutations(self) -> None:
        workflow = yaml.safe_load(PR_WORKFLOW.read_text(encoding="utf-8"))
        jobs = workflow["jobs"]
        runs = (
            jobs["scripts_required_context"]["steps"][1]["run"],
            next(
                step["run"]
                for step in jobs["relay-authority-contract"]["steps"]
                if step.get("name") == "Pin required-check mirror content (#5321)"
            ),
        )
        helper = (REPO_ROOT / "scripts/required-check-mirror.sh").read_text(
            encoding="utf-8"
        )
        gate = (REPO_ROOT / "scripts/check-ci-runner-hardening.sh").read_text(
            encoding="utf-8"
        )
        cases = (
            ("one-byte helper mutation", helper + "#", gate, None),
            ("helper pin mutation", helper, gate, REQUIRED_CHECK_MIRROR_SHA256),
            ("one-byte gate mutation", helper, gate + "#", None),
            ("gate pin mutation", helper, gate, CI_RUNNER_HARDENING_SHA256),
        )
        for label, helper_candidate, gate_candidate, pin_to_corrupt in cases:
            for index, run in enumerate(runs):
                with self.subTest(case=label, backstop=index), tempfile.TemporaryDirectory() as temp:
                    root = Path(temp)
                    (root / "scripts").mkdir()
                    (root / "scripts/required-check-mirror.sh").write_text(
                        helper_candidate, encoding="utf-8"
                    )
                    (root / "scripts/check-ci-runner-hardening.sh").write_text(
                        gate_candidate, encoding="utf-8"
                    )
                    pin_only = run.rsplit("\nscripts/check-ci-runner-hardening.sh\n", 1)[0]
                    if pin_to_corrupt is not None:
                        self.assertEqual(pin_only.count(pin_to_corrupt), 1)
                        pin_only = pin_only.replace(
                            pin_to_corrupt,
                            "0" * 64,
                            1,
                        )
                    result = subprocess.run(
                        ["bash", "-c", pin_only],
                        cwd=root,
                        text=True,
                        capture_output=True,
                        check=False,
                    )
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("content hash mismatch", result.stdout + result.stderr)

        mutated_gate = gate + "# reviewed gate edit\n"
        repinned_gate_sha256 = hashlib.sha256(mutated_gate.encode()).hexdigest()
        for index, run in enumerate(runs):
            with self.subTest(case="gate repin roundtrip", backstop=index), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                (root / "scripts").mkdir()
                (root / "scripts/required-check-mirror.sh").write_text(
                    helper, encoding="utf-8"
                )
                (root / "scripts/check-ci-runner-hardening.sh").write_text(
                    mutated_gate, encoding="utf-8"
                )
                pin_only = run.rsplit("\nscripts/check-ci-runner-hardening.sh\n", 1)[0]
                self.assertEqual(pin_only.count(CI_RUNNER_HARDENING_SHA256), 1)
                pin_only = pin_only.replace(
                    CI_RUNNER_HARDENING_SHA256,
                    repinned_gate_sha256,
                    1,
                )
                result = subprocess.run(
                    ["bash", "-c", pin_only],
                    cwd=root,
                    text=True,
                    capture_output=True,
                    check=False,
                )
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

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
        relay_job = job_block(workflow, "relay_authority_targets")
        mutated_relay_job = relay_job.replace(
            "      - uses: actions/checkout@v4\n\n",
            "      - uses: actions/checkout@v4\n\n"
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
            hardening, mutated_workflow, "relay_authority_targets"
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
            "relay_authority_targets": (
                (
                    "Verify named relay-authority targets and selection floors",
                    "must retain exact continue-on-error policy",
                ),
                (
                    "Run named relay-authority contract targets",
                    "must retain exact continue-on-error policy",
                ),
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
        # Hosted cache opt-out remains unconditional, including docs-only pushes.
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
        self.assertEqual(pr_workflow.count("name: Script checks\n"), 1)
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
require "yaml"
require "json"
require "digest"

def canonical_yaml(value)
  case value
  when Hash
    value.keys.sort_by(&:to_s).each_with_object({}) do |key, canonical|
      item = value[key]
      next if key.to_s == "continue-on-error" && (item.nil? || item == false)

      canonical[key.to_s] = canonical_yaml(item)
    end
  when Array
    value.map { |item| canonical_yaml(item) }
  else
    value
  end
end

def normalize_required_check_pin(value)
  case value
  when Hash
    value.transform_values { |item| normalize_required_check_pin(item) }
  when Array
    value.map { |item| normalize_required_check_pin(item) }
  when String
    value.gsub(/expected=[0-9a-f]{64}/, "expected=<required-check-pin-sha256>")
  else
    value
  end
end

workflow, job_id = ARGV
document = YAML.load_file(workflow)
job = document.fetch("jobs").fetch(job_id)
canonical = canonical_yaml(job)
canonical = normalize_required_check_pin(canonical) if job_id == "relay-authority-contract"
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

    def run_hardening_fixture(
        self,
        pr_workflow: str,
        extra_workflows: dict[str, str] | None = None,
        workflow_symlinks: dict[str, str] | None = None,
        hardening_script: str | None = None,
        mirror_helper: str | None = None,
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
            "  scripts_required_context:\n",
            collision + "  scripts_required_context:\n",
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
                    "  scripts_required_context:\n",
                    probe + "  scripts_required_context:\n",
                    1,
                )
                result = self.run_hardening_fixture(mutated)
                self.assertNotEqual(result.returncode, 0)
                self.assertRegex(
                    result.stderr,
                    r"(?:non-string YAML job keys|ambiguous YAML plain job keys)",
                )
        quoted = workflow.replace(
            "  scripts_required_context:\n",
            '  "yes":\n    name: quoted-key probe\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n\n  scripts_required_context:\n',
            1,
        )
        self.assertEqual(self.run_hardening_fixture(quoted).returncode, 0)

    def test_fixed_surfaces_use_raw_github_scalar_values(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        mirror = job_block(workflow, "scripts_required_context")
        relay = job_block(workflow, "relay-authority-contract")
        targets = job_block(workflow, "relay_authority_targets")
        relay_pin = step_block(relay, "Pin required-check mirror content (#5321)")
        cases = (
            (
                "plain yes is not the true string",
                workflow.replace(
                    mirror,
                    mirror.replace("FILTER_OUTPUT: true", "FILTER_OUTPUT: yes", 1),
                    1,
                ),
                1,
            ),
            (
                "plain on is not the true string",
                workflow.replace(
                    mirror,
                    mirror.replace("FILTER_OUTPUT: true", "FILTER_OUTPUT: on", 1),
                    1,
                ),
                1,
            ),
            (
                "quoted true changes the pinned scalar style",
                workflow.replace(
                    mirror,
                    mirror.replace(
                        "FILTER_OUTPUT: true", 'FILTER_OUTPUT: "true"', 1
                    ),
                    1,
                ),
                1,
            ),
            (
                "explicit binary tag is not discarded",
                workflow.replace(
                    mirror,
                    mirror.replace(
                        "FILTER_OUTPUT: true",
                        "FILTER_OUTPUT: !!binary dHJ1ZQ==",
                        1,
                    ),
                    1,
                ),
                1,
            ),
            (
                "backstop timeout leading zero is not decimal 10",
                workflow.replace(
                    relay,
                    relay.replace(
                        relay_pin,
                        relay_pin.replace(
                            "        timeout-minutes: 10\n",
                            "        timeout-minutes: 012\n",
                            1,
                        ),
                        1,
                    ),
                    1,
                ),
                1,
            ),
            (
                "job timeout leading zero is not decimal 30",
                workflow.replace(
                    targets,
                    targets.replace(
                        "    timeout-minutes: 30\n", "    timeout-minutes: 036\n", 1
                    ),
                    1,
                ),
                1,
            ),
        )
        for label, mutated, expected_rc in cases:
            with self.subTest(case=label):
                self.assertNotEqual(mutated, workflow)
                result = self.run_hardening_fixture(mutated)
                self.assertEqual(result.returncode, expected_rc, result.stderr)

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

    def test_ci_script_checks_runs_lane_coverage_contract(self) -> None:
        script = (REPO_ROOT / "scripts/ci-script-checks.sh").read_text(
            encoding="utf-8"
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
