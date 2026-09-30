use super::CLAUDE_IDLE_RESPONSE_TAILS;

/// Whether a Claude idle response tail runs for the session, read without deciding anything.
pub(in crate::services::discord) fn claude_idle_tail_running(tmux_session_name: &str) -> bool {
    CLAUDE_IDLE_RESPONSE_TAILS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .contains(tmux_session_name)
}
