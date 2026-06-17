use poise::serenity_prelude::ChannelId;
use serde::Serialize;

use super::HealthRegistry;
use super::mailbox::MailboxHealthSnapshot;
use super::provider_probe::{self, ProviderHealthSnapshot};
use super::redaction;
use super::session_enrichment::SessionEnrichment;
use crate::services::discord;
use crate::services::discord::SharedData;
use crate::services::discord::relay_health::{
    RelayActiveTurn, RelayHealthSnapshot, RelayStallClassifier, RelayStallState,
};
use crate::services::provider::ProviderKind;

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
    /// #3126: `true` when the in-flight row records a turn whose terminal
    /// assistant response has already been committed
    /// (`InflightTurnState::terminal_delivery_committed`). A row with this set
    /// is a completed turn that is now idle (waiting on a `ScheduleWakeup` /
    /// loop wind-down), NOT a hung provider turn. The stall watchdog uses it as
    /// a false-positive guard so a normally-finished-then-sleeping session is
    /// never force-cleaned as a deadlock.
    pub(in crate::services::discord) inflight_terminal_delivery_committed: bool,
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
        .dispatch
        .thread_parents
        .get(&channel_id)
        .map(|entry| entry.value().get());
    let parent_channel_id = shared
        .dispatch
        .thread_parents
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
    watcher_attached_stale: bool,
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
        watcher_attached_stale: input.watcher_attached_stale,
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
        let channel = ChannelId::new(channel_id);
        if let Some(shared) = self.shared_for_provider_on_channel(provider, channel).await {
            return watcher_state_snapshot_for_shared(provider.as_str(), shared, channel).await;
        }

        self.snapshot_watcher_state_filtered(channel_id, Some(provider))
            .await
    }

    /// Snapshot a channel against a specific runtime.
    ///
    /// Multi-bot deployments can register several runtimes under the same
    /// provider name. Callers that have already resolved the owning
    /// `SharedData` must not go back through provider-name scanning, because
    /// persisted inflight state is keyed by provider+channel and can make the
    /// first registered runtime look like it owns another bot's channel.
    pub(crate) async fn snapshot_watcher_state_for_shared(
        &self,
        provider: &ProviderKind,
        shared: std::sync::Arc<SharedData>,
        channel_id: u64,
    ) -> Option<WatcherStateSnapshot> {
        watcher_state_snapshot_for_shared(provider.as_str(), shared, ChannelId::new(channel_id))
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
            let provider_kind = ProviderKind::from_str(&entry.name);
            if let Some(snapshot) = watcher_state_snapshot_for_shared(
                provider_kind
                    .as_ref()
                    .map(ProviderKind::as_str)
                    .unwrap_or(entry.name.as_str()),
                entry.shared.clone(),
                channel,
            )
            .await
            {
                return Some(snapshot);
            }
        }
        None
    }
}

