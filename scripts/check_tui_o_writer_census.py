#!/usr/bin/env python3
"""E4 census gate: pin every Legacy Discord send site and every O cutover gate.

While `O_TUI_WRITER` is off Legacy is the only TUI body writer. When it flips,
O owns the body on delegated channels and Legacy must skip its body send at the
gated funnels. This gate keeps that cut honest on every intermediate head:

  (a) EXPECTED_PRIMITIVES pins, per file under src/services/discord/, the exact
      production count of each send/edit primitive. A new or moved send fails.
  (b) CENSUS gives every file in (a) a census row and a target. A missing row,
      an unknown target, or a `TBD`/`?` target fails ("zero undecided").
  (c) EXPECTED_GATES pins, per file under src/, the exact count of the cutover
      helper tokens. CUT_D/CUT_T files and UNREACH_G gate files need at least
      one; R-EVID files (real delivery evidence readers) must have none, so a
      delegated verdict can never be read as Posted evidence. The
      `O_TUI_WRITER` token itself may appear only in the O_TUI_WRITER_FILES.

TO CHANGE A COUNT: edit the map in this file in the same commit that moves the
call, and say in the commit message which site moved and why. A new file with a
primitive also needs a CENSUS row.

LEXICAL LIMITS: this scans stripped production text (comments, strings and
`#[cfg(test)]` items removed by the durable frontier gate's classifier). It
does not resolve types, `use .. as` aliases, re-exports or name-building
macros, so a primitive reached through one of those is not counted. Helper
references passed as values (`.is_some_and(helper)`) are counted as gates.
"""

from __future__ import annotations

import importlib.util
import re
import sys
from pathlib import Path

PRIMITIVE_ROOT = "src/services/discord/"
PRIMITIVES: dict[str, str] = {
    "send_channel_message*": r"\bsend_channel_message\w*\s*\(",
    "edit_channel_message*": r"\bedit_channel_message\w*\s*\(",
    "replace_long_message*": r"\breplace_long_message\w*\s*\(",
    "send_long_message*": r"\bsend_long_message\w*\s*\(",
    "replace_message_with_outcome": r"\breplace_message_with_outcome\s*\(",
    ".send_message": r"\.\s*send_message\s*\(",
    ".edit_message": r"\.\s*edit_message\s*\(",
    "TurnGateway::send_message": r"\bTurnGateway\s*::\s*send_message\s*\(",
    "TurnGateway::edit_message": r"\bTurnGateway\s*::\s*edit_message\s*\(",
    "deliver_turn_output*": r"\bdeliver_turn_output\w*\s*\(",
    "relay_recovered_terminal_text_to_placeholder": (
        r"\brelay_recovered_terminal_text_to_placeholder\s*\("
    ),
    "send_task_response_chunks_with_card_repair": (
        r"\bsend_task_response_chunks_with_card_repair\s*\("
    ),
    ".say": r"\.\s*say\s*\(",
    "send_outbound_message": r"\bsend_outbound_message\s*\(",
    "edit_outbound_message": r"\bedit_outbound_message\s*\(",
}
GATE_RE = re.compile(
    r"\b(?:o_owns_tui_output(?:_for_channel_tmux|_for_channel|_for_tmux_session|_with)?|bridge_o_body_cut_decision)\b"
)
FLAG_RE = re.compile(r"\bO_TUI_WRITER\b")
DEFN_RE = re.compile(r"\bfn\s+$")
# The switch is defined in topology.rs; the intake gate and its health probe read it there.
O_TUI_WRITER_FILES = {
    "src/services/tui_o/cutover.rs",
    "src/services/tui_o/cutover/channel_gate.rs",
    "src/services/tui_o/topology.rs",
    "src/services/discord/runtime_bootstrap/intake.rs",
    "src/services/discord/health/provider_probe.rs",
}

