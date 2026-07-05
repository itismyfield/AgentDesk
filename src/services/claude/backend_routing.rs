use crate::services::platform::tmux::PaneLiveness;
use crate::services::session_backend::terminate_process_session_before_tmux;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LocalTmuxStartupPlan {
    /// Existing tmux pane plus both runtime paths are present. The provider
    /// writes the prompt to FIFO, reads this turn from the current JSONL
    /// offset, then emits `TmuxReady` for watcher handoff.
    WarmFollowup,
    /// A tmux session name exists, but the pane or runtime paths are stale.
    /// The provider kills it and recreates it through the cold-start path.
    RecreateStaleSession,
    /// No usable existing session exists. The provider starts a new wrapper
    /// and hands JSONL ownership to the watcher from offset 0.
    ColdStart,
}

pub(super) fn classify_local_tmux_startup_plan(
    session_exists: bool,
    has_live_pane: bool,
    has_output_path: bool,
    has_input_fifo_path: bool,
) -> LocalTmuxStartupPlan {
    if session_exists && has_live_pane && has_output_path && has_input_fifo_path {
        LocalTmuxStartupPlan::WarmFollowup
    } else if session_exists {
        LocalTmuxStartupPlan::RecreateStaleSession
    } else {
        LocalTmuxStartupPlan::ColdStart
    }
}

/// Decide whether a stale-classified tmux session must be preserved rather than
/// killed-and-recreated. Mirrors the Codex (`codex.rs`) and Qwen (`qwen.rs`)
/// guards: a pane that is still live (`has_live_pane`) AND was selected for
/// provider-session reuse (a non-empty resume id) is carrying an active
/// conversation, so missing wrapper I/O files alone must not trigger a kill.
pub(super) fn should_preserve_live_reused_provider_session(
    resume_session_id: Option<&str>,
    has_live_pane: bool,
) -> bool {
    resume_session_id
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
        && has_live_pane
}

/// Decide whether ProcessBackend demotion must be refused when tmux is not
/// currently reported available. A live pane always wins over cached-missing
/// state because it proves a tmux-owned conversation exists. A probe spawn
/// failure under cached-missing, however, only corroborates that tmux is missing;
/// treating it as new unknown state would strand genuinely tmux-less hosts on
/// the tmux path forever.
///
/// Accepted residual risk: if the tmux binary is transiently absent while a live
/// tmux server is still running within the 45s cache TTL window, this can allow
/// ProcessBackend demotion and therefore a narrow double-resume risk.
pub(super) fn should_refuse_process_backend_demotion(
    tmux_available: bool,
    tmux_missing: bool,
    pane_liveness: PaneLiveness,
) -> bool {
    !tmux_available
        && (matches!(pane_liveness, PaneLiveness::Live)
            || (!tmux_missing && matches!(pane_liveness, PaneLiveness::ProbeError)))
}

pub(super) fn process_backend_demotion_guard_liveness_from_cached_missing(
    tmux_missing: bool,
    tmux_session_name: Option<&str>,
    pane_liveness_probe: impl FnOnce(&str) -> PaneLiveness,
) -> (bool, PaneLiveness) {
    let pane_liveness = match tmux_session_name.filter(|name| !name.trim().is_empty()) {
        // A recorded session name deserves a real pane probe even when the
        // availability cache says the tmux binary was missing.
        Some(tmux_session_name) => pane_liveness_probe(tmux_session_name),
        None => PaneLiveness::DeadOrAbsent,
    };
    (tmux_missing, pane_liveness)
}

pub(super) fn process_backend_demotion_guard_liveness(
    tmux_session_name: Option<&str>,
) -> (bool, PaneLiveness) {
    process_backend_demotion_guard_liveness_from_cached_missing(
        crate::services::platform::tmux::cached_unavailable_due_to_missing(),
        tmux_session_name,
        crate::services::tmux_diagnostics::tmux_session_pane_liveness,
    )
}

pub(super) fn prepare_tmux_backend_after_refused_process_demotion(
    tmux_session_name: &str,
    pane_liveness: PaneLiveness,
) {
    match pane_liveness {
        PaneLiveness::Live => {
            crate::services::platform::tmux::mark_available_from_live_session();
        }
        PaneLiveness::ProbeError => {
            crate::services::platform::tmux::invalidate_availability_cache();
        }
        PaneLiveness::DeadOrAbsent => {}
    }
    tracing::warn!(
        tmux_session_name = tmux_session_name,
        pane_liveness = ?pane_liveness,
        "routing through tmux backend instead of ProcessBackend demotion"
    );
    cleanup_process_backend_before_tmux(tmux_session_name);
}

pub(super) fn cleanup_process_backend_before_tmux(session_name: &str) -> bool {
    let cleaned = terminate_process_session_before_tmux(session_name);
    if cleaned {
        tracing::warn!(
            tmux_session_name = session_name,
            "terminated orphan ProcessBackend wrapper before returning to tmux backend"
        );
    }
    cleaned
}
