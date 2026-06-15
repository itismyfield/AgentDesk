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
    !state.rebind_origin
        && state.request_owner_user_id != 0
        && state.user_msg_id != 0
        && state.current_msg_id != 0
        && state.full_response.trim().is_empty()
        && state.last_watcher_relayed_offset.is_none()
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