# CUT_D/CUT_T: gated here. COV:<row>: covered by that row's funnel gate.
# UNREACH_G: guarded in the third field's file. KEEP_36: kept output (§3.6).
# KEEP_NONBODY: panel/card/notice/command reply. KEEP_TRANSPORT: raw transport
# or primitive owner, cut at its callers. DEFER_A1_4B: recovery resend rows cut
# in the next slice; ungated until then.
TARGETS = {
    "CUT_D", "CUT_T", "KEEP_36", "KEEP_NONBODY", "KEEP_TRANSPORT", "UNREACH_G",
    "DEFER_A1_4B",
}
R_EVID = (
    "src/services/discord/outbound/delivery_record.rs",
    "src/services/discord/outbound/completed_turn_ledger.rs",
    "src/services/discord/catch_up.rs",
    "src/services/discord/catch_up/",
    "src/services/turn_orchestrator/active_source_dedup.rs",
    "src/services/discord/turn_bridge/terminal_outcome_delivery/rowless_receipt.rs",
    "src/services/discord/tmux_placeholder_suppression/evidence.rs",
    "src/services/discord/tmux_watcher/committed_placeholder_cleanup.rs",
    "src/services/discord/session_relay_sink/idle_jsonl.rs",
)

# Keys are relative to PRIMITIVE_ROOT.
EXPECTED_PRIMITIVES: dict[str, dict[str, int]] = {
    "abandon_request_store.rs": {"edit_outbound_message": 1},
    "commands/config.rs": {".say": 12, "send_long_message*": 1},
    "commands/control.rs": {".say": 15, "send_long_message*": 1},
    "commands/diagnostics/mod.rs": {".say": 9, "send_long_message*": 7},
    "commands/fast_mode.rs": {".say": 2},
    "commands/goals.rs": {".say": 2},
    "commands/help.rs": {".say": 2},
    "commands/inspect/mod.rs": {"send_long_message*": 2},
    "commands/meeting_cmd.rs": {".say": 4},
    "commands/mod.rs": {".say": 1},
    "commands/model_picker.rs": {".say": 1, ".send_message": 1},
    "commands/node.rs": {".say": 3},
    "commands/receipt.rs": {".say": 2, "send_long_message*": 1},
    "commands/recovery_ops.rs": {".say": 3, "send_long_message*": 1},
    "commands/restart.rs": {".say": 3},
    "commands/session.rs": {".say": 8, "send_long_message*": 2},
    "commands/skill.rs": {".say": 10, "send_long_message*": 3},
    "commands/text_commands.rs": {".send_message": 2, "send_long_message*": 10},
    "commands/tui_passthrough.rs": {".say": 8},
    "commands/voice.rs": {".say": 6},
    "discord_io.rs": {".send_message": 1},
    "footer_view_reconciler/mod.rs": {"edit_channel_message*": 7},
    "formatting/delivery.rs": {".say": 3, "send_channel_message*": 6, "send_long_message*": 2},
    "formatting/long_send_rollback.rs": {"send_channel_message*": 6, "send_long_message*": 5},
    "formatting/replace_long_message.rs": {"edit_channel_message*": 1, "replace_long_message*": 5, "send_channel_message*": 1, "send_long_message*": 1},
    "gateway.rs": {".edit_message": 1, ".send_message": 2, "TurnGateway::send_message": 1, "replace_long_message*": 2, "replace_message_with_outcome": 1, "send_long_message*": 1, "send_outbound_message": 1},
    "health/recovery.rs": {"edit_channel_message*": 1, "send_channel_message*": 1},
    "http.rs": {".edit_message": 2, ".send_message": 5, "send_channel_message*": 2},
    "idle_recap/card.rs": {"edit_channel_message*": 1, "send_channel_message*": 1},
    "meeting_orchestrator/records.rs": {"send_long_message*": 3},
    "meeting_orchestrator/rounds.rs": {"send_long_message*": 1},
    "meeting_orchestrator/selection_runtime.rs": {".edit_message": 1, ".send_message": 1},
    "monitoring_status.rs": {".edit_message": 1, ".send_message": 1},
    "outbound/delivery.rs": {".edit_message": 1},
    "outbound/manual_delivery.rs": {".send_message": 3},
    "outbound/o_writer_io.rs": {".send_message": 1},
    "outbound/serenity_reference.rs": {".send_message": 2},
    "outbound/transport.rs": {".send_message": 1},
    "outbound/turn_output_controller.rs": {"deliver_turn_output*": 1},
    "outbound/turn_output_controller/fresh_send.rs": {".send_message": 1},
    "outbound/turn_output_controller/transport.rs": {".send_message": 1, "send_long_message*": 2},
    "placeholder_controller.rs": {".edit_message": 1},
    "placeholder_controller/queued_card_gate.rs": {"edit_channel_message*": 1},
    "placeholder_sweeper.rs": {"edit_outbound_message": 1},
    "recovery_engine/completion_delivery.rs": {"relay_recovered_terminal_text_to_placeholder": 1},
    "recovery_engine/restore_inflight.rs": {"relay_recovered_terminal_text_to_placeholder": 5},
    "recovery_engine/terminal_text_idempotency.rs": {"replace_long_message*": 2, "send_long_message*": 2},
    "recovery_engine/two_message_panel.rs": {"send_channel_message*": 1},
    "recovery_paths/controller_cutover.rs": {"deliver_turn_output*": 1},
    "recovery_paths/restart.rs": {"relay_recovered_terminal_text_to_placeholder": 1},
    "router/intake_dispatch/notice.rs": {"send_channel_message*": 1},
    "router/intake_gate.rs": {".say": 3},
    "router/message_handler/attachments.rs": {".say": 4},
    "router/message_handler/control.rs": {".say": 1},
    "router/message_handler/goal_lifecycle.rs": {".say": 1},
    "router/message_handler/intake_turn.rs": {".say": 2},
    "router/message_handler/pre_admission_control.rs": {".say": 1},
    "router/message_handler/tui_followup.rs": {"edit_channel_message*": 1},
    "session_relay_sink.rs": {"replace_long_message*": 1, "replace_message_with_outcome": 1},
    "session_relay_sink/journal.rs": {"send_long_message*": 2},
    "session_relay_sink/short_controller.rs": {"deliver_turn_output*": 1},
    "session_relay_sink/task_notification_context.rs": {"send_long_message*": 1, "send_task_response_chunks_with_card_repair": 1},
    "standby_relay.rs": {"deliver_turn_output*": 1, "replace_long_message*": 1, "send_long_message*": 2},
    "startup_reclaim.rs": {"edit_outbound_message": 1},
    "task_notification_delivery/response_chunks.rs": {"send_channel_message*": 2},
    "terminal_ui_obligation.rs": {"edit_channel_message*": 1},
    "tmux_placeholder_suppression/ops.rs": {"edit_channel_message*": 1},
    "tmux_restart_handoff.rs": {"replace_long_message*": 1},
    "tmux_watcher.rs": {"edit_channel_message*": 1},
    "tmux_watcher/no_result_exits.rs": {"edit_channel_message*": 2, "send_channel_message*": 2},
    "tmux_watcher/pre_emit_guard.rs": {"edit_channel_message*": 1},
    "tmux_watcher/provider_output_guard.rs": {"edit_channel_message*": 1},
    "tmux_watcher/streaming_status_tick.rs": {"edit_channel_message*": 3, "send_channel_message*": 3},
    "tmux_watcher/streaming_status_tick/existing_panel_update.rs": {"edit_channel_message*": 1},
    "tmux_watcher/task_response_authority.rs": {"send_task_response_chunks_with_card_repair": 1},
    "tmux_watcher/terminal_abort_exits.rs": {"edit_channel_message*": 1, "send_channel_message*": 1},
    "tmux_watcher/terminal_direct_fallback.rs": {"replace_long_message*": 1, "send_long_message*": 2},
    "tmux_watcher/terminal_long_chunks.rs": {"deliver_turn_output*": 1, "send_long_message*": 1},
    "tmux_watcher/terminal_send.rs": {"deliver_turn_output*": 1},
    "tmux_watcher/two_message_panel.rs": {"send_channel_message*": 1},
    "tui_prompt_relay.rs": {".say": 2},
    "tui_prompt_relay/bridge_gateway.rs": {"edit_outbound_message": 1, "replace_long_message*": 1, "send_long_message*": 1, "send_outbound_message": 1},
    "tui_prompt_relay/synthetic_start_wiring.rs": {".say": 1},
    "turn_bridge/current_message_anchor.rs": {"TurnGateway::edit_message": 1, "TurnGateway::send_message": 1},
    "turn_bridge/headless_delivery.rs": {"edit_channel_message*": 1, "send_long_message*": 1},
    "turn_bridge/mod.rs": {"TurnGateway::edit_message": 1},
    "turn_bridge/single_message_footer.rs": {".send_message": 1},
    "turn_bridge/status_panel.rs": {".edit_message": 1, "TurnGateway::edit_message": 1, "edit_channel_message*": 2},
    "turn_bridge/status_panel/fallback.rs": {".send_message": 1, "send_channel_message*": 2},
    "turn_bridge/stream_loop/types.rs": {"replace_message_with_outcome": 1},
    "turn_bridge/stream_tick.rs": {"TurnGateway::edit_message": 5, "TurnGateway::send_message": 1},
    "turn_bridge/terminal_controller_cutover.rs": {"deliver_turn_output*": 2},
    "turn_bridge/terminal_delivery.rs": {"send_long_message*": 1},
    "turn_bridge/terminal_outcome_delivery.rs": {"TurnGateway::edit_message": 1, "replace_message_with_outcome": 1},
    "turn_bridge/terminal_outcome_delivery/cancel_prompt_replace.rs": {"replace_message_with_outcome": 2},
    "turn_bridge/terminal_outcome_delivery/foreign_terminal_handoff.rs": {"TurnGateway::send_message": 1},
    "turn_bridge/terminal_outcome_delivery/recovery_retry.rs": {".edit_message": 1},
    "turn_bridge/two_message_panel.rs": {".send_message": 2},
    "voice_barge_in/final_result_playback.rs": {"send_channel_message*": 1},
    "voice_barge_in/progress_playback.rs": {"send_channel_message*": 1},
    "voice_barge_in/routing.rs": {"send_channel_message*": 1},
    "voice_barge_in/runtime_lifecycle.rs": {".say": 1},
}
# file (relative to PRIMITIVE_ROOT): (census rows, target[, gate file]).
CENSUS: dict[str, tuple[str, ...]] = {
    "abandon_request_store.rs": ("1-D-notice", "KEEP_NONBODY"),
    "commands/config.rs": ("CMD", "KEEP_NONBODY"),
    "commands/control.rs": ("CMD", "KEEP_NONBODY"),
    "commands/diagnostics/mod.rs": ("CMD", "KEEP_NONBODY"),
    "commands/fast_mode.rs": ("CMD", "KEEP_NONBODY"),
    "commands/goals.rs": ("CMD", "KEEP_NONBODY"),
    "commands/help.rs": ("CMD", "KEEP_NONBODY"),
    "commands/inspect/mod.rs": ("CMD", "KEEP_NONBODY"),
    "commands/meeting_cmd.rs": ("CMD", "KEEP_NONBODY"),
    "commands/mod.rs": ("CMD", "KEEP_NONBODY"),
    "commands/model_picker.rs": ("CMD", "KEEP_NONBODY"),
    "commands/node.rs": ("CMD", "KEEP_NONBODY"),
    "commands/receipt.rs": ("CMD", "KEEP_NONBODY"),
    "commands/recovery_ops.rs": ("CMD", "KEEP_NONBODY"),
    "commands/restart.rs": ("CMD", "KEEP_NONBODY"),
    "commands/session.rs": ("CMD", "KEEP_NONBODY"),
    "commands/skill.rs": ("CMD", "KEEP_NONBODY"),
    "commands/text_commands.rs": ("CMD", "KEEP_NONBODY"),
    "commands/tui_passthrough.rs": ("CMD", "KEEP_NONBODY"),
    "commands/voice.rs": ("CMD", "KEEP_NONBODY"),
    "discord_io.rs": ("W19", "KEEP_TRANSPORT"),
    "footer_view_reconciler/mod.rs": ("1-B-panel", "KEEP_NONBODY"),
    "formatting/delivery.rs": ("W19", "KEEP_TRANSPORT"),
    "formatting/long_send_rollback.rs": ("W19", "KEEP_TRANSPORT"),
    "formatting/replace_long_message.rs": ("W19", "KEEP_TRANSPORT"),
    "gateway.rs": ("W19", "KEEP_TRANSPORT"),
    "health/recovery.rs": ("W33", "DEFER_A1_4B"),
    "http.rs": ("W19", "KEEP_TRANSPORT"),
    "idle_recap/card.rs": ("W25", "KEEP_NONBODY"),
    "meeting_orchestrator/records.rs": ("MEETING", "KEEP_NONBODY"),
    "meeting_orchestrator/rounds.rs": ("MEETING", "KEEP_NONBODY"),
    "meeting_orchestrator/selection_runtime.rs": ("MEETING", "KEEP_NONBODY"),
    "monitoring_status.rs": ("OPS", "KEEP_NONBODY"),
    "outbound/delivery.rs": ("W19", "KEEP_TRANSPORT"),
    "outbound/manual_delivery.rs": ("W19", "KEEP_TRANSPORT"),
    "outbound/o_writer_io.rs": ("O", "KEEP_TRANSPORT"),
    "outbound/serenity_reference.rs": ("W19", "KEEP_TRANSPORT"),
    "outbound/transport.rs": ("W19", "KEEP_TRANSPORT"),
    "outbound/turn_output_controller.rs": ("W19", "KEEP_TRANSPORT"),
    "outbound/turn_output_controller/fresh_send.rs": ("W19", "KEEP_TRANSPORT"),
    "outbound/turn_output_controller/transport.rs": ("W19", "KEEP_TRANSPORT"),
    "placeholder_controller.rs": ("1-B-panel", "KEEP_NONBODY"),
    "placeholder_controller/queued_card_gate.rs": ("1-B-panel", "KEEP_NONBODY"),
    "placeholder_sweeper.rs": ("1-D-notice", "KEEP_NONBODY"),
    "recovery_engine/completion_delivery.rs": ("W30,W31", "DEFER_A1_4B"),
    "recovery_engine/restore_inflight.rs": ("W30", "DEFER_A1_4B"),
    "recovery_engine/terminal_text_idempotency.rs": ("W32", "COV:W32"),
    "recovery_engine/two_message_panel.rs": ("1-D-panel", "KEEP_NONBODY"),
    "recovery_paths/controller_cutover.rs": ("W30a", "COV:W30"),
    "recovery_paths/restart.rs": ("W35", "DEFER_A1_4B"),
    "router/intake_dispatch/notice.rs": ("INTAKE", "KEEP_NONBODY"),
    "router/intake_gate.rs": ("INTAKE", "KEEP_NONBODY"),
    "router/message_handler/attachments.rs": ("INTAKE", "KEEP_NONBODY"),
    "router/message_handler/control.rs": ("INTAKE", "KEEP_NONBODY"),
    "router/message_handler/goal_lifecycle.rs": ("INTAKE", "KEEP_NONBODY"),
    "router/message_handler/intake_turn.rs": ("INTAKE", "KEEP_NONBODY"),
    "router/message_handler/pre_admission_control.rs": ("INTAKE", "KEEP_NONBODY"),
    "router/message_handler/tui_followup.rs": ("W26", "KEEP_NONBODY"),
    "session_relay_sink.rs": ("W20", "CUT_D"),
    "session_relay_sink/journal.rs": ("W20b", "COV:W20"),
    "session_relay_sink/short_controller.rs": ("W20a", "COV:W20"),
    "session_relay_sink/task_notification_context.rs": ("W20d,W21", "COV:W20"),
    "standby_relay.rs": ("W06", "UNREACH_G", "turn_bridge/runtime_handoff_loop/watcher_handoff.rs"),
    "startup_reclaim.rs": ("1-D-notice", "KEEP_NONBODY"),
    "task_notification_delivery/response_chunks.rs": ("W21", "COV:W20"),
    "terminal_ui_obligation.rs": ("1-B-panel", "KEEP_NONBODY"),
    "tmux_placeholder_suppression/ops.rs": ("W05", "KEEP_NONBODY"),
    "tmux_restart_handoff.rs": ("W34", "DEFER_A1_4B"),
    "tmux_watcher.rs": ("W01,W03", "CUT_D"),
    "tmux_watcher/no_result_exits.rs": ("1-A-notice", "KEEP_NONBODY"),
    "tmux_watcher/pre_emit_guard.rs": ("1-A-notice", "KEEP_NONBODY"),
    "tmux_watcher/provider_output_guard.rs": ("W02b", "COV:W02"),
    "tmux_watcher/streaming_status_tick.rs": ("W02", "CUT_D"),
    "tmux_watcher/streaming_status_tick/existing_panel_update.rs": ("1-A-panel", "KEEP_NONBODY"),
    "tmux_watcher/task_response_authority.rs": ("W01g", "COV:W01"),
    "tmux_watcher/terminal_abort_exits.rs": ("1-A-notice", "KEEP_NONBODY"),
    "tmux_watcher/terminal_direct_fallback.rs": ("W01a-c", "COV:W01"),
    "tmux_watcher/terminal_long_chunks.rs": ("W01e-f", "COV:W01"),
    "tmux_watcher/terminal_send.rs": ("W01d", "COV:W01"),
    "tmux_watcher/two_message_panel.rs": ("1-A-panel", "KEEP_NONBODY"),
    "tui_prompt_relay.rs": ("W24", "KEEP_NONBODY"),
    "tui_prompt_relay/bridge_gateway.rs": ("W23", "COV:W10"),
    "tui_prompt_relay/synthetic_start_wiring.rs": ("W24", "KEEP_NONBODY"),
    "turn_bridge/current_message_anchor.rs": ("W15", "KEEP_NONBODY"),
    "turn_bridge/headless_delivery.rs": ("W17", "KEEP_36"),
    "turn_bridge/mod.rs": ("W18", "KEEP_NONBODY"),
    "turn_bridge/single_message_footer.rs": ("W16", "KEEP_NONBODY"),
    "turn_bridge/status_panel.rs": ("1-B-panel", "KEEP_NONBODY"),
    "turn_bridge/status_panel/fallback.rs": ("1-B-panel", "KEEP_NONBODY"),
    "turn_bridge/stream_loop/types.rs": ("W10e", "COV:W10"),
    "turn_bridge/stream_tick.rs": ("W14", "CUT_D"),
    "turn_bridge/terminal_controller_cutover.rs": ("W10b-c", "COV:W10"),
    "turn_bridge/terminal_delivery.rs": ("W10d", "COV:W10"),
    "turn_bridge/terminal_outcome_delivery.rs": ("W10", "CUT_D"),
    "turn_bridge/terminal_outcome_delivery/cancel_prompt_replace.rs": ("W11", "CUT_D"),
    "turn_bridge/terminal_outcome_delivery/foreign_terminal_handoff.rs": ("W13", "DEFER_A1_4B"),
    "turn_bridge/terminal_outcome_delivery/recovery_retry.rs": ("W18", "KEEP_NONBODY"),
    "turn_bridge/two_message_panel.rs": ("1-B-panel", "KEEP_NONBODY"),
    "voice_barge_in/final_result_playback.rs": ("1-E", "KEEP_36"),
    "voice_barge_in/progress_playback.rs": ("1-E", "KEEP_36"),
    "voice_barge_in/routing.rs": ("1-E", "KEEP_36"),
    "voice_barge_in/runtime_lifecycle.rs": ("1-E", "KEEP_36"),
}
EXPECTED_GATES: dict[str, int] = {
    "src/services/discord/session_relay_sink.rs": 2,
    "src/services/discord/session_relay_sink/task_notification_context.rs": 1,
    "src/services/discord/tmux_watcher.rs": 1,
    "src/services/discord/tmux_watcher/completion_producer.rs": 1,
    "src/services/discord/tmux_watcher/streaming_status_tick.rs": 1,
    "src/services/discord/turn_bridge/runtime_handoff_loop/watcher_handoff.rs": 1,
    "src/services/discord/turn_bridge/stream_tick.rs": 1,
    "src/services/discord/turn_bridge/terminal_controller_cutover.rs": 1,
    "src/services/discord/turn_bridge/terminal_controller_cutover/o_body.rs": 1,
    "src/services/discord/turn_bridge/terminal_outcome_delivery.rs": 1,
    "src/services/discord/turn_bridge/terminal_outcome_delivery/cancel_prompt_replace.rs": 1,
    "src/services/tui_o/cutover.rs": 4,
}


