use poise::serenity_prelude::ChannelId;
use serde::Serialize;

use super::liveness_authority::CaptureCoordinateObservation;
use super::mailbox::MailboxHealthSnapshot;
use super::provider_probe::{self, ProviderHealthSnapshot};
use super::redaction;
use super::session_enrichment::SessionEnrichment;
use super::stall_verdict;
use super::{BotTokenReloadScopes, HealthRegistry, bot_token_reload_scopes};
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
    #[serde(skip)]
    pub(in crate::services::discord) capture_coordinate: CaptureCoordinateObservation,
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
    /// Internal episode identity captured with the active message ID. It is
    /// intentionally excluded from operator JSON and only authorizes repair.
    #[serde(skip)]
    pub(in crate::services::discord) mailbox_active_turn_nonce: Option<String>,
    /// #4408 phase-2 (I1): the transcript path the dcserver actually binds its
    /// relay tail to. Resolved with per-field precedence — a live inflight row's
    /// persisted `output_path` wins; otherwise the in-memory tmux runtime
    /// binding's `relay_output_path` (a sync single-shot lookup, never held
    /// across an await). Lets the out-of-band watchdog compare the server's
    /// asserted selector (B) against its own growth-aware transcript pick (F).
    /// `null`/absent means neither source is known, so the watchdog fails closed
    /// instead of alarming on an unknown bind. See `resolve_bound_selector`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bound_output_path: Option<String>,
    /// #4408 phase-2 (I1): the provider session the bound transcript belongs to,
    /// resolved from the inflight row's `session_id` first, else the runtime
    /// binding. Read-only; the side-effecting claude-session-id GET (which
    /// advances a watermark) is intentionally never consulted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bound_session_id: Option<String>,
    /// #3126: `true` when the in-flight row records a turn whose terminal
    /// assistant response has already been committed
    /// (`InflightTurnState::terminal_delivery_committed`). A row with this set
    /// is a completed turn that is now idle (waiting on a `ScheduleWakeup` /
    /// loop wind-down), NOT a hung provider turn. The stall watchdog uses it as
    /// a false-positive guard so a normally-finished-then-sleeping session is
    /// never force-cleaned as a deadlock.
    pub(in crate::services::discord) inflight_terminal_delivery_committed: bool,
    #[serde(skip)]
    pub(in crate::services::discord) inflight_identity:
        Option<discord::inflight::InflightTurnIdentity>,
    #[serde(skip)]
    pub(in crate::services::discord) inflight_finalizer_turn_id: Option<u64>,
    #[serde(skip)]
    pub(in crate::services::discord) inflight_output_path: Option<String>,
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
    bot_token_reload_scopes: BotTokenReloadScopes,
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

/// #3631: a rebind-origin inflight row (POST /api/inflight/rebind) is a
/// synthetic origin marker — `turn_id`/`dispatch_id` null, `user_msg_id`/
/// `current_msg_id` 0, `full_response` empty — NOT a real user/agent turn.
/// With no mailbox cancel token there is no live turn, so the channel is idle.
/// The classifier previously fell through to `Foreground`, falsely reporting
/// `active_foreground_stream` and stranding queued messages (they never
/// dispatch because no real turn ever ends to drain the queue). A cancel token
/// present means a real turn HAS since started on the adopted session, so it is
/// genuinely active — only treat it as idle when no cancel token is held.
///
/// Pure seam so the idle decision is unit-testable without constructing a full
/// `InflightTurnState`.
fn rebind_origin_inflight_is_idle(mailbox_has_cancel_token: bool, rebind_origin: bool) -> bool {
    rebind_origin && !mailbox_has_cancel_token
}

fn ownerless_external_input_inflight_is_idle(
    inflight: Option<&discord::inflight::InflightTurnState>,
) -> bool {
    inflight.is_some_and(discord::inflight::ownerless_external_input_inflight_is_stale)
}

