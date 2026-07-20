use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock};

use poise::serenity_prelude::ChannelId;

use super::snapshot::WatcherStateSnapshot;
use super::{HealthRegistry, stall_liveness};
use crate::services::discord::inflight::{InflightTurnIdentity, InflightTurnState};
use crate::services::discord::relay_health::RelayStallState;
use crate::services::discord::relay_recovery::{
    self, RelayRecoveryActionKind, RelayRecoveryApplySource, RelayRecoveryError,
};
use crate::services::discord::{RelayFrontierToken, SharedData};
use crate::services::provider::ProviderKind;

// Delay after each admitted attempt. The sixth (960s) is the terminal capped
// horizon from the issue contract; no seventh action is admitted. Consequently
// the six actual action times are cumulative +0/+30/+90/+210/+450/+930. The
// hard placeholder shield expires at +900, so the final action intentionally
// runs under the reclaim semantics restored by that bound.
const REDRIVE_BACKOFF_SECS: [i64; 6] = [30, 60, 120, 240, 480, 960];
const REDRIVE_MAX_NO_PROGRESS_ATTEMPTS: u8 = 6;
const _: () = assert!(REDRIVE_BACKOFF_SECS.len() == REDRIVE_MAX_NO_PROGRESS_ATTEMPTS as usize);

type RedriveKey = (String, String, u64);

#[derive(Clone, Debug, Eq, PartialEq)]
struct RedriveEpisode {
    frontier: u64,
    reset_incarnation: u64,
    identity: Option<InflightTurnIdentity>,
    turn_nonce: Option<String>,
    reconnect_count: u64,
}

impl RedriveEpisode {
    fn resets(&self, previous: &Self) -> bool {
        self.reset_incarnation != previous.reset_incarnation
            || self.frontier > previous.frontier
            || (self.identity.is_some() && self.identity != previous.identity)
            || (self.turn_nonce.is_some() && self.turn_nonce != previous.turn_nonce)
            || self.reconnect_count != previous.reconnect_count
    }

    fn self_reattach_identity(
        &self,
        post: &InflightTurnState,
    ) -> Option<(InflightTurnIdentity, Option<String>)> {
        let previous = self.identity.as_ref()?;
        let post_identity = InflightTurnIdentity::from_state(post);
        let previous_nonce = self.turn_nonce.as_deref().filter(|nonce| !nonce.is_empty());
        let post_nonce = post.turn_nonce.as_deref().filter(|nonce| !nonce.is_empty());
        let same_nonce = previous_nonce.is_some() && previous_nonce == post_nonce;
        let same_managed_turn = previous.user_msg_id != 0
            && previous.user_msg_id == post_identity.user_msg_id
            && previous.started_at == post_identity.started_at
            && previous.tmux_session_name == post_identity.tmux_session_name;
        let synthetic_rebind = post.rebind_origin
            && post_identity.user_msg_id == 0
            && previous.tmux_session_name == post_identity.tmux_session_name
            && !matches!(
                post.turn_source,
                crate::services::discord::inflight::TurnSource::MonitorTriggered
            );
        (same_nonce || same_managed_turn || synthetic_rebind)
            .then_some((post_identity, post_nonce.map(str::to_string)))
    }
}

#[derive(Clone, Debug)]
struct RedriveAttemptState {
    episode: RedriveEpisode,
    attempts: u8,
    last_attempt_unix: i64,
    capped_alarm_emitted: bool,
    retry_not_before_unix: Option<i64>,
    shield_started_at_millis: Option<i64>,
}

