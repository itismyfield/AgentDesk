//! Retry budgets, exhausted holds, and per-channel sweep ownership.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use poise::serenity_prelude::{ChannelId, MessageId};

use super::super::reaction_lifecycle::is_real_discord_message_id_value;
use super::super::{
    SharedData, advance_last_message_checkpoint, is_synthetic_headless_message_id_raw,
    mailbox_clear_channel,
};
use super::clock::CatchUpClock;
use super::settled_frontier::RetainedBarrier;
use super::{
    CATCH_UP_RETRY_DEFERRED_REARM_LIMIT, CATCH_UP_RETRY_FETCH_FAILURE_LIMIT, CatchUpRetryState,
};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::ClearChannelResult;

const HOLD_TTL: std::time::Duration = std::time::Duration::from_secs(86_400);

pub(super) fn arm_catch_up_retry_state(
    shared: &SharedData,
    channel_id: ChannelId,
    retry_state: CatchUpRetryState,
) -> CatchUpRetryState {
    let mut pending = shared
        .catch_up_retry_pending
        .entry(channel_id)
        .or_insert(retry_state);
    if pending.is_exhausted_hold() {
        return *pending;
    }
    *pending = merge_catch_up_retry_state(Some(*pending), retry_state);
    *pending
}

pub(super) fn collect_catch_up_retry_pending_channels(shared: &SharedData) -> HashSet<ChannelId> {
    shared
        .catch_up_retry_pending
        .iter()
        .filter(|entry| !entry.is_exhausted_hold())
        .map(|entry| *entry.key())
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CatchUpRetryScanDecision {
    Proceed(Option<CatchUpRetryState>),
    SkipConsumed,
}

pub(super) fn consume_catch_up_retry_state_for_scan(
    shared: &SharedData,
    channel_id: ChannelId,
    retry_checkpoints: &HashMap<ChannelId, CatchUpRetryState>,
    pending_retry_channels: &HashSet<ChannelId>,
) -> CatchUpRetryScanDecision {
    let reserved = shared
        .catch_up_scan_in_progress
        .get_mut(&channel_id)
        .and_then(|mut slot| slot.1.take());
    let mut retry_state = reserved
        .map(|state| merge_catch_up_retry_state(retry_checkpoints.get(&channel_id).copied(), state))
        .or_else(|| retry_checkpoints.get(&channel_id).copied());
    if let Some(mut hold) = shared
        .catch_up_retry_pending
        .get_mut(&channel_id)
        .filter(|state| state.is_exhausted_hold())
    {
        if let Some(reserved) = reserved {
            hold.checkpoint = hold.checkpoint.min(reserved.checkpoint);
            hold.armed_at = hold.armed_at.min(reserved.armed_at);
        }
        return CatchUpRetryScanDecision::Proceed(Some(*hold));
    }
    if !pending_retry_channels.contains(&channel_id) {
        return CatchUpRetryScanDecision::Proceed(retry_state);
    }

    let Some((_, pending_retry_state)) = shared
        .catch_up_retry_pending
        .remove_if(&channel_id, |_, state| !state.is_exhausted_hold())
    else {
        // A drain may take the map entry first; preserve any retry handed to this scan.
        return match retry_state {
            Some(state) => CatchUpRetryScanDecision::Proceed(Some(state)),
            None => CatchUpRetryScanDecision::SkipConsumed,
        };
    };
    retry_state = Some(merge_catch_up_retry_state(retry_state, pending_retry_state));
    CatchUpRetryScanDecision::Proceed(retry_state)
}

pub(super) fn rearm_catch_up_retry_after_fetch_failure(
    shared: &SharedData,
    channel_id: ChannelId,
    retry_state: CatchUpRetryState,
) -> Option<CatchUpRetryState> {
    if retry_state.is_exhausted_hold() {
        return Some(retry_state);
    }
    let ts = chrono::Local::now().format("%H:%M:%S");
    let attempted_failures = retry_state.fetch_failures.saturating_add(1);
    let Some(next_retry_state) = retry_state.after_fetch_failure() else {
        tracing::warn!(
            "  [{ts}] ⚠ catch-up: retry scan for channel {} failed {} time(s); giving up after {} re-arm(s) at checkpoint {}",
            channel_id,
            attempted_failures,
            CATCH_UP_RETRY_FETCH_FAILURE_LIMIT,
            retry_state.checkpoint
        );
        return None;
    };
    let rearmed = arm_catch_up_retry_state(shared, channel_id, next_retry_state);
    tracing::warn!(
        "  [{ts}] 🔁 catch-up: retry scan failed for channel {}; re-armed after checkpoint {} (failure {}/{})",
        channel_id,
        rearmed.checkpoint,
        rearmed.fetch_failures,
        CATCH_UP_RETRY_FETCH_FAILURE_LIMIT
    );
    Some(rearmed)
}

// Exhaustion retains the origin for periodic recovery without arming queue-drain retries.
pub(super) fn rearm_catch_up_retry_after_defer(
    shared: &SharedData,
    channel_id: ChannelId,
    retry_after: u64,
    threaded_prior: Option<CatchUpRetryState>,
    clock: &dyn CatchUpClock,
) -> Option<u64> {
    let prior = catch_up_retry_prior(shared, channel_id, threaded_prior);
    let base = prior.unwrap_or_else(|| CatchUpRetryState::new_at(retry_after, clock.sample().1));
    if base.is_exhausted_hold() {
        return None;
    }
    let ts = chrono::Local::now().format("%H:%M:%S");
    let attempted_rearms = base.deferred_rearms.saturating_add(1);
    let Some(next_retry_state) = base.after_deferred_rearm(retry_after) else {
        if retain_exhausted_hold(shared, channel_id, base, retry_after) {
            tracing::warn!(
                "  [{ts}] ⚠ catch-up: channel {} deferred {} time(s); giving up after {} re-arm(s) at checkpoint {}; \
             repair the cause; the next periodic sweep automatically recovers this backlog \
             within 24h of its original arm (in-process only); a restart before repair \
             loses the hold and follows the existing TooOld DLQ path",
                channel_id,
                attempted_rearms,
                CATCH_UP_RETRY_DEFERRED_REARM_LIMIT,
                retry_after
            );
        }
        return None;
    };
    let rearmed = arm_catch_up_retry_state(shared, channel_id, next_retry_state);
    Some(rearmed.checkpoint)
}

pub(super) fn catch_up_message_age_reference_time(
    scan_wall_time: chrono::DateTime<chrono::Utc>,
    scan_instant: Instant,
    retry_state: Option<CatchUpRetryState>,
) -> chrono::DateTime<chrono::Utc> {
    let Some(retry_state) = retry_state else {
        return scan_wall_time;
    };
    if retry_state.hold_expired_at(scan_instant) {
        return scan_wall_time;
    }
    let elapsed_since_arm = scan_instant
        .checked_duration_since(retry_state.armed_at)
        .unwrap_or_default();
    let Ok(elapsed_since_arm) = chrono::Duration::from_std(elapsed_since_arm) else {
        return scan_wall_time;
    };
    scan_wall_time - elapsed_since_arm
}

impl CatchUpRetryState {
    pub(super) fn is_exhausted_hold(self) -> bool {
        self.deferred_rearms > CATCH_UP_RETRY_DEFERRED_REARM_LIMIT
    }

    pub(super) fn hold_expired_at(self, now: Instant) -> bool {
        self.is_exhausted_hold() && now.saturating_duration_since(self.armed_at) >= HOLD_TTL
    }

    #[cfg(test)]
    pub(super) fn new(checkpoint: u64) -> Self {
        Self::new_at(checkpoint, Instant::now())
    }

    pub(super) fn new_at(checkpoint: u64, armed_at: Instant) -> Self {
        Self {
            checkpoint,
            fetch_failures: 0,
            deferred_rearms: 0,
            armed_at,
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

    // An exhausted arm is retained separately from the bounded retry budget.
    pub(super) fn after_deferred_rearm(self, checkpoint: u64) -> Option<Self> {
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
        // An older cursor inherits the exhausted budget rather than gaining fresh retries.
        fetch_failures: existing.fetch_failures.max(retry_state.fetch_failures),
        deferred_rearms: existing.deferred_rearms.max(retry_state.deferred_rearms),
        armed_at: existing.armed_at.min(retry_state.armed_at),
    }
}

pub(super) fn catch_up_retry_prior(
    shared: &SharedData,
    channel_id: ChannelId,
    threaded_prior: Option<CatchUpRetryState>,
) -> Option<CatchUpRetryState> {
    let map_prior = shared
        .catch_up_retry_pending
        .get(&channel_id)
        .map(|entry| *entry.value());
    if map_prior.is_some_and(|state| state.is_exhausted_hold()) {
        return map_prior;
    }
    match (
        threaded_prior.filter(|state| !state.is_exhausted_hold()),
        map_prior,
    ) {
        (Some(threaded), Some(mapped)) => Some(merge_catch_up_retry_state(Some(threaded), mapped)),
        (Some(state), None) | (None, Some(state)) => Some(state),
        (None, None) => None,
    }
}

// Capacity and membership waits retain the origin without spending Deferred budget.
pub(super) fn arm_catch_up_retry_preserving_prior(
    shared: &SharedData,
    channel_id: ChannelId,
    retry_after: u64,
    threaded_prior: Option<CatchUpRetryState>,
    clock: &dyn CatchUpClock,
) -> u64 {
    let prior = catch_up_retry_prior(shared, channel_id, threaded_prior);
    if let Some(hold) = prior.filter(|state| state.is_exhausted_hold()) {
        return hold.checkpoint;
    }
    let base = prior.unwrap_or_else(|| CatchUpRetryState::new_at(retry_after, clock.sample().1));
    let state = CatchUpRetryState {
        checkpoint: retry_after,
        ..base
    };
    arm_catch_up_retry_state(shared, channel_id, state).checkpoint
}

pub(super) fn merge_catch_up_retry_checkpoint(existing: Option<u64>, retry_after: u64) -> u64 {
    existing.map_or(retry_after, |checkpoint| checkpoint.min(retry_after))
}

const HOLD_IDLE: u8 = CATCH_UP_RETRY_DEFERRED_REARM_LIMIT + 1;
// Every sweep holds the channel lease before consuming retry state or changing its hold.
pub(super) struct CatchUpChannelScan<'a> {
    shared: &'a SharedData,
    channel_id: ChannelId,
    settled_checkpoint: Option<u64>,
}

impl CatchUpChannelScan<'_> {
    pub(super) fn settled(&mut self, checkpoint: Option<u64>) {
        self.settled_checkpoint = checkpoint;
    }
}

impl Drop for CatchUpChannelScan<'_> {
    fn drop(&mut self) {
        if let dashmap::mapref::entry::Entry::Occupied(mut entry) =
            self.shared.catch_up_scan_in_progress.entry(self.channel_id)
        {
            if let Some(mut retry) = entry.get_mut().1.take() {
                let settled = self.settled_checkpoint.unwrap_or(retry.checkpoint);
                if settled <= retry.checkpoint {
                    retry.checkpoint = retry.checkpoint.max(settled);
                    arm_catch_up_retry_state(self.shared, self.channel_id, retry);
                }
            }
            entry.remove();
        }
    }
}

