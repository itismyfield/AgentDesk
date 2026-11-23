use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};

/// Tmux sessions whose Claude idle response tail is running; the tail's guard removes its entry.
pub(in crate::services::discord) static CLAUDE_IDLE_RESPONSE_TAILS: LazyLock<
    Mutex<HashSet<String>>,
> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// Whether a Claude idle response tail runs for the session, read without deciding anything.
pub(in crate::services::discord) fn claude_idle_tail_running(tmux_session_name: &str) -> bool {
    CLAUDE_IDLE_RESPONSE_TAILS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .contains(tmux_session_name)
}