impl RedriveAttemptState {
    fn new(episode: RedriveEpisode, now_unix: i64) -> Self {
        Self {
            episode,
            attempts: 0,
            last_attempt_unix: now_unix,
            capped_alarm_emitted: false,
            retry_not_before_unix: None,
            shield_started_at_millis: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RedriveAttemptDecision {
    attempt: Option<u8>,
    emit_capped_alarm: bool,
}

static REDRIVE_ATTEMPTS: LazyLock<dashmap::DashMap<RedriveKey, RedriveAttemptState>> =
    LazyLock::new(dashmap::DashMap::new);
static REDRIVE_PLACEHOLDER_SHIELDS: LazyLock<dashmap::DashMap<RedriveKey, (RedriveEpisode, i64)>> =
    LazyLock::new(dashmap::DashMap::new);

impl SharedData {
    fn redrive_key(&self, provider: &ProviderKind, channel_id: ChannelId) -> RedriveKey {
        (
            self.token_hash.clone(),
            provider.as_str().to_string(),
            channel_id.get(),
        )
    }

    fn redrive_episode(
        &self,
        provider: &ProviderKind,
        channel_id: ChannelId,
        snapshot: &WatcherStateSnapshot,
    ) -> RedriveEpisode {
        let turn_nonce =
            crate::services::discord::inflight::load_inflight_state(provider, channel_id.get())
                .filter(|state| {
                    snapshot
                        .inflight_identity
                        .as_ref()
                        .is_some_and(|identity| identity.matches_state(state))
                })
                .and_then(|state| state.turn_nonce.filter(|nonce| !nonce.is_empty()));
        RedriveEpisode {
            frontier: snapshot.last_relay_offset,
            reset_incarnation: self.relay_frontier_token(channel_id).reset_incarnation,
            identity: snapshot.inflight_identity.clone(),
            turn_nonce,
            reconnect_count: snapshot.reconnect_count,
        }
    }

    fn redrive_attempt_decision(
        &self,
        provider: &ProviderKind,
        channel_id: ChannelId,
        snapshot: &WatcherStateSnapshot,
        now_unix: i64,
    ) -> RedriveAttemptDecision {
        let key = self.redrive_key(provider, channel_id);
        let episode = self.redrive_episode(provider, channel_id, snapshot);
        let mut state = REDRIVE_ATTEMPTS
            .entry(key)
            .or_insert_with(|| RedriveAttemptState::new(episode.clone(), now_unix));
        if episode.resets(&state.episode) {
            *state = RedriveAttemptState::new(episode, now_unix);
        }
        if state.attempts >= REDRIVE_MAX_NO_PROGRESS_ATTEMPTS {
            debug_assert_eq!(state.attempts, REDRIVE_MAX_NO_PROGRESS_ATTEMPTS);
            debug_assert_eq!(REDRIVE_BACKOFF_SECS[REDRIVE_BACKOFF_SECS.len() - 1], 960);
            let emit_capped_alarm = !state.capped_alarm_emitted;
            state.capped_alarm_emitted = true;
            return RedriveAttemptDecision {
                attempt: None,
                emit_capped_alarm,
            };
        }
        if state
            .retry_not_before_unix
            .is_some_and(|not_before| now_unix < not_before)
        {
            return RedriveAttemptDecision {
                attempt: None,
                emit_capped_alarm: false,
            };
        }
        if state.attempts > 0 {
            let delay = REDRIVE_BACKOFF_SECS[usize::from(state.attempts - 1)];
            if now_unix.saturating_sub(state.last_attempt_unix).max(0) < delay {
                return RedriveAttemptDecision {
                    attempt: None,
                    emit_capped_alarm: false,
                };
            }
        }
        RedriveAttemptDecision {
            attempt: Some(state.attempts + 1),
            emit_capped_alarm: false,
        }
    }

    fn commit_redrive_success(
        &self,
        provider: &ProviderKind,
        channel_id: ChannelId,
        shield_channel_id: ChannelId,
        now_unix: i64,
        reattached: bool,
    ) -> RedriveAttemptDecision {
        let now_millis = chrono::Utc::now().timestamp_millis();
        let key = self.redrive_key(provider, channel_id);
        let mut state = REDRIVE_ATTEMPTS
            .get_mut(&key)
            .expect("redrive success must follow an admitted attempt");
        state.attempts += 1;
        let first_attempt_of_episode = state.attempts == 1;
        let shield_started_at_millis = *state.shield_started_at_millis.get_or_insert(now_millis);
        state.last_attempt_unix = now_unix;
        state.retry_not_before_unix = None;
        if reattached {
            let current = self
                .tmux_relay_coord(channel_id)
                .reconnect_count
                .load(Ordering::Acquire);
            if current == state.episode.reconnect_count.saturating_add(1) {
                state.episode.reconnect_count = current;
            }
            if let Some(post) =
                crate::services::discord::inflight::load_inflight_state(provider, channel_id.get())
                && let Some((identity, turn_nonce)) = state.episode.self_reattach_identity(&post)
            {
                state.episode.identity = Some(identity);
                state.episode.turn_nonce = turn_nonce;
            }
        }
        let emit_capped_alarm =
            state.attempts == REDRIVE_MAX_NO_PROGRESS_ATTEMPTS && !state.capped_alarm_emitted;
        state.capped_alarm_emitted |= emit_capped_alarm;
        let decision = RedriveAttemptDecision {
            attempt: Some(state.attempts),
            emit_capped_alarm,
        };
        let episode = if shield_channel_id == channel_id {
            state.episode.clone()
        } else {
            let owner_inflight = crate::services::discord::inflight::load_inflight_state(
                provider,
                shield_channel_id.get(),
            );
            let shield_token = self.relay_frontier_token(shield_channel_id);
            RedriveEpisode {
                frontier: shield_token.committed_offset,
                reset_incarnation: shield_token.reset_incarnation,
                identity: owner_inflight
                    .as_ref()
                    .map(InflightTurnIdentity::from_state),
                turn_nonce: owner_inflight
                    .and_then(|post| post.turn_nonce.filter(|nonce| !nonce.is_empty())),
                reconnect_count: self
                    .tmux_relay_coord(shield_channel_id)
                    .reconnect_count
                    .load(Ordering::Acquire),
            }
        };
        drop(state);

        let mut shield = REDRIVE_PLACEHOLDER_SHIELDS
            .entry(self.redrive_key(provider, shield_channel_id))
            .or_insert_with(|| (episode.clone(), shield_started_at_millis));
        if first_attempt_of_episode {
            shield.0 = episode;
        } else if shield_channel_id == channel_id {
            shield.0 = episode;
        }
        shield.1 = shield_started_at_millis;
        decision
    }

    fn note_redrive_noop(
        &self,
        provider: &ProviderKind,
        channel_id: ChannelId,
        now_unix: i64,
        cooldown_secs: i64,
    ) {
        if let Some(mut state) = REDRIVE_ATTEMPTS.get_mut(&self.redrive_key(provider, channel_id)) {
            state.retry_not_before_unix = Some(now_unix.saturating_add(cooldown_secs.max(30)));
        }
    }

    pub(in crate::services::discord) fn redrive_placeholder_shield_context(
        &self,
        provider: &ProviderKind,
        channel_id: ChannelId,
    ) -> Option<(
        i64,
        RelayFrontierToken,
        Option<crate::services::discord::inflight::InflightTurnIdentity>,
    )> {
        REDRIVE_PLACEHOLDER_SHIELDS
            .get(&self.redrive_key(provider, channel_id))
            .filter(|shield| {
                self.relay_frontier_token(channel_id).reset_incarnation
                    == shield.0.reset_incarnation
            })
            .map(|shield| {
                (
                    shield.1,
                    RelayFrontierToken {
                        reset_incarnation: shield.0.reset_incarnation,
                        committed_offset: shield.0.frontier,
                    },
                    shield.0.identity.clone(),
                )
            })
    }
}

pub(super) async fn apply_watchdog_orphan_token_cleanup(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    shared: Arc<SharedData>,
    channel_id: ChannelId,
) -> bool {
    match apply_orphan_pending_token_cleanup(
        registry,
        provider,
        shared,
        channel_id,
        RelayRecoveryApplySource::StallWatchdog,
    )
    .await
    {
        Ok(applied) => applied,
        Err(error) => {
            trace_orphan_auto_heal_error(provider, channel_id, &error);
            false
        }
    }
}

pub(super) async fn run_orphan_token_auto_heal_pass(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    runtimes: &[Arc<SharedData>],
) -> usize {
    let mut applied = 0usize;
    for shared in runtimes {
        let mut redrive_channels = std::collections::HashSet::new();
        let mailbox_snapshots = shared.mailboxes.snapshot_all().await;
        for (channel_id, mailbox) in mailbox_snapshots {
            redrive_channels.insert(channel_id);
            if mailbox.cancel_token.is_some() {
                match apply_orphan_pending_token_cleanup(
                    registry,
                    provider,
                    shared.clone(),
                    channel_id,
                    RelayRecoveryApplySource::ProbeAutoHeal,
                )
                .await
                {
                    Ok(true) => applied += 1,
                    Ok(false) => {}
                    Err(RelayRecoveryError::SnapshotNotFound { .. }) => {}
                    Err(error) => trace_orphan_auto_heal_error(provider, channel_id, &error),
                }
            }

            match redrive_undelivered_backlog(registry, provider, shared.clone(), channel_id).await
            {
                Ok(true) => applied += 1,
                Ok(false) => {}
                Err(RelayRecoveryError::SnapshotNotFound { .. }) => {}
                Err(error) => trace_orphan_auto_heal_error(provider, channel_id, &error),
            }
        }

        let watcher_owner_channels: Vec<ChannelId> = shared
            .tmux_watchers
            .iter()
            .filter_map(|entry| {
                shared
                    .tmux_watchers
                    .owner_channel_for_tmux_session(entry.key())
            })
            .collect();
        for channel_id in watcher_owner_channels {
            if !redrive_channels.insert(channel_id) {
                continue;
            }
            match redrive_undelivered_backlog(registry, provider, shared.clone(), channel_id).await
            {
                Ok(true) => applied += 1,
                Ok(false) => {}
                Err(RelayRecoveryError::SnapshotNotFound { .. }) => {}
                Err(error) => trace_orphan_auto_heal_error(provider, channel_id, &error),
            }
        }
    }
    applied
}

async fn redrive_undelivered_backlog(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    shared: Arc<SharedData>,
    channel_id: ChannelId,
) -> Result<bool, RelayRecoveryError> {
    registry
        .redrive_undelivered_backlog_at(
            provider,
            shared,
            channel_id,
            chrono::Utc::now().timestamp(),
        )
        .await
}

impl HealthRegistry {
    pub(in crate::services::discord) async fn redrive_undelivered_backlog_at(
        &self,
        provider: &ProviderKind,
        shared: Arc<SharedData>,
        channel_id: ChannelId,
        now_unix_secs: i64,
    ) -> Result<bool, RelayRecoveryError> {
        #[cfg(test)]
        let _test_clock = stall_liveness::set_redrive_grace_test_clock(now_unix_secs);
        let Some(snapshot) = self
            .snapshot_watcher_state_for_shared(provider, shared.clone(), channel_id.get())
            .await
        else {
            return Ok(false);
        };

        let token = shared.tmux_relay_coord(channel_id).frontier_token();
        if !should_redrive_undelivered_backlog(provider, channel_id, &snapshot, token) {
            return Ok(false);
        }
        if redrive_should_yield_to_live_relay(&shared, channel_id, &snapshot) {
            return Ok(false);
        }
        let Some(_frontier_mutation) = shared.acquire_relay_frontier_mutation(channel_id, token)
        else {
            return Ok(false);
        };
        let attempt =
            shared.redrive_attempt_decision(provider, channel_id, &snapshot, now_unix_secs);
        trace_redrive_cap_if_needed(provider, channel_id, &snapshot, attempt);
        if attempt.attempt.is_none() {
            return Ok(false);
        }

        let (applied, reattached, noop_cooldown_secs) = if nudge_existing_watcher_for_backlog(
            &shared,
            provider,
            &snapshot,
            channel_id,
            now_unix_secs,
            token,
        ) {
            (true, false, None)
        } else {
            if redrive_should_yield_to_live_relay(&shared, channel_id, &snapshot)
                || !shared.relay_frontier_token_is_current(channel_id, token)
            {
                return Ok(false);
            }
            let response = relay_recovery::auto_apply_relay_recovery_for_shared(
                self,
                shared.clone(),
                provider,
                channel_id.get(),
                RelayRecoveryActionKind::ReattachWatcher,
                RelayRecoveryApplySource::ProbeAutoHeal,
            )
            .await?;
            (
                response.applied,
                true,
                Some(response.decision.auto_heal.window_secs),
            )
        };
        if applied {
            if !shared.relay_frontier_token_is_current(channel_id, token) {
                return Ok(false);
            }
            let shield_channel_id =
                redrive_shield_channel_for_action(&shared, &snapshot, channel_id, reattached);
            let committed = shared.commit_redrive_success(
                provider,
                channel_id,
                shield_channel_id,
                now_unix_secs,
                reattached,
            );
            trace_redrive_cap_if_needed(provider, channel_id, &snapshot, committed);
        } else if let Some(cooldown_secs) = noop_cooldown_secs {
            shared.note_redrive_noop(provider, channel_id, now_unix_secs, cooldown_secs);
        }
        Ok(applied)
    }
}

fn redrive_shield_channel_for_action(
    shared: &SharedData,
    snapshot: &WatcherStateSnapshot,
    fallback_channel_id: ChannelId,
    reattached: bool,
) -> ChannelId {
    if !reattached {
        return snapshot
            .watcher_owner_channel_id
            .map(ChannelId::new)
            .unwrap_or(fallback_channel_id);
    }
    snapshot
        .tmux_session
        .as_deref()
        .and_then(|tmux_session| {
            shared
                .tmux_watchers
                .owner_channel_for_tmux_session(tmux_session)
        })
        .unwrap_or(fallback_channel_id)
}

fn trace_redrive_cap_if_needed(
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
    decision: RedriveAttemptDecision,
) {
    if decision.emit_capped_alarm {
        tracing::error!(
            target: "agentdesk::discord::relay_recovery",
            event = "redrive_no_progress_capped",
            provider = provider.as_str(),
            channel_id = channel_id.get(),
            last_relay_offset = snapshot.last_relay_offset,
            attempts = REDRIVE_MAX_NO_PROGRESS_ATTEMPTS,
            "redrive stopped after the no-progress attempt cap"
        );
    }
}

fn has_live_undelivered_backlog(snapshot: &WatcherStateSnapshot) -> bool {
    snapshot.unread_bytes.is_some_and(|bytes| bytes > 0)
        && snapshot.tmux_session_alive == Some(true)
        && !snapshot.inflight_terminal_delivery_committed
}

fn should_redrive_undelivered_backlog(
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
    token: RelayFrontierToken,
) -> bool {
    has_live_undelivered_backlog(snapshot)
        && stall_liveness::stalled_undelivered_backlog_for_redrive(
            provider, channel_id, snapshot, token,
        )
}

fn live_relay_frontier_advanced_since_snapshot(
    shared: &SharedData,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
) -> bool {
    shared.committed_relay_offset(channel_id) > snapshot.last_relay_offset
}

/// #4181 item-1: redrive must yield to a live relay either because the committed
/// frontier already advanced past the snapshot (delivery landed) OR because a
/// relay emission is still in-flight (`relay_slot` non-zero). The committed-only
/// check has a TOCTOU: a single relay POST held >stall-grace under extreme
/// rate-limiting freezes the committed offset without the emission having
/// finished, so the offset-only stall test can pass while a POST is mid-flight;
/// redriving then double-sends the range that POST is about to commit (a
/// duplicate, not a loss). Consulting the in-flight slot closes that window.
fn redrive_should_yield_to_live_relay(
    shared: &SharedData,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
) -> bool {
    live_relay_frontier_advanced_since_snapshot(shared, channel_id, snapshot)
        || shared.relay_emission_in_flight(channel_id)
}

fn nudge_existing_watcher_for_backlog(
    shared: &SharedData,
    provider: &ProviderKind,
    snapshot: &WatcherStateSnapshot,
    channel_id: ChannelId,
    now_unix_secs: i64,
    token: RelayFrontierToken,
) -> bool {
    #[cfg(test)]
    let _test_clock = stall_liveness::set_redrive_grace_test_clock(now_unix_secs);
    if !should_redrive_undelivered_backlog(provider, channel_id, snapshot, token) {
        return false;
    }
    let Some(_frontier_mutation) = shared.acquire_relay_frontier_mutation(channel_id, token) else {
        return false;
    };

    let owner_channel_id = snapshot
        .watcher_owner_channel_id
        .map(ChannelId::new)
        .unwrap_or(channel_id);
    let Some(watcher) = shared.tmux_watchers.get(&owner_channel_id) else {
        return false;
    };
    if snapshot.tmux_session.as_deref() != Some(watcher.tmux_session_name.as_str()) {
        return false;
    }
    if snapshot.inflight_output_path.as_deref() != Some(watcher.output_path.as_str()) {
        return false;
    }
    if !nudge_watcher_handle_for_backlog(shared, snapshot, watcher.value(), channel_id, token) {
        return false;
    }

    tracing::warn!(
        target: "agentdesk::discord::relay_recovery",
        channel_id = channel_id.get(),
        watcher_owner_channel_id = owner_channel_id.get(),
        tmux_session = %watcher.tmux_session_name,
        output_path = %watcher.output_path,
        last_relay_offset = snapshot.last_relay_offset,
        unread_bytes = ?snapshot.unread_bytes,
        "redrive nudged existing tmux watcher to re-read undelivered backlog from confirmed frontier"
    );
    true
}

fn nudge_watcher_handle_for_backlog(
    shared: &SharedData,
    snapshot: &WatcherStateSnapshot,
    watcher: &crate::services::discord::TmuxWatcherHandle,
    channel_id: ChannelId,
    token: RelayFrontierToken,
) -> bool {
    if watcher.cancel.load(Ordering::Relaxed)
        || watcher.heartbeat_stale()
        || watcher.paused.load(Ordering::Relaxed)
    {
        return false;
    }
    let Ok(mut resume_offset) = watcher.resume_offset.lock() else {
        return false;
    };
    if redrive_should_yield_to_live_relay(shared, channel_id, snapshot)
        || !shared.relay_frontier_token_is_current(channel_id, token)
    {
        return false;
    }
    *resume_offset = Some(snapshot.last_relay_offset);
    watcher.turn_delivered.store(false, Ordering::Release);
    true
}

async fn apply_orphan_pending_token_cleanup(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    shared: Arc<SharedData>,
    channel_id: ChannelId,
    source: RelayRecoveryApplySource,
) -> Result<bool, RelayRecoveryError> {
    if source == RelayRecoveryApplySource::ProbeAutoHeal {
        let Some(snapshot) = registry
            .snapshot_watcher_state_for_shared(provider, shared.clone(), channel_id.get())
            .await
        else {
            return Ok(false);
        };
        if snapshot.relay_stall_state != RelayStallState::OrphanPendingToken {
            return Ok(false);
        }
    }

    let watchdog_watcher = (source == RelayRecoveryApplySource::StallWatchdog)
        .then(|| {
            shared.tmux_watchers.get(&channel_id).map(|watcher| {
                (
                    watcher.tmux_session_name.clone(),
                    watcher.output_path.clone(),
                    watcher.cancel.clone(),
                )
            })
        })
        .flatten();
    let response = relay_recovery::auto_apply_relay_recovery_for_shared(
        registry,
        shared.clone(),
        provider,
        channel_id.get(),
        RelayRecoveryActionKind::ClearOrphanPendingToken,
        source,
    )
    .await?;
    let removed_mailbox_token = response.applied
        && response
            .apply_result
            .as_ref()
            .is_some_and(|result| result.removed_mailbox_token);

    if removed_mailbox_token
        && let Some((tmux_session_name, output_path, cancel)) = watchdog_watcher
    {
        shared.tmux_watchers.cancel_and_remove_channel_if_current(
            &channel_id,
            &tmux_session_name,
            &output_path,
            &cancel,
        );
    }

    Ok(removed_mailbox_token)
}

fn trace_orphan_auto_heal_error(
    provider: &ProviderKind,
    channel_id: ChannelId,
    error: &RelayRecoveryError,
) {
    tracing::warn!(
        target: "agentdesk::discord::relay_recovery",
        provider = provider.as_str(),
        channel_id = channel_id.get(),
        status = error.status_str(),
        body = %error.body(),
        "relay recovery auto-heal skipped"
    );
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use crate::services::discord::relay_health::{RelayActiveTurn, RelayHealthSnapshot};
    use tracing_subscriber::fmt::MakeWriter;

    use super::*;

    fn watcher_handle(
        tmux_session_name: &str,
        output_path: &str,
        resume_offset: Arc<Mutex<Option<u64>>>,
        turn_delivered: Arc<AtomicBool>,
    ) -> crate::services::discord::TmuxWatcherHandle {
        crate::services::discord::TmuxWatcherHandle {
            tmux_session_name: tmux_session_name.to_string(),
            output_path: output_path.to_string(),
            paused: Arc::new(AtomicBool::new(false)),
            resume_offset,
            cancel: Arc::new(AtomicBool::new(false)),
            pause_epoch: Arc::new(AtomicU64::new(0)),
            turn_delivered,
            last_heartbeat_ts_ms: Arc::new(AtomicI64::new(
                crate::services::discord::tmux_watcher_now_ms(),
            )),
        }
    }

    fn backlog_snapshot(
        channel_id: ChannelId,
        tmux_session: &str,
        output_path: &str,
        last_relay_offset: u64,
        capture_offset: u64,
    ) -> WatcherStateSnapshot {
        let unread_bytes = capture_offset.saturating_sub(last_relay_offset);
        WatcherStateSnapshot {
            provider: ProviderKind::Codex.as_str().to_string(),
            attached: true,
            tmux_session: Some(tmux_session.to_string()),
            watcher_owner_channel_id: Some(channel_id.get()),
            last_relay_offset,
            inflight_state_present: true,
            last_relay_ts_ms: 1_700_000_000_000,
            last_capture_offset: Some(capture_offset),
            capture_coordinate: crate::services::discord::health::liveness_authority::CaptureCoordinateObservation {
                offset: Some(capture_offset),
                path_hash: 0,
                file_id: None,
                status: crate::services::discord::health::liveness_authority::CoordinateStatus::Observed,
            },
            unread_bytes: Some(unread_bytes),
            desynced: true,
            reconnect_count: 0,
            inflight_started_at: Some("2026-06-12 00:00:00".to_string()),
            inflight_updated_at: Some("2026-06-12 00:00:00".to_string()),
            inflight_user_msg_id: Some(9001),
            inflight_current_msg_id: Some(9002),
            tmux_session_alive: Some(true),
            has_pending_queue: false,
            mailbox_active_user_msg_id: Some(9001),
            bound_output_path: None,
            bound_session_id: None,
            inflight_terminal_delivery_committed: false,
            inflight_identity: Some(crate::services::discord::inflight::InflightTurnIdentity {
                user_msg_id: 9001,
                started_at: "2026-06-12 00:00:00".to_string(),
                tmux_session_name: Some(tmux_session.to_string()),
                turn_start_offset: Some(last_relay_offset),
            }),
            inflight_finalizer_turn_id: None,
            inflight_output_path: Some(output_path.to_string()),
            relay_stall_state: RelayStallState::TmuxAliveRelayDead,
            relay_health: RelayHealthSnapshot {
                provider: ProviderKind::Codex.as_str().to_string(),
                channel_id: channel_id.get(),
                active_turn: RelayActiveTurn::Foreground,
                tmux_session: Some(tmux_session.to_string()),
                tmux_alive: Some(true),
                watcher_attached: true,
                watcher_attached_stale: false,
                watcher_owner_channel_id: Some(channel_id.get()),
                watcher_owns_live_relay: true,
                bridge_inflight_present: true,
                bridge_current_msg_id: Some(9002),
                mailbox_has_cancel_token: true,
                mailbox_active_user_msg_id: Some(9001),
                mailbox_turn_started_at_ms: None,
                queue_depth: 0,
                pending_discord_callback_msg_id: Some(9002),
                pending_thread_proof: false,
                parent_channel_id: None,
                thread_channel_id: None,
                last_relay_ts_ms: Some(1_700_000_000_000),
                last_outbound_activity_ms: None,
                last_capture_offset: Some(capture_offset),
                last_relay_offset,
                unread_bytes: Some(unread_bytes),
                desynced: true,
                stale_thread_proof: false,
            },
        }
    }

    fn synthetic_rebind_state(
        provider: &ProviderKind,
        channel_id: ChannelId,
        tmux_session: &str,
        output_path: &str,
        started_at: &str,
        last_offset: u64,
    ) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            provider.clone(),
            channel_id.get(),
            None,
            0,
            0,
            0,
            "/api/inflight/rebind".to_string(),
            None,
            Some(tmux_session.to_string()),
            Some(output_path.to_string()),
            None,
            last_offset,
        );
        state.started_at = started_at.to_string();
        state.updated_at = started_at.to_string();
        state.rebind_origin = true;
        state
    }

    #[test]
    fn redrive_nudge_skips_healthy_advancing_backlog() {
        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(4_178_301);
        let tmux_session = "AgentDesk-codex-4178-healthy-drain";
        let output_path = "/tmp/agentdesk-4178-healthy-drain.jsonl";
        let shared = crate::services::discord::make_shared_data_for_tests();
        let resume_offset = Arc::new(Mutex::new(None));
        let turn_delivered = Arc::new(AtomicBool::new(true));
        let watcher = watcher_handle(
            tmux_session,
            output_path,
            resume_offset.clone(),
            turn_delivered.clone(),
        );
        shared.tmux_watchers.insert(channel_id, watcher);
        stall_liveness::clear_stall_watchdog_liveness_state(
            &provider,
            channel_id,
            Some(tmux_session),
        );

        let capture_offset = 301_613;
        let now = 1_800_000_000;
        let snapshot = backlog_snapshot(channel_id, tmux_session, output_path, 128, capture_offset);
        assert!(!nudge_existing_watcher_for_backlog(
            &shared,
            &provider,
            &snapshot,
            channel_id,
            now,
            shared.relay_frontier_token(channel_id),
        ));
        assert_eq!(*resume_offset.lock().unwrap(), None);
        assert!(turn_delivered.load(Ordering::Acquire));

        let advanced_snapshot =
            backlog_snapshot(channel_id, tmux_session, output_path, 256, capture_offset);
        assert!(!nudge_existing_watcher_for_backlog(
            &shared,
            &provider,
            &advanced_snapshot,
            channel_id,
            now + 30,
            shared.relay_frontier_token(channel_id),
        ));
        assert_eq!(*resume_offset.lock().unwrap(), None);
        assert!(turn_delivered.load(Ordering::Acquire));

        stall_liveness::clear_stall_watchdog_liveness_state(
            &provider,
            channel_id,
            Some(tmux_session),
        );
    }

    #[test]
    fn redrive_nudge_requires_matching_output_path() {
        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(4_178_302);
        let tmux_session = "AgentDesk-codex-4178-output-path";
        let watcher_output_path = "/tmp/agentdesk-4178-watcher.jsonl";
        let inflight_output_path = "/tmp/agentdesk-4178-inflight.jsonl";
        let shared = crate::services::discord::make_shared_data_for_tests();
        let resume_offset = Arc::new(Mutex::new(None));
        let turn_delivered = Arc::new(AtomicBool::new(true));
        let watcher = watcher_handle(
            tmux_session,
            watcher_output_path,
            resume_offset.clone(),
            turn_delivered.clone(),
        );
        shared.tmux_watchers.insert(channel_id, watcher);
        stall_liveness::clear_stall_watchdog_liveness_state(
            &provider,
            channel_id,
            Some(tmux_session),
        );

        let capture_offset = 301_613;
        let now = 1_800_000_000;
        let snapshot = backlog_snapshot(
            channel_id,
            tmux_session,
            inflight_output_path,
            128,
            capture_offset,
        );
        assert!(!nudge_existing_watcher_for_backlog(
            &shared,
            &provider,
            &snapshot,
            channel_id,
            now,
            shared.relay_frontier_token(channel_id),
        ));
        assert!(!nudge_existing_watcher_for_backlog(
            &shared,
            &provider,
            &snapshot,
            channel_id,
            now + stall_liveness::STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS as i64,
            shared.relay_frontier_token(channel_id),
        ));
        assert_eq!(*resume_offset.lock().unwrap(), None);
        assert!(turn_delivered.load(Ordering::Acquire));

        stall_liveness::clear_stall_watchdog_liveness_state(
            &provider,
            channel_id,
            Some(tmux_session),
        );
    }

    #[test]
    fn redrive_nudge_skips_if_live_frontier_advanced_after_snapshot() {
        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(4_178_303);
        let tmux_session = "AgentDesk-codex-4178-live-frontier";
        let output_path = "/tmp/agentdesk-4178-live-frontier.jsonl";
        let shared = crate::services::discord::make_shared_data_for_tests();
        let resume_offset = Arc::new(Mutex::new(None));
        let turn_delivered = Arc::new(AtomicBool::new(true));
        let watcher = watcher_handle(
            tmux_session,
            output_path,
            resume_offset.clone(),
            turn_delivered.clone(),
        );
        shared.tmux_watchers.insert(channel_id, watcher);
        stall_liveness::clear_stall_watchdog_liveness_state(
            &provider,
            channel_id,
            Some(tmux_session),
        );

        let capture_offset = 301_613;
        let now = 1_800_000_000;
        let snapshot = backlog_snapshot(channel_id, tmux_session, output_path, 128, capture_offset);
        assert!(!nudge_existing_watcher_for_backlog(
            &shared,
            &provider,
            &snapshot,
            channel_id,
            now,
            shared.relay_frontier_token(channel_id),
        ));
        shared
            .tmux_relay_coord(channel_id)
            .confirmed_end_offset
            .store(256, Ordering::Release);

        assert!(!nudge_existing_watcher_for_backlog(
            &shared,
            &provider,
            &snapshot,
            channel_id,
            now + stall_liveness::STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS as i64,
            shared.relay_frontier_token(channel_id),
        ));
        assert_eq!(*resume_offset.lock().unwrap(), None);
        assert!(turn_delivered.load(Ordering::Acquire));

        stall_liveness::clear_stall_watchdog_liveness_state(
            &provider,
            channel_id,
            Some(tmux_session),
        );
    }

    // #4181 item-1: a relay emission still in-flight (`relay_slot` non-zero)
    // freezes the committed offset without the turn being stalled; redrive must
    // yield to it and NOT rewind the watcher over the in-flight range (which
    // would double-send the bytes that POST is about to commit).
    #[test]
    fn redrive_stale_frontier_token_cannot_nudge_after_reset_4181() {
        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(4_181_002);
        let tmux_session = "AgentDesk-codex-4181-reset-token";
        let output_path = "/tmp/agentdesk-4181-reset-token.jsonl";
        let shared = crate::services::discord::make_shared_data_for_tests();
        let resume_offset = Arc::new(Mutex::new(None));
        let turn_delivered = Arc::new(AtomicBool::new(true));
        shared.tmux_watchers.insert(
            channel_id,
            watcher_handle(
                tmux_session,
                output_path,
                resume_offset.clone(),
                turn_delivered.clone(),
            ),
        );
        let snapshot = backlog_snapshot(channel_id, tmux_session, output_path, 100, 301_613);
        let coord = shared.tmux_relay_coord(channel_id);
        coord.confirmed_end_offset.store(100, Ordering::Release);
        let high_token = shared.relay_frontier_token(channel_id);

        assert!(coord.reset_confirmed_frontier(100, 0));
        assert!(
            !nudge_existing_watcher_for_backlog(
                &shared,
                &provider,
                &snapshot,
                channel_id,
                1_800_000_000,
                high_token,
            ),
            "a reset between admission and nudge must veto the stale-H redrive"
        );
        assert_eq!(*resume_offset.lock().unwrap(), None);
        assert!(turn_delivered.load(Ordering::Acquire));
    }

    #[test]
    fn redrive_nudge_yields_while_relay_emission_in_flight() {
        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(4_181_777);
        let tmux_session = "AgentDesk-codex-4181-inflight-slot";
        let output_path = "/tmp/agentdesk-4181-inflight-slot.jsonl";
        let shared = crate::services::discord::make_shared_data_for_tests();
        let resume_offset = Arc::new(Mutex::new(None));
        let turn_delivered = Arc::new(AtomicBool::new(true));
        let watcher = watcher_handle(
            tmux_session,
            output_path,
            resume_offset.clone(),
            turn_delivered.clone(),
        );
        shared.tmux_watchers.insert(channel_id, watcher);
        stall_liveness::clear_stall_watchdog_liveness_state(
            &provider,
            channel_id,
            Some(tmux_session),
        );

        let capture_offset = 301_613;
        let now = 1_800_000_000;
        let snapshot = backlog_snapshot(channel_id, tmux_session, output_path, 128, capture_offset);
        shared
            .tmux_relay_coord(channel_id)
            .confirmed_end_offset
            .store(snapshot.last_relay_offset, Ordering::Release);
        // Prime the stall observation, then mark a relay emission in-flight
        // (non-zero `relay_slot`) while the committed frontier stays frozen.
        assert!(!nudge_existing_watcher_for_backlog(
            &shared,
            &provider,
            &snapshot,
            channel_id,
            now,
            shared.relay_frontier_token(channel_id),
        ));
        shared
            .tmux_relay_coord(channel_id)
            .relay_slot
            .store(128, Ordering::Release);
        assert!(shared.relay_emission_in_flight(channel_id));

        // Even past the no-progress grace, the in-flight slot must veto redrive.
        assert!(!nudge_existing_watcher_for_backlog(
            &shared,
            &provider,
            &snapshot,
            channel_id,
            now + stall_liveness::STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS as i64,
            shared.relay_frontier_token(channel_id),
        ));
        assert_eq!(*resume_offset.lock().unwrap(), None);
        assert!(turn_delivered.load(Ordering::Acquire));

        // Once the emission completes (slot cleared) and no frontier advanced,
        // the nudge is allowed again (rewinds to the last relayed offset).
        shared
            .tmux_relay_coord(channel_id)
            .relay_slot
            .store(0, Ordering::Release);
        assert!(nudge_existing_watcher_for_backlog(
            &shared,
            &provider,
            &snapshot,
            channel_id,
            now + stall_liveness::STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS as i64,
            shared.relay_frontier_token(channel_id),
        ));
        assert_eq!(
            *resume_offset.lock().unwrap(),
            Some(snapshot.last_relay_offset)
        );

        stall_liveness::clear_stall_watchdog_liveness_state(
            &provider,
            channel_id,
            Some(tmux_session),
        );
    }

    #[derive(Clone)]
    struct CapturingWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for CapturingWriter {
        type Writer = CapturingWriter;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn capture_errors<R>(run: impl FnOnce() -> R) -> (R, String) {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::ERROR)
            .with_ansi(false)
            .without_time()
            .with_writer(CapturingWriter(buffer.clone()))
            .finish();
        let result = tracing::subscriber::with_default(subscriber, run);
        let output = String::from_utf8_lossy(&buffer.lock().unwrap()).into_owned();
        (result, output)
    }

    fn clear_redrive_test_state(
        shared: &SharedData,
        provider: &ProviderKind,
        channel_id: ChannelId,
        tmux_session: &str,
    ) {
        let key = shared.redrive_key(provider, channel_id);
        REDRIVE_ATTEMPTS.remove(&key);
        REDRIVE_PLACEHOLDER_SHIELDS.remove(&key);
        stall_liveness::clear_stall_watchdog_liveness_state(
            provider,
            channel_id,
            Some(tmux_session),
        );
    }

    fn gated_nudge(
        shared: &SharedData,
        provider: &ProviderKind,
        snapshot: &WatcherStateSnapshot,
        channel_id: ChannelId,
        now: i64,
    ) -> (bool, Option<u8>) {
        let _test_clock = stall_liveness::set_redrive_grace_test_clock(now);
        let token = shared.relay_frontier_token(channel_id);
        if !should_redrive_undelivered_backlog(provider, channel_id, snapshot, token)
            || !shared.relay_frontier_token_is_current(channel_id, token)
        {
            return (false, None);
        }
        let decision = shared.redrive_attempt_decision(provider, channel_id, snapshot, now);
        trace_redrive_cap_if_needed(provider, channel_id, snapshot, decision);
        let Some(reserved_attempt) = decision.attempt else {
            return (false, None);
        };
        let nudged = nudge_existing_watcher_for_backlog(
            shared,
            provider,
            snapshot,
            channel_id,
            now,
            shared.relay_frontier_token(channel_id),
        );
        if nudged {
            let shield_channel_id = snapshot
                .watcher_owner_channel_id
                .map(ChannelId::new)
                .unwrap_or(channel_id);
            let committed =
                shared.commit_redrive_success(provider, channel_id, shield_channel_id, now, false);
            trace_redrive_cap_if_needed(provider, channel_id, snapshot, committed);
            assert_eq!(committed.attempt, Some(reserved_attempt));
            return (true, committed.attempt);
        }
        (false, None)
    }

    #[test]
    fn redrive_frozen_backlog_backs_off_and_caps_once_4299() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(4_299_001);
        let tmux_session = "AgentDesk-codex-4299-green";
        let output_path = "/tmp/agentdesk-4299-green.jsonl";
        let shared = crate::services::discord::make_shared_data_for_tests();
        let resume_offset = Arc::new(Mutex::new(None));
        let turn_delivered = Arc::new(AtomicBool::new(true));
        shared.tmux_watchers.insert(
            channel_id,
            watcher_handle(tmux_session, output_path, resume_offset, turn_delivered),
        );
        clear_redrive_test_state(&shared, &provider, channel_id, tmux_session);

        let snapshot = backlog_snapshot(channel_id, tmux_session, output_path, 128, 301_613);
        shared
            .tmux_relay_coord(channel_id)
            .confirmed_end_offset
            .store(snapshot.last_relay_offset, Ordering::Release);
        let base = 1_800_000_000;
        assert_eq!(
            gated_nudge(
                &shared,
                &provider,
                &snapshot,
                channel_id,
                base - stall_liveness::STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS as i64,
            ),
            (false, None)
        );
        let ((mut nudge_times, mut attempts), logs) = capture_errors(|| {
            let mut nudge_times = Vec::new();
            let mut attempts = Vec::new();
            for pass in 0..20 {
                let elapsed = i64::from(pass) * 30;
                let (nudged, attempt) =
                    gated_nudge(&shared, &provider, &snapshot, channel_id, base + elapsed);
                if nudged {
                    nudge_times.push(elapsed);
                    attempts.push(attempt.expect("a successful nudge is an admitted attempt"));
                }
            }
            (nudge_times, attempts)
        });
        let ((sixth_nudge, sixth_attempt), sixth_logs) =
            capture_errors(|| gated_nudge(&shared, &provider, &snapshot, channel_id, base + 930));
        if sixth_nudge {
            nudge_times.push(930);
            attempts.push(sixth_attempt.expect("sixth nudge must carry attempt ordinal"));
        }
        let (_, capped_logs) = capture_errors(|| {
            for elapsed in [960, 1_890, 86_400] {
                assert_eq!(
                    gated_nudge(&shared, &provider, &snapshot, channel_id, base + elapsed),
                    (false, None),
                    "time alone must never re-arm a capped episode"
                );
            }
        });
        let alarm_count = logs.matches("redrive_no_progress_capped").count()
            + sixth_logs.matches("redrive_no_progress_capped").count()
            + capped_logs.matches("redrive_no_progress_capped").count();
        assert_eq!(REDRIVE_BACKOFF_SECS, [30, 60, 120, 240, 480, 960]);
        assert_eq!(
            nudge_times,
            [0, 30, 90, 210, 450, 930],
            "exponential schedule must be cumulative and capped after six attempts"
        );
        assert_eq!(
            attempts,
            [1, 2, 3, 4, 5, 6],
            "counter must advance once per pass"
        );
        assert_eq!(
            alarm_count, 1,
            "the capped error event must fire exactly once"
        );
        eprintln!(
            "#4299 GREEN: N=20 + cap tick, nudge_times={nudge_times:?}, alarm_count={alarm_count}"
        );
        clear_redrive_test_state(&shared, &provider, channel_id, tmux_session);
    }