pub(super) fn try_channel_scan(
    shared: &SharedData,
    channel_id: ChannelId,
    handed_retry: Option<CatchUpRetryState>,
) -> Option<CatchUpChannelScan<'_>> {
    let mut slot = shared
        .catch_up_scan_in_progress
        .entry(channel_id)
        .or_insert((false, None));
    if slot.1.is_none()
        && handed_retry.is_some_and(|retry| {
            shared
                .last_message_ids
                .get(&channel_id)
                .is_some_and(|live| *live > retry.checkpoint)
                && shared
                    .catch_up_retry_pending
                    .get(&channel_id)
                    .is_none_or(|pending| pending.checkpoint > retry.checkpoint)
        })
    {
        drop(slot);
        shared
            .catch_up_scan_in_progress
            .remove_if(&channel_id, |_, slot| !slot.0 && slot.1.is_none());
        return None;
    }
    if let Some(retry) = handed_retry.filter(|state| !state.is_exhausted_hold()) {
        slot.1 = Some(merge_catch_up_retry_state(slot.1, retry));
    }
    if slot.0 {
        return None;
    }
    slot.0 = true;
    Some(CatchUpChannelScan {
        shared,
        channel_id,
        settled_checkpoint: None,
    })
}

pub(super) fn retain_exhausted_hold(
    shared: &SharedData,
    channel_id: ChannelId,
    base: CatchUpRetryState,
    checkpoint: u64,
) -> bool {
    let hold = CatchUpRetryState {
        checkpoint,
        deferred_rearms: HOLD_IDLE,
        ..base
    };
    match shared.catch_up_retry_pending.entry(channel_id) {
        dashmap::mapref::entry::Entry::Occupied(mut entry) => {
            if entry.get().is_exhausted_hold() {
                return false;
            }
            let existing = *entry.get();
            *entry.get_mut() = merge_catch_up_retry_state(Some(existing), hold);
            true
        }
        dashmap::mapref::entry::Entry::Vacant(entry) => {
            entry.insert(hold);
            true
        }
    }
}

