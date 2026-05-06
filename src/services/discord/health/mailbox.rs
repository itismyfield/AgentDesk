use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use poise::serenity_prelude as serenity;
use serde::Serialize;
use serenity::ChannelId;

use super::{HealthRegistry, ProviderEntry, WatcherStateSnapshot};
use crate::services::discord;
use crate::services::discord::relay_health::{
    RelayActiveTurn, RelayHealthSnapshot, RelayStallClassifier, RelayStallState,
};
use crate::services::discord::{
    SharedData, clear_inflight_state, mailbox_cancel_active_turn, mailbox_clear_channel,
    mailbox_clear_recovery_marker, mailbox_finish_turn,
};
use crate::services::provider::ProviderKind;

pub(super) const WATCHER_STATE_DESYNC_STALE_MS: i64 = 30_000;

#[derive(Debug, Serialize)]
pub(super) struct MailboxHealthSnapshot {
    provider: String,
    channel_id: u64,
    has_cancel_token: bool,
    queue_depth: usize,
    recovery_started: bool,
    active_request_owner: Option<u64>,
    active_user_message_id: Option<u64>,
    agent_turn_status: &'static str,
    watcher_attached: bool,
    inflight_state_present: bool,
    tmux_present: bool,
    process_present: bool,
    active_dispatch_present: bool,
    relay_stall_state: RelayStallState,
    relay_health: RelayHealthSnapshot,
}

pub(super) struct ProviderMailboxSummary {
    pub active_turns: usize,
    pub queue_depth: usize,
    pub recovering_channels: usize,
    pub entries: Vec<MailboxHealthSnapshot>,
}

#[derive(Clone, Copy, Debug, Default)]
struct RelayThreadProofSnapshot {
    parent_channel_id: Option<u64>,
    thread_channel_id: Option<u64>,
    stale_thread_proof: bool,
}

fn relay_active_turn_from_inflight(
    mailbox_has_cancel_token: bool,
    inflight: Option<&discord::inflight::InflightTurnState>,
) -> RelayActiveTurn {
    if !mailbox_has_cancel_token && inflight.is_none() {
        return RelayActiveTurn::None;
    }

    if inflight.is_some_and(|state| {
        state.long_running_placeholder_active || state.task_notification_kind.is_some()
    }) {
        RelayActiveTurn::ExplicitBackground
    } else {
        RelayActiveTurn::Foreground
    }
}

fn last_outbound_activity_ms(
    last_relay_ts_ms: i64,
    inflight: Option<&discord::inflight::InflightTurnState>,
) -> Option<i64> {
    if last_relay_ts_ms > 0 {
        return Some(last_relay_ts_ms);
    }

    let inflight = inflight?;
    let has_discord_write_evidence = inflight.current_msg_len > 0
        || inflight.response_sent_offset > 0
        || inflight.last_watcher_relayed_offset.is_some();
    if !has_discord_write_evidence {
        return None;
    }

    discord::inflight::parse_updated_at_unix(&inflight.updated_at)
        .and_then(|seconds| seconds.checked_mul(1000))
}

fn trace_relay_health_classification(
    relay_health: &RelayHealthSnapshot,
    relay_stall_state: RelayStallState,
) {
    if relay_stall_state.should_log_at_debug() {
        tracing::debug!(
            target: "agentdesk::discord::relay_health",
            provider = relay_health.provider.as_str(),
            channel_id = relay_health.channel_id,
            relay_stall_state = relay_stall_state.as_str(),
            queue_depth = relay_health.queue_depth,
            tmux_alive = ?relay_health.tmux_alive,
            desynced = relay_health.desynced,
            pending_thread_proof = relay_health.pending_thread_proof,
            "relay health classified"
        );
    } else {
        tracing::trace!(
            target: "agentdesk::discord::relay_health",
            provider = relay_health.provider.as_str(),
            channel_id = relay_health.channel_id,
            relay_stall_state = relay_stall_state.as_str(),
            queue_depth = relay_health.queue_depth,
            "relay health classified"
        );
    }
}

async fn relay_thread_proof_for_channel(
    shared: &SharedData,
    provider: Option<&ProviderKind>,
    channel_id: ChannelId,
    current_channel_has_live_evidence: bool,
) -> RelayThreadProofSnapshot {
    let thread_channel_id = shared
        .dispatch_thread_parents
        .get(&channel_id)
        .map(|entry| entry.value().get());
    let parent_channel_id = shared
        .dispatch_thread_parents
        .iter()
        .find_map(|entry| (*entry.value() == channel_id).then_some(entry.key().get()));

    let child_has_live_evidence = match thread_channel_id {
        Some(thread_id) => {
            let thread_channel = ChannelId::new(thread_id);
            let thread_mailbox = discord::mailbox_snapshot(shared, thread_channel).await;
            let thread_inflight = provider
                .and_then(|provider| discord::inflight::load_inflight_state(provider, thread_id));
            thread_mailbox.cancel_token.is_some()
                || thread_inflight.is_some()
                || shared.tmux_watchers.contains_key(&thread_channel)
        }
        None => false,
    };

    RelayThreadProofSnapshot {
        parent_channel_id,
        thread_channel_id,
        stale_thread_proof: thread_channel_id.is_some_and(|_| !child_has_live_evidence)
            || parent_channel_id.is_some_and(|_| !current_channel_has_live_evidence),
    }
}

