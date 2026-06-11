//! #3038 S1 tmux watcher terminal commit decisions.

use super::*;

pub(super) fn watcher_tui_gate_blocks_lifecycle(
    gate_outcome: TuiCompletionGateOutcome,
    terminal_delivery_committed: bool,
) -> bool {
    matches!(gate_outcome, TuiCompletionGateOutcome::TimedOut) && !terminal_delivery_committed
}

pub(super) fn watcher_commit_should_advance_runtime_binding(
    terminal_output_committed: bool,
    gate_outcome: TuiCompletionGateOutcome,
    terminal_delivery_committed: bool,
) -> bool {
    terminal_output_committed
        && !watcher_tui_gate_blocks_lifecycle(gate_outcome, terminal_delivery_committed)
}

pub(super) fn mark_watcher_terminal_delivery_committed(
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    expected_identity: Option<&crate::services::discord::inflight::InflightTurnIdentity>,
    full_response: &str,
    turn_data_start_offset: u64,
    generation_mtime_ns: Option<i64>,
    last_offset: u64,
) -> bool {
    let Some(expected_identity) = expected_identity else {
        return false;
    };
    if full_response.trim().is_empty() {
        return false;
    }
    // #3169 P1: self-paced loop turns carry `user_msg_id == 0` (no anchored
    // Discord user message), so the original `user_msg_id != 0` requirement
    // skipped them entirely — they never set `terminal_delivery_committed`, and
    // the #3126 stall-watchdog guard (recovery.rs:1346) had no architectural
    // "this turn finished delivering" signal for them, producing the death #1
    // false-positive force-clean. Allow `user_msg_id == 0` turns to commit, but
    // (NOT a blanket relaxation) only when the frame-carried identity is fully
    // anchored: such turns are disambiguated solely by `started_at` +
    // `turn_start_offset` (#3041 P1-3, inflight.rs:669), so a loop turn without a
    // known `turn_start_offset` cannot be safely matched and is still skipped.
    let is_loop_turn = expected_identity.user_msg_id == 0;
    if is_loop_turn && expected_identity.turn_start_offset.is_none() {
        return false;
    }
    let Some(mut inflight) =
        crate::services::discord::inflight::load_inflight_state(provider, channel_id.get())
    else {
        return false;
    };
    if inflight.restart_mode.is_some() || inflight.rebind_origin {
        return false;
    }
    if inflight.user_msg_id != expected_identity.user_msg_id
        || inflight.started_at.as_str() != expected_identity.started_at.as_str()
        || inflight.tmux_session_name.as_deref() != expected_identity.tmux_session_name.as_deref()
        || inflight.tmux_session_name.as_deref() != Some(tmux_session_name)
    {
        return false;
    }
    // #3169 P1: for a loop turn (`user_msg_id == 0`) the 1-second-resolution
    // `started_at` can collide across two consecutive self-triggered turns, so it
    // is insufficient to prove this completion belongs to the loaded inflight.
    // Require the monotonic `turn_start_offset` (#3041 P1-3) to match as well so a
    // late completion can never commit the WRONG (newer, still-running) loop turn.
    if is_loop_turn && inflight.turn_start_offset != expected_identity.turn_start_offset {
        return false;
    }

    inflight.terminal_delivery_committed = true;
    inflight.full_response = full_response.to_string();
    inflight.response_sent_offset = full_response.len();
    inflight.last_offset = last_offset;
    inflight.last_watcher_relayed_offset = Some(turn_data_start_offset);
    inflight.last_watcher_relayed_generation_mtime_ns = generation_mtime_ns;

    match crate::services::discord::inflight::save_inflight_state(&inflight) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id.get(),
                tmux_session = %tmux_session_name,
                error = %error,
                "watcher failed to mirror committed terminal delivery into inflight state"
            );
            false
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct WatcherTerminalCommitSideEffects {
    pub(super) advance_runtime_binding: bool,
    pub(super) advance_confirmed_end: bool,
    pub(super) clear_inflight: bool,
    pub(super) finish_restored_turn: bool,
    pub(super) late_output_retry_possible: bool,
}