def _load_classifier():
    name = "durable_frontier_writer_classifier"
    if name in sys.modules:
        return sys.modules[name]
    path = Path(__file__).resolve().parent / "check_durable_frontier_writer_call_sites.py"
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


def _count(pattern: re.Pattern[str], text: str) -> int:
    return sum(
        1
        for match in pattern.finditer(text)
        if not DEFN_RE.search(text[max(0, match.start() - 40) : match.start()])
    )


def measure(root: Path, pinned_test_only_files=None):
    classifier = _load_classifier()
    if pinned_test_only_files is None:
        pinned_test_only_files = classifier.PINNED_TEST_ONLY_MODULE_FILES
    files, skips = classifier._scan_inputs(root, pinned_test_only_files)
    compiled = {name: re.compile(regex) for name, regex in PRIMITIVES.items()}
    primitives: dict[str, dict[str, int]] = {}
    gates: dict[str, int] = {}
    flags: dict[str, int] = {}
    for path in files:
        if path in skips:
            continue
        rel = path.relative_to(root).as_posix()
        text = classifier._production_text(path)
        if rel.startswith(PRIMITIVE_ROOT):
            for name, pattern in compiled.items():
                if n := _count(pattern, text):
                    primitives.setdefault(rel[len(PRIMITIVE_ROOT) :], {})[name] = n
        if n := _count(GATE_RE, text):
            gates[rel] = n
        if n := len(FLAG_RE.findall(text)):
            flags[rel] = n
    return primitives, gates, flags


