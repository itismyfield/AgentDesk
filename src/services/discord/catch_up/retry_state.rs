//! Budget arithmetic of a pending catch-up retry: how a fetch failure or a
//! Deferred re-arm spends it, and how two arms of one channel merge.

use std::time::Instant;

use super::{
    CATCH_UP_RETRY_DEFERRED_REARM_LIMIT, CATCH_UP_RETRY_FETCH_FAILURE_LIMIT, CatchUpRetryState,
};

impl CatchUpRetryState {
    pub(super) fn new(checkpoint: u64) -> Self {
        Self {
            checkpoint,
            fetch_failures: 0,
            deferred_rearms: 0,
            armed_at: Instant::now(),
        }
    }

    pub(super) fn after_fetch_failure(self) -> Option<Self> {
        let fetch_failures = self.fetch_failures.saturating_add(1);
        (fetch_failures <= CATCH_UP_RETRY_FETCH_FAILURE_LIMIT).then_some(Self {
            checkpoint: self.checkpoint,
            fetch_failures,
            deferred_rearms: self.deferred_rearms,
            armed_at: self.armed_at,
        })
    }

    // #4156: advance the Deferred re-arm budget. Returns `None` once the cap is
    // exhausted so the caller stops re-arming (the backlog then ages out or a
    // fresh catch-up trigger restarts the cycle), matching `after_fetch_failure`.
    pub(super) fn after_deferred_rearm(self, checkpoint: u64) -> Option<Self> {
        let deferred_rearms = self.deferred_rearms.saturating_add(1);
        (deferred_rearms <= CATCH_UP_RETRY_DEFERRED_REARM_LIMIT).then_some(Self {
            checkpoint,
            fetch_failures: self.fetch_failures,
            deferred_rearms,
            // Preserve the original arm time so the arm-time age window
            // (`catch_up_message_age_reference_time`) is NOT reset each cycle.
            armed_at: self.armed_at,
        })
    }
}

pub(super) fn merge_catch_up_retry_state(
    existing: Option<CatchUpRetryState>,
    retry_state: CatchUpRetryState,
) -> CatchUpRetryState {
    let Some(existing) = existing else {
        return retry_state;
    };
    CatchUpRetryState {
        checkpoint: merge_catch_up_retry_checkpoint(
            Some(existing.checkpoint),
            retry_state.checkpoint,
        ),
        // A merged older checkpoint inherits the most exhausted budget so the
        // same old backlog cannot gain unbounded retries through fresh arms.
        fetch_failures: existing.fetch_failures.max(retry_state.fetch_failures),
        // #4156: same most-exhausted rule for the Deferred re-arm budget.
        deferred_rearms: existing.deferred_rearms.max(retry_state.deferred_rearms),
        armed_at: existing.armed_at.min(retry_state.armed_at),
    }
}

pub(super) fn merge_catch_up_retry_checkpoint(existing: Option<u64>, retry_after: u64) -> u64 {
    existing.map_or(retry_after, |checkpoint| checkpoint.min(retry_after))
}
