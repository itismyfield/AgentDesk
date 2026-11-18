use std::future::Future;
use std::time::{Duration, Instant};

use crate::db::postgres::{BACKPRESSURE_LOG_THROTTLE, PressureSnapshot};

const FAST_POLL: Duration = Duration::from_millis(500);
const MAX_POLL: Duration = Duration::from_secs(5);

pub(super) struct OutboxPressure {
    poll_interval: Duration,
    last_pressure_log: Instant,
    window_started: Instant,
    skip_count: u64,
    drain_attempt_count: u64,
    claim_count: u64,
    last_completed_drain: Option<Instant>,
}

impl OutboxPressure {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            poll_interval: FAST_POLL,
            last_pressure_log: now - BACKPRESSURE_LOG_THROTTLE,
            window_started: now,
            skip_count: 0,
            drain_attempt_count: 0,
            claim_count: 0,
            last_completed_drain: None,
        }
    }

    pub(super) fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    pub(super) fn yield_tick(
        &mut self,
        snapshot: PressureSnapshot,
        claim_owner: &str,
        now: Instant,
    ) -> bool {
        if !snapshot.should_yield() {
            return false;
        }
        self.skip_count = self.skip_count.saturating_add(1);
        if now.duration_since(self.last_pressure_log) >= BACKPRESSURE_LOG_THROTTLE {
            let last_completed_drain_elapsed_ms = self
                .last_completed_drain
                .map(|completed| serde_json::Value::from(elapsed_ms(now, completed)))
                .unwrap_or(serde_json::Value::Null);
            tracing::warn!(
                target: "agentdesk::services::dispatches::outbox_queue",
                claim_owner,
                pool_size = snapshot.pool_size,
                pool_idle = snapshot.pool_idle,
                pool_max = snapshot.pool_max,
                effective_reserve = snapshot.effective_reserve,
                in_flight = snapshot.in_flight(),
                background_budget = snapshot.background_budget(),
                skip_count = self.skip_count,
                drain_attempt_count = self.drain_attempt_count,
                claim_count = self.claim_count,
                window_elapsed_ms = elapsed_ms(now, self.window_started),
                last_completed_drain_elapsed_ms = %last_completed_drain_elapsed_ms,
                "[dispatch-outbox] yielding drain to foreground under pool pressure"
            );
            self.last_pressure_log = now;
            self.window_started = now;
            self.skip_count = 0;
            self.drain_attempt_count = 0;
            self.claim_count = 0;
        }
        self.poll_interval = MAX_POLL;
        true
    }

    pub(super) async fn drain(
        &mut self,
        batch: impl Future<Output = usize>,
        clock: impl FnOnce() -> Instant,
    ) {
        self.drain_attempt_count = self.drain_attempt_count.saturating_add(1);
        let processed = batch.await;
        self.claim_count = self.claim_count.saturating_add(processed as u64);
        // Completion means the batch future returned, including an empty claim.
        // It does not establish successful DB work or notification delivery.
        self.last_completed_drain = Some(clock());
        self.poll_interval = if processed == 0 {
            self.poll_interval.mul_f64(1.5).min(MAX_POLL)
        } else {
            FAST_POLL
        };
    }
}

fn elapsed_ms(now: Instant, since: Instant) -> u64 {
    now.saturating_duration_since(since)
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
#[path = "outbox_pressure_tests.rs"]
mod tests;
