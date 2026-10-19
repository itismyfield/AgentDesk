//! Default rollout wait, drain, and recovery budgets.

use std::time::Duration;

use super::RolloutTailOptions;

const DEFAULT_ROLLOUT_WAIT_SECS: u64 = 30;
/// Legacy EOF drain fallback; modern turns complete from provider records or hooks.
/// Tool-call gating suppresses this drain while tools remain in flight.
const DEFAULT_TERMINAL_DRAIN_MS: u64 = 1000;
/// Event-less rollouts need a longer quiet window to survive pauses between output bursts.
const DEFAULT_LEGACY_TERMINAL_DRAIN_MS: u64 = 15000;
/// Upper bound on how long the tailer will sit at EOF waiting for the assistant
/// response to begin streaming. Without this guard, a stuck Codex TUI (tool
/// loop, network hang, etc.) keeps the tailer thread alive indefinitely and the
/// caller never sees a terminal `StreamMessage::Done`.
const DEFAULT_ASSISTANT_RESPONSE_DEADLINE_SECS: u64 = 30 * 60;
/// #2419 follow-up: bounded recovery for a pending tool call whose matching
/// `function_call_output` never arrives (hung tool, malformed line, call_id
/// mismatch, Codex schema skew). Without this, `has_pending_tool_call()`
/// would hold the drain gate shut forever while the tmux pane stays alive,
/// stranding the Discord turn. 5 minutes of inactivity after the last
/// lifecycle event is well past any realistic tool runtime — at that point
/// we surface a terminal Done so the bridge can advance.
const DEFAULT_PENDING_TOOL_CALL_DEADLINE_SECS: u64 = 5 * 60;
/// A fresh pane marker may extend pending-tool recovery, but never indefinitely.
/// Two additional minutes start only after the five-minute inactivity deadline,
/// covering normal foreground commands that typically finish within seconds or
/// tens of seconds while still forcing a wedged TUI turn to fail open.
pub(super) const DEFAULT_PANE_BUSY_VETO_CAP_SECS: u64 = 2 * 60;

impl Default for RolloutTailOptions {
    fn default() -> Self {
        Self {
            wait_for_rollout: Duration::from_secs(DEFAULT_ROLLOUT_WAIT_SECS),
            terminal_drain: Duration::from_millis(DEFAULT_TERMINAL_DRAIN_MS),
            assistant_response_deadline: Some(Duration::from_secs(
                DEFAULT_ASSISTANT_RESPONSE_DEADLINE_SECS,
            )),
            pending_tool_call_deadline: Some(Duration::from_secs(
                DEFAULT_PENDING_TOOL_CALL_DEADLINE_SECS,
            )),
            enable_task_complete_fast_path: true,
            legacy_terminal_drain: Some(Duration::from_millis(DEFAULT_LEGACY_TERMINAL_DRAIN_MS)),
            apply_pending_tool_deadline_without_assistant_text: true,
            tmux_session_name: None,
            discord_origin_prompt: None,
            pane_busy_probe: None,
            pane_busy_veto_cap: Duration::from_secs(DEFAULT_PANE_BUSY_VETO_CAP_SECS),
            terminal_range_eligible: false,
            own_turn: None,
        }
    }
}
