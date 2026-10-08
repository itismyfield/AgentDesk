#!/usr/bin/env python3
"""E4 census gate: pin every Legacy Discord send site and every O cutover gate.

While `O_TUI_WRITER` is off Legacy is the only TUI body writer. When it flips,
O owns the body on delegated channels and Legacy must skip its body send at the
gated funnels. This gate keeps that cut honest on every intermediate head:

  (a) EXPECTED_PRIMITIVES pins, per file under src/services/discord/, the exact
      production count of each send/edit primitive. A new or moved send fails.
  (b) CENSUS gives every file in (a) a census row and a target. A missing row,
      an unknown target, or a `TBD`/`?` target fails ("zero undecided").
  (c) EXPECTED_GATES pins, per file under src/, each cutover helper token as
      `enclosing fn:kind` in source order: `claim` (a body is about to be sent,
      so a pending adoption ends) or `peek` (no body, the adoption is only
      read). A claim swapped for a peek or back, alone or as a pair within one
      function, fails its file's pins. CUT_D/CUT_T files and UNREACH_G gate
      files need at least one claim; R-EVID files (real delivery evidence
      readers) must have none of either, so a delegated verdict can never be
      read as Posted evidence. The `O_TUI_WRITER` token itself may appear only
      in the O_TUI_WRITER_FILES.
  (d) A claim is the `claim_then_send` helper, which claims just before the
      send it runs; its closure is the transport call alone. `claim_at_post`
      wraps a task-response transport so each chunk post claims that way. A
      raw claim (a claiming gate fn, or under tui_o any `.claim(` call or
      `::claim` path) may appear only in the RAW_CLAIM_SITES functions, each
      listed with its reason.

Census PASS is not flip readiness. `flip_ready` is reported on its own line and
is true only when no census row is deferred and every FLIP_READY_TESTS funnel
test exists in src/; `--require-flip-ready` turns a false verdict into rc 1.

TO CHANGE A COUNT: edit the map in this file in the same commit that moves the
call, and say in the commit message which site moved and why. A new file with a
primitive also needs a CENSUS row. A branch whose condition is negated while its
two gates keep their order is not seen; the per-site adoption tests cover that.

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
    "relay_recovered_body_to_placeholder": r"\brelay_recovered_body_to_placeholder\s*\(",
    "send_task_response_chunks_with_card_repair": (
        r"\bsend_task_response_chunks_with_card_repair\s*\("
    ),
    ".say": r"\.\s*say\s*\(",
    "send_outbound_message": r"\bsend_outbound_message\s*\(",
    "edit_outbound_message": r"\bedit_outbound_message\s*\(",
}
_OWNS = r"o_owns_tui_output(?:_for_channel_tmux|_for_channel|_for_tmux_session|_with)?"
_RAW_CLAIM = rf"\b{_OWNS}\b|\bcandidate\s*\.\s*claim\s*\("
RAW_CLAIM_RE = re.compile(_RAW_CLAIM)
# `Candidate::claim` is visible only under tui_o, so there every `.claim(` call and every
# `::claim` path counts, whatever the receiver or chain, and whether called or taken as a value.
TUI_O_ROOT = "src/services/tui_o/"
TUI_O_RAW_CLAIM_RE = re.compile(rf"{_RAW_CLAIM}|\.\s*claim\s*\(|::\s*claim\b")
GATE_RES = {
    "claim": re.compile(rf"{_RAW_CLAIM}|\bclaim_then_(?:direct_)?send\b|\bclaim_at_post\b"),
    "peek": re.compile(rf"\b(?:peek_{_OWNS}|bridge_o_body_peek_decision)\b"),
}
FLAG_RE = re.compile(r"\bO_TUI_WRITER\b")
DEFN_RE = re.compile(r"\bfn\s+$")
FN_RE = re.compile(r"\bfn\s+(\w+)")
# The switch is defined in topology.rs; the intake gate and its health probe read it there.
O_TUI_WRITER_FILES = {
    "src/services/tui_o/cutover.rs",
    "src/services/tui_o/cutover/channel_gate.rs",
    "src/services/tui_o/topology.rs",
    "src/services/discord/runtime_bootstrap/intake.rs",
    "src/services/discord/health/provider_probe.rs",
}

# CUT_D/CUT_T: gated here, or at the transport in the third field's file. COV:<row>: covered
# by that row's funnel gate. UNREACH_G: guarded in the third field's file. KEEP_36: kept output (§3.6).
# KEEP_NONBODY: panel/card/notice/command reply. KEEP_TRANSPORT: raw transport
# or primitive owner, cut at its callers. DEFER_*: ungated for now; census passes
# but flip_ready stays false.
TARGETS = {
    "CUT_D", "CUT_T", "KEEP_36", "KEEP_NONBODY", "KEEP_TRANSPORT", "UNREACH_G",
}
DEFER_RE = re.compile(r"DEFER_[A-Z0-9_]+")
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
    "admin_host_guard.rs": {".say": 1},
    "commands/config.rs": {".say": 12, "send_long_message*": 1},
    "commands/control.rs": {".say": 15, "send_long_message*": 1},
    "commands/control/home_fence.rs": {".say": 1},
    "commands/control/input_clear.rs": {".say": 1},
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
    "commands/restart.rs": {".say": 4},
    "commands/session.rs": {".say": 8, "send_long_message*": 2},
    "commands/skill.rs": {".say": 10, "send_long_message*": 3},
    "commands/text_commands.rs": {".send_message": 2, "send_long_message*": 10},
    "commands/tui_passthrough.rs": {".say": 8},
    "commands/voice.rs": {".say": 6},
    "discord_io.rs": {".send_message": 1},
    "footer_view_reconciler/mod.rs": {"edit_channel_message*": 6},
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
    "queue_io/turn_admission.rs": {"send_channel_message*": 1},
    "recovery_engine/completion_delivery.rs": {"relay_recovered_body_to_placeholder": 3},
    "recovery_engine/restore_inflight.rs": {"relay_recovered_terminal_text_to_placeholder": 2},
    "recovery_engine/terminal_text_idempotency.rs": {"replace_long_message*": 2, "send_long_message*": 2},
    "recovery_engine/two_message_panel.rs": {"send_channel_message*": 1},
    "recovery_paths/controller_cutover.rs": {"deliver_turn_output*": 1},
    "recovery_paths/restart.rs": {"relay_recovered_body_to_placeholder": 1},
    "router/intake_dispatch/notice.rs": {"send_channel_message*": 1},
    "router/intake_gate.rs": {".say": 3},
    "router/intake_gate/busy_inject.rs": {".say": 1},
    "router/message_handler/attachments.rs": {".say": 4},
    "router/message_handler/control.rs": {".say": 1},
    "router/message_handler/goal_lifecycle.rs": {".say": 1},
    "router/message_handler/intake_turn.rs": {".say": 2},
    "router/message_handler/intake_turn/host_refusal.rs": {".say": 1},
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
    "tui_prompt_relay/synthetic_start_wiring.rs": {".say": 3},
    "turn_bridge/completion_postlude/o_panel_below.rs": {"TurnGateway::send_message": 1, "send_channel_message*": 1},
    "turn_bridge/current_message_anchor.rs": {"TurnGateway::edit_message": 1, "TurnGateway::send_message": 1},
    "turn_bridge/headless_delivery.rs": {"edit_channel_message*": 1, "send_long_message*": 1},
    "turn_bridge/mod.rs": {"TurnGateway::edit_message": 1},
    "turn_bridge/single_message_footer.rs": {".send_message": 1},
    "turn_bridge/status_panel.rs": {".edit_message": 1, "TurnGateway::edit_message": 1, "edit_channel_message*": 2},
    "turn_bridge/status_panel/fallback.rs": {".send_message": 1, "send_channel_message*": 2},
    "turn_bridge/stream_loop/types.rs": {"replace_message_with_outcome": 1},
    "turn_bridge/stream_tick.rs": {"TurnGateway::edit_message": 3, "TurnGateway::send_message": 1},
    "turn_bridge/stream_tick/o_panel.rs": {"TurnGateway::edit_message": 1, "TurnGateway::send_message": 1},
    "turn_bridge/stream_tick/rollover_guard.rs": {"TurnGateway::edit_message": 2},
    "turn_bridge/terminal_controller_cutover.rs": {"deliver_turn_output*": 2},
    "turn_bridge/terminal_delivery.rs": {"send_long_message*": 1},
    "turn_bridge/terminal_outcome_delivery.rs": {"TurnGateway::edit_message": 1, "replace_message_with_outcome": 1},
    "turn_bridge/terminal_outcome_delivery/cancel_prompt_replace.rs": {"replace_message_with_outcome": 2},
    "turn_bridge/terminal_outcome_delivery/foreign_terminal_handoff.rs": {"TurnGateway::send_message": 1},
    "turn_bridge/terminal_outcome_delivery/recovery_retry.rs": {".edit_message": 1},
    "turn_bridge/two_message_panel.rs": {".send_message": 2},
    "turn_presence/supervisor.rs": {".say": 1},
    "voice_barge_in/final_result_playback.rs": {"send_channel_message*": 1},
    "voice_barge_in/progress_playback.rs": {"send_channel_message*": 1},
    "voice_barge_in/routing.rs": {"send_channel_message*": 1},
    "voice_barge_in/runtime_lifecycle.rs": {".say": 1},
}
# file (relative to PRIMITIVE_ROOT): (census rows, target[, gate file]).
CENSUS: dict[str, tuple[str, ...]] = {
    "abandon_request_store.rs": ("1-D-notice", "KEEP_NONBODY"),
    "admin_host_guard.rs": ("CMD", "KEEP_NONBODY"),
    "commands/config.rs": ("CMD", "KEEP_NONBODY"),
    "commands/control.rs": ("CMD", "KEEP_NONBODY"),
    "commands/control/home_fence.rs": ("CMD", "KEEP_NONBODY"),
    "commands/control/input_clear.rs": ("CMD", "KEEP_NONBODY"),
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
    "health/recovery.rs": ("W33", "CUT_D"),
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
    # Control Notice via the reference helper; no O body authority or original/body text.
    "queue_io/turn_admission.rs": ("INPUT-FENCE-notice", "KEEP_NONBODY"),
    "recovery_engine/completion_delivery.rs": ("W30,W31", "CUT_D"),
    "recovery_engine/restore_inflight.rs": ("1-D-notice", "KEEP_NONBODY"),
    "recovery_engine/terminal_text_idempotency.rs": ("W32", "COV:W32"),
    "recovery_engine/two_message_panel.rs": ("1-D-panel", "KEEP_NONBODY"),
    "recovery_paths/controller_cutover.rs": ("W30a", "COV:W30"),
    "recovery_paths/restart.rs": ("W35", "CUT_D", "recovery_engine/terminal_text_idempotency.rs"),
    "router/intake_dispatch/notice.rs": ("INTAKE", "KEEP_NONBODY"),
    "router/intake_gate.rs": ("INTAKE", "KEEP_NONBODY"),
    # Injection notices (unconfirmed paste, unknown handback); no body text.
    "router/intake_gate/busy_inject.rs": ("INTAKE", "KEEP_NONBODY"),
    "router/message_handler/attachments.rs": ("INTAKE", "KEEP_NONBODY"),
    "router/message_handler/control.rs": ("INTAKE", "KEEP_NONBODY"),
    "router/message_handler/goal_lifecycle.rs": ("INTAKE", "KEEP_NONBODY"),
    "router/message_handler/intake_turn.rs": ("INTAKE", "KEEP_NONBODY"),
    "router/message_handler/intake_turn/host_refusal.rs": ("INTAKE", "KEEP_NONBODY"),
    "router/message_handler/pre_admission_control.rs": ("INTAKE", "KEEP_NONBODY"),
    "router/message_handler/tui_followup.rs": ("W26", "KEEP_NONBODY"),
    "session_relay_sink.rs": ("W20", "CUT_D"),
    "session_relay_sink/journal.rs": ("W20b", "COV:W20"),
    "session_relay_sink/short_controller.rs": ("W20a", "COV:W20"),
    "session_relay_sink/task_notification_context.rs": ("W20d,W21", "COV:W20"),
    "standby_relay.rs": ("W06", "CUT_D"),
    "startup_reclaim.rs": ("1-D-notice", "KEEP_NONBODY"),
    "task_notification_delivery/response_chunks.rs": ("W21", "COV:W20"),
    "terminal_ui_obligation.rs": ("1-B-panel", "KEEP_NONBODY"),
    "tmux_placeholder_suppression/ops.rs": ("W05", "KEEP_NONBODY"),
    "tmux_restart_handoff.rs": ("W34", "CUT_D"),
    "tmux_watcher.rs": ("W01,W03", "CUT_D", "tmux_watcher/terminal_direct_fallback.rs"),
    "tmux_watcher/no_result_exits.rs": ("1-A-notice", "KEEP_NONBODY"),
    "tmux_watcher/pre_emit_guard.rs": ("1-A-notice", "KEEP_NONBODY"),
    "tmux_watcher/provider_output_guard.rs": ("W02b", "COV:W02"),
    "tmux_watcher/streaming_status_tick.rs": ("W02", "CUT_D"),
    "tmux_watcher/streaming_status_tick/existing_panel_update.rs": ("1-A-panel", "KEEP_NONBODY"),
    "tmux_watcher/task_response_authority.rs": ("W01g", "CUT_D"),
    "tmux_watcher/terminal_abort_exits.rs": ("1-A-notice", "KEEP_NONBODY"),
    "tmux_watcher/terminal_direct_fallback.rs": ("W01a-c", "COV:W01"),
    "tmux_watcher/terminal_long_chunks.rs": ("W01e-f", "COV:W01"),
    "tmux_watcher/terminal_send.rs": ("W01d", "COV:W01"),
    "tmux_watcher/two_message_panel.rs": ("1-A-panel", "KEEP_NONBODY"),
    "tui_prompt_relay.rs": ("W24", "KEEP_NONBODY"),
    "tui_prompt_relay/bridge_gateway.rs": ("W23", "COV:W10"),
    "tui_prompt_relay/synthetic_start_wiring.rs": ("W24", "KEEP_NONBODY"),
    "turn_bridge/completion_postlude/o_panel_below.rs": ("1-B-panel", "KEEP_NONBODY"),
    "turn_bridge/current_message_anchor.rs": ("W15", "KEEP_NONBODY"),
    "turn_bridge/headless_delivery.rs": ("W17", "KEEP_36"),
    "turn_bridge/mod.rs": ("W18", "KEEP_NONBODY"),
    "turn_bridge/single_message_footer.rs": ("W16", "KEEP_NONBODY"),
    "turn_bridge/status_panel.rs": ("1-B-panel", "KEEP_NONBODY"),
    "turn_bridge/status_panel/fallback.rs": ("1-B-panel", "KEEP_NONBODY"),
    "turn_bridge/stream_loop/types.rs": ("W10e", "COV:W10"),
    "turn_bridge/stream_tick.rs": ("W14", "CUT_D"),
    "turn_bridge/stream_tick/o_panel.rs": ("1-B-panel", "KEEP_NONBODY"),
    "turn_bridge/stream_tick/rollover_guard.rs": ("W14a", "COV:W14"),
    "turn_bridge/terminal_controller_cutover.rs": ("W10b-c", "COV:W10"),
    "turn_bridge/terminal_delivery.rs": ("W10d", "COV:W10"),
    "turn_bridge/terminal_outcome_delivery.rs": (
        "W10",
        "CUT_D",
        "turn_bridge/terminal_controller_cutover/o_body.rs",
    ),
    "turn_bridge/terminal_outcome_delivery/cancel_prompt_replace.rs": ("W11", "CUT_D"),
    "turn_bridge/terminal_outcome_delivery/foreign_terminal_handoff.rs": ("W13", "CUT_D"),
    "turn_bridge/terminal_outcome_delivery/recovery_retry.rs": ("W18", "KEEP_NONBODY"),
    "turn_bridge/two_message_panel.rs": ("1-B-panel", "KEEP_NONBODY"),
    "turn_presence/supervisor.rs": ("1-D-notice", "KEEP_NONBODY"),
    "voice_barge_in/final_result_playback.rs": ("1-E", "KEEP_36"),
    "voice_barge_in/progress_playback.rs": ("1-E", "KEEP_36"),
    "voice_barge_in/routing.rs": ("1-E", "KEEP_36"),
    "voice_barge_in/runtime_lifecycle.rs": ("1-E", "KEEP_36"),
}
# Each file's cutover gates as `enclosing fn:kind`, in source order.
# claim: a body is sent here (`claim_then_send`, or a RAW_CLAIM_SITES claim). peek: only read.
EXPECTED_GATES: dict[str, tuple[str, ...]] = {
    "src/services/discord/commands/control/native.rs": ("select:peek",),
    "src/services/discord/footer_view_reconciler/mod.rs": (
        "edit_body_message:claim",
    ),
    "src/services/discord/health/recovery.rs": (
        "maybe_recover_completed_stale_leak:peek",
        "maybe_recover_completed_stale_leak:claim",
    ),
    "src/services/discord/idle_recap.rs": (
        "probe_relay_integrity:peek",
    ),
    "src/services/discord/outbound/turn_output_controller.rs": (
        "deliver_turn_output_with_fallback_revalidation:claim",
    ),
    "src/services/discord/outbound/turn_output_controller/fresh_send.rs": (
        "deliver:claim",
    ),
    "src/services/discord/recovery_engine/completion_delivery.rs": (
        "o_owns_recovery_body:peek",
        "relay_recovered_body_to_placeholder:claim",
    ),
    "src/services/discord/recovery_engine/terminal_text_idempotency.rs": (
        "relay_no_anchor_terminal_text:claim",
        "relay_no_anchor_terminal_text:claim",
    ),
    "src/services/discord/recovery_paths/restart.rs": (
        "try_recover_anchor_repost:peek",
    ),
    "src/services/discord/session_relay_sink.rs": (
        "deliver_response:peek",
        "deliver_response:peek",
        "deliver_response:claim",
        "deliver_response:claim",
    ),
    "src/services/discord/session_relay_sink/task_notification_context.rs": (
        "task_response_claim_for_card:peek",
        "deliver_new_message_with_task_authority:claim",
        "deliver_new_message_with_task_authority:claim",
    ),
    "src/services/discord/standby_relay.rs": (
        "run_standby_relay:claim",
    ),
    "src/services/discord/task_notification_delivery/response_chunks.rs": (
        "post_chunk:claim",
    ),
    "src/services/discord/tmux_restart_handoff.rs": (
        "start_restart_handoff_from_state:peek",
        "start_restart_handoff_from_state:claim",
    ),
    "src/services/discord/tmux_watcher.rs": (
        "tmux_output_watcher_with_restore:peek",
    ),
    "src/services/discord/tmux_watcher/completion_producer.rs": (
        "complete_watcher_terminal_footer_or_status_panel_with_sniffer:peek",
    ),
    "src/services/discord/tmux_watcher/o_delegated_arm.rs": (
        "o_took_channel:peek",
    ),
    "src/services/discord/tmux_watcher/streaming_status_tick.rs": (
        "update_streaming_status_tick:peek",
        "update_streaming_status_tick:claim",
        "update_streaming_status_tick:claim",
    ),
    "src/services/discord/tmux_watcher/task_response_authority.rs": (
        "apply_watcher_task_response:claim",
    ),
    "src/services/discord/tmux_watcher/terminal_direct_fallback.rs": (
        "apply_watcher_direct_fallback_send:claim",
        "apply_watcher_direct_fallback_send:claim",
    ),
    "src/services/discord/tmux_watcher/terminal_long_chunks.rs": (
        "apply_watcher_long_chunks_legacy:claim",
    ),
    "src/services/discord/tui_prompt_relay/claude_idle_bridge.rs": ("idle_tail_tool_opens:peek",),
    "src/services/discord/turn_bridge/completion_postlude/o_panel_below.rs": (
        "follow:peek",
    ),
    "src/services/discord/turn_bridge/headless_delivery.rs": (
        "enqueue_claimed_headless_delivery:claim",
    ),
    "src/services/discord/turn_bridge/runtime_handoff_loop/watcher_handoff.rs": (
        "handle_watcher_runtime_handoff:peek",
    ),
    "src/services/discord/turn_bridge/stream_loop/types.rs": (
        "deliver:claim",
        "deliver:claim",
    ),
    "src/services/discord/turn_bridge/stream_tick.rs": (
        "run_bridge_stream_tick:peek",
        "run_bridge_stream_tick:claim",
    ),
    "src/services/discord/turn_bridge/stream_tick/rollover_guard.rs": (
        "guarded_bridge_rollover_edit:claim",
    ),
    "src/services/discord/turn_bridge/terminal_controller_cutover/o_body.rs": (
        "bridge_o_body_peek_decision:peek",
        "claimed_send:claim",
        "sent_under:claim",
    ),
    "src/services/discord/turn_bridge/terminal_outcome_delivery.rs": (
        "run_terminal_outcome_delivery:peek",
    ),
    "src/services/discord/turn_bridge/terminal_outcome_delivery/cancel_prompt_replace.rs": (
        "handle_cancel_prompt_replace:peek",
        "handle_cancel_prompt_replace:claim",
    ),
    "src/services/discord/turn_bridge/terminal_outcome_delivery/foreign_terminal_handoff.rs": (
        "handle_known_owner:peek",
        "resume_with_gateway:peek",
        "resume_with_gateway:claim",
    ),
    "src/services/discord/turn_bridge/watcher_handoff.rs": ("o_body_needs_bridge_terminal:peek",),
    "src/services/discord/turn_finalizer/watcher_backstop.rs": (
        "watcher_backstop_turn_is_terminal:peek",
    ),
    "src/services/herdr_launch.rs": ("o_ready_at:peek",),
    "src/services/tui_o/cutover.rs": (
        "claim_for_placement:claim",
    ),
    "src/services/tui_o/cutover/channel_gate.rs": (
        "claim:claim",
        "claim:claim",
        "decide_with_snapshot:claim",
    ),
}
# Functions where a raw claim may stand outside `claim_then_send`, keyed by file.
RAW_CLAIM_SITES: dict[str, tuple[str, ...]] = {
    # The helper's own claim, and the adoption transition it reaches.
    "src/services/tui_o/cutover/channel_gate.rs": ("claim", "claim_then_send", "decide_with_snapshot"),
    # Re-exports; a placement releases a pending adoption by design, with no body.
    "src/services/tui_o/cutover.rs": ("<module>", "claim_for_placement"),
    # The writer host's once-per-channel actor slot, not an adoption.
    "src/services/tui_o/writer/host.rs": ("spawn_hosts",),
    # Claimed right before the first unconfirmed chunk's edit or post, across a resumable loop.
    "src/services/discord/health/recovery.rs": ("maybe_recover_completed_stale_leak",),
}
# Funnel -> tests that drive it with O owning the channel. Each must exist as a
# non-ignored test-attributed `fn` in src/; empty funnels or missing tests block the flip.
FLIP_READY_TESTS: dict[str, tuple[str, ...]] = {
    "W01": (
        "o_delegated_watcher_turn_shows_no_body_and_records_no_frontier",
        "o_delegated_task_notification_turn_promotes_card_without_body_or_claim",
        "o_delegated_mid_turn_cutover_shows_no_post_cutover_body",
    ),
    "W02": ("o_delegated_rollover_tick_writes_no_body",),
    "W04": ("o_delegated_single_message_footer_completion_sends_no_body",),
    "W10": ("o_delegated_tui_body_is_cut_on_direct_gateways_but_not_headless",),
    "W11": ("o_delegated_cancelled_partial_body_is_not_replaced",),
    "W13": ("o_delegated_foreign_custody_follows_destination_membership",),
    "W20": ("o_delegated_idle_range_is_consumed_once_without_transport_or_evidence",),
    "W21": ("o_delegated_task_response_leaves_no_legacy_claim",),
    "W30": ("o_delegated_recovery_body_posts_only_the_marker_without_evidence",),
    "W31": ("o_delegated_captured_recovery_range_is_consumed_without_send",),
    "W33": ("o_delegated_stale_leak_recovery_resends_nothing",),
    "W34": ("o_delegated_restart_handoff_keeps_only_the_marker",),
    "W35": ("o_delegated_anchor_repost_is_skipped",),
    "backstop": ("o_delegated_done_turn_needs_no_legacy_delivery_confirmation",),
    "idle_recap": ("o_delegated_idle_recap_probe_reports_unknown",),
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


def _fn_bodies(text: str) -> list[tuple[str, int, int]]:
    """Each named fn's body span in stripped text (strings and comments are already blank)."""
    bodies = []
    for match in FN_RE.finditer(text):
        at = match.end()
        while at < len(text) and text[at] not in "{;":
            at += 1
        if at >= len(text) or text[at] == ";":
            continue
        depth, end = 0, at
        while end < len(text):
            depth += {"{": 1, "}": -1}.get(text[end], 0)
            if depth == 0:
                break
            end += 1
        bodies.append((match.group(1), at, end))
    return bodies