/// #4408 phase-2 (I1): resolve the transcript path / provider session the relay
/// tail is bound to, surfaced on `watcher-state` so the out-of-band watchdog can
/// compare the server's asserted selector (B) against its own growth-aware
/// transcript pick (F).
///
/// Precedence is per field: a live inflight row's persisted `output_path` /
/// `session_id` win because they are the authoritative binding; when the inflight
/// row is absent — or leaves a field blank — we fall back to the in-memory tmux
/// runtime binding's `relay_output_path` / `session_id`. Both inputs come from
/// sync single-shot lookups that never straddle an await (so no
/// `await_holding_lock` allow is introduced), and the side-effecting
/// claude-session-id GET path is intentionally NOT consulted. A field is `None`
/// when neither source knows it, so serialization omits it and the watchdog fails
/// closed instead of alarming on an unknown bind.
fn resolve_bound_selector(
    inflight_output_path: Option<&str>,
    inflight_session_id: Option<&str>,
    binding: Option<&crate::services::tui_prompt_dedupe::TuiRuntimeBinding>,
) -> (Option<String>, Option<String>) {
    fn non_blank(value: Option<&str>) -> Option<String> {
        value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    }

    let bound_output_path = non_blank(inflight_output_path)
        .or_else(|| non_blank(binding.map(|binding| binding.relay_output_path())));
    let bound_session_id = non_blank(inflight_session_id)
        .or_else(|| non_blank(binding.and_then(|binding| binding.session_id.as_deref())));
    (bound_output_path, bound_session_id)
}