    fn drive_attempt_state_to_cap(
        shared: &SharedData,
        provider: &ProviderKind,
        channel_id: ChannelId,
        snapshot: &WatcherStateSnapshot,
        base: i64,
    ) {
        for (expected, elapsed) in [0, 30, 90, 210, 450, 930].into_iter().enumerate() {
            let reserved =
                shared.redrive_attempt_decision(provider, channel_id, snapshot, base + elapsed);
            assert_eq!(
                reserved.attempt,
                Some(expected as u8 + 1),
                "the next ordinal must be reserved without consuming it"
            );
            assert!(!reserved.emit_capped_alarm);
            assert_eq!(
                shared.commit_redrive_success(
                    provider,
                    channel_id,
                    channel_id,
                    base + elapsed,
                    false,
                ),
                RedriveAttemptDecision {
                    attempt: Some(expected as u8 + 1),
                    emit_capped_alarm: expected == 5,
                }
            );
        }
    }

    #[test]
    fn frontier_reset_rearms_capped_attempt_episode_4181() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(4_181_008);
        let tmux_session = "AgentDesk-codex-4181-reset-cap";
        let snapshot = backlog_snapshot(channel_id, tmux_session, "/tmp/4181.jsonl", 128, 256);
        let coord = shared.tmux_relay_coord(channel_id);
        coord
            .confirmed_end_offset
            .store(snapshot.last_relay_offset, Ordering::Release);
        let base = 1_800_000_000;
        drive_attempt_state_to_cap(&shared, &provider, channel_id, &snapshot, base);
        assert!(
            shared
                .redrive_attempt_decision(&provider, channel_id, &snapshot, base + 2_000)
                .attempt
                .is_none()
        );