USE_RE = re.compile(r"(?:^|[;{}])\s*(?:pub(?:\s*\([^)]*\))?\s+)?use\s[^;]*$")


def _gate_sites(text: str, patterns=None) -> list[str]:
    """Each gate token as `innermost enclosing fn:kind`, in source order. Gate pins skip `use`
    items; the raw-claim scan (explicit `patterns`) keeps them, so a re-export is still seen."""
    bodies = _fn_bodies(text)
    found = []
    for kind, pattern in (patterns or GATE_RES).items():
        for match in pattern.finditer(text):
            if DEFN_RE.search(text[max(0, match.start() - 40) : match.start()]):
                continue
            if patterns is None and USE_RE.search(text[max(0, match.start() - 400) : match.start()]):
                continue
            owners = [b for b in bodies if b[1] < match.start() < b[2]]
            owner = max(owners, key=lambda b: b[1])[0] if owners else "<module>"
            found.append((match.start(), f"{owner}:{kind}"))
    return [site for _, site in sorted(found)]


def measure(root: Path, pinned_test_only_files=None):
    classifier = _load_classifier()
    if pinned_test_only_files is None:
        pinned_test_only_files = classifier.PINNED_TEST_ONLY_MODULE_FILES
    files, skips = classifier._scan_inputs(root, pinned_test_only_files)
    compiled = {name: re.compile(regex) for name, regex in PRIMITIVES.items()}
    primitives: dict[str, dict[str, int]] = {}
    gates: dict[str, list[str]] = {}
    raw: dict[str, list[str]] = {}
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
        if sites := _gate_sites(text):
            gates[rel] = sites
        raw_re = TUI_O_RAW_CLAIM_RE if rel.startswith(TUI_O_ROOT) else RAW_CLAIM_RE
        if sites := _gate_sites(text, {"raw": raw_re}):
            raw[rel] = [site.removesuffix(":raw") for site in sites]
        if n := len(FLAG_RE.findall(text)):
            flags[rel] = n
    return primitives, gates, flags, raw


