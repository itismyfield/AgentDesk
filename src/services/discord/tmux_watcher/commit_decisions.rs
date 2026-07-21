//! #3038 S1 tmux watcher terminal commit decisions.

use super::*;

pub(super) fn watcher_tui_gate_blocks_lifecycle(
    gate_outcome: TuiCompletionGateOutcome,
    terminal_delivery_committed: bool,
) -> bool {
    let _ = (gate_outcome, terminal_delivery_committed);
    false
}

pub(super) fn watcher_commit_should_advance_runtime_binding(
    terminal_output_committed: bool,
    gate_outcome: TuiCompletionGateOutcome,
    terminal_delivery_committed: bool,
) -> bool {
    terminal_output_committed
        && !watcher_tui_gate_blocks_lifecycle(gate_outcome, terminal_delivery_committed)
}

#[allow(clippy::too_many_arguments)]
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

    // #3558: the old unlocked `load_inflight_state` → mutate → `save_inflight_state`
    // re-wrote `last_offset`/`response_sent_offset` from a stale snapshot, racing a
    // concurrent owner-gated `refresh_inflight_last_offset_*` advance and emitting a
    // spurious `response_sent_offset_monotonic` / `last_offset_monotonic` violation.
    // The locked RMW helper holds the sidecar flock across reload → identity guard →
    // patch → persist. The strong identity guard below (user_msg_id + started_at +
    // tmux_session + turn_start_offset, including the #3169 loop-turn pin) is enforced
    // inside the helper via `InflightTurnIdentity::matches_state`, which compares all
    // four fields — `expected_identity` already carries them — plus the caller-supplied
    // `tmux_session_name`. The commit IS the watermark owner, so it writes
    // `last_offset`/`response_sent_offset`, but the helper `max`-serializes both
    // against the in-lock reload so a late commit never moves them backward.
    let outcome = crate::services::discord::inflight::commit_watcher_terminal_delivery_locked(
        provider,
        channel_id.get(),
        expected_identity,
        tmux_session_name,
        crate::services::discord::inflight::WatcherTerminalCommitPatch {
            full_response: full_response.to_string(),
            last_offset,
            last_watcher_relayed_offset: Some(turn_data_start_offset),
            last_watcher_relayed_generation_mtime_ns: generation_mtime_ns,
        },
    );
    match outcome {
        crate::services::discord::inflight::WatcherTerminalCommitOutcome::Committed => {
            crate::services::discord::outbound::delivery_record::record_delivered_content_fingerprint(
                provider,
                channel_id,
                tmux_session_name,
                full_response,
            );
            true
        }
        crate::services::discord::inflight::WatcherTerminalCommitOutcome::Skipped => false,
        crate::services::discord::inflight::WatcherTerminalCommitOutcome::IoError => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                tmux_session = %tmux_session_name,
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

/// #3350 issue-1 pure core: must a committed pass tombstone+drain the row even
/// when the TUI-direct anchor body was NOT visible (e.g. a suppressed
/// task-notification completion)? A watcher-owned `ExternalInput` synthetic
/// row converges its anchor `⏳ → ✅` on EVERY committed pass — suppressed
/// included (`terminal_output_committed = relay_ok || relay_suppressed`) — and
/// its #3303/#3350 DeferredClaim marker pins exactly this row's identity, so
/// skipping the tombstone lets the TTL sweep stack a false `⚠` on that `✅`.
/// Anything else (bridge-owned, Managed, id-0, session-less) keeps the #3296
/// body-visible-only tombstone scope.
pub(super) fn committed_synthetic_commit_requires_marker_tombstone(
    turn_source_external: bool,
    relay_owner_watcher: bool,
    user_msg_id: u64,
    tmux_session_present: bool,
) -> bool {
    turn_source_external && relay_owner_watcher && user_msg_id != 0 && tmux_session_present
}

/// Row adapter for [`committed_synthetic_commit_requires_marker_tombstone`].
pub(super) fn committed_row_requires_marker_tombstone(
    row: &crate::services::discord::inflight::InflightTurnState,
) -> bool {
    use crate::services::discord::inflight::{RelayOwnerKind, TurnSource};
    committed_synthetic_commit_requires_marker_tombstone(
        row.turn_source == TurnSource::ExternalInput,
        row.relay_owner_kind == RelayOwnerKind::Watcher,
        row.user_msg_id,
        row.tmux_session_name.is_some(),
    )
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
    fn busy_observation_without_delivery_still_advances_runtime_binding() {
        assert!(watcher_commit_should_advance_runtime_binding(
            true,
            TuiCompletionGateOutcome::BusyObserved,
            false,
        ));
    }

    #[test]
    fn busy_observation_without_terminal_delivery_allows_cleanup() {
        let side_effects = watcher_terminal_commit_side_effects_for_test(
            true,
            TuiCompletionGateOutcome::BusyObserved,
            false,
        );

        assert!(side_effects.advance_runtime_binding);
        assert!(side_effects.advance_confirmed_end);
        assert!(side_effects.clear_inflight);
        assert!(side_effects.finish_restored_turn);
        assert!(!side_effects.late_output_retry_possible);

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
    fn tui_completion_gate_busy_observation_after_terminal_delivery_allows_lifecycle_cleanup() {
        let side_effects = watcher_terminal_commit_side_effects_for_test(
            true,
            TuiCompletionGateOutcome::BusyObserved,
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

    /// #3350 issue-1: a suppressed (body-invisible) committed pass must still
    /// tombstone a watcher-owned ExternalInput synthetic row — that is the
    /// exact class whose `⏳ → ✅` block fires while the old body-visible-only
    /// tombstone gate skipped, stacking a TTL `⚠` on the `✅`.
    #[test]
    fn suppressed_commit_requires_tombstone_only_for_watcher_synthetic_rows() {
        // The false-⚠ class: watcher-owned synthetic row with a real anchor.
        assert!(committed_synthetic_commit_requires_marker_tombstone(
            true, true, 42, true
        ));
        // Bridge-owned synthetic turn (SC3): finalizes via the bridge, no
        // watcher tombstone owed outside the body-visible #3296 scope.
        assert!(!committed_synthetic_commit_requires_marker_tombstone(
            true, false, 42, true
        ));
        // Managed (non-synthetic) row keeps the #3296 body-visible-only scope.
        assert!(!committed_synthetic_commit_requires_marker_tombstone(
            false, true, 42, true
        ));
        // id-0 rows can never carry an own-pin marker (record rejects zero).
        assert!(!committed_synthetic_commit_requires_marker_tombstone(
            true, true, 0, true
        ));
        // Session-less rows are outside every marker's reconcile scope.
        assert!(!committed_synthetic_commit_requires_marker_tombstone(
            true, true, 42, false
        ));
    }
}
