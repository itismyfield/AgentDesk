//! #3038 (giant-file decompose, registry deadline 2026-08-31): the early TUI
//! completion gate — the #2293/#2780 pre-mailbox-release observation that
//! computes `bridge_tui_gate_outcome_early` — moved verbatim out of the middle
//! of `spawn_turn_bridge`'s async body.
//!
//! Unlike the tail-block epilogue (`finalize_epilogue.rs`), this block sits
//! mid-body and its outcome is consumed later by the late-gate reuse at the
//! `bridge_gate_outcome` site. The context it reads is threaded in by SHARED
//! REFERENCE (`inflight_state`, `provider` — both still live afterwards) and by
//! `Copy` value (`channel_id`, the four eligibility flags); the exact
//! `#[cfg(unix)]` / `#[cfg(not(unix))]` split remains at the call site.

use super::*;

/// Run the #2293/#2780 early TUI quiescence gate BEFORE the visible
/// completion/status cleanup (and BEFORE the channel-mailbox release), applying
/// the same eligibility filter the late gate uses. Do NOT treat this as a
/// mailbox correctness primitive — see the call-site comment; the hosted-TUI
/// pre-submit guard is the correctness barrier.
pub(super) async fn run_early_tui_completion_gate(
    cancelled: bool,
    is_prompt_too_long: bool,
    transport_error: bool,
    recovery_retry: bool,
    inflight_state: &InflightTurnState,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> Option<super::super::tmux::TuiCompletionGateOutcome> {
    let mut bridge_tui_gate_outcome_early: Option<super::super::tmux::TuiCompletionGateOutcome> =
        None;
    // Reproduce the same eligibility filter the late gate already
    // applies, but BEFORE the channel-mailbox release.
    let eligible_for_early_gate =
        !cancelled && !is_prompt_too_long && !transport_error && !recovery_retry;
    if eligible_for_early_gate
        && let Some(tmux_session_name) = inflight_state.tmux_session_name.as_deref()
    {
        if inflight_state.relay_ownership_only {
            tracing::info!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                tmux_session = %tmux_session_name,
                inflight_user_msg_id = inflight_state.user_msg_id,
                inflight_current_msg_id = inflight_state.current_msg_id,
                "early TUI completion observation skipped for relay-only synthetic turn"
            );
            bridge_tui_gate_outcome_early =
                Some(super::super::tmux::TuiCompletionGateOutcome::NotGated);
            return bridge_tui_gate_outcome_early;
        }
        bridge_tui_gate_outcome_early = Some(
            super::super::tmux::run_tui_completion_gate(
                provider,
                channel_id,
                tmux_session_name,
                inflight_state.task_notification_kind,
            )
            .await,
        );
    }
    bridge_tui_gate_outcome_early
}