def problems_for(primitives, gates, flags, raw) -> list[str]:
    problems: list[str] = []
    for rel, owners in sorted(raw.items()):
        for owner in sorted(set(owners) - set(RAW_CLAIM_SITES.get(rel, ()))):
            problems.append(
                f"raw claim: {rel} claims in {owner} outside claim_then_send and RAW_CLAIM_SITES"
            )
    # Fail closed: an exception whose claim moved away must leave the list with it.
    for rel, allowed in sorted(RAW_CLAIM_SITES.items()):
        for owner in sorted(set(allowed) - set(raw.get(rel, ()))):
            problems.append(f"raw claim: stale RAW_CLAIM_SITES entry {rel} {owner}")
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
        if (
            target not in TARGETS
            and not re.fullmatch(r"COV:W\d+", target)
            and not DEFER_RE.fullmatch(target)
        ):
            problems.append(f"census: {rel} has undecided target {target!r}")
            continue
        gate_file = PRIMITIVE_ROOT + (row[2] if len(row) > 2 else rel)
        claims = [site for site in gates.get(gate_file, []) if site.endswith(":claim")]
        if target in {"CUT_D", "CUT_T", "UNREACH_G"} and not claims:
            problems.append(f"census: {target} row {row[0]} has no claim gate in {gate_file}")
    for rel in sorted(set(EXPECTED_GATES) | set(gates)):
        want, have = list(EXPECTED_GATES.get(rel, ())), gates.get(rel, [])
        if want != have:
            problems.append(f"gate sites: {rel} has {have}, expected {want}")
        if gates.get(rel) and any(
            rel == evid or (evid.endswith("/") and rel.startswith(evid)) for evid in R_EVID
        ):
            problems.append(f"gate: cutover helper in R-EVID file {rel}")
    for rel in sorted(set(flags) - O_TUI_WRITER_FILES):
        problems.append(f"flag: O_TUI_WRITER outside {sorted(O_TUI_WRITER_FILES)}: {rel}")
    return problems