struct RelayHealthBuildInput {
    provider: String,
    channel_id: u64,
    mailbox_has_cancel_token: bool,
    mailbox_active_user_msg_id: Option<u64>,
    queue_depth: usize,
    watcher_attached: bool,
    watcher_owner_channel_id: Option<u64>,
    tmux_session: Option<String>,
    tmux_alive: Option<bool>,
    bridge_inflight_present: bool,
    bridge_current_msg_id: Option<u64>,
    watcher_owns_live_relay: bool,
    last_relay_ts_ms: i64,
    last_relay_offset: u64,
    last_capture_offset: Option<u64>,
    unread_bytes: Option<u64>,
    desynced: bool,
    thread_proof: RelayThreadProofSnapshot,
    active_turn: RelayActiveTurn,
    last_outbound_activity_ms: Option<i64>,
}

fn build_relay_health_snapshot(input: RelayHealthBuildInput) -> RelayHealthSnapshot {
    RelayHealthSnapshot {
        provider: input.provider,
        channel_id: input.channel_id,
        active_turn: input.active_turn,
        tmux_session: input.tmux_session,
        tmux_alive: input.tmux_alive,
        watcher_attached: input.watcher_attached,
        watcher_owner_channel_id: input.watcher_owner_channel_id,
        watcher_owns_live_relay: input.watcher_owns_live_relay,
        bridge_inflight_present: input.bridge_inflight_present,
        bridge_current_msg_id: input.bridge_current_msg_id,
        mailbox_has_cancel_token: input.mailbox_has_cancel_token,
        mailbox_active_user_msg_id: input.mailbox_active_user_msg_id,
        queue_depth: input.queue_depth,
        pending_discord_callback_msg_id: input
            .bridge_current_msg_id
            .or(input.mailbox_active_user_msg_id),
        pending_thread_proof: input.thread_proof.parent_channel_id.is_some()
            || input.thread_proof.thread_channel_id.is_some(),
        parent_channel_id: input.thread_proof.parent_channel_id,
        thread_channel_id: input.thread_proof.thread_channel_id,
        last_relay_ts_ms: (input.last_relay_ts_ms > 0).then_some(input.last_relay_ts_ms),
        last_outbound_activity_ms: input.last_outbound_activity_ms,
        last_capture_offset: input.last_capture_offset,
        last_relay_offset: input.last_relay_offset,
        unread_bytes: input.unread_bytes,
        desynced: input.desynced,
        stale_thread_proof: input.thread_proof.stale_thread_proof,
    }
}

