use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::{RelayRecoveryActionKind, RelayRecoveryApplySource, is_agentdesk_tmux_session};
use crate::services::discord::relay_health::RelayHealthSnapshot;

pub(super) const AUTO_HEAL_WINDOW_SECS: i64 = 600;
pub(super) const AUTO_HEAL_DEFAULT_MAX_ATTEMPTS_PER_WINDOW: u32 = 1;
pub(super) const AUTO_HEAL_DEAD_FRONTIER_REATTACH_MAX_ATTEMPTS_PER_WINDOW: u32 = 2;
pub(super) const AUTO_HEAL_REFUND_BACKOFF_THRESHOLD: u32 = 3;
const AUTO_HEAL_MAX_REFUND_BACKOFF_EXPONENT: u32 = 4;

#[derive(Clone, Copy, Debug)]
struct AttemptWindow {
    window_start_ms: i64,
    attempts: u32,
    consecutive_refunds: u32,
    retry_not_before_ms: Option<i64>,
}

impl AttemptWindow {
    fn new(now_ms: i64) -> Self {
        Self {
            window_start_ms: now_ms,
            attempts: 0,
            consecutive_refunds: 0,
            retry_not_before_ms: None,
        }
    }

    fn refresh(&mut self, now_ms: i64) {
        if self
            .retry_not_before_ms
            .is_some_and(|retry_at| now_ms >= retry_at)
        {
            self.retry_not_before_ms = None;
            self.window_start_ms = now_ms;
            self.attempts = 0;
        } else if self.retry_not_before_ms.is_none()
            && now_ms.saturating_sub(self.window_start_ms) >= AUTO_HEAL_WINDOW_SECS * 1000
        {
            self.window_start_ms = now_ms;
            self.attempts = 0;
        }
    }

    fn backoff_active(&self, now_ms: i64) -> bool {
        self.retry_not_before_ms
            .is_some_and(|retry_at| now_ms < retry_at)
    }
}

fn auto_heal_attempts() -> &'static Mutex<HashMap<String, AttemptWindow>> {
    static ATTEMPTS: OnceLock<Mutex<HashMap<String, AttemptWindow>>> = OnceLock::new();
    ATTEMPTS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn auto_heal_key(
    provider: &str,
    channel_id: u64,
    action: RelayRecoveryActionKind,
    source: RelayRecoveryApplySource,
) -> String {
    format!(
        "{}:{}:{}:{}",
        provider,
        channel_id,
        action.as_str(),
        source.as_str()
    )
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
    window.refresh(now_ms);
    if window.backoff_active(now_ms) {
        return 0;
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
    let window = attempts
        .entry(key.to_string())
        .or_insert_with(|| AttemptWindow::new(now_ms));
    window.refresh(now_ms);
    if window.backoff_active(now_ms) {
        return Err("auto_heal_failure_backoff");
    }
    if window.attempts >= max_attempts_per_window {
        return Err("auto_heal_rate_limited");
    }
    window.attempts += 1;
    Ok(max_attempts_per_window.saturating_sub(window.attempts))
}

/// Return a reservation consumed by a spawn/rebind failure. Consecutive
/// refunds are retained across ordinary windows; the third failure opens a
/// 1,200s retry window and later consecutive failures expand it exponentially.
pub(super) fn refund_auto_heal_attempt(key: &str, now_ms: i64) {
    let mut attempts = auto_heal_attempts()
        .lock()
        .expect("relay recovery attempt map poisoned");
    let Some(window) = attempts.get_mut(key) else {
        return;
    };
    window.attempts = window.attempts.saturating_sub(1);
    window.consecutive_refunds = window.consecutive_refunds.saturating_add(1);
    if window.consecutive_refunds < AUTO_HEAL_REFUND_BACKOFF_THRESHOLD {
        return;
    }
    let exponent = window
        .consecutive_refunds
        .saturating_sub(AUTO_HEAL_REFUND_BACKOFF_THRESHOLD)
        .saturating_add(1)
        .min(AUTO_HEAL_MAX_REFUND_BACKOFF_EXPONENT);
    let expanded_window_secs = AUTO_HEAL_WINDOW_SECS.saturating_mul(1_i64 << exponent);
    window.retry_not_before_ms = Some(now_ms.saturating_add(expanded_window_secs * 1000));
}

/// A startup-graced, not-yet-confirmed spawn is neither success nor failure.
/// Release only the reservation and preserve the current failure episode.
pub(super) fn release_auto_heal_attempt(key: &str) {
    let mut attempts = auto_heal_attempts()
        .lock()
        .expect("relay recovery attempt map poisoned");
    if let Some(window) = attempts.get_mut(key) {
        window.attempts = window.attempts.saturating_sub(1);
    }
}

pub(super) fn record_auto_heal_confirm_failure(key: &str, now_ms: i64) {
    let mut attempts = auto_heal_attempts()
        .lock()
        .expect("relay recovery attempt map poisoned");
    let window = attempts
        .entry(key.to_string())
        .or_insert_with(|| AttemptWindow::new(now_ms));
    window.consecutive_refunds = 0;
    window.retry_not_before_ms = Some(now_ms.saturating_add(AUTO_HEAL_WINDOW_SECS * 1000));
}

pub(super) fn commit_auto_heal_attempt(key: &str) {
    let mut attempts = auto_heal_attempts()
        .lock()
        .expect("relay recovery attempt map poisoned");
    if let Some(window) = attempts.get_mut(key) {
        window.consecutive_refunds = 0;
        window.retry_not_before_ms = None;
    }
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

#[cfg(test)]
pub(super) fn auto_heal_test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> String {
        auto_heal_key(
            "codex",
            4_423_101,
            RelayRecoveryActionKind::ReattachWatcher,
            RelayRecoveryApplySource::ProbeAutoHeal,
        )
    }

    #[tokio::test]
    async fn relay_recovery_failed_spawn_refunds_reserved_budget() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
        let key = key();
        assert_eq!(reserve_auto_heal_attempt(&key, 1_000, 1), Ok(0));

        refund_auto_heal_attempt(&key, 2_000);

        assert_eq!(reserve_auto_heal_attempt(&key, 3_000, 1), Ok(0));
    }

    #[tokio::test]
    async fn relay_recovery_three_consecutive_refunds_expand_retry_window() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
        let key = key();
        for now_ms in [1_000, 2_000, 3_000] {
            assert_eq!(reserve_auto_heal_attempt(&key, now_ms, 1), Ok(0));
            refund_auto_heal_attempt(&key, now_ms);
        }

        assert_eq!(
            reserve_auto_heal_attempt(&key, 4_000, 1),
            Err("auto_heal_failure_backoff")
        );
        assert_eq!(
            reserve_auto_heal_attempt(&key, 3_000 + 1_200_000, 1),
            Ok(0),
            "the third consecutive refund must expand the base 600s window to 1200s"
        );
    }

    #[tokio::test]
    async fn relay_recovery_confirm_failure_counts_attempt_and_backs_off() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
        let key = key();
        assert_eq!(reserve_auto_heal_attempt(&key, 1_000, 2), Ok(1));

        record_auto_heal_confirm_failure(&key, 2_000);

        assert_eq!(
            reserve_auto_heal_attempt(&key, 3_000, 2),
            Err("auto_heal_failure_backoff")
        );
        assert_eq!(
            reserve_auto_heal_attempt(&key, 2_000 + AUTO_HEAL_WINDOW_SECS * 1000, 2),
            Ok(1)
        );
    }
}
