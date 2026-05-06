use poise::serenity_prelude as serenity;
use serde::Serialize;
use serenity::ChannelId;

use super::super::relay_health::{
    RelayActiveTurn, RelayHealthSnapshot, RelayStallClassifier, RelayStallState,
};
use super::super::{SharedData, inflight, mailbox_snapshot};
use super::HealthRegistry;
use crate::services::provider::ProviderKind;

pub(in crate::services::discord) const WATCHER_STATE_DESYNC_STALE_MS: i64 = 30_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    Healthy,
    Degraded,
    Unhealthy,
}

/// #964 / #1133: per-channel watcher + relay state surfaced via
/// `GET /api/channels/:id/watcher-state`.
///
/// #1133 enriched the read-only response with operational diagnostics:
/// inflight timing/IDs (PII-free), `tmux_session_alive` (PID check),
/// `has_pending_queue`, and `mailbox_active_user_msg_id`. All new fields
/// are scalar (no message text, no user IDs, no transcripts) so the
/// response remains safe for non-privileged operator dashboards.
#[derive(Clone, Debug, Serialize)]
pub struct WatcherStateSnapshot {
    pub provider: String,
    pub attached: bool,
    pub tmux_session: Option<String>,
    /// #1170: Channel that owns the tmux-keyed watcher slot. Usually this is
    /// the requested channel; when a duplicate attach reuses an existing
    /// watcher, diagnostics can still show which channel owns the live relay.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watcher_owner_channel_id: Option<u64>,
    pub last_relay_offset: u64,
    pub inflight_state_present: bool,
    pub last_relay_ts_ms: i64,
    /// Current tmux output JSONL length when an inflight `output_path` is known.
    /// `null` means the endpoint could not identify a capture file.
    pub last_capture_offset: Option<u64>,
    /// Bytes present in the capture file but not yet confirmed as relayed.
    /// `null` when `last_capture_offset` is unknown.
    pub unread_bytes: Option<u64>,
    /// True when a live tmux-backed turn appears detached/cross-owned or its
    /// capture file diverges from relay telemetry after
    /// `WATCHER_STATE_DESYNC_STALE_MS`. Never-relayed turns use the inflight
    /// `started_at` timestamp as the stale anchor.
    pub desynced: bool,
    /// Process-local watcher reattach/reconnect count for this channel.
    pub reconnect_count: u64,
    /// #1133: Persisted `started_at` from the inflight JSON
    /// (`YYYY-MM-DD HH:MM:SS` localtime). `None` when no inflight is on disk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inflight_started_at: Option<String>,
    /// #1133: Persisted `updated_at` from the inflight JSON. Updated on each
    /// streaming chunk; large skew vs wall clock indicates a stuck turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inflight_updated_at: Option<String>,
    /// #1133: Discord message ID that originated the inflight turn. `None`
    /// when no inflight is on disk; `Some(0)` is filtered to `None` because
    /// rebind-origin inflights use placeholder IDs that do not identify a
    /// real user-authored message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inflight_user_msg_id: Option<u64>,
    /// #1133: Currently streaming Discord message ID for the inflight turn.
    /// Same zero-filtering as `inflight_user_msg_id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inflight_current_msg_id: Option<u64>,
    /// #1133: `true` when `tmux::has_session` confirms the tmux session in
    /// `tmux_session` is alive, `false` when the session is gone, `None`
    /// when no `tmux_session` was known to probe. Backed by a
    /// `tmux has-session` shell-out so the check reflects real PID liveness.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tmux_session_alive: Option<bool>,
    /// #1133: `true` when the per-channel mailbox has at least one queued
    /// intervention waiting for the active turn to finish.
    pub has_pending_queue: bool,
    /// #1133: Discord message ID currently held by the mailbox as the
    /// active-turn anchor (`active_user_message_id`). `None` when the
    /// mailbox is idle (no active turn).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mailbox_active_user_msg_id: Option<u64>,
    /// #1455: Pure relay-stall classifier output derived from the nested
    /// relay-health snapshot. Read-only diagnostic; no recovery behavior is
    /// triggered from this value.
    pub(in crate::services::discord) relay_stall_state: RelayStallState,
    /// #1455: Focused relay-health model shared with the detailed health
    /// endpoint and future recovery/UI code.
    pub(in crate::services::discord) relay_health: RelayHealthSnapshot,
}