pub(super) async fn snapshot_watcher_state_filtered(
    registry: &HealthRegistry,
    channel_id: u64,
    provider_filter: Option<&ProviderKind>,
) -> Option<WatcherStateSnapshot> {
    let channel = ChannelId::new(channel_id);
    let providers = registry.providers.lock().await;
    for entry in providers.iter() {
        if let Some(filter) = provider_filter
            && !entry.name.eq_ignore_ascii_case(filter.as_str())
        {
            continue;
        }
        let shared = entry.shared.clone();
        let watcher_binding = shared.tmux_watchers.channel_binding(&channel);
        let provider_kind = ProviderKind::from_str(&entry.name);
        let inflight = provider_kind
            .as_ref()
            .and_then(|pk| discord::inflight::load_inflight_state(pk, channel_id));
        let inflight_tmux_session = inflight
            .as_ref()
            .and_then(|state| state.tmux_session_name.clone());
        let inflight_owner_channel_id = inflight_tmux_session
            .as_deref()
            .and_then(|tmux| shared.tmux_watchers.owner_channel_for_tmux_session(tmux));
        let inflight_owner_matches_channel = inflight_owner_channel_id == Some(channel);
        let attached = watcher_binding.is_some() || inflight_owner_matches_channel;
        let watcher_binding_tmux_session = watcher_binding
            .as_ref()
            .map(|binding| binding.tmux_session_name.clone());
        let relay_state_matches_inflight = match (
            inflight_tmux_session.as_deref(),
            watcher_binding_tmux_session.as_deref(),
        ) {
            (Some(inflight_tmux), Some(binding_tmux)) => inflight_tmux == binding_tmux,
            _ => true,
        };
        let has_relay_coord = shared.tmux_relay_coords.contains_key(&channel);
        let inflight_state_present = inflight.is_some();
        let tmux_session_mismatch = inflight_state_present
            && !relay_state_matches_inflight
            && watcher_binding_tmux_session.is_some()
            && inflight_tmux_session.is_some();
        let mailbox_snapshot = discord::mailbox_snapshot(&shared, channel).await;
        let mailbox_has_cancel_token = mailbox_snapshot.cancel_token.is_some();
        let mailbox_active_user_msg_id = mailbox_snapshot.active_user_message_id.map(|id| id.get());
        let has_pending_queue = !mailbox_snapshot.intervention_queue.is_empty();
        let mailbox_engaged = mailbox_active_user_msg_id.is_some() || has_pending_queue;
        let has_thread_proof = shared.dispatch_thread_parents.contains_key(&channel)
            || shared
                .dispatch_thread_parents
                .iter()
                .any(|entry| *entry.value() == channel);
        if !attached
            && !has_relay_coord
            && !inflight_state_present
            && !mailbox_engaged
            && !has_thread_proof
        {
            continue;
        }
        let (last_relay_offset, last_relay_ts_ms, reconnect_count) = shared
            .tmux_relay_coords
            .get(&channel)
            .map(|coord| {
                (
                    coord
                        .confirmed_end_offset
                        .load(std::sync::atomic::Ordering::Acquire),
                    coord
                        .last_relay_ts_ms
                        .load(std::sync::atomic::Ordering::Acquire),
                    coord
                        .reconnect_count
                        .load(std::sync::atomic::Ordering::Acquire),
                )
            })
            .unwrap_or((0, 0, 0));
        let watcher_owner_channel_id = watcher_binding
            .as_ref()
            .map(|binding| binding.owner_channel_id)
            .or(inflight_owner_channel_id)
            .map(|id| id.get());
        let tmux_session = watcher_binding
            .map(|binding| binding.tmux_session_name)
            .or(inflight_tmux_session);
        let inflight_started_at = inflight.as_ref().map(|state| state.started_at.clone());
        let inflight_updated_at = inflight.as_ref().map(|state| state.updated_at.clone());
        let inflight_user_msg_id = inflight
            .as_ref()
            .map(|state| state.user_msg_id)
            .filter(|id| *id != 0);
        let inflight_current_msg_id = inflight
            .as_ref()
            .map(|state| state.current_msg_id)
            .filter(|id| *id != 0);
        let tmux_session_alive = match tmux_session.as_ref() {
            Some(name) => {
                let probe_target = name.clone();
                let alive = tokio::task::spawn_blocking(move || {
                    crate::services::platform::tmux::has_session(&probe_target)
                })
                .await
                .unwrap_or(false);
                Some(alive)
            }
            None => None,
        };
        let output_path_for_metadata = inflight
            .as_ref()
            .and_then(|state| state.output_path.as_deref())
            .map(str::to_string);
        let last_capture_offset = match output_path_for_metadata {
            Some(path) => tokio::task::spawn_blocking(move || {
                std::fs::metadata(path).ok().map(|meta| meta.len())
            })
            .await
            .unwrap_or(None),
            None => None,
        };
        let unread_bytes = relay_state_matches_inflight
            .then(|| last_capture_offset.map(|capture| capture.saturating_sub(last_relay_offset)))
            .flatten();
        let now_ms = chrono::Utc::now().timestamp_millis();
        let relay_stale_anchor_ms = if last_relay_ts_ms > 0 {
            Some(last_relay_ts_ms)
        } else {
            inflight
                .as_ref()
                .and_then(|state| discord::inflight::parse_started_at_unix(&state.started_at))
                .and_then(|seconds| seconds.checked_mul(1000))
        };
        let relay_stale = relay_stale_anchor_ms
            .map(|anchor_ms| now_ms.saturating_sub(anchor_ms) >= WATCHER_STATE_DESYNC_STALE_MS)
            .unwrap_or(false);
        let capture_lagged = last_capture_offset
            .map(|capture| {
                relay_state_matches_inflight
                    && inflight_state_present
                    && capture != last_relay_offset
                    && relay_stale
            })
            .unwrap_or(false);
        let live_tmux_orphaned =
            tmux_session_alive == Some(true) && inflight_state_present && !attached && relay_stale;
        let desynced =
            capture_lagged || live_tmux_orphaned || (tmux_session_mismatch && relay_stale);
        let active_turn =
            relay_active_turn_from_inflight(mailbox_has_cancel_token, inflight.as_ref());
        let relay_thread_proof = relay_thread_proof_for_channel(
            &shared,
            provider_kind.as_ref(),
            channel,
            mailbox_has_cancel_token || inflight_state_present || attached,
        )
        .await;
        let relay_health = build_relay_health_snapshot(RelayHealthBuildInput {
            provider: entry.name.clone(),
            channel_id,
            mailbox_has_cancel_token,
            mailbox_active_user_msg_id,
            queue_depth: mailbox_snapshot.intervention_queue.len(),
            watcher_attached: attached,
            watcher_owner_channel_id,
            tmux_session: tmux_session.clone(),
            tmux_alive: tmux_session_alive,
            bridge_inflight_present: inflight_state_present,
            bridge_current_msg_id: inflight_current_msg_id,
            watcher_owns_live_relay: inflight
                .as_ref()
                .is_some_and(|state| state.watcher_owns_live_relay),
            last_relay_ts_ms,
            last_relay_offset,
            last_capture_offset,
            unread_bytes,
            desynced,
            thread_proof: relay_thread_proof,
            active_turn,
            last_outbound_activity_ms: last_outbound_activity_ms(
                last_relay_ts_ms,
                inflight.as_ref(),
            ),
        });
        let relay_stall_state = RelayStallClassifier::classify(&relay_health);
        trace_relay_health_classification(&relay_health, relay_stall_state);
        return Some(WatcherStateSnapshot {
            provider: entry.name.clone(),
            attached,
            tmux_session,
            watcher_owner_channel_id,
            last_relay_offset,
            inflight_state_present,
            last_relay_ts_ms,
            last_capture_offset,
            unread_bytes,
            desynced,
            reconnect_count,
            inflight_started_at,
            inflight_updated_at,
            inflight_user_msg_id,
            inflight_current_msg_id,
            tmux_session_alive,
            has_pending_queue,
            mailbox_active_user_msg_id,
            relay_stall_state,
            relay_health,
        });
    }
    None
}

