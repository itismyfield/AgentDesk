//! Pure recovery-phase decision predicates (#3479 r8 split).
//!
//! Behavior-preserving extraction from `recovery_engine.rs`: these are the
//! side-effect-free predicates that classify an `InflightTurnState` and map a
//! recovery situation onto a `RecoveryPhase`. They depend only on the in-memory
//! inflight snapshot and the `RecoveryPhase` enum (re-resolved via `super::*`).
//! Readiness probes that touch `tui_turn_state`/`provider` stay in the root
//! module because they are not pure state predicates.

use super::*;

fn canonical_recovery_phase(phase: RecoveryPhase) -> RecoveryPhase {
    RecoveryPhase::from_optional_str(Some(phase.as_str())).unwrap_or(phase)
}

fn can_replace_stale_rebind_inflight(state: &inflight::InflightTurnState) -> bool {
    state.rebind_origin
        && state.full_response.trim().is_empty()
        && state.last_watcher_relayed_offset.is_none()
}

fn can_resume_existing_rebind_inflight(state: &inflight::InflightTurnState) -> bool {
    let ownerless_live_relay = state.effective_relay_owner_kind() == inflight::RelayOwnerKind::None;
    let unstarted_relay =
        state.full_response.trim().is_empty() && state.last_watcher_relayed_offset.is_none();
    let has_restore_anchor = state
        .tmux_session_name
        .as_deref()
        .is_some_and(|name| !name.trim().is_empty())
        && state
            .output_path
            .as_deref()
            .is_some_and(|path| !path.trim().is_empty());
    let planned_restart_ownerless_restore = state.restart_mode.is_some()
        && ownerless_live_relay
        && has_restore_anchor
        && recovery_has_post_work_ready_evidence(state);

    !state.rebind_origin
        && state.request_owner_user_id != 0
        && state.user_msg_id != 0
        && state.current_msg_id != 0
        && !state.terminal_delivery_committed
        && ((state.restart_mode.is_none() && (unstarted_relay || ownerless_live_relay))
            || planned_restart_ownerless_restore)
}

pub(super) fn recovery_phase_for_existing_inflight_rebind(
    state: &inflight::InflightTurnState,
) -> RecoveryPhase {
    let phase = match (
        can_replace_stale_rebind_inflight(state),
        can_resume_existing_rebind_inflight(state),
    ) {
        (true, _) => RecoveryPhase::WatcherReattach,
        (false, true) => RecoveryPhase::InflightRestore,
        (false, false) => RecoveryPhase::Pending,
    };
    canonical_recovery_phase(phase)
}

pub(super) fn can_fast_path_captured_full_response(
    state: &inflight::InflightTurnState,
    output_already_completed: bool,
) -> bool {
    // Deliberately keep the gate narrow: only real user-authored inflights
    // with a captured but completely unsent response are eligible, and only
    // after authoritative completion evidence exists in the output stream.
    !state.rebind_origin
        && output_already_completed
        && !state.full_response.trim().is_empty()
        && state.response_sent_offset == 0
        && state.last_watcher_relayed_offset.is_none()
}

pub(super) fn recovery_phase_after_output_scan(
    output_already_completed: bool,
    tmux_ready_without_new_output: bool,
) -> RecoveryPhase {
    let phase = match (output_already_completed, tmux_ready_without_new_output) {
        (true, _) | (_, true) => RecoveryPhase::Done,
        (false, false) => RecoveryPhase::Pending,
    };
    canonical_recovery_phase(phase)
}

pub(super) fn recovery_has_post_work_ready_evidence(state: &inflight::InflightTurnState) -> bool {
    !state.full_response.trim().is_empty()
        || state.any_tool_used
        || state.has_post_tool_text
        || state
            .current_tool_line
            .as_deref()
            .is_some_and(|line| !line.trim().is_empty())
        || state
            .last_tool_name
            .as_deref()
            .is_some_and(|name| !name.trim().is_empty())
        || state
            .last_tool_summary
            .as_deref()
            .is_some_and(|summary| !summary.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::provider::ProviderKind;

    #[test]
    fn ownerless_live_inflight_with_partial_response_can_reattach_watcher() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            1_479_671_301_387_059_200,
            Some("adk-cdx".to_string()),
            343_742_347_365_974_026,
            1_518_710_986_180_137_051,
            1_518_719_883_708_207_285,
            "diagnose relay".to_string(),
            Some("session".to_string()),
            None,
            Some("/tmp/rollout.jsonl".to_string()),
            None,
            0,
        );
        state.full_response = "partial response".to_string();
        state.response_sent_offset = state.full_response.len();
        state.set_relay_owner_kind(inflight::RelayOwnerKind::None);

        assert_eq!(
            recovery_phase_for_existing_inflight_rebind(&state),
            RecoveryPhase::InflightRestore,
            "relay recovery must be able to reattach a watcher to a real live inflight whose watcher owner was lost"
        );
    }

    #[test]
    fn planned_restart_ownerless_live_inflight_with_restore_anchor_can_reattach_watcher() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            1_479_671_301_387_059_200,
            Some("adk-cdx".to_string()),
            343_742_347_365_974_026,
            1_518_710_986_180_137_051,
            1_518_719_883_708_207_285,
            "diagnose relay".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/rollout.jsonl".to_string()),
            None,
            0,
        );
        state.restart_mode = Some(crate::services::discord::InflightRestartMode::DrainRestart);
        state.full_response = "partial response".to_string();
        state.response_sent_offset = state.full_response.len();
        state.set_relay_owner_kind(inflight::RelayOwnerKind::None);

        assert_eq!(
            recovery_phase_for_existing_inflight_rebind(&state),
            RecoveryPhase::InflightRestore,
            "planned restart markers from deploy must not block restoring an ownerless real turn"
        );
    }
}

pub(super) fn recovery_ready_without_output_already_delivered(
    state: &inflight::InflightTurnState,
) -> bool {
    state.response_sent_offset > 0 || state.last_watcher_relayed_offset.is_some()
}

pub(super) fn recovery_ready_without_output_has_captured_response(
    state: &inflight::InflightTurnState,
) -> bool {
    !state.full_response.trim().is_empty()
}

pub(super) fn recovery_terminal_delivery_already_committed(
    state: &inflight::InflightTurnState,
) -> bool {
    // Planned restart and rebind-origin rows carry their own lifecycle owners:
    // this fast path is only for stale ordinary turns whose Discord terminal
    // response was already delivered before recovery tried to re-register them.
    state.terminal_delivery_completed() && state.restart_mode.is_none() && !state.rebind_origin
}

pub(super) fn recovery_phase_after_tmux_probe(
    can_recover: bool,
    pane_alive: Option<bool>,
) -> RecoveryPhase {
    let phase = match (can_recover, pane_alive) {
        (false, _) => RecoveryPhase::Done,
        (true, Some(true)) => RecoveryPhase::WatcherReattach,
        (true, Some(false)) => RecoveryPhase::InflightRestore,
        (true, None) => RecoveryPhase::Pending,
    };
    canonical_recovery_phase(phase)
}