        assert!(coord.reset_confirmed_frontier(snapshot.last_relay_offset, 0));
        let mut low = snapshot.clone();
        low.last_relay_offset = 0;
        assert_eq!(
            shared
                .redrive_attempt_decision(&provider, channel_id, &low, base + 2_001)
                .attempt,
            Some(1),
            "a new reset incarnation must not inherit the stale-H attempt cap"
        );
        clear_redrive_test_state(&shared, &provider, channel_id, tmux_session);
    }

    #[test]
    fn frontier_reset_invalidates_placeholder_shield_episode_4181() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(4_181_009);
        let tmux_session = "AgentDesk-codex-4181-reset-shield";
        let snapshot =
            backlog_snapshot(channel_id, tmux_session, "/tmp/4181-shield.jsonl", 128, 256);
        let coord = shared.tmux_relay_coord(channel_id);
        coord
            .confirmed_end_offset
            .store(snapshot.last_relay_offset, Ordering::Release);
        let key = shared.redrive_key(&provider, channel_id);
        REDRIVE_PLACEHOLDER_SHIELDS.insert(
            key,
            (
                RedriveEpisode {
                    frontier: snapshot.last_relay_offset,
                    reset_incarnation: 0,
                    identity: snapshot.inflight_identity.clone(),
                    turn_nonce: None,
                    reconnect_count: snapshot.reconnect_count,
                },
                1_800_000_000,
            ),
        );
        assert!(
            shared
                .redrive_placeholder_shield_context(&provider, channel_id)
                .is_some(),
            "shield is active in its original frontier incarnation"
        );

        assert!(coord.reset_confirmed_frontier(snapshot.last_relay_offset, 0));
        assert!(
            shared
                .redrive_placeholder_shield_context(&provider, channel_id)
                .is_none(),
            "an H shield must not defer reclaim in the fresh L incarnation"
        );
        clear_redrive_test_state(&shared, &provider, channel_id, tmux_session);
    }

    #[test]
    fn redrive_self_reattach_identity_rejects_unrelated_synthetic_turns_4299() {
        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(4_299_008);
        let tmux_session = "AgentDesk-codex-4299-synthetic-identity";
        let output_path = "/tmp/agentdesk-4299-synthetic-identity.jsonl";
        let mut previous = synthetic_rebind_state(
            &provider,
            channel_id,
            tmux_session,
            output_path,
            "2026-06-12 00:00:01",
            128,
        );
        previous.turn_nonce = Some("previous-turn".to_string());
        let episode = RedriveEpisode {
            frontier: 128,
            reset_incarnation: 0,
            identity: Some(InflightTurnIdentity::from_state(&previous)),
            turn_nonce: previous.turn_nonce.clone(),
            reconnect_count: 0,
        };

        let mut monitor = synthetic_rebind_state(
            &provider,
            channel_id,
            tmux_session,
            output_path,
            "2026-06-12 00:00:02",
            129,
        );
        monitor.turn_nonce = Some("monitor-turn".to_string());
        monitor.turn_source = crate::services::discord::inflight::TurnSource::MonitorTriggered;
        assert_eq!(
            episode.self_reattach_identity(&monitor),
            None,
            "a monitor auto-turn is a successor, not reattach's synthetic row"
        );

        let mut explicit_rebind = monitor.clone();
        explicit_rebind.turn_source =
            crate::services::discord::inflight::TurnSource::ExternalAdopted;
        assert!(
            episode.self_reattach_identity(&explicit_rebind).is_some(),
            "an explicit synthetic rebind must still be absorbed"
        );

        let mut empty_nonce_episode = episode;
        empty_nonce_episode.turn_nonce = Some(String::new());
        let mut empty_nonce_successor = previous;
        empty_nonce_successor.turn_nonce = Some(String::new());
        empty_nonce_successor.rebind_origin = false;
        assert_eq!(
            empty_nonce_episode.self_reattach_identity(&empty_nonce_successor),
            None,
            "empty nonce values are absent and must never authenticate a turn"
        );
    }

    #[test]
    fn redrive_cap_resets_only_for_progress_identity_or_watcher_4299() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmp = tempfile::tempdir().expect("temp runtime root");
        let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            tmp.path(),
        );
        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(4_299_002);
        let tmux_session = "AgentDesk-codex-4299-reset";
        let output_path = "/tmp/agentdesk-4299-reset.jsonl";
        let shared = crate::services::discord::make_shared_data_for_tests();
        shared.tmux_watchers.insert(
            channel_id,
            watcher_handle(
                tmux_session,
                output_path,
                Arc::new(Mutex::new(None)),
                Arc::new(AtomicBool::new(true)),
            ),
        );
        clear_redrive_test_state(&shared, &provider, channel_id, tmux_session);

        let snapshot = backlog_snapshot(channel_id, tmux_session, output_path, 128, 301_613);
        let base = 1_810_000_000;
        assert_eq!(
            shared
                .redrive_attempt_decision(&provider, channel_id, &snapshot, base - 20_000,)
                .attempt,
            Some(1)
        );
        assert_eq!(
            shared
                .redrive_attempt_decision(&provider, channel_id, &snapshot, base - 10_000,)
                .attempt,
            Some(1),
            "a TOCTOU-suppressed action must not consume its reservation"
        );
        clear_redrive_test_state(&shared, &provider, channel_id, tmux_session);
        drive_attempt_state_to_cap(&shared, &provider, channel_id, &snapshot, base);
        stall_liveness::gc_stall_watchdog_liveness_state(
            base + stall_liveness::STALL_LIVENESS_STATE_TTL_SECS as i64 + 1,
        );
        assert_eq!(
            shared.redrive_attempt_decision(&provider, channel_id, &snapshot, base + 10 * 86_400,),
            RedriveAttemptDecision {
                attempt: None,
                emit_capped_alarm: false
            },
            "elapsed time and liveness-state GC must not re-arm capped redrive"
        );
        let mut missing_identity = snapshot.clone();
        missing_identity.inflight_identity = None;
        assert_eq!(
            shared
                .redrive_attempt_decision(
                    &provider,
                    channel_id,
                    &missing_identity,
                    base + 11 * 86_400,
                )
                .attempt,
            None,
            "identity disappearance is not a replacement and must not re-arm"
        );

        let mut progressed = snapshot.clone();
        progressed.last_relay_offset += 1;
        progressed.relay_health.last_relay_offset += 1;
        drive_attempt_state_to_cap(
            &shared,
            &provider,
            channel_id,
            &progressed,
            base + 1_000_000,
        );

        let mut next_identity = progressed.clone();
        next_identity
            .inflight_identity
            .as_mut()
            .expect("test snapshot identity")
            .turn_start_offset = Some(9_999_999);
        drive_attempt_state_to_cap(
            &shared,
            &provider,
            channel_id,
            &next_identity,
            base + 2_000_000,
        );

        let mut next_watcher = next_identity.clone();
        next_watcher.reconnect_count += 1;
        let key = shared.redrive_key(&provider, channel_id);
        REDRIVE_PLACEHOLDER_SHIELDS
            .get_mut(&key)
            .expect("the previous episode records a shield")
            .1 = 1_234;
        assert_eq!(
            shared
                .redrive_attempt_decision(&provider, channel_id, &next_watcher, base + 3_000_000,),
            RedriveAttemptDecision {
                attempt: Some(1),
                emit_capped_alarm: false,
            },
            "replacing the live watcher instance must re-arm the episode"
        );
        assert_eq!(
            shared.commit_redrive_success(
                &provider,
                channel_id,
                channel_id,
                base + 3_000_000,
                false,
            ),
            RedriveAttemptDecision {
                attempt: Some(1),
                emit_capped_alarm: false,
            }
        );
        assert_ne!(
            shared
                .redrive_placeholder_shield_context(&provider, channel_id)
                .expect("watcher replacement starts a new shield")
                .0,
            1_234,
            "an external watcher replacement must refresh the shield start"
        );
        clear_redrive_test_state(&shared, &provider, channel_id, tmux_session);

        let first =
            shared.redrive_attempt_decision(&provider, channel_id, &snapshot, base + 4_000_000);
        assert_eq!(first.attempt, Some(1));
        let mut rebound = synthetic_rebind_state(
            &provider,
            channel_id,
            tmux_session,
            output_path,
            "2026-06-12 00:00:01",
            snapshot.last_relay_offset,
        );
        crate::services::discord::inflight::save_inflight_state(&rebound)
            .expect("persist first self-reattach identity");
        let first_rebound_identity = InflightTurnIdentity::from_state(&rebound);
        shared
            .tmux_relay_coord(channel_id)
            .reconnect_count
            .store(1, Ordering::Release);
        assert_eq!(
            shared.commit_redrive_success(
                &provider,
                channel_id,
                channel_id,
                base + 4_000_000,
                true,
            ),
            RedriveAttemptDecision {
                attempt: Some(1),
                emit_capped_alarm: false,
            }
        );
        let mut self_reattached = snapshot.clone();
        self_reattached.reconnect_count = 1;
        self_reattached.inflight_identity = Some(first_rebound_identity);
        let second = shared.redrive_attempt_decision(
            &provider,
            channel_id,
            &self_reattached,
            base + 4_000_030,
        );
        assert_eq!(second.attempt, Some(2));
        let first_shield_started = shared
            .redrive_placeholder_shield_context(&provider, channel_id)
            .expect("first reattach records shield")
            .0;
        rebound.started_at = "2026-06-12 00:00:02".to_string();
        rebound.updated_at = rebound.started_at.clone();
        rebound.turn_start_offset = Some(snapshot.last_relay_offset + 1);
        crate::services::discord::inflight::save_inflight_state(&rebound)
            .expect("persist second self-reattach identity");
        let second_rebound_identity = InflightTurnIdentity::from_state(&rebound);
        assert_eq!(
            shared.commit_redrive_success(
                &provider,
                channel_id,
                channel_id,
                base + 4_000_030,
                true,
            ),
            RedriveAttemptDecision {
                attempt: Some(2),
                emit_capped_alarm: false,
            },
            "a reuse-existing reattach must advance the same capped episode"
        );
        let (shield_started, _, shield_identity) = shared
            .redrive_placeholder_shield_context(&provider, channel_id)
            .expect("self-reattach must preserve shield");
        assert_eq!(
            shield_started, first_shield_started,
            "self-reattach must not extend 900s"
        );
        assert_eq!(shield_identity, Some(second_rebound_identity.clone()));
        let mut twice_reattached = self_reattached.clone();
        twice_reattached.reconnect_count = 1;
        twice_reattached.inflight_identity = Some(second_rebound_identity);
        assert_eq!(
            shared
                .redrive_attempt_decision(
                    &provider,
                    channel_id,
                    &twice_reattached,
                    base + 4_000_090,
                )
                .attempt,
            Some(3),
            "a self-induced identity rewrite without reconnect must not reset the cap episode"
        );
        let mut same_second_successor = synthetic_rebind_state(
            &provider,
            channel_id,
            tmux_session,
            output_path,
            &rebound.started_at,
            snapshot.last_relay_offset + 2,
        );
        same_second_successor.rebind_origin = false;
        crate::services::discord::inflight::save_inflight_state(&same_second_successor)
            .expect("persist same-second TUI successor identity");
        assert_eq!(
            shared.commit_redrive_success(
                &provider,
                channel_id,
                channel_id,
                base + 4_000_090,
                true,
            ),
            RedriveAttemptDecision {
                attempt: Some(3),
                emit_capped_alarm: false,
            }
        );
        let mut successor_snapshot = twice_reattached;
        successor_snapshot.inflight_identity =
            Some(InflightTurnIdentity::from_state(&same_second_successor));
        assert_eq!(
            shared
                .redrive_attempt_decision(
                    &provider,
                    channel_id,
                    &successor_snapshot,
                    base + 4_000_091,
                )
                .attempt,
            Some(1),
            "a same-second user-id-zero successor must still reset the episode"
        );
        crate::services::discord::inflight::clear_inflight_state(&provider, channel_id.get());
        clear_redrive_test_state(&shared, &provider, channel_id, tmux_session);

        assert_eq!(
            shared
                .redrive_attempt_decision(&provider, channel_id, &snapshot, base + 5_000_000,)
                .attempt,
            Some(1)
        );
        shared.note_redrive_noop(&provider, channel_id, base + 5_000_000, 600);
        assert_eq!(
            shared
                .redrive_attempt_decision(&provider, channel_id, &snapshot, base + 5_000_599,)
                .attempt,
            None,
            "no-op recovery calls must honor the response cooldown"
        );
        assert_eq!(
            shared
                .redrive_attempt_decision(&provider, channel_id, &snapshot, base + 5_000_600,)
                .attempt,
            Some(1),
            "a no-op cooldown must not consume the first real attempt"
        );
        clear_redrive_test_state(&shared, &provider, channel_id, tmux_session);
    }

    #[test]
    fn redrive_owner_shield_freezes_first_episode_snapshot_4299() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmp = tempfile::tempdir().expect("temp runtime root");
        let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            tmp.path(),
        );
        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(4_299_005);
        let owner_channel_id = ChannelId::new(4_299_006);
        let tmux_session = "AgentDesk-codex-4299-owner-shield";
        let output_path = "/tmp/agentdesk-4299-owner-shield.jsonl";
        let shared = crate::services::discord::make_shared_data_for_tests();
        shared.tmux_watchers.insert(
            owner_channel_id,
            watcher_handle(
                tmux_session,
                output_path,
                Arc::new(Mutex::new(None)),
                Arc::new(AtomicBool::new(true)),
            ),
        );
        clear_redrive_test_state(&shared, &provider, channel_id, tmux_session);
        REDRIVE_PLACEHOLDER_SHIELDS.remove(&shared.redrive_key(&provider, owner_channel_id));

        let mut snapshot = backlog_snapshot(channel_id, tmux_session, output_path, 128, 301_613);
        shared
            .tmux_relay_coord(channel_id)
            .confirmed_end_offset
            .store(snapshot.last_relay_offset, Ordering::Release);
        snapshot.watcher_owner_channel_id = Some(owner_channel_id.get());
        snapshot.relay_health.watcher_owner_channel_id = Some(owner_channel_id.get());
        let stale_snapshot_owner = ChannelId::new(4_299_007);
        let mut routing_snapshot = snapshot.clone();
        routing_snapshot.watcher_owner_channel_id = Some(stale_snapshot_owner.get());
        assert_eq!(
            redrive_shield_channel_for_action(&shared, &routing_snapshot, channel_id, false,),
            stale_snapshot_owner,
            "a nudge must shield the owner captured by its snapshot"
        );
        assert_eq!(
            redrive_shield_channel_for_action(&shared, &routing_snapshot, channel_id, true),
            owner_channel_id,
            "reuse-existing reattach must shield the incumbent watcher owner"
        );
        let first_owner = synthetic_rebind_state(
            &provider,
            owner_channel_id,
            tmux_session,
            output_path,
            "2026-06-12 00:00:01",
            0,
        );
        crate::services::discord::inflight::save_inflight_state(&first_owner)
            .expect("persist first owner identity");
        let first_owner_identity = InflightTurnIdentity::from_state(&first_owner);
        let owner_key = shared.redrive_key(&provider, owner_channel_id);
        REDRIVE_PLACEHOLDER_SHIELDS.insert(
            owner_key.clone(),
            (
                RedriveEpisode {
                    frontier: 0,
                    reset_incarnation: 0,
                    identity: Some(first_owner_identity.clone()),
                    turn_nonce: first_owner.turn_nonce.clone(),
                    reconnect_count: 0,
                },
                1_234,
            ),
        );
        let request_key = shared.redrive_key(&provider, channel_id);
        REDRIVE_PLACEHOLDER_SHIELDS.insert(
            request_key,
            (
                RedriveEpisode {
                    frontier: snapshot.last_relay_offset,
                    reset_incarnation: 0,
                    identity: snapshot.inflight_identity.clone(),
                    turn_nonce: None,
                    reconnect_count: snapshot.reconnect_count,
                },
                5_678,
            ),
        );
        let base = 1_820_000_000;
        assert_eq!(
            gated_nudge(
                &shared,
                &provider,
                &snapshot,
                channel_id,
                base - stall_liveness::STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS as i64,
            ),
            (false, None)
        );
        assert_eq!(
            gated_nudge(&shared, &provider, &snapshot, channel_id, base),
            (true, Some(1))
        );
        let episode_started = shared
            .redrive_placeholder_shield_context(&provider, owner_channel_id)
            .expect("first attempt re-arms the owner shield")
            .0;
        assert_ne!(
            episode_started, 1_234,
            "the first action of a new request episode must replace a stale owner shield"
        );

        shared
            .tmux_relay_coord(owner_channel_id)
            .confirmed_end_offset
            .store(64, Ordering::Release);
        let second_owner = synthetic_rebind_state(
            &provider,
            owner_channel_id,
            tmux_session,
            output_path,
            "2026-06-12 00:00:02",
            64,
        );
        crate::services::discord::inflight::save_inflight_state(&second_owner)
            .expect("persist successor owner identity");
        let second_owner_identity = InflightTurnIdentity::from_state(&second_owner);
        assert_eq!(
            gated_nudge(&shared, &provider, &snapshot, channel_id, base + 30),
            (true, Some(2))
        );
        let (shield_started, frontier_at_first_nudge, shield_identity) = shared
            .redrive_placeholder_shield_context(&provider, owner_channel_id)
            .expect("owner shield survives the request-channel episode");
        assert_eq!(
            shield_started, episode_started,
            "repeat nudge must not extend 900s"
        );
        assert_eq!(
            frontier_at_first_nudge.committed_offset, 0,
            "owner progress must not move the frozen first-nudge frontier"
        );
        assert_eq!(shield_identity, Some(first_owner_identity));
        assert_ne!(
            shield_identity,
            Some(second_owner_identity),
            "a successor owner turn must not be absorbed into the old shield"
        );
        assert!(
            shared.committed_relay_offset(owner_channel_id)
                > frontier_at_first_nudge.committed_offset,
            "owner progress must restore reclaim against the frozen shield snapshot"
        );

        assert_eq!(
            shared
                .redrive_attempt_decision(&provider, channel_id, &snapshot, base + 90)
                .attempt,
            Some(3)
        );
        shared.commit_redrive_success(&provider, channel_id, channel_id, base + 90, true);
        assert_eq!(
            shared
                .redrive_placeholder_shield_context(&provider, channel_id)
                .expect("shield follows a mid-episode owner move")
                .0,
            episode_started,
            "a shield-key move must retain the first action's 900s anchor"
        );

        REDRIVE_PLACEHOLDER_SHIELDS.remove(&owner_key);
        if let Some((_, handle)) = shared.tmux_watchers.remove(&owner_channel_id) {
            handle.cancel.store(true, Ordering::Release);
        }
        crate::services::discord::inflight::clear_inflight_state(&provider, owner_channel_id.get());
        clear_redrive_test_state(&shared, &provider, channel_id, tmux_session);
    }
}
