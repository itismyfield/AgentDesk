use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::{RelayRecoveryActionKind, is_agentdesk_tmux_session};
use crate::services::discord::relay_health::RelayHealthSnapshot;

pub(super) const AUTO_HEAL_WINDOW_SECS: i64 = 600;
pub(super) const AUTO_HEAL_DEFAULT_MAX_ATTEMPTS_PER_WINDOW: u32 = 1;
pub(super) const AUTO_HEAL_DEAD_FRONTIER_REATTACH_MAX_ATTEMPTS_PER_WINDOW: u32 = 2;

#[derive(Clone, Copy, Debug)]
struct AttemptWindow {
    window_start_ms: i64,
    attempts: u32,
}

fn auto_heal_attempts() -> &'static Mutex<HashMap<String, AttemptWindow>> {
    static ATTEMPTS: OnceLock<Mutex<HashMap<String, AttemptWindow>>> = OnceLock::new();
    // Short-lived process memory guard only; persistence across restarts is out
    // of scope for this bounded local auto-heal limiter.
    ATTEMPTS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn auto_heal_key(
    provider: &str,
    channel_id: u64,
    action: RelayRecoveryActionKind,
) -> String {
    format!("{}:{}:{}", provider, channel_id, action.as_str())
}

pub(super) fn remaining_auto_heal_attempts(
    key: &str,
    now_ms: i64,
    max_attempts_per_window: u32,
) -> u32 {
    let mut attempts = auto_heal_attempts()
        .lock()
        .expect("relay recovery attempt map poisoned");
    let Some(window) = attempts.get_mut(key) else {
        return max_attempts_per_window;
    };
    if now_ms.saturating_sub(window.window_start_ms) >= AUTO_HEAL_WINDOW_SECS * 1000 {
        attempts.remove(key);
        return max_attempts_per_window;
    }
    max_attempts_per_window.saturating_sub(window.attempts)
}

pub(super) fn reserve_auto_heal_attempt(
    key: &str,
    now_ms: i64,
    max_attempts_per_window: u32,
) -> Result<u32, &'static str> {
    let mut attempts = auto_heal_attempts()
        .lock()
        .expect("relay recovery attempt map poisoned");
    let window = attempts.entry(key.to_string()).or_insert(AttemptWindow {
        window_start_ms: now_ms,
        attempts: 0,
    });
    if now_ms.saturating_sub(window.window_start_ms) >= AUTO_HEAL_WINDOW_SECS * 1000 {
        window.window_start_ms = now_ms;
        window.attempts = 0;
    }
    if window.attempts >= max_attempts_per_window {
        return Err("auto_heal_rate_limited");
    }
    window.attempts += 1;
    Ok(max_attempts_per_window.saturating_sub(window.attempts))
}

pub(super) fn max_attempts_per_window_for_snapshot(
    snapshot: &RelayHealthSnapshot,
    action: RelayRecoveryActionKind,
) -> u32 {
    if action == RelayRecoveryActionKind::ReattachWatcher
        && is_agentdesk_tmux_session(snapshot.tmux_session.as_deref())
        && snapshot.relay_frontier_never_advanced_with_unread_tail()
    {
        return AUTO_HEAL_DEAD_FRONTIER_REATTACH_MAX_ATTEMPTS_PER_WINDOW;
    }
    AUTO_HEAL_DEFAULT_MAX_ATTEMPTS_PER_WINDOW
}

#[cfg(test)]
pub(super) fn clear_auto_heal_attempts_for_tests() {
    auto_heal_attempts()
        .lock()
        .expect("relay recovery attempt map poisoned")
        .clear();
}