impl HealthStatus {
    fn rank(self) -> u8 {
        match self {
            Self::Healthy => 0,
            Self::Degraded => 1,
            Self::Unhealthy => 2,
        }
    }

    pub fn worsen(self, other: Self) -> Self {
        if self.rank() >= other.rank() {
            self
        } else {
            other
        }
    }

    pub fn is_http_ready(self) -> bool {
        matches!(self, Self::Healthy | Self::Degraded)
    }
}

#[derive(Debug, Serialize)]
struct ProviderHealthSnapshot {
    name: String,
    connected: bool,
    active_turns: usize,
    queue_depth: usize,
    sessions: usize,
    restart_pending: bool,
    last_turn_at: Option<String>,
}

#[derive(Debug, Serialize)]
struct MailboxHealthSnapshot {
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

#[derive(Debug, Serialize)]
pub struct DiscordHealthSnapshot {
    status: HealthStatus,
    fully_recovered: bool,
    version: &'static str,
    uptime_secs: u64,
    global_active: usize,
    global_finalizing: usize,
    deferred_hooks: usize,
    queue_depth: usize,
    watcher_count: usize,
    recovery_duration: f64,
    degraded_reasons: Vec<String>,
    providers: Vec<ProviderHealthSnapshot>,
    mailboxes: Vec<MailboxHealthSnapshot>,
}

impl DiscordHealthSnapshot {
    pub fn status(&self) -> HealthStatus {
        self.status
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct RelayThreadProofSnapshot {
    parent_channel_id: Option<u64>,
    thread_channel_id: Option<u64>,
    stale_thread_proof: bool,
}

fn relay_active_turn_from_inflight(
    mailbox_has_cancel_token: bool,
    inflight: Option<&inflight::InflightTurnState>,
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
    inflight: Option<&inflight::InflightTurnState>,
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

    inflight::parse_updated_at_unix(&inflight.updated_at)
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
            let thread_mailbox = mailbox_snapshot(shared, thread_channel).await;
            let thread_inflight =
                provider.and_then(|provider| inflight::load_inflight_state(provider, thread_id));
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

impl HealthRegistry {
    /// #964 / #1133: Snapshot per-channel watcher/relay state for
    /// observability.
    ///
    /// Scans every registered provider and returns the first entry that
    /// knows about this `channel_id`. When no watcher, no relay-coord, no
    /// inflight state, and no mailbox active-turn / queue entry exist,
    /// returns `None` so the handler can emit 404. #1133 widens the
    /// "knows about" criteria to include the mailbox so that a channel
    /// with a queued intervention (but no live tmux yet) still surfaces.
    ///
    /// All new #1133 fields are derived from the same in-memory snapshot
    /// or a single inflight-JSON read (no extra IO per provider). The
    /// `tmux_session_alive` probe shells out to `tmux has-session`; the
    /// call is wrapped in `spawn_blocking` so it never stalls the axum
    /// runtime even if tmux is wedged.
    pub async fn snapshot_watcher_state(&self, channel_id: u64) -> Option<WatcherStateSnapshot> {
        self.snapshot_watcher_state_filtered(channel_id, None).await
    }

    /// #1446 — provider-scoped variant of `snapshot_watcher_state`. Used by
    /// the stall watchdog so a multi-provider deployment that shares a
    /// single Discord channel never has provider B's pass skip cleanup
    /// because provider A happened to be the first registered entry that
    /// "knew" the channel.
    ///
    /// `provider_filter == None` preserves the legacy behaviour
    /// (first-match across all providers).
    pub(crate) async fn snapshot_watcher_state_for_provider(
        &self,
        provider: &ProviderKind,
        channel_id: u64,
    ) -> Option<WatcherStateSnapshot> {
        self.snapshot_watcher_state_filtered(channel_id, Some(provider))
            .await
    }

    async fn snapshot_watcher_state_filtered(
        &self,
        channel_id: u64,
        provider_filter: Option<&ProviderKind>,
    ) -> Option<WatcherStateSnapshot> {
        let channel = ChannelId::new(channel_id);
        let providers = self.providers.lock().await;
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
                .and_then(|pk| inflight::load_inflight_state(pk, channel_id));
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
            let mailbox_snapshot = mailbox_snapshot(&shared, channel).await;
            let mailbox_has_cancel_token = mailbox_snapshot.cancel_token.is_some();
            let mailbox_active_user_msg_id =
                mailbox_snapshot.active_user_message_id.map(|id| id.get());
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
                .then(|| {
                    last_capture_offset.map(|capture| capture.saturating_sub(last_relay_offset))
                })
                .flatten();
            let now_ms = chrono::Utc::now().timestamp_millis();
            let relay_stale_anchor_ms = if last_relay_ts_ms > 0 {
                Some(last_relay_ts_ms)
            } else {
                inflight
                    .as_ref()
                    .and_then(|state| inflight::parse_started_at_unix(&state.started_at))
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
            let live_tmux_orphaned = tmux_session_alive == Some(true)
                && inflight_state_present
                && !attached
                && relay_stale;
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
}

/// Build the detailed health check snapshot for authenticated/local diagnostics.
pub async fn build_health_snapshot(registry: &HealthRegistry) -> DiscordHealthSnapshot {
    build_health_snapshot_with_options(registry, true).await
}

/// Build the public health check snapshot without detail-only mailbox probes.
pub async fn build_public_health_snapshot(registry: &HealthRegistry) -> DiscordHealthSnapshot {
    build_health_snapshot_with_options(registry, false).await
}

async fn build_health_snapshot_with_options(
    registry: &HealthRegistry,
    include_mailbox_details: bool,
) -> DiscordHealthSnapshot {
    let uptime_secs = registry.started_at.elapsed().as_secs();
    let version = env!("CARGO_PKG_VERSION");

    let providers = registry.providers.lock().await;
    let mut provider_entries = Vec::new();
    let mut degraded_reasons = Vec::new();
    let mut status = HealthStatus::Healthy;
    let mut fully_recovered = !providers.is_empty();
    let mut deferred_hooks = 0usize;
    let mut queue_depth = 0usize;
    let mut watcher_count = 0usize;
    let mut recovery_duration = 0.0f64;
    let mut mailbox_entries = Vec::new();

    if providers.is_empty() {
        degraded_reasons.push("no_providers_registered".to_string());
        status = HealthStatus::Unhealthy;
        fully_recovered = false;
    }

    for entry in providers.iter() {
        let session_count = entry
            .shared
            .core
            .try_lock()
            .map(|data| data.sessions.len())
            .unwrap_or(0);
        let mailbox_snapshots = entry.shared.mailboxes.snapshot_all().await;
        let active_turns = mailbox_snapshots
            .values()
            .filter(|snapshot| snapshot.cancel_token.is_some())
            .count();
        let provider_queue_depth: usize = mailbox_snapshots
            .values()
            .map(|snapshot| snapshot.intervention_queue.len())
            .sum();

        let restart_pending = entry
            .shared
            .restart_pending
            .load(std::sync::atomic::Ordering::Relaxed);
        let connected = entry
            .shared
            .bot_connected
            .load(std::sync::atomic::Ordering::Relaxed);
        let reconcile_done = entry
            .shared
            .reconcile_done
            .load(std::sync::atomic::Ordering::Relaxed);
        let provider_deferred_hooks = entry
            .shared
            .deferred_hook_backlog
            .load(std::sync::atomic::Ordering::Relaxed);
        let provider_watchers = entry.shared.tmux_watchers.len();
        let recovering_channels = mailbox_snapshots
            .values()
            .filter(|snapshot| snapshot.recovery_started_at.is_some())
            .count();
        let provider_recovery_duration = recovery_duration_secs(&entry.shared);
        let last_turn_at = entry
            .shared
            .last_turn_at
            .lock()
            .ok()
            .and_then(|g| g.clone());

        deferred_hooks += provider_deferred_hooks;
        queue_depth += provider_queue_depth;
        watcher_count += provider_watchers;
        recovery_duration = recovery_duration.max(provider_recovery_duration);
        if include_mailbox_details {
            let provider_kind = ProviderKind::from_str(&entry.name);
            for (channel_id, snapshot) in &mailbox_snapshots {
                let channel = *channel_id;
                let inflight_state = provider_kind
                    .as_ref()
                    .and_then(|pk| inflight::load_inflight_state(pk, channel.get()));
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
                        .and_then(|state| inflight::parse_started_at_unix(&state.started_at))
                        .and_then(|seconds| seconds.checked_mul(1000))
                };
                let relay_stale = relay_stale_anchor_ms
                    .map(|anchor_ms| {
                        now_ms.saturating_sub(anchor_ms) >= WATCHER_STATE_DESYNC_STALE_MS
                    })
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
                let active_turn = relay_active_turn_from_inflight(
                    mailbox_has_cancel_token,
                    inflight_state.as_ref(),
                );
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
                mailbox_entries.push(MailboxHealthSnapshot {
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

        if !connected {
            status = status.worsen(HealthStatus::Unhealthy);
            degraded_reasons.push(format!("provider:{}:disconnected", entry.name));
        }
        if restart_pending {
            status = status.worsen(HealthStatus::Unhealthy);
            degraded_reasons.push(format!("provider:{}:restart_pending", entry.name));
        }
        if !reconcile_done {
            status = status.worsen(HealthStatus::Degraded);
            degraded_reasons.push(format!("provider:{}:reconcile_in_progress", entry.name));
            fully_recovered = false;
        }
        if provider_deferred_hooks > 0 {
            status = status.worsen(HealthStatus::Degraded);
            degraded_reasons.push(format!(
                "provider:{}:deferred_hooks_backlog:{}",
                entry.name, provider_deferred_hooks
            ));
        }
        if provider_queue_depth > 0 {
            status = status.worsen(HealthStatus::Degraded);
            degraded_reasons.push(format!(
                "provider:{}:pending_queue_depth:{}",
                entry.name, provider_queue_depth
            ));
        }
        if recovering_channels > 0 {
            status = status.worsen(HealthStatus::Degraded);
            degraded_reasons.push(format!(
                "provider:{}:recovering_channels:{}",
                entry.name, recovering_channels
            ));
            fully_recovered = false;
        }

        provider_entries.push(ProviderHealthSnapshot {
            name: entry.name.clone(),
            connected,
            active_turns,
            queue_depth: provider_queue_depth,
            sessions: session_count,
            restart_pending,
            last_turn_at,
        });
    }

    let global_active = if let Some(p) = providers.first() {
        p.shared
            .global_active
            .load(std::sync::atomic::Ordering::Relaxed)
    } else {
        0
    };
    let global_finalizing = if let Some(p) = providers.first() {
        p.shared
            .global_finalizing
            .load(std::sync::atomic::Ordering::Relaxed)
    } else {
        0
    };

    DiscordHealthSnapshot {
        status,
        fully_recovered,
        version,
        uptime_secs,
        global_active: global_active as usize,
        global_finalizing: global_finalizing as usize,
        deferred_hooks,
        queue_depth,
        watcher_count,
        recovery_duration,
        degraded_reasons,
        providers: provider_entries,
        mailboxes: mailbox_entries,
    }
}

fn recovery_duration_secs(shared: &SharedData) -> f64 {
    let recorded_ms = shared
        .recovery_duration_ms
        .load(std::sync::atomic::Ordering::Relaxed);
    let duration_ms = if recorded_ms > 0 {
        recorded_ms
    } else {
        let elapsed_ms = shared.recovery_started_at.elapsed().as_millis();
        elapsed_ms.min(u64::MAX as u128) as u64
    };
    duration_ms as f64 / 1000.0
}
