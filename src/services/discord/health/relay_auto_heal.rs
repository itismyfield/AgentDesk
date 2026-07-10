use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock};

use poise::serenity_prelude::ChannelId;

use super::snapshot::WatcherStateSnapshot;
use super::{HealthRegistry, stall_liveness};
use crate::services::discord::SharedData;
use crate::services::discord::relay_health::RelayStallState;
use crate::services::discord::relay_recovery::{
    self, RelayRecoveryActionKind, RelayRecoveryApplySource, RelayRecoveryError,
};
use crate::services::provider::ProviderKind;

const REDRIVE_BACKOFF_SECS: [i64; 6] = [30, 60, 120, 240, 480, 960];
const REDRIVE_MAX_NO_PROGRESS_ATTEMPTS: u8 = 6;

type RedriveKey = (String, String, u64);

#[derive(Clone, Debug, Eq, PartialEq)]
struct RedriveEpisode {
    frontier: u64,
    identity: Option<crate::services::discord::inflight::InflightTurnIdentity>,
    reconnect_count: u64,
}

impl RedriveEpisode {
    fn resets(&self, previous: &Self) -> bool {
        self.frontier > previous.frontier
            || self.identity != previous.identity
            || self.reconnect_count != previous.reconnect_count
    }
}

#[derive(Clone, Debug)]
struct RedriveAttemptState {
    episode: RedriveEpisode,
    attempts: u8,
    last_attempt_unix: i64,
    capped_alarm_emitted: bool,
    shield_started_unix: Option<i64>,
}