def problems_for(primitives, gates, flags) -> list[str]:
    problems: list[str] = []
    for rel in sorted(set(EXPECTED_PRIMITIVES) | set(primitives)):
        want, have = EXPECTED_PRIMITIVES.get(rel, {}), primitives.get(rel, {})
        for name in sorted(set(want) | set(have)):
            if want.get(name, 0) != have.get(name, 0):
                problems.append(
                    f"primitive {name}: {rel} has {have.get(name, 0)}x, "
                    f"expected {want.get(name, 0)}x"
                )
    for rel in sorted(set(primitives) | set(CENSUS)):
        row = CENSUS.get(rel)
        if row is None:
            problems.append(f"census: {rel} sends but has no CENSUS row")
            continue
        if rel not in primitives:
            problems.append(f"census: stale CENSUS row for {rel} (no primitive left)")
        target = row[1] if len(row) > 1 else ""
        if target not in TARGETS and not re.fullmatch(r"COV:W\d+", target):
            problems.append(f"census: {rel} has undecided target {target!r}")
            continue
        gate_file = PRIMITIVE_ROOT + (row[2] if len(row) > 2 else rel)
        if target in {"CUT_D", "CUT_T", "UNREACH_G"} and gates.get(gate_file, 0) < 1:
            problems.append(f"census: {target} row {row[0]} has no gate in {gate_file}")
    for rel in sorted(set(EXPECTED_GATES) | set(gates)):
        if EXPECTED_GATES.get(rel, 0) != gates.get(rel, 0):
            problems.append(
                f"gate: {rel} has {gates.get(rel, 0)}x, expected {EXPECTED_GATES.get(rel, 0)}x"
            )
        if gates.get(rel) and any(
            rel == evid or (evid.endswith("/") and rel.startswith(evid)) for evid in R_EVID
        ):
            problems.append(f"gate: cutover helper in R-EVID file {rel}")
    for rel in sorted(set(flags) - O_TUI_WRITER_FILES):
        problems.append(f"flag: O_TUI_WRITER outside {sorted(O_TUI_WRITER_FILES)}: {rel}")
    return problems


def check(root: Path, pinned_test_only_files=None) -> tuple[bool, str]:
    try:
        primitives, gates, flags = measure(root, pinned_test_only_files)
    except RuntimeError as exc:
        return False, str(exc)
    problems = problems_for(primitives, gates, flags)
    sites = sum(sum(m.values()) for m in primitives.values())
    deferred = sorted(rel for rel, row in CENSUS.items() if row[1] == "DEFER_A1_4B")
    if problems:
        return False, (
            "FAIL: TUI O writer census drifted.\n  " + "\n  ".join(problems)
            + "\nUpdate the maps in scripts/check_tui_o_writer_census.py in the same "
            "commit (see TO CHANGE A COUNT in its docstring)."
        )
    return True, (
        f"OK: TUI O writer census: {sites} send sites in {len(primitives)} files, "
        f"{sum(gates.values())} cutover gate tokens in {len(gates)} files, "
        f"{len(deferred)} rows deferred to A1-4b; lexical scan (see docstring)"
    )


def main() -> int:
    ok, message = check(Path(__file__).resolve().parent.parent)
    print(message, file=sys.stdout if ok else sys.stderr)
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