pub(super) async fn summarize_provider_mailboxes(
    entry: &ProviderEntry,
    include_details: bool,
) -> ProviderMailboxSummary {
    let mailbox_snapshots = entry.shared.mailboxes.snapshot_all().await;
    let active_turns = mailbox_snapshots
        .values()
        .filter(|snapshot| snapshot.cancel_token.is_some())
        .count();
    let queue_depth: usize = mailbox_snapshots
        .values()
        .map(|snapshot| snapshot.intervention_queue.len())
        .sum();
    let recovering_channels = mailbox_snapshots
        .values()
        .filter(|snapshot| snapshot.recovery_started_at.is_some())
        .count();
    let mut entries = Vec::new();

    if include_details {
        let provider_kind = ProviderKind::from_str(&entry.name);
        for (channel_id, snapshot) in &mailbox_snapshots {
            let channel = *channel_id;
            let inflight_state = provider_kind
                .as_ref()
                .and_then(|pk| discord::inflight::load_inflight_state(pk, channel.get()));
            let watcher_binding = entry.shared.tmux_watchers.channel_binding(&channel);
            let watcher_attached = watcher_binding.is_some();
            let watcher_binding_tmux_session = watcher_binding
                .as_ref()
                .map(|binding| binding.tmux_session_name.clone());
            let inflight_tmux_session = inflight_state
                .as_ref()
                .and_then(|state| state.tmux_session_name.clone());
            let inflight_owner_channel_id = inflight_tmux_session.as_deref().and_then(|tmux| {
                entry
                    .shared
                    .tmux_watchers
                    .owner_channel_for_tmux_session(tmux)
            });
            let watcher_owner_channel_id = watcher_binding
                .as_ref()
                .map(|binding| binding.owner_channel_id)
                .or(inflight_owner_channel_id)
                .map(|id| id.get());
            let tmux_session_name = watcher_binding_tmux_session
                .clone()
                .or_else(|| inflight_tmux_session.clone());
            let relay_state_matches_inflight = match (
                inflight_tmux_session.as_deref(),
                watcher_binding_tmux_session.as_deref(),
            ) {
                (Some(inflight_tmux), Some(binding_tmux)) => inflight_tmux == binding_tmux,
                _ => true,
            };
            let inflight_state_present = inflight_state.is_some();
            let tmux_session_mismatch = inflight_state_present
                && !relay_state_matches_inflight
                && watcher_binding_tmux_session.is_some()
                && inflight_tmux_session.is_some();
            let tmux_present = tmux_session_name
                .as_deref()
                .is_some_and(crate::services::platform::tmux::has_session);
            let process_present = tmux_session_name
                .as_deref()
                .is_some_and(|name| crate::services::platform::tmux::pane_pid(name).is_some());
            let (last_relay_offset, last_relay_ts_ms) = entry
                .shared
                .tmux_relay_coords
                .get(&channel)
                .map(|coord| {
                    (
                        coord
                            .confirmed_end_offset
                            .load(std::sync::atomic::Ordering::Acquire),
                        coord
                            .last_relay_ts_ms
                            .load(std::sync::atomic::Ordering::Acquire),
                    )
                })
                .unwrap_or((0, 0));
            let last_capture_offset = inflight_state
                .as_ref()
                .and_then(|state| state.output_path.as_deref())
                .and_then(|path| std::fs::metadata(path).ok().map(|meta| meta.len()));
            let unread_bytes = relay_state_matches_inflight
                .then(|| {
                    last_capture_offset.map(|capture| capture.saturating_sub(last_relay_offset))
                })
                .flatten();
            let now_ms = chrono::Utc::now().timestamp_millis();
            let relay_stale_anchor_ms = if last_relay_ts_ms > 0 {
                Some(last_relay_ts_ms)
            } else {
                inflight_state
                    .as_ref()
                    .and_then(|state| discord::inflight::parse_started_at_unix(&state.started_at))
                    .and_then(|seconds| seconds.checked_mul(1000))
            };
            let relay_stale = relay_stale_anchor_ms
                .map(|anchor_ms| now_ms.saturating_sub(anchor_ms) >= WATCHER_STATE_DESYNC_STALE_MS)
                .unwrap_or(false);
            let capture_lagged = last_capture_offset
                .map(|capture| {
                    relay_state_matches_inflight
                        && inflight_state_present
                        && capture != last_relay_offset
                        && relay_stale
                })
                .unwrap_or(false);
            let live_tmux_orphaned =
                tmux_present && inflight_state_present && !watcher_attached && relay_stale;
            let desynced =
                capture_lagged || live_tmux_orphaned || (tmux_session_mismatch && relay_stale);
            let mailbox_has_cancel_token = snapshot.cancel_token.is_some();
            let queue_depth = snapshot.intervention_queue.len();
            let mailbox_active_user_msg_id = snapshot.active_user_message_id.map(|id| id.get());
            let relay_thread_proof = relay_thread_proof_for_channel(
                &entry.shared,
                provider_kind.as_ref(),
                channel,
                mailbox_has_cancel_token || inflight_state_present || watcher_attached,
            )
            .await;
            let active_turn =
                relay_active_turn_from_inflight(mailbox_has_cancel_token, inflight_state.as_ref());
            let relay_health = build_relay_health_snapshot(RelayHealthBuildInput {
                provider: entry.name.clone(),
                channel_id: channel.get(),
                mailbox_has_cancel_token,
                mailbox_active_user_msg_id,
                queue_depth,
                watcher_attached,
                watcher_owner_channel_id,
                tmux_session: tmux_session_name.clone(),
                tmux_alive: tmux_session_name.as_ref().map(|_| tmux_present),
                bridge_inflight_present: inflight_state_present,
                bridge_current_msg_id: inflight_state
                    .as_ref()
                    .map(|state| state.current_msg_id)
                    .filter(|id| *id != 0),
                watcher_owns_live_relay: inflight_state
                    .as_ref()
                    .is_some_and(|state| state.watcher_owns_live_relay),
                last_relay_ts_ms,
                last_relay_offset,
                last_capture_offset,
                unread_bytes,
                desynced,
                thread_proof: relay_thread_proof,
                active_turn,
                last_outbound_activity_ms: last_outbound_activity_ms(
                    last_relay_ts_ms,
                    inflight_state.as_ref(),
                ),
            });
            let relay_stall_state = RelayStallClassifier::classify(&relay_health);
            trace_relay_health_classification(&relay_health, relay_stall_state);
            entries.push(MailboxHealthSnapshot {
                provider: entry.name.clone(),
                channel_id: channel.get(),
                has_cancel_token: mailbox_has_cancel_token,
                queue_depth,
                recovery_started: snapshot.recovery_started_at.is_some(),
                active_request_owner: snapshot.active_request_owner.map(|id| id.get()),
                active_user_message_id: mailbox_active_user_msg_id,
                agent_turn_status: if mailbox_has_cancel_token {
                    "active"
                } else {
                    "idle"
                },
                watcher_attached,
                inflight_state_present,
                tmux_present,
                process_present,
                active_dispatch_present: inflight_state
                    .as_ref()
                    .and_then(|state| state.dispatch_id.as_deref())
                    .is_some(),
                relay_stall_state,
                relay_health,
            });
        }
    }

    ProviderMailboxSummary {
        active_turns,
        queue_depth,
        recovering_channels,
        entries,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeTurnStopResult {
    pub lifecycle_path: &'static str,
    pub had_active_turn: bool,
    pub queue_depth: usize,
    pub persistent_inflight_cleared: bool,
    pub termination_recorded: bool,
}

fn decrement_counter(counter: &AtomicUsize) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        current.checked_sub(1)
    });
}