impl RedriveAttemptState {
    fn new(episode: RedriveEpisode, now_unix: i64) -> Self {
        Self {
            episode,
            attempts: 0,
            last_attempt_unix: now_unix,
            capped_alarm_emitted: false,
            shield_started_unix: None,
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

impl SharedData {
    fn redrive_key(&self, provider: &ProviderKind, channel_id: ChannelId) -> RedriveKey {
        (
            self.token_hash.clone(),
            provider.as_str().to_string(),
            channel_id.get(),
        )
    }

    fn redrive_episode(snapshot: &WatcherStateSnapshot) -> RedriveEpisode {
        RedriveEpisode {
            frontier: snapshot.last_relay_offset,
            identity: snapshot.inflight_identity.clone(),
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
        let episode = Self::redrive_episode(snapshot);
        let mut state = REDRIVE_ATTEMPTS
            .entry(key)
            .or_insert_with(|| RedriveAttemptState::new(episode.clone(), now_unix));
        if episode.resets(&state.episode) {
            *state = RedriveAttemptState::new(episode, now_unix);
        }
        if state.attempts >= REDRIVE_MAX_NO_PROGRESS_ATTEMPTS {
            let emit_capped_alarm = !state.capped_alarm_emitted;
            state.capped_alarm_emitted = true;
            return RedriveAttemptDecision {
                attempt: None,
                emit_capped_alarm,
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
        state.attempts += 1;
        state.last_attempt_unix = now_unix;
        let emit_capped_alarm = state.attempts == REDRIVE_MAX_NO_PROGRESS_ATTEMPTS;
        state.capped_alarm_emitted |= emit_capped_alarm;
        RedriveAttemptDecision {
            attempt: Some(state.attempts),
            emit_capped_alarm,
        }
    }

    fn record_redrive_placeholder_shield(
        &self,
        provider: &ProviderKind,
        channel_id: ChannelId,
        now_unix: i64,
    ) {
        let key = self.redrive_key(provider, channel_id);
        if let Some(mut state) = REDRIVE_ATTEMPTS.get_mut(&key) {
            state.shield_started_unix.get_or_insert(now_unix);
        }
    }

    pub(in crate::services::discord) fn redrive_placeholder_shield_context(
        &self,
        provider: &ProviderKind,
        channel_id: ChannelId,
    ) -> Option<(i64, u64)> {
        REDRIVE_ATTEMPTS
            .get(&self.redrive_key(provider, channel_id))
            .and_then(|state| {
                state
                    .shield_started_unix
                    .map(|started| (started, state.episode.frontier))
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
        let Some(snapshot) = self
            .snapshot_watcher_state_for_shared(provider, shared.clone(), channel_id.get())
            .await
        else {
            return Ok(false);
        };

        if !should_redrive_undelivered_backlog(provider, channel_id, &snapshot, now_unix_secs) {
            return Ok(false);
        }
        if redrive_should_yield_to_live_relay(&shared, channel_id, &snapshot) {
            return Ok(false);
        }
        let attempt =
            shared.redrive_attempt_decision(provider, channel_id, &snapshot, now_unix_secs);
        trace_redrive_cap_if_needed(provider, channel_id, &snapshot, attempt);
        if attempt.attempt.is_none() {
            return Ok(false);
        }

        let applied = if nudge_existing_watcher_for_backlog(
            &shared,
            provider,
            &snapshot,
            channel_id,
            now_unix_secs,
        ) {
            true
        } else {
            if redrive_should_yield_to_live_relay(&shared, channel_id, &snapshot) {
                return Ok(false);
            }
            relay_recovery::auto_apply_relay_recovery_for_shared(
                self,
                shared.clone(),
                provider,
                channel_id.get(),
                RelayRecoveryActionKind::ReattachWatcher,
                RelayRecoveryApplySource::ProbeAutoHeal,
            )
            .await?
            .applied
        };
        if applied {
            shared.record_redrive_placeholder_shield(provider, channel_id, now_unix_secs);
        }
        Ok(applied)
    }
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
    now_unix_secs: i64,
) -> bool {
    has_live_undelivered_backlog(snapshot)
        && stall_liveness::stalled_undelivered_backlog_for_redrive(
            provider,
            channel_id,
            snapshot,
            now_unix_secs,
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
) -> bool {
    if !should_redrive_undelivered_backlog(provider, channel_id, snapshot, now_unix_secs) {
        return false;
    }

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
    if !nudge_watcher_handle_for_backlog(shared, snapshot, watcher.value(), channel_id) {
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
    if redrive_should_yield_to_live_relay(shared, channel_id, snapshot) {
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
    if source == RelayRecoveryApplySource::StallWatchdog
        && let Some((_, watcher)) = shared.tmux_watchers.remove(&channel_id)
    {
        watcher.cancel.store(true, Ordering::Relaxed);
    }

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

    let response = relay_recovery::auto_apply_relay_recovery_for_shared(
        registry,
        shared,
        provider,
        channel_id.get(),
        RelayRecoveryActionKind::ClearOrphanPendingToken,
        source,
    )
    .await?;

    Ok(response.applied
        && response
            .apply_result
            .as_ref()
            .is_some_and(|result| result.removed_mailbox_token))
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
            &shared, &provider, &snapshot, channel_id, now,
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
            &shared, &provider, &snapshot, channel_id, now,
        ));
        assert!(!nudge_existing_watcher_for_backlog(
            &shared,
            &provider,
            &snapshot,
            channel_id,
            now + stall_liveness::STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS as i64,
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
            &shared, &provider, &snapshot, channel_id, now,
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
        // Prime the stall observation, then mark a relay emission in-flight
        // (non-zero `relay_slot`) while the committed frontier stays frozen.
        assert!(!nudge_existing_watcher_for_backlog(
            &shared, &provider, &snapshot, channel_id, now,
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
        if !should_redrive_undelivered_backlog(provider, channel_id, snapshot, now) {
            return (false, None);
        }
        let decision = shared.redrive_attempt_decision(provider, channel_id, snapshot, now);
        trace_redrive_cap_if_needed(provider, channel_id, snapshot, decision);
        let Some(attempt) = decision.attempt else {
            return (false, None);
        };
        let nudged =
            nudge_existing_watcher_for_backlog(shared, provider, snapshot, channel_id, now);
        if nudged {
            shared.record_redrive_placeholder_shield(provider, channel_id, now);
        }
        (nudged, Some(attempt))
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
            assert_eq!(
                shared.redrive_attempt_decision(provider, channel_id, snapshot, base + elapsed),
                RedriveAttemptDecision {
                    attempt: Some(expected as u8 + 1),
                    emit_capped_alarm: expected == 5,
                }
            );
        }
    }

    #[test]
    fn redrive_cap_resets_only_for_progress_identity_or_watcher_4299() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
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
        assert_eq!(
            shared
                .redrive_attempt_decision(&provider, channel_id, &next_watcher, base + 3_000_000,),
            RedriveAttemptDecision {
                attempt: Some(1),
                emit_capped_alarm: false,
            },
            "replacing the live watcher instance must re-arm the episode"
        );
        clear_redrive_test_state(&shared, &provider, channel_id, tmux_session);
    }
}