def flip_readiness(root: Path) -> tuple[bool, str]:
    """Fail closed: deferred rows, no funnel tests, or a missing test all block the flip."""
    reasons: list[str] = []
    deferred = sorted(rel for rel, row in CENSUS.items() if DEFER_RE.fullmatch(row[1]))
    if deferred:
        reasons.append(f"deferred census rows: {', '.join(deferred)}")
    names = sorted({name for tests in FLIP_READY_TESTS.values() for name in tests})
    if not names:
        reasons.append("FLIP_READY_TESTS is empty")
    empty = sorted(funnel for funnel, tests in FLIP_READY_TESTS.items() if not tests)
    if empty:
        reasons.append(f"funnel test lists empty: {', '.join(empty)}")
    defined: set[str] = set()
    src = root / "src"
    if names and src.is_dir():
        classifier = _load_classifier()
        fn_re = re.compile(
            r"((?:#\s*\[[^\[\]]*\]\s*)+)"
            r"(?:pub(?:\s*\([^)]*\))?\s+)?(?:async\s+)?fn\s+("
            + "|".join(map(re.escape, names)) + r")\s*\("
        )
        test_attr = re.compile(r"#\s*\[\s*(?:test|tokio\s*::\s*test(?:\s*\([^\[\]]*\))?)\s*\]")
        ignore_attr = re.compile(r"#\s*\[\s*ignore\b")
        for path in src.rglob("*.rs"):
            state = classifier.StripState()
            text = "\n".join(
                classifier.strip_line(line, state)
                for line in path.read_text(encoding="utf-8", errors="replace").splitlines()
            )
            # An #[ignore] test never runs by default, so it cannot vouch for the funnel.
            defined.update(
                name for attrs, name in fn_re.findall(text)
                if test_attr.search(attrs) and not ignore_attr.search(attrs)
            )
    missing = [name for name in names if name not in defined]
    if missing:
        reasons.append(f"funnel tests missing from src/: {', '.join(missing)}")
    if reasons:
        return False, "flip_ready=false: " + "; ".join(reasons)
    return True, f"flip_ready=true: {len(names)} funnel tests over {len(FLIP_READY_TESTS)} rows"