pub(super) fn finish_hold_scan(
    shared: &SharedData,
    channel_id: ChannelId,
    prior: Option<CatchUpRetryState>,
    durable: Option<u64>,
    remaining: Option<RetainedBarrier>,
) {
    if let Some(prior) = prior.filter(|state| state.is_exhausted_hold()) {
        shared
            .catch_up_retry_pending
            .remove_if_mut(&channel_id, |_, entry| {
                if entry.armed_at != prior.armed_at || entry.deferred_rearms != HOLD_IDLE {
                    return false;
                }
                // Partial progress transfers the same origin to the remaining frontier.
                if remaining.is_some()
                    && let Some(durable) = durable
                {
                    entry.checkpoint = entry.checkpoint.max(durable);
                }
                durable.is_some_and(|settled| settled > entry.checkpoint)
            });
    }
}

// TTL ends identity uncertainty; resolved identities and sender authorization still filter DLQ.
pub(super) fn hold_utility_resolution(
    expired: bool,
    resolution: super::super::health::UtilityBotUserIdResolution,
    ambiguous_announce: Option<u64>,
) -> super::super::health::UtilityBotUserIdResolution {
    use super::super::health::UtilityBotUserIdResolution::{Resolved, Unavailable, Unconfigured};
    match (expired, resolution, ambiguous_announce) {
        (true, Unavailable, Some(author)) => Resolved(author),
        (true, Unavailable, None) => Unconfigured,
        _ => resolution,
    }
}

