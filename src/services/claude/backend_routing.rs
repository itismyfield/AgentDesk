use crate::services::session_backend::terminate_process_session;

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

pub(super) fn should_refuse_process_backend_demotion(
    tmux_available: bool,
    session_exists: bool,
    has_live_pane: bool,
) -> bool {
    !tmux_available && session_exists && has_live_pane
}

pub(super) fn cleanup_process_backend_before_tmux(session_name: &str) -> bool {
    let cleaned = terminate_process_session(session_name);
    if cleaned {
        tracing::warn!(
            tmux_session_name = session_name,
            "terminated orphan ProcessBackend wrapper before returning to tmux backend"
        );
    }
    cleaned
}

