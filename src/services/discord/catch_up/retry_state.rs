//! Budget arithmetic of a pending catch-up retry: how a fetch failure or a
//! Deferred re-arm spends it, and how two arms of one channel merge.

use std::time::Instant;

use poise::serenity_prelude::{ChannelId, MessageId};

use super::super::reaction_lifecycle::is_real_discord_message_id_value;
use super::super::{
    SharedData, advance_last_message_checkpoint, is_synthetic_headless_message_id_raw,
    mailbox_clear_channel,
};
use super::{
    CATCH_UP_RETRY_DEFERRED_REARM_LIMIT, CATCH_UP_RETRY_FETCH_FAILURE_LIMIT, CatchUpRetryState,
};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::ClearChannelResult;

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

    // #4156: advance the Deferred re-arm budget; `None` once the cap is spent
    // stops re-arming, as in `after_fetch_failure`, until a fresh trigger.
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

/// #6035: `/clear` also drops the pending retry and lifts the checkpoint past the cleared ids,
/// so no sweep reruns them; teardown clears keep `mailbox_clear_channel` so their loss recovers.
pub(in crate::services::discord) async fn clear_channel_discarding_catch_up_backlog(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> ClearChannelResult {
    let cleared = mailbox_clear_channel(shared, provider, channel_id).await;
    // A failed persist restored the queue and reservation, so their ids still need the sweep.
    if cleared.persistence_error.is_some() {
        return cleared;
    }
    shared.catch_up_retry_pending.remove(&channel_id);
    // Only real Discord ids are cursors: a synthetic headless/voice id would hide every later message.
    let real = |id: &u64| {
        is_real_discord_message_id_value(*id) && !is_synthetic_headless_message_id_raw(*id)
    };
    let discarded = cleared.discarded_message_ids.iter().map(|id| id.get());
    if let Some(newest) = discarded.filter(real).max() {
        advance_last_message_checkpoint(shared, provider, channel_id, MessageId::new(newest));
    }
    cleared
}