// Reserve a drained retry before releasing the map entry so periodic scans retain its origin.
pub(in crate::services::discord) fn take_catch_up_retry_checkpoint_after_queue_drain(
    shared: &SharedData,
    channel_id: ChannelId,
    queue_len_after: usize,
) -> Option<CatchUpRetryState> {
    if !super::should_trigger_catch_up_retry(queue_len_after) {
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

pub(super) fn record_exhausted_hold_expiry<A: super::api::CatchUpDiscordApi + ?Sized>(
    api: &A,
    shared: &SharedData,
    prior: Option<CatchUpRetryState>,
    message: &poise::serenity_prelude::Message,
) -> bool {
    let Some(prior) = prior else {
        return false;
    };
    let claimed = shared
        .catch_up_retry_pending
        .get(&message.channel_id)
        .is_some_and(|entry| *entry == prior && entry.deferred_rearms == HOLD_IDLE);
    if !claimed {
        return false;
    }
    let _ = api.record_too_old_dead_letter(
        shared.pg_pool.as_ref(),
        crate::db::relay_dead_letter::RelayDeadLetterRecord {
            kind: crate::db::relay_dead_letter::KIND_CATCH_UP_TOO_OLD.to_string(),
            channel_id: message.channel_id.to_string(),
            author_id: Some(message.author.id.get().to_string()),
            message_id: Some(message.id.to_string()),
            content: message.content.trim().to_string(),
            reason: "exhausted catch-up hold expired at 24h TTL".to_string(),
        },
    );
    tracing::warn!(
        channel_id = message.channel_id.get(),
        message_id = message.id.get(),
        "catch-up exhausted hold expired at 24h TTL; message preserved in TooOld DLQ"
    );
    true
}

/// `/clear` also drops the pending retry and lifts the checkpoint past the cleared ids,
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

#[cfg(test)]
pub(in crate::services::discord) fn arm_catch_up_retry_for_tests(
    shared: &SharedData,
    channel_id: ChannelId,
    checkpoint: u64,
) {
    let state = CatchUpRetryState::new(checkpoint);
    shared.catch_up_retry_pending.insert(channel_id, state);
}