fn relay_active_turn_from_inflight(
    mailbox_has_cancel_token: bool,
    inflight: Option<&discord::inflight::InflightTurnState>,
) -> RelayActiveTurn {
    if !mailbox_has_cancel_token && inflight.is_none() {
        return RelayActiveTurn::None;
    }

    // #3631: a rebind-origin row (POST /api/inflight/rebind) is a synthetic
    // origin marker, NOT a real user/agent turn — treat it as idle when there
    // is no live turn. See `rebind_origin_inflight_is_idle`.
    if inflight.is_some_and(|state| {
        rebind_origin_inflight_is_idle(mailbox_has_cancel_token, state.rebind_origin)
    }) {
        return RelayActiveTurn::None;
    }

    // A stale bridge-owned TUI-direct synthetic row has no live relay owner left
    // after a restart. Restart recovery can recreate a mailbox cancel token for
    // the persisted row, but that token is not evidence that the lost bridge
    // tail can still make progress.
    if ownerless_external_input_inflight_is_idle(inflight) {
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
    mailbox_turn_started_at_ms: Option<i64>,
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

fn authoritative_tmux_session(
    enriched_session: Option<&str>,
    mailbox_cancel_session: Option<&str>,
) -> Option<String> {
    enriched_session
        .or(mailbox_cancel_session)
        .map(str::to_string)
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
        mailbox_turn_started_at_ms: input.mailbox_turn_started_at_ms,
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
        match tokio::time::timeout(
            std::time::Duration::from_secs(2),
            self.shared_for_provider_on_channel(provider, channel),
        )
        .await
        {
            Ok(Some(shared)) => {
                return watcher_state_snapshot_for_shared(provider.as_str(), shared, channel).await;
            }
            Ok(None) => {}
            Err(_) => {
                tracing::warn!(
                    provider = provider.as_str(),
                    channel_id,
                    "watcher-state provider/channel runtime resolve timed out; skipping provider scan to preserve channel ownership",
                );
                return None;
            }
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
    let mailbox_cancel_tmux_session = mailbox_snapshot
        .cancel_token
        .as_ref()
        .and_then(|token| token.tmux_session_name());
    // Use one authority for both the probe target and the published identity.
    // The cancel token is the earliest turn-owned tmux proof and can exist
    // before inflight/watcher enrichment. Keeping only the probe fallback would
    // publish `tmux_alive=None` with `tmux_session=None` on a transient probe
    // error, allowing aged orphan cleanup to bypass AgentDesk-name protection.
    let authoritative_tmux_session = authoritative_tmux_session(
        session.tmux_session.as_deref(),
        mailbox_cancel_tmux_session.as_deref(),
    );
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
        SessionEnrichment::probe_tmux_session_alive(authoritative_tmux_session.as_deref()).await;
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
        mailbox_turn_started_at_ms: mailbox_snapshot
            .turn_started_at
            .map(|started_at| started_at.timestamp_millis()),
        queue_depth: mailbox_snapshot.intervention_queue.len(),
        watcher_attached: session.attached,
        watcher_attached_stale: session.watcher_attached_stale,
        watcher_owner_channel_id: session.watcher_owner_channel_id,
        tmux_session: authoritative_tmux_session.clone(),
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
    // #4408 phase-2 (I1): resolve the relay tail's bound transcript/session. The
    // runtime binding is a sync single-shot lookup (its Mutex guard is released
    // inside the call and never held across the awaits above/below), so this adds
    // no `await_holding_lock` allow.
    let tmux_runtime_binding = authoritative_tmux_session
        .as_deref()
        .and_then(crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session);
    let (bound_output_path, bound_session_id) = resolve_bound_selector(
        session
            .inflight
            .as_ref()
            .and_then(|state| state.output_path.as_deref()),
        session
            .inflight
            .as_ref()
            .and_then(|state| state.session_id.as_deref()),
        tmux_runtime_binding.as_ref(),
    );
    Some(WatcherStateSnapshot {
        provider: provider_name.to_string(),
        attached: session.attached,
        tmux_session: authoritative_tmux_session,
        watcher_owner_channel_id: session.watcher_owner_channel_id,
        last_relay_offset: session.last_relay_offset,
        inflight_state_present: session.inflight_state_present,
        last_relay_ts_ms: session.last_relay_ts_ms,
        last_capture_offset: session.last_capture_offset,
        capture_coordinate: session.capture_coordinate.clone(),
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
        mailbox_active_turn_nonce: mailbox_snapshot.active_turn_nonce.clone(),
        bound_output_path,
        bound_session_id,
        inflight_terminal_delivery_committed: session.inflight_terminal_delivery_committed(),
        inflight_identity: session
            .inflight
            .as_ref()
            .map(discord::inflight::InflightTurnIdentity::from_state),
        inflight_finalizer_turn_id: session
            .inflight
            .as_ref()
            .map(|state| state.effective_finalizer_turn_id()),
        inflight_output_path: session
            .inflight
            .as_ref()
            .and_then(|state| state.output_path.clone()),
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
                    mailbox_turn_started_at_ms: snapshot
                        .turn_started_at
                        .map(|started_at| started_at.timestamp_millis()),
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
                let stall_shadow_verdict = stall_verdict::classify_health_snapshot_lossy(
                    provider_kind.as_ref(),
                    channel,
                    &session,
                    &relay_health,
                    registry.started_at_unix(),
                );
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
                    stall_shadow_verdict,
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
        bot_token_reload_scopes: bot_token_reload_scopes(),
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use poise::serenity_prelude::{ChannelId, MessageId, UserId};

    use super::{
        HealthRegistry, authoritative_tmux_session, build_health_snapshot,
        rebind_origin_inflight_is_idle, relay_active_turn_from_inflight, resolve_bound_selector,
    };
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::discord::inflight::InflightTurnState;
    use crate::services::discord::relay_health::RelayActiveTurn;
    use crate::services::provider::{CancelToken, ProviderKind};
    use crate::services::tui_prompt_dedupe::TuiRuntimeBinding;
    use chrono::TimeZone;

    const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";
    static NEXT_ABSENT_MAILBOX_CHANNEL: AtomicU64 = AtomicU64::new(9_406_800_000_000);

    struct EnvGuard;

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
        }
    }

    /// #3631: a rebind-origin row with NO cancel token is idle (so the channel
    /// is not falsely reported as an active foreground stream and queued
    /// messages can dispatch). A cancel token present (a real turn started on
    /// the adopted session) or a non-rebind-origin row is NOT idle.
    #[test]
    fn rebind_origin_idle_only_without_cancel_token() {
        // rebind-origin + no cancel token → idle.
        assert!(rebind_origin_inflight_is_idle(false, true));
        // rebind-origin + live cancel token → NOT idle (real turn running).
        assert!(!rebind_origin_inflight_is_idle(true, true));
        // not a rebind-origin row → never idle via this seam.
        assert!(!rebind_origin_inflight_is_idle(false, false));
        assert!(!rebind_origin_inflight_is_idle(true, false));
    }

    #[test]
    fn stale_ownerless_external_input_inflight_is_not_foreground_even_with_cancel_token() {
        let now_unix = chrono::Utc::now().timestamp();
        let stale_unix = now_unix
            - (crate::services::discord::inflight::INFLIGHT_STALENESS_THRESHOLD_SECS as i64)
            - 1;
        let stale_updated_at = chrono::Local
            .timestamp_opt(stale_unix, 0)
            .single()
            .expect("valid local time")
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let state: InflightTurnState = serde_json::from_value(serde_json::json!({
            "version": 9,
            "provider": "codex",
            "channel_id": 42,
            "channel_name": "adk-cdx",
            "request_owner_user_id": 7,
            "user_msg_id": 8,
            "current_msg_id": 0,
            "current_msg_len": 3,
            "user_text": "typed in TUI",
            "source": "text",
            "session_id": null,
            "tmux_session_name": "AgentDesk-codex-adk-cdx",
            "output_path": "/tmp/rollout.jsonl",
            "input_fifo_path": null,
            "last_offset": 0,
            "full_response": "",
            "response_sent_offset": 0,
            "started_at": stale_updated_at,
            "updated_at": stale_updated_at,
            "terminal_delivery_committed": false,
            "relay_owner_kind": "none",
            "turn_source": "external_input",
            "injected_prompt_message_id": 8
        }))
        .expect("deserialize external-input inflight row");

        assert_eq!(
            relay_active_turn_from_inflight(false, Some(&state)),
            RelayActiveTurn::None,
            "stale ownerless TUI-direct synthetic rows must not strand recovery in active_foreground_stream"
        );
        assert_eq!(
            relay_active_turn_from_inflight(true, Some(&state)),
            RelayActiveTurn::None,
            "restart recovery can resurrect a cancel token for the stale row, but not the lost bridge tail"
        );
    }

    /// #4408 phase-2 (I1) case 1 (inflight): a live inflight row's persisted
    /// `output_path`/`session_id` are authoritative and win over any runtime
    /// binding, so B reflects the turn's own bind.
    #[test]
    fn bound_selector_prefers_inflight_over_binding() {
        let binding = TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: "/tmp/binding-primary.jsonl".to_string(),
            relay_output_path: Some("/tmp/binding-relay.jsonl".to_string()),
            input_fifo_path: None,
            session_id: Some("binding-session".to_string()),
            last_offset: 0,
            relay_last_offset: None,
        };
        let (bound_output_path, bound_session_id) = resolve_bound_selector(
            Some("/tmp/inflight.jsonl"),
            Some("inflight-session"),
            Some(&binding),
        );
        assert_eq!(bound_output_path.as_deref(), Some("/tmp/inflight.jsonl"));
        assert_eq!(bound_session_id.as_deref(), Some("inflight-session"));
    }

    /// #4408 phase-2 (I1) case 2 (binding-only): with no inflight row the bind
    /// falls back to the in-memory runtime binding's `relay_output_path`/
    /// `session_id`. This is the m5 mutation target — deleting the server-side
    /// binding fallback in `resolve_bound_selector` collapses B to `None` here
    /// and this assertion FAILs.
    #[test]
    fn bound_selector_falls_back_to_runtime_binding() {
        let binding = TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: "/tmp/binding-primary.jsonl".to_string(),
            relay_output_path: Some("/tmp/binding-relay.jsonl".to_string()),
            input_fifo_path: None,
            session_id: Some("binding-session".to_string()),
            last_offset: 0,
            relay_last_offset: None,
        };
        let (bound_output_path, bound_session_id) =
            resolve_bound_selector(None, None, Some(&binding));
        assert_eq!(
            bound_output_path.as_deref(),
            Some("/tmp/binding-relay.jsonl")
        );
        assert_eq!(bound_session_id.as_deref(), Some("binding-session"));
    }

    /// #4408 phase-2 (I1) case 3 (neither): with no inflight row and no runtime
    /// binding both fields are `None`, so `skip_serializing_if` omits them from
    /// the `watcher-state` JSON and the watchdog reads B as absent (fail-closed).
    #[test]
    fn bound_selector_absent_when_no_source_and_omitted_from_json() {
        #[derive(serde::Serialize)]
        struct BoundFieldsProbe {
            #[serde(skip_serializing_if = "Option::is_none")]
            bound_output_path: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            bound_session_id: Option<String>,
        }

        let (bound_output_path, bound_session_id) = resolve_bound_selector(None, None, None);
        assert!(bound_output_path.is_none());
        assert!(bound_session_id.is_none());
        let omitted = serde_json::to_value(BoundFieldsProbe {
            bound_output_path,
            bound_session_id,
        })
        .expect("serialize omitted bound selector");
        assert!(omitted.get("bound_output_path").is_none());
        assert!(omitted.get("bound_session_id").is_none());

        // A resolved value IS emitted under the same attribute (blank-guarded).
        let (bound_output_path, bound_session_id) =
            resolve_bound_selector(Some("/tmp/live.jsonl"), Some("   "), None);
        let emitted = serde_json::to_value(BoundFieldsProbe {
            bound_output_path,
            bound_session_id,
        })
        .expect("serialize present bound selector");
        assert_eq!(
            emitted.get("bound_output_path").and_then(|v| v.as_str()),
            Some("/tmp/live.jsonl")
        );
        // A whitespace-only session id is treated as absent, not an empty bind.
        assert!(emitted.get("bound_session_id").is_none());
    }

    #[test]
    fn enriched_tmux_identity_precedes_mailbox_fallback() {
        assert_eq!(
            authoritative_tmux_session(Some("inflight-owner"), Some("token-fallback")),
            Some("inflight-owner".to_string())
        );
        assert_eq!(
            authoritative_tmux_session(None, Some("token-fallback")),
            Some("token-fallback".to_string())
        );
    }

    #[tokio::test]
    async fn provider_scoped_snapshot_timeout_does_not_fallback_to_provider_scan() {
        let registry = HealthRegistry::new();
        let shared = crate::services::discord::make_shared_data_for_tests();
        registry
            .register(ProviderKind::Codex.as_str().to_string(), shared.clone())
            .await;

        let channel = ChannelId::new(42);
        let token = Arc::new(CancelToken::new());
        assert!(
            crate::services::discord::mailbox_try_start_turn(
                shared.as_ref(),
                channel,
                token,
                UserId::new(1),
                MessageId::new(2),
            )
            .await,
            "test mailbox turn should make fallback provider scan look engaged"
        );

        let _settings_guard = shared.settings.write().await;
        let snapshot = registry
            .snapshot_watcher_state_for_provider(&ProviderKind::Codex, channel.get())
            .await;

        assert!(
            snapshot.is_none(),
            "provider/channel resolve timeout must not scan a possibly wrong same-provider runtime"
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn mailbox_snapshot_absent_channel_is_peek_only_for_health() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().expect("temp runtime root");
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        let registry = HealthRegistry::new();
        let shared = crate::services::discord::make_shared_data_for_tests();
        registry
            .register(ProviderKind::Codex.as_str().to_string(), shared.clone())
            .await;

        let channel = ChannelId::new(NEXT_ABSENT_MAILBOX_CHANNEL.fetch_add(1, Ordering::Relaxed));
        assert!(
            crate::services::discord::ChannelMailboxRegistry::global_handle(channel).is_none(),
            "test channel should start without a process-global mailbox"
        );

        let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel).await;
        assert!(snapshot.cancel_token.is_none());
        assert!(snapshot.active_user_message_id.is_none());
        assert!(snapshot.intervention_queue.is_empty());
        assert!(
            crate::services::discord::ChannelMailboxRegistry::global_handle(channel).is_none(),
            "snapshotting an absent mailbox must not create or globalize one"
        );

        let watcher = registry
            .snapshot_watcher_state_for_provider(&ProviderKind::Codex, channel.get())
            .await;
        assert!(
            watcher.is_none(),
            "health watcher-state for an absent mailbox/session should report absence"
        );
        assert!(
            crate::services::discord::ChannelMailboxRegistry::global_handle(channel).is_none(),
            "health observation must not materialize a mailbox"
        );

        let health = build_health_snapshot(&registry).await;
        assert!(
            health.mailboxes.is_empty(),
            "health snapshot should tolerate providers with no mailbox entries"
        );
        assert!(
            crate::services::discord::ChannelMailboxRegistry::global_handle(channel).is_none(),
            "health snapshot construction must remain peek-only for absent channels"
        );
    }
}
