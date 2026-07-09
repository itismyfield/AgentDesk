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

fn has_restore_anchor(state: &inflight::InflightTurnState) -> bool {
    state
        .tmux_session_name
        .as_deref()
        .is_some_and(|name| !name.trim().is_empty())
        && state
            .output_path
            .as_deref()
            .is_some_and(|path| !path.trim().is_empty())
}

fn can_resume_existing_rebind_inflight(state: &inflight::InflightTurnState) -> bool {
    let ownerless_live_relay = state.effective_relay_owner_kind() == inflight::RelayOwnerKind::None;
    let unstarted_relay =
        state.full_response.trim().is_empty() && state.last_watcher_relayed_offset.is_none();
    let planned_restart_ownerless_restore = state.restart_mode.is_some()
        && ownerless_live_relay
        && has_restore_anchor(state)
        && recovery_has_post_work_ready_evidence(state);

    !state.rebind_origin
        && state.request_owner_user_id != 0
        && state.user_msg_id != 0
        && state.current_msg_id != 0
        && !state.terminal_delivery_committed
        && ((state.restart_mode.is_none() && (unstarted_relay || ownerless_live_relay))
            || planned_restart_ownerless_restore)
}

/// #4400 (b): adopt the headless synthetic row that a dying watcher's #3107
/// self-heal (`reacquire_watcher_inflight_for_active_stream`,
/// `tmux_watcher/liveness.rs`) re-mints AFTER the stall-watchdog force-clean
/// deleted the real row. That row is zero-id (`user_msg_id == 0`,
/// `request_owner_user_id == 0`), watcher-owned, and carries the restore
/// anchors of the still-live tmux stream — but it fits neither the replace arm
/// (needs `rebind_origin`) nor the resume arm (needs non-zero ids), so pre-fix
/// the respawn preflight classified it `Pending` and every watchdog tick's
/// respawn died with `InflightAlreadyExists`: a permanent 409 self-deadlock
/// that left the relay dead until the next inbound message (#4400 16:32Z
/// incident).
///
/// Adoption routes the row onto the existing `InflightRestore` resume
/// machinery, which starts the watcher at the row's committed offset (no
/// rebase — invariant I3) so the backlog written while the watcher was dead is
/// still relayed. Every conjunct is load-bearing:
///   - `!rebind_origin` — rebind-origin rows have their own replace/reap
///     lifecycle (#3581) and must stay on it.
///   - zero ids — a REAL user turn is never adopted away from its owner
///     (invariant I2); it stays on the resume/`Pending` arms.
///   - `!terminal_delivery_committed` — a committed row keeps today's
///     `Pending` outcome so the committed-cleanup path stays authoritative.
///   - Watcher-owned — only the watcher self-heal mints this shape; a
///     bridge-owned/default (`None`) zero-id row is not the #4400 orphan.
///   - restore anchors — without a tmux session + output path there is
///     nothing to reattach a watcher to.
///
/// The predicate body lives on `InflightTurnState`
/// (`is_adoptable_orphaned_synthetic_watcher_row`) because the adoption-save
/// identity gate and the adopted-transcript offset check must apply the SAME
/// shape test — review r2 found that classifying adopt here while the deeper
/// layers still refused the zero-id row merely converted the 409 loop into a
/// 500 loop.
fn can_adopt_orphaned_synthetic_watcher_row(state: &inflight::InflightTurnState) -> bool {
    state.is_adoptable_orphaned_synthetic_watcher_row()
}