def check(root: Path, pinned_test_only_files=None) -> tuple[bool, str]:
    try:
        primitives, gates, flags, raw = measure(root, pinned_test_only_files)
    except RuntimeError as exc:
        return False, str(exc)
    problems = problems_for(primitives, gates, flags, raw)
    sites = sum(sum(m.values()) for m in primitives.values())
    deferred = sorted(rel for rel, row in CENSUS.items() if DEFER_RE.fullmatch(row[1]))
    if problems:
        return False, (
            "FAIL: TUI O writer census drifted.\n  " + "\n  ".join(problems)
            + "\nUpdate the maps in scripts/check_tui_o_writer_census.py in the same "
            "commit (see TO CHANGE A COUNT in its docstring)."
        )
    return True, (
        f"OK: TUI O writer census: {sites} send sites in {len(primitives)} files, "
        f"{sum(map(len, gates.values()))} cutover gate sites in {len(gates)} files, "
        f"{len(deferred)} rows deferred; lexical scan (see docstring)"
    )


def main(argv: list[str] | None = None) -> int:
    args = sys.argv[1:] if argv is None else argv
    root = Path(__file__).resolve().parent.parent
    ok, message = check(root)
    print(message, file=sys.stdout if ok else sys.stderr)
    ready, verdict = flip_readiness(root)
    print(verdict)
    if not ok:
        return 1
    return 1 if "--require-flip-ready" in args and not ready else 0


if __name__ == "__main__":
    raise SystemExit(main())