#[cfg(test)]
pub(super) fn watcher_terminal_commit_side_effects_for_test(
    terminal_output_committed: bool,
    gate_outcome: TuiCompletionGateOutcome,
    terminal_delivery_committed: bool,
) -> WatcherTerminalCommitSideEffects {
    let lifecycle_allowed = terminal_output_committed
        && !watcher_tui_gate_blocks_lifecycle(gate_outcome, terminal_delivery_committed);
    WatcherTerminalCommitSideEffects {
        advance_runtime_binding: watcher_commit_should_advance_runtime_binding(
            terminal_output_committed,
            gate_outcome,
            terminal_delivery_committed,
        ),
        advance_confirmed_end: lifecycle_allowed,
        clear_inflight: lifecycle_allowed,
        finish_restored_turn: lifecycle_allowed,
        late_output_retry_possible: terminal_output_committed && !lifecycle_allowed,
    }
}

pub(super) fn watcher_terminal_kind_requires_tui_completion_gate(
    terminal_kind: Option<WatcherTerminalKind>,
) -> bool {
    !matches!(terminal_kind, Some(WatcherTerminalKind::SoftUserBoundary))
}

pub(super) fn missing_inflight_after_session_bound_delivery(
    inflight_missing: bool,
    session_bound_relay_delivered: bool,
) -> bool {
    inflight_missing && !session_bound_relay_delivered
}

#[cfg(test)]
mod runtime_binding_offset_tests {
    use super::*;

    #[test]
    fn committed_watcher_output_advances_runtime_binding_even_without_inflight() {
        assert!(watcher_commit_should_advance_runtime_binding(
            true,
            TuiCompletionGateOutcome::ConfirmedIdle,
            false,
        ));
    }

    #[test]
    fn uncommitted_watcher_output_does_not_advance_runtime_binding() {
        assert!(!watcher_commit_should_advance_runtime_binding(
            false,
            TuiCompletionGateOutcome::ConfirmedIdle,
            false,
        ));
    }

    #[test]
    fn tui_timeout_without_delivery_keeps_previous_runtime_binding() {
        assert!(!watcher_commit_should_advance_runtime_binding(
            true,
            TuiCompletionGateOutcome::TimedOut,
            false,
        ));
    }

    #[test]
    fn tui_completion_gate_timeout_without_terminal_delivery_preserves_cleanup_for_retry() {
        let side_effects = watcher_terminal_commit_side_effects_for_test(
            true,
            TuiCompletionGateOutcome::TimedOut,
            false,
        );

        assert!(!side_effects.advance_runtime_binding);
        assert!(!side_effects.advance_confirmed_end);
        assert!(!side_effects.clear_inflight);
        assert!(!side_effects.finish_restored_turn);
        assert!(side_effects.late_output_retry_possible);

        let confirmed = watcher_terminal_commit_side_effects_for_test(
            true,
            TuiCompletionGateOutcome::ConfirmedIdle,
            false,
        );
        assert!(confirmed.advance_runtime_binding);
        assert!(confirmed.advance_confirmed_end);
        assert!(confirmed.clear_inflight);
        assert!(confirmed.finish_restored_turn);
        assert!(!confirmed.late_output_retry_possible);
    }

    #[test]
    fn tui_completion_gate_timeout_after_terminal_delivery_allows_lifecycle_cleanup() {
        let side_effects = watcher_terminal_commit_side_effects_for_test(
            true,
            TuiCompletionGateOutcome::TimedOut,
            true,
        );

        assert!(side_effects.advance_runtime_binding);
        assert!(side_effects.advance_confirmed_end);
        assert!(side_effects.clear_inflight);
        assert!(side_effects.finish_restored_turn);
        assert!(!side_effects.late_output_retry_possible);
    }

    #[test]
    fn soft_user_boundary_terminal_skips_tui_completion_gate() {
        assert!(!watcher_terminal_kind_requires_tui_completion_gate(Some(
            WatcherTerminalKind::SoftUserBoundary
        )));
        assert!(watcher_terminal_kind_requires_tui_completion_gate(Some(
            WatcherTerminalKind::SoftStopHookSummary
        )));
        assert!(watcher_terminal_kind_requires_tui_completion_gate(Some(
            WatcherTerminalKind::HardResult
        )));
        assert!(watcher_terminal_kind_requires_tui_completion_gate(None));
    }

    #[test]
    fn acknowledged_session_bound_delivery_is_not_missing_inflight_fallback() {
        assert!(!missing_inflight_after_session_bound_delivery(true, true));
        assert!(missing_inflight_after_session_bound_delivery(true, false));
        assert!(!missing_inflight_after_session_bound_delivery(false, false));
    }
}
