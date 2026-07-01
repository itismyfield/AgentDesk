//! #3038 (giant-file decompose, registry deadline 2026-08-31): the early TUI
//! completion gate — the #2293/#2780 pre-mailbox-release quiescence probe that
//! computes `bridge_tui_gate_outcome_early` + `bridge_early_gate_timed_out` —
//! moved verbatim out of the middle of `spawn_turn_bridge`'s async body.
//!
//! Unlike the tail-block epilogue (`finalize_epilogue.rs`), this block sits
//! mid-body and its two outputs are consumed later (the timed-out flag by the
//! watcher-handoff + mailbox-release sites, the outcome by the late-gate reuse
//! at the `bridge_gate_outcome` site), so the context it reads is threaded in by
//! SHARED REFERENCE (`inflight_state`, `provider` — both still live afterwards)
//! and by `Copy` value (`channel_id`, the four eligibility flags), and the two
//! computed values are RETURNED — the `#[cfg]` `let` declarations stay at the
//! call site so their exact `#[cfg(unix)]` / `#[cfg(not(unix))]` split is
//! preserved. Behavior-preserving-by-construction: the eligibility filter, the
//! `run_tui_completion_gate` call, the timed-out warning, and the two `matches!`
//! are byte-identical. The only textual changes from the original block are the
//! three discord-level `super::tmux::` refs deepened to `super::super::tmux::`
//! from the child (same seam-fix as `finalize_epilogue.rs`) and `&provider` ->
//! `provider` because the owned local is now threaded in by shared reference.
//! The whole module is `#[cfg(unix)]` (via the gated `mod` declaration) because
//! the block is unix-only: on non-unix the flag is a plain `= false` at the call
//! site and the outcome value does not exist. All other deps reach via
//! `use super::*;`.

use super::*;

/// Run the #2293/#2780 early TUI quiescence gate BEFORE the visible
/// completion/status cleanup (and BEFORE the channel-mailbox release), applying
/// the same eligibility filter the late gate uses. Returns
/// `(bridge_tui_gate_outcome_early, bridge_early_gate_timed_out)` computed
/// exactly as the inline block did. Do NOT treat this as a mailbox correctness
/// primitive — see the call-site comment; the hosted-TUI pre-submit guard is the
/// correctness barrier.
pub(super) async fn run_early_tui_completion_gate(
    cancelled: bool,
    is_prompt_too_long: bool,
    transport_error: bool,
    recovery_retry: bool,
    inflight_state: &InflightTurnState,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> (Option<super::super::tmux::TuiCompletionGateOutcome>, bool) {
    let bridge_early_gate_timed_out;
    #[allow(unused_assignments, unused_mut)]
    let mut bridge_tui_gate_outcome_early: Option<
        super::super::tmux::TuiCompletionGateOutcome,
    > = None;
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
                channel = channel_id.get(),
                tmux_session = %tmux_session_name,
                inflight_user_msg_id = inflight_state.user_msg_id,
                inflight_current_msg_id = inflight_state.current_msg_id,
                "early TUI completion gate suppressed for relay-only synthetic turn"
            );
            bridge_tui_gate_outcome_early =
                Some(super::super::tmux::TuiCompletionGateOutcome::TimedOut);
            bridge_early_gate_timed_out = true;
            return (bridge_tui_gate_outcome_early, bridge_early_gate_timed_out);
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
        if matches!(
            bridge_tui_gate_outcome_early,
            Some(super::super::tmux::TuiCompletionGateOutcome::TimedOut)
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id.get(),
                tmux_session = %tmux_session_name,
                "[{ts}] ⚠ #2293/#2780: bridge TUI quiescence gate timed out before visible completion; mailbox release will continue and hosted-TUI pre-submit will guard follow-up injection"
            );
        }
    }
    bridge_early_gate_timed_out = matches!(
        bridge_tui_gate_outcome_early,
        Some(super::super::tmux::TuiCompletionGateOutcome::TimedOut)
    );
    (bridge_tui_gate_outcome_early, bridge_early_gate_timed_out)
}
