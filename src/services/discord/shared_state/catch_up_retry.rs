use std::time::Instant;

use poise::serenity_prelude::ChannelId;

use crate::services::turn_orchestrator::MAX_INTERVENTIONS_PER_CHANNEL;

use super::super::SharedData;

const CATCH_UP_RETRY_QUEUE_THRESHOLD: usize = MAX_INTERVENTIONS_PER_CHANNEL / 2;
pub(in crate::services::discord) const CATCH_UP_RETRY_FETCH_FAILURE_LIMIT: u8 = 4;
// #4156: a channel whose fetch keeps SUCCEEDING but whose messages keep being
// Deferred (persistently busy / repeated ActorUnreachable) re-arms the catch-up
// retry every scan. Without a bound that cycle is unbounded and unlogged. Cap
// the consecutive Deferred re-arms (independently of the fetch-failure budget)
// and emit a giving-up WARN at the cap, mirroring the fetch-failure path.
pub(in crate::services::discord) const CATCH_UP_RETRY_DEFERRED_REARM_LIMIT: u8 = 8;

const HOLD_TTL: std::time::Duration = std::time::Duration::from_secs(86_400);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services) struct CatchUpRetryState {
    pub(in crate::services::discord) checkpoint: u64,
    pub(in crate::services::discord) fetch_failures: u8,
    // #4156: consecutive Deferred re-arms (fetch succeeded, enqueue deferred).
    // Tracked separately from `fetch_failures` because a Deferred re-arm is not
    // a fetch error; both budgets share the merge's most-exhausted semantics.
    // A value beyond the retry limit retains the exhausted arm for periodic sweeps.
    pub(in crate::services::discord) deferred_rearms: u8,
    pub(in crate::services::discord) armed_at: Instant,
}

impl CatchUpRetryState {
    pub(in crate::services::discord) fn is_exhausted_hold(self) -> bool {
        self.deferred_rearms > CATCH_UP_RETRY_DEFERRED_REARM_LIMIT
    }

    pub(in crate::services::discord) fn hold_expired_at(self, now: Instant) -> bool {
        self.is_exhausted_hold() && now.saturating_duration_since(self.armed_at) >= HOLD_TTL
    }

    pub(in crate::services::discord) fn new_at(checkpoint: u64, armed_at: Instant) -> Self {
        Self {
            checkpoint,
            fetch_failures: 0,
            deferred_rearms: 0,
            armed_at,
        }
    }

    pub(in crate::services::discord) fn after_fetch_failure(self) -> Option<Self> {
        let fetch_failures = self.fetch_failures.saturating_add(1);
        (fetch_failures <= CATCH_UP_RETRY_FETCH_FAILURE_LIMIT).then_some(Self {
            checkpoint: self.checkpoint,
            fetch_failures,
            deferred_rearms: self.deferred_rearms,
            armed_at: self.armed_at,
        })
    }

    // An exhausted arm is retained separately from the bounded retry budget.
    pub(in crate::services::discord) fn after_deferred_rearm(
        self,
        checkpoint: u64,
    ) -> Option<Self> {
        let deferred_rearms = self.deferred_rearms.saturating_add(1);
        (deferred_rearms <= CATCH_UP_RETRY_DEFERRED_REARM_LIMIT).then_some(Self {
            checkpoint,
            fetch_failures: self.fetch_failures,
            deferred_rearms,
            // Preserve the original arm-time age window across retries.
            armed_at: self.armed_at,
        })
    }
}

pub(in crate::services::discord) fn should_trigger_catch_up_retry(queue_len: usize) -> bool {
    queue_len <= CATCH_UP_RETRY_QUEUE_THRESHOLD
}

// Reserve a drained retry before releasing the map entry so periodic scans retain its origin.
pub(in crate::services::discord) fn take_catch_up_retry_checkpoint_after_queue_drain(
    shared: &SharedData,
    channel_id: ChannelId,
    queue_len_after: usize,
) -> Option<CatchUpRetryState> {
    if !should_trigger_catch_up_retry(queue_len_after) {
        return None;
    }
    if !shared
        .catch_up_retry_pending
        .get(&channel_id)
        .is_some_and(|state| !state.is_exhausted_hold())
    {
        return None;
    }
    let mut slot = shared
        .catch_up_scan_in_progress
        .entry(channel_id)
        .or_insert((false, None));
    if slot.0 || slot.1.is_some() {
        return None;
    }
    let retry = shared
        .catch_up_retry_pending
        .remove_if(&channel_id, |_, state| !state.is_exhausted_hold())
        .map(|(_, state)| state);
    slot.1 = retry;
    drop(slot);
    if retry.is_none() {
        shared
            .catch_up_scan_in_progress
            .remove_if(&channel_id, |_, slot| !slot.0 && slot.1.is_none());
    }
    retry
}
