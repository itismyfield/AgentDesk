use super::*;
use crate::services::session_host::legacy_collapse::tmux_present_bool;

/// Retry-aware tmux session check for recovery after dcserver restart.
/// The first check can false-negative if tmux CLI has not fully initialized yet.
pub(super) fn tmux_session_alive_with_retry(name: &str) -> bool {
    if tmux_live_pane_bool(name) {
        return true;
    }
    for attempt in 1..=2u32 {
        std::thread::sleep(recovery_retry_backoff(attempt));
        if tmux_live_pane_bool(name) {
            tracing::info!(
                "  [recovery] tmux pane alive on retry {} for {}",
                attempt,
                name
            );
            return true;
        }
    }
    false
}

pub(super) fn tmux_has_session_with_retry(name: &str) -> bool {
    if tmux_present_bool(name) {
        return true;
    }
    for attempt in 1..=2u32 {
        std::thread::sleep(recovery_retry_backoff(attempt));
        if tmux_present_bool(name) {
            tracing::info!(
                "  [recovery] tmux session found on retry {} for {}",
                attempt,
                name
            );
            return true;
        }
    }
    false
}