pub(super) fn recovery_phase_for_existing_inflight_rebind(
    state: &inflight::InflightTurnState,
) -> RecoveryPhase {
    let phase = match (
        can_replace_stale_rebind_inflight(state),
        can_resume_existing_rebind_inflight(state),
        can_adopt_orphaned_synthetic_watcher_row(state),
    ) {
        (true, _, _) => RecoveryPhase::WatcherReattach,
        (false, true, _) | (false, false, true) => RecoveryPhase::InflightRestore,
        (false, false, false) => RecoveryPhase::Pending,
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

    /// The row exactly as the #3107 watcher self-heal
    /// (`reacquire_watcher_inflight_for_active_stream`) re-mints it after a
    /// stall-watchdog force-clean deleted the real row: headless zero ids,
    /// placeholder `current_msg_id`, Watcher relay owner, live-stream anchors.
    fn orphaned_synthetic_watcher_row() -> inflight::InflightTurnState {
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Claude,
            1_479_671_298_497_183_835,
            None,
            0,                         // request_owner_user_id — headless re-acquire, no owner
            0,                         // user_msg_id — synthetic-turn signal
            1_518_888_000_000_000_001, // current_msg_id — surviving placeholder/panel message
            String::new(),
            None,
            Some("AgentDesk-claude-adk-cc".to_string()),
            Some("/tmp/claude-transcript.jsonl".to_string()),
            None,
            8_192,
        );
        state.turn_source = inflight::TurnSource::ExternalInput;
        state.set_relay_owner_kind(inflight::RelayOwnerKind::Watcher);
        state
    }

    /// #4400 (b) adoption quadrant table. Every row is a mutation kill for one
    /// conjunct of `can_adopt_orphaned_synthetic_watcher_row`: delete that
    /// conjunct (or the adoption arm itself) and the row's own assert fails.
    #[test]
    fn orphaned_synthetic_watcher_row_adoption_quadrants() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

        // (1) The #4400 orphan itself: adopted onto the resume machinery
        // instead of the pre-fix Pending → permanent 409 (kills the arm).
        assert_eq!(
            recovery_phase_for_existing_inflight_rebind(&orphaned_synthetic_watcher_row()),
            RecoveryPhase::InflightRestore,
            "the re-minted zero-id watcher-owned orphan must be adopted, not 409ed forever (I1)"
        );

        // (2) Bridge-owned/default zero-id row: NOT the self-heal shape.
        // (kills the Watcher-owner conjunct)
        let mut bridge_owned = orphaned_synthetic_watcher_row();
        bridge_owned.set_relay_owner_kind(inflight::RelayOwnerKind::None);
        assert_eq!(
            recovery_phase_for_existing_inflight_rebind(&bridge_owned),
            RecoveryPhase::Pending,
            "a bridge-owned zero-id row is not the #4400 orphan and keeps today's Pending"
        );

        // (3) A REAL live turn (non-zero ids, watcher-owned, already relayed a
        // prefix) must never be adopted away from its owner (I2).
        // (kills the user_msg_id == 0 conjunct)
        let mut real_turn = orphaned_synthetic_watcher_row();
        real_turn.request_owner_user_id = 343_742_347_365_974_026;
        real_turn.user_msg_id = 1_518_710_986_180_137_051;
        real_turn.current_msg_id = 1_518_719_883_708_207_285;
        real_turn.full_response = "already relayed prefix".to_string();
        real_turn.last_watcher_relayed_offset = Some(2_048);
        assert_eq!(
            recovery_phase_for_existing_inflight_rebind(&real_turn),
            RecoveryPhase::Pending,
            "a real live turn stays unadoptable — rebind must 409, never steal it (I2)"
        );
        // (3b) The split-id variants keep each zero-id conjunct independently
        // load-bearing: owner set but user_msg_id 0 …
        let mut owned_headless = orphaned_synthetic_watcher_row();
        owned_headless.request_owner_user_id = 343_742_347_365_974_026;
        assert_eq!(
            recovery_phase_for_existing_inflight_rebind(&owned_headless),
            RecoveryPhase::Pending,
            "an owner-carrying headless row is not the ownerless #4400 orphan (I2)"
        );
        // … and user_msg_id set but owner 0.
        let mut ownerless_real_msg = orphaned_synthetic_watcher_row();
        ownerless_real_msg.user_msg_id = 1_518_710_986_180_137_051;
        assert_eq!(
            recovery_phase_for_existing_inflight_rebind(&ownerless_real_msg),
            RecoveryPhase::Pending,
            "a row pinned to a real user message is not the zero-id #4400 orphan (I2)"
        );

        // (4) Terminal delivery already committed: keep today's behavior
        // (Pending) so the committed-cleanup path stays authoritative.
        // (kills the !terminal_delivery_committed conjunct)
        let mut committed = orphaned_synthetic_watcher_row();
        committed.terminal_delivery_committed = true;
        assert_eq!(
            recovery_phase_for_existing_inflight_rebind(&committed),
            RecoveryPhase::Pending,
            "a terminal-committed row keeps its existing (non-adopted) classification"
        );

        // (5) A progressed rebind-origin row stays on its own #3581 lifecycle.
        // (kills the !rebind_origin conjunct — replace does not catch this row
        // because its response is non-empty)
        let mut progressed_rebind_origin = orphaned_synthetic_watcher_row();
        progressed_rebind_origin.rebind_origin = true;
        progressed_rebind_origin.full_response = "progressed".to_string();
        assert_eq!(
            recovery_phase_for_existing_inflight_rebind(&progressed_rebind_origin),
            RecoveryPhase::Pending,
            "a progressed rebind-origin row must not be silently adopted"
        );

        // (6) No restore anchor: nothing to reattach a watcher to.
        // (kills the has_restore_anchor conjunct)
        let mut anchorless = orphaned_synthetic_watcher_row();
        anchorless.output_path = None;
        assert_eq!(
            recovery_phase_for_existing_inflight_rebind(&anchorless),
            RecoveryPhase::Pending,
            "an anchorless zero-id row cannot be adopted onto the watcher resume path"
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