async fn shared_for_provider(
    registry: &HealthRegistry,
    provider: &ProviderKind,
) -> Option<Arc<SharedData>> {
    registry.shared_for_provider(provider).await
}

async fn wait_for_turn_end(
    shared: &SharedData,
    channel_id: ChannelId,
    timeout: std::time::Duration,
) -> bool {
    let start = tokio::time::Instant::now();
    while shared.mailbox(channel_id).has_active_turn().await {
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    true
}

fn runtime_stop_wait_timeout() -> std::time::Duration {
    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    {
        std::time::Duration::from_millis(150)
    }
    #[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
    {
        std::time::Duration::from_secs(3)
    }
}

fn clear_persistent_inflight_for_stop(
    provider: &ProviderKind,
    channel_id: ChannelId,
    was_present_at_stop_start: bool,
) -> bool {
    let removed_now = clear_inflight_state(provider, channel_id.get());
    let disappeared_during_stop = was_present_at_stop_start
        && !discord::inflight::inflight_state_file_exists(provider, channel_id.get());
    removed_now || disappeared_during_stop
}

pub(crate) async fn stop_provider_channel_runtime_with_policy(
    registry: &HealthRegistry,
    provider_name: &str,
    channel_id: ChannelId,
    reason: &str,
    cleanup_policy: discord::TmuxCleanupPolicy,
) -> Option<RuntimeTurnStopResult> {
    let provider = ProviderKind::from_str(provider_name)?;
    let shared = shared_for_provider(registry, &provider).await?;
    let cleanup_requested = cleanup_policy.should_cleanup_tmux();
    let should_clear_persistent_inflight = cleanup_policy.should_clear_inflight();
    let persistent_inflight_was_present = should_clear_persistent_inflight
        && discord::inflight::inflight_state_file_exists(&provider, channel_id.get());
    let result = mailbox_cancel_active_turn(&shared, channel_id).await;

    if let Some(token) = result.token.as_ref() {
        let termination_recorded = if !result.already_stopping || cleanup_requested {
            discord::turn_bridge::stop_active_turn(&provider, token, cleanup_policy, reason).await
        } else {
            false
        };
        if wait_for_turn_end(&shared, channel_id, runtime_stop_wait_timeout()).await {
            let snapshot = shared.mailbox(channel_id).snapshot().await;
            return Some(RuntimeTurnStopResult {
                lifecycle_path: "canonical",
                had_active_turn: true,
                queue_depth: snapshot.intervention_queue.len(),
                persistent_inflight_cleared: should_clear_persistent_inflight
                    && clear_persistent_inflight_for_stop(
                        &provider,
                        channel_id,
                        persistent_inflight_was_present,
                    ),
                termination_recorded,
            });
        }
    }

    let finish = mailbox_finish_turn(&shared, &provider, channel_id).await;
    let mut termination_recorded = false;
    if let Some(token) = finish.removed_token.as_ref() {
        termination_recorded =
            discord::turn_bridge::stop_active_turn(&provider, token, cleanup_policy, reason).await;
    }
    apply_runtime_hard_stop_cleanup(
        &shared,
        &provider,
        channel_id,
        &finish,
        "runtime_stop_fallback",
        cleanup_requested,
    )
    .await;
    let queue_depth = shared
        .mailbox(channel_id)
        .snapshot()
        .await
        .intervention_queue
        .len();
    mailbox_clear_recovery_marker(&shared, channel_id).await;
    let persistent_inflight_cleared = if should_clear_persistent_inflight {
        clear_persistent_inflight_for_stop(&provider, channel_id, persistent_inflight_was_present)
    } else {
        false
    };

    Some(RuntimeTurnStopResult {
        lifecycle_path: "runtime-fallback",
        had_active_turn: finish.removed_token.is_some(),
        queue_depth,
        persistent_inflight_cleared,
        termination_recorded,
    })
}

pub async fn stop_provider_channel_runtime(
    registry: &HealthRegistry,
    provider_name: &str,
    channel_id: ChannelId,
    reason: &str,
) -> Option<RuntimeTurnStopResult> {
    stop_provider_channel_runtime_with_policy(
        registry,
        provider_name,
        channel_id,
        reason,
        discord::TmuxCleanupPolicy::PreserveSession,
    )
    .await
}

pub async fn force_kill_provider_channel_runtime(
    registry: &HealthRegistry,
    provider_name: &str,
    channel_id: ChannelId,
    reason: &str,
    termination_reason_code: &'static str,
) -> Option<RuntimeTurnStopResult> {
    stop_provider_channel_runtime_with_policy(
        registry,
        provider_name,
        channel_id,
        reason,
        discord::TmuxCleanupPolicy::CleanupSession {
            termination_reason_code: Some(termination_reason_code),
        },
    )
    .await
}

/// #1672: Snapshot the per-channel pending-queue state from both the
/// in-memory mailbox and the disk-backed `discord_pending_queue` file.
///
/// Used by the cancel API + text-stop helpers to verify their
/// "pending_queue must be preserved across cancel" invariant *after*
/// the cancel completes, instead of asserting it via a hardcoded
/// `queue_preserved=true`.
///
/// Returns `None` only when the registered shared runtime cannot be
/// resolved for `provider_name`. A missing channel mailbox or absent
/// disk file are reported as `(0, false)` rather than `None`.
pub async fn snapshot_pending_queue_state(
    registry: &HealthRegistry,
    provider_name: &str,
    channel_id: ChannelId,
) -> Option<PendingQueueSnapshot> {
    let provider = ProviderKind::from_str(provider_name)?;
    let shared = shared_for_provider(registry, &provider).await?;
    Some(snapshot_pending_queue_state_for_shared(&shared, &provider, channel_id).await)
}

#[derive(Clone, Debug, Default)]
pub struct PendingQueueSnapshot {
    pub queue_depth: usize,
    pub disk_present: bool,
    pub disk_path: Option<std::path::PathBuf>,
}

async fn snapshot_pending_queue_state_for_shared(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> PendingQueueSnapshot {
    let queue_depth = shared
        .mailbox(channel_id)
        .snapshot()
        .await
        .intervention_queue
        .len();
    let disk_path = discord::runtime_store::discord_pending_queue_root().map(|root| {
        root.join(provider.as_str())
            .join(&shared.token_hash)
            .join(format!("{}.json", channel_id.get()))
    });
    let disk_present = disk_path
        .as_ref()
        .map(|path| path.exists())
        .unwrap_or(false);
    PendingQueueSnapshot {
        queue_depth,
        disk_present,
        disk_path,
    }
}

/// #1672: After a cancel that left the channel idle, kick the deferred
/// idle-queue drain so any survived `pending_queue` items are picked up
/// without requiring the next user message to arrive first.
///
/// Returns `true` when the drain was scheduled (registered shared runtime
/// found and at least one item is queued in memory or on disk), `false`
/// otherwise.
///
/// codex review round-3 P2: when the in-memory mailbox is empty but the
/// disk-backed `discord_pending_queue/<provider>/<token>/<channel>.json`
/// file is still present, hydrate the mailbox from disk before
/// scheduling the drain. Without this, the cancel response correctly
/// reports `queue_disk_present_after=true` but the queued items remain
/// stranded -- the drain helper sees an empty mailbox and bails out, and
/// the next `mailbox_enqueue_intervention` may overwrite the disk file
/// before the items are ever absorbed.
pub async fn schedule_pending_queue_drain_after_cancel(
    registry: &HealthRegistry,
    provider_name: &str,
    channel_id: ChannelId,
    reason: &'static str,
) -> PostCancelDrainOutcome {
    let Some(provider) = ProviderKind::from_str(provider_name) else {
        return PostCancelDrainOutcome::skipped();
    };
    let Some(shared) = shared_for_provider(registry, &provider).await else {
        return PostCancelDrainOutcome::skipped();
    };
    let snapshot = snapshot_pending_queue_state_for_shared(&shared, &provider, channel_id).await;
    // codex review round-4 P2-1 (#1672): hydrate from disk *whenever*
    // the disk file is present, not just when the in-memory queue is
    // empty. If a concurrent `mailbox_enqueue_intervention` slipped a
    // fresh message in between the cancel and this helper running, we
    // still need to merge whatever the disk holds. Actor-local hydrate
    // dedupes by `message_id` and prepends disk items so neither the
    // surviving disk payload nor the live racer is dropped.
    let post_depth = if snapshot.disk_present {
        let hydrate_result = hydrate_pending_queue_from_disk(&shared, &provider, channel_id).await;
        let _absorbed = hydrate_result.absorbed;
        hydrate_result.queue_len_after
    } else {
        snapshot.queue_depth
    };
    if post_depth == 0 {
        return PostCancelDrainOutcome {
            scheduled: false,
            queue_depth_after: 0,
        };
    }
    discord::schedule_deferred_idle_queue_kickoff(shared.clone(), provider, channel_id, reason);
    PostCancelDrainOutcome {
        scheduled: true,
        queue_depth_after: post_depth,
    }
}

/// codex review round-4 P2-2 (#1672): return value of
/// `schedule_pending_queue_drain_after_cancel`. The cancel response
/// builders use `queue_depth_after` as the source of truth for
/// `queued_remaining` so the API contract reflects the post-hydrate
/// state, not the (typically zero) snapshot taken before disk
/// hydration ran.
#[derive(Clone, Copy, Debug, Default)]
pub struct PostCancelDrainOutcome {
    pub scheduled: bool,
    pub queue_depth_after: usize,
}

impl PostCancelDrainOutcome {
    fn skipped() -> Self {
        Self::default()
    }
}

/// codex review round-3 P2 (#1672): load the disk-backed pending queue
/// for `channel_id` and merge it into the in-memory mailbox. Restores
/// the matching `dispatch_role_override` alongside the queue so
/// requeued items target the same destination channel as the original
/// `mailbox_enqueue_intervention` call.
///
/// codex review round-4 P2-1 (#1672): the merge runs through the
/// mailbox actor, so a concurrent `mailbox_enqueue_intervention`
/// racing with this hydrate is preserved rather than clobbered. Disk
/// items are inserted at the head of the queue and any `message_id`
/// already present is skipped to keep the merge idempotent on retry.
///
/// #1683: the disk read also runs inside the actor message. A pending
/// dequeue can no longer remove the queue file after an out-of-actor
/// stale read and then have that stale payload reinserted by hydrate.
///
/// Returns the post-hydrate queue depth plus any restored role override.
async fn hydrate_pending_queue_from_disk(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> crate::services::turn_orchestrator::HydratePendingQueueResult {
    let result =
        discord::mailbox_hydrate_pending_queue_from_disk(shared, provider, channel_id).await;
    if let Some(alt_channel_id) = result.restored_override {
        shared
            .dispatch_role_overrides
            .insert(channel_id, alt_channel_id);
    }
    result
}

/// #1672: Resolve a usable tmux session name for cancel observability.
///
/// Order: live tmux watcher binding -> persistent inflight state file ->
/// `discord_session.channel_name` rendered through the provider's tmux
/// naming convention. Returns `None` when none of those sources knows
/// about the channel -- at which point cancel observability falls back
/// to whatever the caller passed in (typically empty).
pub async fn resolve_tmux_session_for_cancel(
    registry: &HealthRegistry,
    provider_name: &str,
    channel_id: ChannelId,
) -> Option<String> {
    let provider = ProviderKind::from_str(provider_name)?;
    let shared = shared_for_provider(registry, &provider).await?;
    if let Some(binding) = shared.tmux_watchers.channel_binding(&channel_id) {
        return Some(binding.tmux_session_name);
    }
    if let Some(state) = discord::inflight::load_inflight_state(&provider, channel_id.get())
        && let Some(session) = state.tmux_session_name
    {
        return Some(session);
    }
    let data = shared.core.lock().await;
    data.sessions
        .get(&channel_id)
        .and_then(|session| session.channel_name.as_ref())
        .map(|channel_name| provider.build_tmux_session_name(channel_name))
}

pub async fn active_request_owner_for_channel(
    registry: &HealthRegistry,
    channel_id: u64,
) -> Option<u64> {
    let channel_id = ChannelId::new(channel_id);
    let providers: Vec<_> = registry
        .providers
        .lock()
        .await
        .iter()
        .map(|entry| entry.shared.clone())
        .collect();
    for shared in providers {
        let snapshots = shared.mailboxes.snapshot_all().await;
        if let Some(owner) = snapshots
            .get(&channel_id)
            .and_then(|snapshot| snapshot.active_request_owner)
        {
            return Some(owner.get());
        }
    }
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HardStopRuntimeResult {
    pub cleanup_path: &'static str,
    pub had_active_turn: bool,
    pub has_pending_queue: bool,
    pub runtime_session_cleared: bool,
}

impl Default for HardStopRuntimeResult {
    fn default() -> Self {
        Self {
            cleanup_path: "runtime_unavailable_fallback",
            had_active_turn: false,
            has_pending_queue: false,
            runtime_session_cleared: false,
        }
    }
}

struct RuntimeChannelMatch {
    provider: ProviderKind,
    shared: Arc<SharedData>,
    channel_id: ChannelId,
}

async fn find_runtime_channel_match(
    registry: &HealthRegistry,
    provider_name: Option<&str>,
    channel_id: Option<ChannelId>,
    tmux_name: Option<&str>,
) -> Option<RuntimeChannelMatch> {
    let preferred_provider = provider_name.and_then(ProviderKind::from_str);
    let providers: Vec<_> = registry
        .providers
        .lock()
        .await
        .iter()
        .filter_map(|entry| {
            let provider = ProviderKind::from_str(&entry.name)?;
            if preferred_provider
                .as_ref()
                .is_some_and(|preferred| preferred != &provider)
            {
                return None;
            }
            Some((provider, entry.shared.clone()))
        })
        .collect();

    for (provider, shared) in providers {
        if let Some(channel_id) = channel_id {
            let has_session = {
                let data = shared.core.lock().await;
                data.sessions.contains_key(&channel_id)
            };
            if has_session || discord::ChannelMailboxRegistry::global_handle(channel_id).is_some() {
                return Some(RuntimeChannelMatch {
                    provider,
                    shared,
                    channel_id,
                });
            }
            continue;
        }

        let Some(tmux_name) = tmux_name else {
            continue;
        };
        let matched_channel_id = {
            let data = shared.core.lock().await;
            data.sessions
                .iter()
                .find_map(|(candidate_channel_id, session)| {
                    session.channel_name.as_ref().and_then(|channel_name| {
                        let expected_tmux_name = provider.build_tmux_session_name(channel_name);
                        (expected_tmux_name == tmux_name).then_some(*candidate_channel_id)
                    })
                })
        };
        if let Some(channel_id) = matched_channel_id {
            return Some(RuntimeChannelMatch {
                provider,
                shared,
                channel_id,
            });
        }
    }

    None
}

async fn apply_runtime_hard_stop_cleanup(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    finish: &discord::FinishTurnResult,
    stop_source: &'static str,
    stop_watcher: bool,
) -> bool {
    if let Some(token) = finish.removed_token.as_ref() {
        token.cancelled.store(true, Ordering::Relaxed);
        shared.global_active.fetch_sub(1, Ordering::Relaxed);
    }

    discord::clear_watchdog_deadline_override(channel_id.get()).await;
    shared
        .dispatch_thread_parents
        .retain(|_, thread| *thread != channel_id);
    shared.recovering_channels.remove(&channel_id);
    shared.turn_start_times.remove(&channel_id);

    if !finish.has_pending {
        shared.dispatch_role_overrides.remove(&channel_id);
    }

    if stop_watcher && let Some((_, watcher)) = shared.tmux_watchers.remove(&channel_id) {
        watcher.cancel.store(true, Ordering::Relaxed);
    }

    let runtime_session_cleared = {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            session.clear_provider_session();
            true
        } else {
            false
        }
    };

    if finish.mailbox_online && finish.has_pending {
        discord::schedule_deferred_idle_queue_kickoff(
            shared.clone(),
            provider.clone(),
            channel_id,
            stop_source,
        );
    }

    runtime_session_cleared
}

pub async fn hard_stop_runtime_turn(
    registry: Option<&HealthRegistry>,
    provider_name: Option<&str>,
    channel_id: Option<u64>,
    tmux_name: Option<&str>,
    stop_source: &'static str,
) -> HardStopRuntimeResult {
    runtime_turn_cleanup_by_lookup(
        registry,
        provider_name,
        channel_id,
        tmux_name,
        stop_source,
        true,
    )
    .await
}

pub async fn stop_runtime_turn_preserving_watcher(
    registry: Option<&HealthRegistry>,
    provider_name: Option<&str>,
    channel_id: Option<u64>,
    tmux_name: Option<&str>,
    stop_source: &'static str,
) -> HardStopRuntimeResult {
    runtime_turn_cleanup_by_lookup(
        registry,
        provider_name,
        channel_id,
        tmux_name,
        stop_source,
        false,
    )
    .await
}

async fn runtime_turn_cleanup_by_lookup(
    registry: Option<&HealthRegistry>,
    provider_name: Option<&str>,
    channel_id: Option<u64>,
    tmux_name: Option<&str>,
    stop_source: &'static str,
    stop_watcher: bool,
) -> HardStopRuntimeResult {
    let channel_id = channel_id.map(ChannelId::new);

    if let Some(registry) = registry
        && let Some(runtime) =
            find_runtime_channel_match(registry, provider_name, channel_id, tmux_name).await
    {
        let finish = if let Some(handle) =
            discord::ChannelMailboxRegistry::global_handle(runtime.channel_id)
        {
            handle
                .finish_turn(discord::queue_persistence_context(
                    &runtime.shared,
                    &runtime.provider,
                    runtime.channel_id,
                ))
                .await
        } else {
            discord::FinishTurnResult {
                removed_token: None,
                has_pending: false,
                mailbox_online: false,
                queue_exit_events: Vec::new(),
            }
        };
        let runtime_session_cleared = apply_runtime_hard_stop_cleanup(
            &runtime.shared,
            &runtime.provider,
            runtime.channel_id,
            &finish,
            stop_source,
            stop_watcher,
        )
        .await;
        return HardStopRuntimeResult {
            cleanup_path: if finish.mailbox_online {
                "mailbox_canonical"
            } else {
                "mailbox_fallback"
            },
            had_active_turn: finish.removed_token.is_some(),
            has_pending_queue: finish.has_pending,
            runtime_session_cleared,
        };
    }

    if let Some(channel_id) = channel_id
        && let Some(handle) = discord::ChannelMailboxRegistry::global_handle(channel_id)
    {
        let finish = handle.hard_stop().await;
        discord::clear_watchdog_deadline_override(channel_id.get()).await;
        return HardStopRuntimeResult {
            cleanup_path: if finish.mailbox_online {
                "mailbox_canonical"
            } else {
                "mailbox_fallback"
            },
            had_active_turn: finish.removed_token.is_some(),
            has_pending_queue: finish.has_pending,
            runtime_session_cleared: false,
        };
    }

    HardStopRuntimeResult::default()
}

/// Best-effort runtime-side equivalent of `/clear` for an existing Discord channel session.
/// Used by auto-queue slot recycling so pooled unified-thread slots start the next group fresh
/// without killing the shared thread itself.
pub async fn clear_provider_channel_runtime(
    registry: &HealthRegistry,
    provider_name: &str,
    channel_id: ChannelId,
    session_key: Option<&str>,
) -> bool {
    let Some(provider) = ProviderKind::from_str(provider_name) else {
        return false;
    };

    let shared = {
        let providers = registry.providers.lock().await;
        providers
            .iter()
            .find(|entry| entry.name.eq_ignore_ascii_case(provider.as_str()))
            .map(|entry| entry.shared.clone())
    };
    let Some(shared) = shared else {
        return false;
    };

    let tmux_name = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.as_ref())
            .map(|channel_name| provider.build_tmux_session_name(channel_name))
            .or_else(|| {
                session_key
                    .and_then(|key| key.split_once(':'))
                    .map(|(_, tmux_name)| tmux_name.to_string())
            })
    };

    let cleared = mailbox_clear_channel(&shared, &provider, channel_id).await;
    if let Some(token) = cleared.removed_token {
        discord::turn_bridge::stop_active_turn(
            &provider,
            &token,
            discord::TmuxCleanupPolicy::PreserveSession,
            "auto-queue slot clear",
        )
        .await;
        decrement_counter(shared.global_active.as_ref());
    }

    {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            discord::settings::cleanup_channel_uploads(channel_id);
            session.clear_provider_session();
            session.history.clear();
            session.pending_uploads.clear();
            session.cleared = true;
        }
    }

    #[cfg(unix)]
    if let Some(name) = tmux_name {
        if provider.uses_managed_tmux_backend() {
            discord::commands::reset_managed_process_session(&name);
        }
    }

    true
}
