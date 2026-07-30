use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::{RelayRecoveryActionKind, RelayRecoveryApplySource, is_agentdesk_tmux_session};
use crate::services::discord::relay_health::RelayHealthSnapshot;

pub(super) const AUTO_HEAL_WINDOW_SECS: i64 = 600;
pub(super) const AUTO_HEAL_DEFAULT_MAX_ATTEMPTS_PER_WINDOW: u32 = 1;
pub(super) const AUTO_HEAL_DEAD_FRONTIER_REATTACH_MAX_ATTEMPTS_PER_WINDOW: u32 = 2;
pub(super) const AUTO_HEAL_REFUND_BACKOFF_THRESHOLD: u32 = 3;
const AUTO_HEAL_MAX_REFUND_BACKOFF_EXPONENT: u32 = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AutoHealBudgetLane {
    Manual,
    Internal,
}

impl AutoHealBudgetLane {
    fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Internal => "internal",
        }
    }
}

impl RelayRecoveryApplySource {
    fn budget_lane(self) -> AutoHealBudgetLane {
        match self {
            Self::Manual => AutoHealBudgetLane::Manual,
            Self::ProbeAutoHeal | Self::StallWatchdog => AutoHealBudgetLane::Internal,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct AttemptWindow {
    window_start_ms: i64,
    attempts: u32,
    consecutive_refunds: u32,
    retry_not_before_ms: Option<i64>,
    generation: u64,
    healed_commits: u64,
}

impl AttemptWindow {
    fn new(now_ms: i64) -> Self {
        Self {
            window_start_ms: now_ms,
            attempts: 0,
            consecutive_refunds: 0,
            retry_not_before_ms: None,
            generation: 0,
            healed_commits: 0,
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
            self.generation = self.generation.wrapping_add(1);
        } else if self.retry_not_before_ms.is_none()
            && now_ms.saturating_sub(self.window_start_ms) >= AUTO_HEAL_WINDOW_SECS * 1000
        {
            self.window_start_ms = now_ms;
            self.attempts = 0;
            self.generation = self.generation.wrapping_add(1);
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
        source.budget_lane().as_str()
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
) -> Result<(u32, u64), &'static str> {
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
    window.generation = window.generation.wrapping_add(1);
    Ok((
        max_attempts_per_window.saturating_sub(window.attempts),
        window.generation,
    ))
}

#[cfg(test)]
pub(super) fn auto_heal_attempt_state(key: &str) -> Option<(u64, u32, Option<i64>, u64)> {
    auto_heal_attempts()
        .lock()
        .expect("relay recovery attempt map poisoned")
        .get(key)
        .map(|window| {
            (
                window.generation,
                window.consecutive_refunds,
                window.retry_not_before_ms,
                window.healed_commits,
            )
        })
}

#[cfg(test)]
pub(super) fn auto_heal_attempt_generation(key: &str) -> Option<u64> {
    auto_heal_attempt_state(key).map(|(generation, _, _, _)| generation)
}

pub(super) fn refund_auto_heal_attempt_if_current(key: &str, generation: u64, now_ms: i64) {
    let mut attempts = auto_heal_attempts()
        .lock()
        .expect("relay recovery attempt map poisoned");
    if let Some(window) = attempts.get_mut(key)
        && window.generation == generation
    {
        refund_auto_heal_attempt_in_window(window, now_ms);
    }
}

pub(super) fn cancel_unapplied_auto_heal_attempt_if_current(key: &str, generation: u64) {
    let mut attempts = auto_heal_attempts()
        .lock()
        .expect("relay recovery attempt map poisoned");
    if let Some(window) = attempts.get_mut(key)
        && window.generation == generation
    {
        window.attempts = window.attempts.saturating_sub(1);
    }
}

pub(super) fn record_auto_heal_confirm_failure_if_current(key: &str, generation: u64, now_ms: i64) {
    let mut attempts = auto_heal_attempts()
        .lock()
        .expect("relay recovery attempt map poisoned");
    if let Some(window) = attempts.get_mut(key)
        && window.generation == generation
    {
        record_auto_heal_confirm_failure_in_window(window, now_ms);
    }
}

pub(super) fn commit_auto_heal_attempt_if_current(key: &str, generation: u64) {
    let mut attempts = auto_heal_attempts()
        .lock()
        .expect("relay recovery attempt map poisoned");
    if let Some(window) = attempts.get_mut(key)
        && window.generation == generation
    {
        commit_auto_heal_attempt_in_window(window);
    }
}

pub(super) fn refund_auto_heal_attempt(key: &str, now_ms: i64) {
    let mut attempts = auto_heal_attempts()
        .lock()
        .expect("relay recovery attempt map poisoned");
    let Some(window) = attempts.get_mut(key) else {
        return;
    };
    refund_auto_heal_attempt_in_window(window, now_ms);
}

fn refund_auto_heal_attempt_in_window(window: &mut AttemptWindow, now_ms: i64) {
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

/// Return a reservation when a fail-closed pre-apply gate refused the action.
/// This is deliberately narrower than confirmation settlement: once rebind was
/// attempted, confirmation policy decides whether to consume or refund it.
#[cfg(test)]
pub(super) fn cancel_unapplied_auto_heal_attempt(key: &str) {
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
    record_auto_heal_confirm_failure_in_window(window, now_ms);
}

fn record_auto_heal_confirm_failure_in_window(window: &mut AttemptWindow, now_ms: i64) {
    window.consecutive_refunds = 0;
    window.retry_not_before_ms = Some(now_ms.saturating_add(AUTO_HEAL_WINDOW_SECS * 1000));
}

pub(super) fn commit_auto_heal_attempt(key: &str) {
    let mut attempts = auto_heal_attempts()
        .lock()
        .expect("relay recovery attempt map poisoned");
    if let Some(window) = attempts.get_mut(key) {
        commit_auto_heal_attempt_in_window(window);
    }
}

fn commit_auto_heal_attempt_in_window(window: &mut AttemptWindow) {
    window.healed_commits = window.healed_commits.saturating_add(1);
    window.consecutive_refunds = 0;
    window.retry_not_before_ms = None;
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

    #[test]
    fn relay_recovery_internal_sources_share_budget_lane_manual_is_distinct() {
        let probe = auto_heal_key(
            "codex",
            4_423_102,
            RelayRecoveryActionKind::ReattachWatcher,
            RelayRecoveryApplySource::ProbeAutoHeal,
        );
        let watchdog = auto_heal_key(
            "codex",
            4_423_102,
            RelayRecoveryActionKind::ReattachWatcher,
            RelayRecoveryApplySource::StallWatchdog,
        );
        let manual = auto_heal_key(
            "codex",
            4_423_102,
            RelayRecoveryActionKind::ReattachWatcher,
            RelayRecoveryApplySource::Manual,
        );

        assert_eq!(
            probe, watchdog,
            "internal actors must share one budget lane"
        );
        assert_ne!(manual, probe, "manual budget must remain operator-only");
    }

    #[tokio::test]
    async fn relay_recovery_probe_exhaustion_blocks_watchdog_but_not_manual() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
        let probe = auto_heal_key(
            "codex",
            4_423_103,
            RelayRecoveryActionKind::ReattachWatcher,
            RelayRecoveryApplySource::ProbeAutoHeal,
        );
        let watchdog = auto_heal_key(
            "codex",
            4_423_103,
            RelayRecoveryActionKind::ReattachWatcher,
            RelayRecoveryApplySource::StallWatchdog,
        );
        let manual = auto_heal_key(
            "codex",
            4_423_103,
            RelayRecoveryActionKind::ReattachWatcher,
            RelayRecoveryApplySource::Manual,
        );

        assert_eq!(
            reserve_auto_heal_attempt(&probe, 1_000, 1).map(|(remaining, _)| remaining),
            Ok(0)
        );
        assert_eq!(
            reserve_auto_heal_attempt(&watchdog, 2_000, 1).map(|(remaining, _)| remaining),
            Err("auto_heal_rate_limited"),
            "probe and watchdog must consume the same internal budget"
        );
        assert_eq!(
            reserve_auto_heal_attempt(&manual, 2_000, 1).map(|(remaining, _)| remaining),
            Ok(0),
            "internal exhaustion must not consume the manual budget"
        );
    }

    #[tokio::test]
    async fn relay_recovery_manual_exhaustion_does_not_block_internal_budget() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
        let manual = auto_heal_key(
            "codex",
            4_423_104,
            RelayRecoveryActionKind::ReattachWatcher,
            RelayRecoveryApplySource::Manual,
        );
        let internal = auto_heal_key(
            "codex",
            4_423_104,
            RelayRecoveryActionKind::ReattachWatcher,
            RelayRecoveryApplySource::ProbeAutoHeal,
        );

        assert_eq!(
            reserve_auto_heal_attempt(&manual, 1_000, 1).map(|(remaining, _)| remaining),
            Ok(0)
        );
        assert_eq!(
            reserve_auto_heal_attempt(&internal, 2_000, 1).map(|(remaining, _)| remaining),
            Ok(0),
            "manual exhaustion must not consume the internal budget"
        );
    }

    #[tokio::test]
    async fn late_settlement_cannot_mutate_new_window_reservation() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
        let key = key();
        assert_eq!(
            reserve_auto_heal_attempt(&key, 1_000, 1).map(|(remaining, _)| remaining),
            Ok(0)
        );
        let stale_generation = auto_heal_attempt_generation(&key).expect("old generation");
        assert_eq!(
            reserve_auto_heal_attempt(&key, 1_000 + AUTO_HEAL_WINDOW_SECS * 1000, 1)
                .map(|(remaining, _)| remaining),
            Ok(0)
        );
        let current_generation = auto_heal_attempt_generation(&key).expect("new generation");
        assert_ne!(stale_generation, current_generation);

        refund_auto_heal_attempt_if_current(&key, stale_generation, 2_000);
        assert_eq!(
            reserve_auto_heal_attempt(&key, 1_000 + AUTO_HEAL_WINDOW_SECS * 1000 + 1, 1)
                .map(|(remaining, _)| remaining),
            Err("auto_heal_rate_limited"),
            "late settlement from the old round must not refund the new reservation"
        );
    }

    #[tokio::test]
    async fn relay_recovery_failed_spawn_refunds_reserved_budget() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
        let key = key();
        assert_eq!(
            reserve_auto_heal_attempt(&key, 1_000, 1).map(|(remaining, _)| remaining),
            Ok(0)
        );

        refund_auto_heal_attempt(&key, 2_000);

        assert_eq!(
            reserve_auto_heal_attempt(&key, 3_000, 1).map(|(remaining, _)| remaining),
            Ok(0)
        );
    }

    #[tokio::test]
    async fn relay_recovery_three_consecutive_refunds_expand_retry_window() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
        let key = key();
        for now_ms in [1_000, 2_000, 3_000] {
            assert_eq!(
                reserve_auto_heal_attempt(&key, now_ms, 1).map(|(remaining, _)| remaining),
                Ok(0)
            );
            refund_auto_heal_attempt(&key, now_ms);
        }

        assert_eq!(
            reserve_auto_heal_attempt(&key, 4_000, 1).map(|(remaining, _)| remaining),
            Err("auto_heal_failure_backoff")
        );
        assert_eq!(
            reserve_auto_heal_attempt(&key, 3_000 + 1_200_000, 1).map(|(remaining, _)| remaining),
            Ok(0),
            "the third consecutive refund must expand the base 600s window to 1200s"
        );
    }

    #[tokio::test]
    async fn relay_recovery_confirm_failure_counts_attempt_and_backs_off() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
        let key = key();
        assert_eq!(
            reserve_auto_heal_attempt(&key, 1_000, 2).map(|(remaining, _)| remaining),
            Ok(1)
        );

        record_auto_heal_confirm_failure(&key, 2_000);

        assert_eq!(
            reserve_auto_heal_attempt(&key, 3_000, 2).map(|(remaining, _)| remaining),
            Err("auto_heal_failure_backoff")
        );
        assert_eq!(
            reserve_auto_heal_attempt(&key, 2_000 + AUTO_HEAL_WINDOW_SECS * 1000, 2)
                .map(|(remaining, _)| remaining),
            Ok(1)
        );
    }
}