async fn watcher_state_snapshot_for_shared(
    provider_name: &str,
    shared: std::sync::Arc<SharedData>,
    channel: ChannelId,
) -> Option<WatcherStateSnapshot> {
    let provider_kind = ProviderKind::from_str(provider_name);
    let session = SessionEnrichment::load(&shared, provider_kind.as_ref(), channel).await;
    let mailbox_snapshot = discord::mailbox_snapshot(&shared, channel).await;
    let mailbox_has_cancel_token = mailbox_snapshot.cancel_token.is_some();
    let mailbox_active_user_msg_id =
        redaction::visible_serenity_message_id(mailbox_snapshot.active_user_message_id);
    let has_pending_queue = !mailbox_snapshot.intervention_queue.is_empty();
    let mailbox_engaged =
        mailbox_has_cancel_token || mailbox_active_user_msg_id.is_some() || has_pending_queue;
    let mailbox_cancel_tmux_session = mailbox_snapshot.cancel_token.as_ref().and_then(|token| {
        token
            .tmux_session
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    });
    let tmux_session_for_liveness_probe = session
        .tmux_session
        .as_deref()
        .or(mailbox_cancel_tmux_session.as_deref());
    let has_thread_proof = shared.dispatch.thread_parents.contains_key(&channel)
        || shared
            .dispatch
            .thread_parents
            .iter()
            .any(|entry| *entry.value() == channel);
    if !session.attached
        && !session.has_relay_coord
        && !session.inflight_state_present
        && !mailbox_engaged
        && !has_thread_proof
    {
        return None;
    }

    let tmux_session_alive =
        SessionEnrichment::probe_tmux_session_alive(tmux_session_for_liveness_probe).await;
    let desynced = session.desynced(tmux_session_alive == Some(true), session.attached);
    let active_turn =
        relay_active_turn_from_inflight(mailbox_has_cancel_token, session.inflight.as_ref());
    let relay_thread_proof = relay_thread_proof_for_channel(
        &shared,
        provider_kind.as_ref(),
        channel,
        mailbox_has_cancel_token || session.inflight_state_present || session.attached,
    )
    .await;
    let relay_health = build_relay_health_snapshot(RelayHealthBuildInput {
        provider: provider_name.to_string(),
        channel_id: channel.get(),
        mailbox_has_cancel_token,
        mailbox_active_user_msg_id,
        queue_depth: mailbox_snapshot.intervention_queue.len(),
        watcher_attached: session.attached,
        watcher_attached_stale: session.watcher_attached_stale,
        watcher_owner_channel_id: session.watcher_owner_channel_id,
        tmux_session: session.tmux_session.clone(),
        tmux_alive: tmux_session_alive,
        bridge_inflight_present: session.inflight_state_present,
        bridge_current_msg_id: session.inflight_current_msg_id(),
        watcher_owns_live_relay: session.watcher_owns_live_relay(),
        last_relay_ts_ms: session.last_relay_ts_ms,
        last_relay_offset: session.last_relay_offset,
        last_capture_offset: session.last_capture_offset,
        unread_bytes: session.unread_bytes,
        desynced,
        thread_proof: relay_thread_proof,
        active_turn,
        last_outbound_activity_ms: last_outbound_activity_ms(
            session.last_relay_ts_ms,
            session.inflight.as_ref(),
        ),
    });
    let relay_stall_state = RelayStallClassifier::classify(&relay_health);
    trace_relay_health_classification(&relay_health, relay_stall_state);
    Some(WatcherStateSnapshot {
        provider: provider_name.to_string(),
        attached: session.attached,
        tmux_session: session.tmux_session.clone(),
        watcher_owner_channel_id: session.watcher_owner_channel_id,
        last_relay_offset: session.last_relay_offset,
        inflight_state_present: session.inflight_state_present,
        last_relay_ts_ms: session.last_relay_ts_ms,
        last_capture_offset: session.last_capture_offset,
        unread_bytes: session.unread_bytes,
        desynced,
        reconnect_count: session.reconnect_count,
        inflight_started_at: session.inflight_started_at(),
        inflight_updated_at: session.inflight_updated_at(),
        inflight_user_msg_id: session.inflight_user_msg_id(),
        inflight_current_msg_id: session.inflight_current_msg_id(),
        tmux_session_alive,
        has_pending_queue,
        mailbox_active_user_msg_id,
        inflight_terminal_delivery_committed: session.inflight_terminal_delivery_committed(),
        relay_stall_state,
        relay_health,
    })
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
    let mut provider_active_turns = 0usize;

    if providers.is_empty() {
        degraded_reasons.push("no_providers_registered".to_string());
        status = HealthStatus::Unhealthy;
        fully_recovered = false;
    }

    for entry in providers.iter() {
        let provider_probe = provider_probe::probe_provider(entry).await;

        deferred_hooks += provider_probe.deferred_hooks;
        queue_depth += provider_probe.queue_depth;
        watcher_count += provider_probe.watcher_count;
        recovery_duration = recovery_duration.max(provider_probe.recovery_duration);
        if include_mailbox_details {
            let provider_kind = ProviderKind::from_str(&entry.name);
            for (channel_id, snapshot) in &provider_probe.mailbox_snapshots {
                let channel = *channel_id;
                let session =
                    SessionEnrichment::load(&entry.shared, provider_kind.as_ref(), channel).await;
                let tmux_present = session.tmux_session_present();
                let process_present = session.process_present();
                let desynced = session.desynced(tmux_present, session.watcher_attached);
                let mailbox_has_cancel_token = snapshot.cancel_token.is_some();
                let queue_depth = snapshot.intervention_queue.len();
                let mailbox_active_user_msg_id =
                    redaction::visible_serenity_message_id(snapshot.active_user_message_id);
                let relay_thread_proof = relay_thread_proof_for_channel(
                    &entry.shared,
                    provider_kind.as_ref(),
                    channel,
                    mailbox_has_cancel_token
                        || session.inflight_state_present
                        || session.watcher_attached,
                )
                .await;
                let active_turn = relay_active_turn_from_inflight(
                    mailbox_has_cancel_token,
                    session.inflight.as_ref(),
                );
                let relay_health = build_relay_health_snapshot(RelayHealthBuildInput {
                    provider: entry.name.clone(),
                    channel_id: channel.get(),
                    mailbox_has_cancel_token,
                    mailbox_active_user_msg_id,
                    queue_depth,
                    watcher_attached: session.watcher_attached,
                    watcher_attached_stale: session.watcher_attached_stale,
                    watcher_owner_channel_id: session.watcher_owner_channel_id,
                    tmux_session: session.tmux_session.clone(),
                    tmux_alive: session.tmux_session.as_ref().map(|_| tmux_present),
                    bridge_inflight_present: session.inflight_state_present,
                    bridge_current_msg_id: session.inflight_current_msg_id(),
                    watcher_owns_live_relay: session.watcher_owns_live_relay(),
                    last_relay_ts_ms: session.last_relay_ts_ms,
                    last_relay_offset: session.last_relay_offset,
                    last_capture_offset: session.last_capture_offset,
                    unread_bytes: session.unread_bytes,
                    desynced,
                    thread_proof: relay_thread_proof,
                    active_turn,
                    last_outbound_activity_ms: last_outbound_activity_ms(
                        session.last_relay_ts_ms,
                        session.inflight.as_ref(),
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
                    watcher_attached: session.watcher_attached,
                    inflight_state_present: session.inflight_state_present,
                    tmux_present,
                    process_present,
                    active_dispatch_present: session.active_dispatch_present(),
                    relay_stall_state,
                    relay_health,
                });
            }
        }

        status = status.worsen(provider_probe.status);
        if !provider_probe.fully_recovered {
            fully_recovered = false;
        }
        provider_active_turns =
            provider_active_turns.saturating_add(count_active_turns(&provider_probe));
        degraded_reasons.extend(provider_probe.degraded_reasons);
        provider_entries.push(provider_probe.snapshot);
    }

    let global_active = if let Some(p) = providers.first() {
        p.shared
            .restart
            .global_active
            .load(std::sync::atomic::Ordering::Relaxed)
    } else {
        0
    };
    let global_finalizing = if let Some(p) = providers.first() {
        p.shared
            .restart
            .global_finalizing
            .load(std::sync::atomic::Ordering::Relaxed)
    } else {
        0
    };
    let (global_active, global_counter_degraded_reason) =
        observe_global_active_invariant(global_active, provider_active_turns, global_finalizing);
    if let Some(reason) = global_counter_degraded_reason {
        // The ONLY degraded reason this can produce now is a pathological
        // wraparound/out-of-bounds read (`global_active_counter_out_of_bounds`).
        // Routine in-band drift between the (non-atomic, sequentially collected)
        // mailbox snapshot and the atomic read is OBSERVE-ONLY — it is reported
        // via a debug-level trace inside the detector but never degrades health
        // and never panics, because that drift is reachable in normal operation
        // (see the detector docs). A wraparound, by contrast, is genuinely
        // unreachable under the saturating-decrement floor (#2934), so we still
        // surface it as degraded for operator visibility.
        status = status.worsen(HealthStatus::Degraded);
        degraded_reasons.push(reason);
    }

    DiscordHealthSnapshot {
        status,
        fully_recovered,
        version,
        uptime_secs,
        global_active,
        global_finalizing,
        deferred_hooks,
        queue_depth,
        watcher_count,
        recovery_duration,
        degraded_reasons,
        providers: provider_entries,
        mailboxes: mailbox_entries,
    }
}

fn count_active_turns(provider_probe: &provider_probe::ProviderProbe) -> usize {
    provider_probe
        .mailbox_snapshots
        .values()
        .filter(|snapshot| snapshot.cancel_token.is_some())
        .count()
}

/// Observe the `global_active` invariant instead of silently papering over it
/// (#3019, sub-issue of #3016).
///
/// HISTORY: this used to be `normalize_global_active_counter`, a SILENT
/// post-hoc band-aid that, on any wrapped/out-of-bounds reading, quietly
/// substituted the snapshot-observed `provider_active_turns` for the real
/// atomic so health snapshots never surfaced the drift. That clamp existed
/// precisely because there was no single authoritative writer: multiple
/// `fetch_add`/`fetch_sub` sites drifted (#2934) and the clamp hid it.
///
/// NOW the counter has a single increment authority
/// ([`increment_global_active`](crate::services::discord::increment_global_active))
/// and a single saturating decrement authority
/// ([`saturating_decrement_global_active`](crate::services::discord::saturating_decrement_global_active)),
/// each fired +1/-1 IFF the matching mailbox slot actually
/// activated/finished. The #3019 deliverable is that we now report the REAL
/// atomic `global_active` instead of the masked-over observed count.
///
/// WHY IN-BAND DRIFT IS OBSERVE-ONLY (codex review): the health snapshot is NOT
/// an atomic view. It reads each mailbox actor SEQUENTIALLY to derive
/// `provider_active_turns`, then reads the `global_active` atomic afterward.
/// Nothing serializes channel transitions against that collection, so multiple
/// channels can legitimately start/finish in the window between those reads.
/// Worse, the turn dispatchers (`headless_turn.rs`, `intake_turn.rs`) acquire
/// the mailbox slot BEFORE they increment `global_active`, so within that window
/// two concurrent normal starts produce a drift greater than 1. A fixed
/// tolerance therefore cannot distinguish a real counter bug from a benign,
/// reachable-in-normal-operation snapshot race. Treating such drift as a
/// `degraded` reason — or, worse, a `debug_assert` panic — produced FALSE
/// POSITIVES and flaky CI on a perfectly healthy relay.
///
/// So in-band drift is now OBSERVE-ONLY: we always report the real atomic value
/// and, when it disagrees with the (non-atomic) observed count, emit at most a
/// debug-level trace as a metric. No degraded health, no panic.
///
/// The wraparound floor still matters for DISPLAY safety: although the
/// saturating decrement floor (#2934) prevents a writer from wrapping 0 →
/// `usize::MAX`, if a wrapped value is ever observed we clamp the DISPLAY to
/// `provider_active_turns` so a single garbage reading does not poison the
/// snapshot. That path is genuinely unreachable under the single-authority
/// invariant, so — unlike in-band drift — it still surfaces a degraded reason
/// for operator visibility. It is a clamp for display safety, never a silent
/// drift-masking path.
///
/// This is the PURE detector (return value is easily unit-testable).
pub(super) fn observe_global_active_invariant(
    raw_global_active: usize,
    provider_active_turns: usize,
    global_finalizing: usize,
) -> (usize, Option<String>) {
    // A reading at/above this threshold can only be a wraparound/garbage value;
    // the single-authority saturating decrement floor (#2934) means a healthy
    // writer can never produce it.
    const WRAPPED_COUNTER_THRESHOLD: usize = usize::MAX / 2;

    if raw_global_active >= WRAPPED_COUNTER_THRESHOLD {
        // Pathological: should be unreachable now that decrement saturates at 0.
        // Make it LOUD and clamp the DISPLAY only (never silently).
        tracing::error!(
            target: "agentdesk::global_active",
            raw = raw_global_active,
            provider_active_turns,
            global_finalizing,
            "global_active wrapped/out-of-bounds (invariant violation, clamping display)"
        );
        return (
            provider_active_turns,
            Some(format!(
                "global_active_counter_out_of_bounds:raw={raw_global_active}:provider_active_turns={provider_active_turns}:global_finalizing={global_finalizing}"
            )),
        );
    }

    // In-band reading: always report the REAL atomic. Any disagreement with the
    // observed mailbox count is a benign, reachable snapshot race (the snapshot
    // is non-atomic and slots are acquired before the counter is incremented),
    // so it is OBSERVE-ONLY: at most a debug-level metric trace, never a
    // degraded reason and never a panic.
    let drift = raw_global_active.abs_diff(provider_active_turns);
    if drift > 0 {
        tracing::debug!(
            target: "agentdesk::global_active",
            global_active = raw_global_active,
            provider_active_turns,
            global_finalizing,
            drift,
            "global_active vs observed mailbox snapshot drift (observe-only; benign snapshot race)"
        );
    }

    (raw_global_active, None)
}
