use poise::serenity_prelude::ChannelId;
use serde::Serialize;

use super::liveness_authority::CaptureCoordinateObservation;
use super::mailbox::MailboxHealthSnapshot;
use super::provider_probe::{self, ProviderHealthSnapshot};
// #5071 T4-B6: `health::reachability` is `#[cfg(unix)]` (see the `mod` decl in
// `health.rs`), so the composition wiring built on it — this import,
// `composite_governs_polarity`, `observe_relay_verdict`, the
// `RelayVerdictReport` published on the mailbox entry, and
// `apply_relay_verdict_polarity` — carries the same gate. Windows keeps the
// pre-B6 snapshot: no composed verdict, no polarity from one.
#[cfg(unix)]
use super::reachability::composite::{
    RelayVerdict, RelayVerdictProbe, RelayVerdictReport, observe_relay_verdict,
    relay_verdict_source,
};
#[cfg(unix)]
use super::reachability::verdict::ReachabilityVerdict;
use super::redaction;
use super::session_enrichment::{self, SessionEnrichment};
use super::stall_verdict;
use super::transcript_binding_stall::{self, resolve_bound_selector};
use super::unpaired_active_token;
use super::unpaired_active_token::{RelayHealthBuildInput, build_relay_health_snapshot};
use super::{BotTokenReloadScopes, HealthRegistry, bot_token_reload_scopes};
use crate::services::discord;
use crate::services::discord::SharedData;
use crate::services::discord::relay_health::{
    FrontierProvenanceReport, RelayActiveTurn, RelayHealthSnapshot, RelayStallClassifier,
    RelayStallState,
};
use crate::services::discord::relay_recovery::authority_observation::{
    self, RelayAuthorityObservationReport,
};
use crate::services::discord::relay_recovery::cohort::{self, RelayAuthorityRolloutReport};
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
#[derive(Clone, Serialize)]
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
    /// #5188 (R5/R6): `"none"`, or why a delivery binding is pointed at a
    /// transcript that stopped growing. See
    /// [`super::transcript_binding_stall::TranscriptBindingStall`].
    pub transcript_binding_stall: &'static str,
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
    /// #5464 T5 S5: fresh, read-only ledger verdict computed with this snapshot.
    /// The field stays live in unit-test builds so consumers and the axis-B
    /// observer share one snapshot shape; producer wiring has separate coverage.
    #[cfg(unix)]
    #[serde(skip)]
    pub(in crate::services::discord) reachability_observation: Option<(ReachabilityVerdict, i64)>,
    /// #1455: Pure relay-stall classifier output derived from the nested
    /// relay-health snapshot. Read-only diagnostic; no recovery behavior is
    /// triggered from this value.
    pub(in crate::services::discord) relay_stall_state: RelayStallState,
    /// #1455: Focused relay-health model shared with the detailed health
    /// endpoint and future recovery/UI code.
    pub(in crate::services::discord) relay_health: RelayHealthSnapshot,
}

impl WatcherStateSnapshot {
    #[cfg(unix)]
    pub(in crate::services::discord) fn reachability_observation(
        &self,
    ) -> Option<(&ReachabilityVerdict, i64)> {
        self.reachability_observation
            .as_ref()
            .map(|(verdict, observed_at_ms)| (verdict, *observed_at_ms))
    }
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
    /// #5464 T5 S1: live position of the AC2-R relay-authority dial (mode,
    /// cohort width, fingerprint).
    ///
    /// Detail-only, by the same rule the mailbox probes follow: the public
    /// `/api/health` payload is an allowlist an operator dashboard depends on,
    /// and a rollout dial is triage data, not a liveness signal. `None` on the
    /// public build keeps the key absent rather than publishing a null.
    #[serde(skip_serializing_if = "Option::is_none")]
    relay_authority_rollout: Option<RelayAuthorityRolloutReport>,
    /// #5464 T5 S2: axis-A old/new observation triage — cumulative counters
    /// plus the last 16 turns per channel. Detail-only for the same reason the
    /// dial above is, and a triage sample rather than the AC3 promotion gate,
    /// which reads the JSONL event log instead (design §5.3).
    #[serde(skip_serializing_if = "Option::is_none")]
    relay_authority_observation: Option<RelayAuthorityObservationReport>,
    /// #5464 T5 S5: detail-only axis-B triage. The public health response is an
    /// allowlist, so absence is serialized as no key rather than JSON `null`.
    #[cfg(unix)]
    #[serde(skip_serializing_if = "Option::is_none")]
    axis_b_observation: Option<crate::services::discord::relay_recovery::AxisBObservationReport>,
}

impl DiscordHealthSnapshot {
    pub fn status(&self) -> HealthStatus {
        self.status
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct RelayThreadProofSnapshot {
    pub(super) parent_channel_id: Option<u64>,
    pub(super) thread_channel_id: Option<u64>,
    pub(super) stale_thread_proof: bool,
}

impl RelayThreadProofSnapshot {
    /// #5071 relay-tail S1 (I-4): the OTHER end of this channel's parent/thread
    /// axis, if it has one.
    ///
    /// Either field can hold it — `dispatch.thread_parents` is keyed parent →
    /// thread, so a PARENT resolves its thread through the lookup and a THREAD
    /// resolves its parent through the reverse scan over the values. For a
    /// channel that is one end of one axis exactly one field is populated —
    /// the scope this reader was written for. r2 review (legB P2): nothing
    /// here proves a channel cannot be a key AND a value, so when both are
    /// populated the array order below decides it, taking the THREAD side.
    /// The polled channel itself is filtered out so a self-edge can never
    /// read as an axis.
    fn counterpart_channel_id(&self, polled_channel_id: u64) -> Option<u64> {
        [self.thread_channel_id, self.parent_channel_id]
            .into_iter()
            .flatten()
            .find(|counterpart| *counterpart != polled_channel_id)
    }
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

fn authoritative_tmux_session(
    enriched_session: Option<&str>,
    mailbox_cancel_session: Option<&str>,
) -> Option<String> {
    enriched_session
        .or(mailbox_cancel_session)
        .map(str::to_string)
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
                return watcher_state_snapshot_for_shared(
                    provider.as_str(),
                    shared,
                    channel,
                    self.started_at_unix(),
                )
                .await;
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
        watcher_state_snapshot_for_shared(
            provider.as_str(),
            shared,
            ChannelId::new(channel_id),
            self.started_at_unix(),
        )
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
                self.started_at_unix(),
            )
            .await
            {
                return Some(snapshot);
            }
        }
        None
    }
}

#[cfg(unix)]
struct RelayVerdictProbeOperands {
    pane_idle_confirmed: bool,
    rowless_active_turn: bool,
    placeholder_present: bool,
    now_epoch_ms: u64,
    process_started_at_epoch_ms: u64,
}

/// Preserve the reachability probe's three-valued tail evidence. A missing
/// `unread_bytes` is not a proven drained tail for a row-backed turn; only the
/// separately witnessed rowless case may use pane-idle as positive evidence.
/// Do not collapse this to `unwrap_or(0) == 0`: the unmeasured-tail mutation is
/// pinned by `call_site_withholds_the_pane_idle_witness_for_an_unmeasured_tail`.
#[cfg(unix)]
fn relay_verdict_probe_operands(
    pane_alive: bool,
    relay_health: &RelayHealthSnapshot,
    rowless_active_turn: bool,
    process_started_at_unix: i64,
) -> RelayVerdictProbeOperands {
    RelayVerdictProbeOperands {
        pane_idle_confirmed: pane_alive
            && matches!(relay_health.active_turn, RelayActiveTurn::None)
            && relay_health.idle_witness_tail_is_not_waiting(),
        rowless_active_turn,
        placeholder_present: relay_health.pending_discord_callback_msg_id.is_some(),
        now_epoch_ms: chrono::Utc::now().timestamp_millis().max(0) as u64,
        process_started_at_epoch_ms: process_started_at_unix.max(0).saturating_mul(1_000) as u64,
    }
}

async fn watcher_state_snapshot_for_shared(
    provider_name: &str,
    shared: std::sync::Arc<SharedData>,
    channel: ChannelId,
    process_started_at_unix: i64,
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
    let unpaired_active_token_reconfirmed = unpaired_active_token::reconfirm(
        &shared,
        provider_kind.as_ref(),
        channel,
        &mailbox_snapshot,
        session.inflight_state_present,
    )
    .await;
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
        unpaired_active_token_reconfirmed,
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
    let transcript_binding_stall = transcript_binding_stall::resolve_transcript_binding_stall(
        provider_name,
        authoritative_tmux_session.as_deref(),
        bound_output_path.as_deref(),
        bound_session_id.as_deref(),
        session.attached,
        tmux_session_alive,
        session.inflight_state_present,
    );
    // #5071 T4-B4 (4987 S4): compare the row's transcript coordinate against
    // the registry's independently resolved one by FILE IDENTITY. This does
    // NOT replace T4-B0's path-string record in `SessionEnrichment`: that one
    // is untouched and has already fired for this poll inside
    // `SessionEnrichment::load`, through its
    // `record_transcript_source_divergence`, so on a split the two records
    // coexist until a follow-up slice retires B0's. Descriptive record only —
    // the outcome feeds no verdict, no classifier, and no recovery here;
    // T4-B6 owns composition. Unix-gated
    // with the reachability tree because identity is the `(dev, ino)` pair.
    #[cfg(unix)]
    super::reachability::divergence::observe_row_coordinate_divergence(
        provider_name,
        channel.get(),
        session
            .inflight
            .as_ref()
            .and_then(|state| state.output_path.as_deref()),
        session.watcher_output_path.as_deref(),
    );
    #[cfg(unix)]
    let reachability_observation = {
        let operands = relay_verdict_probe_operands(
            tmux_session_alive == Some(true),
            &relay_health,
            unpaired_active_token_reconfirmed,
            process_started_at_unix,
        );
        Some((
            observe_relay_verdict(RelayVerdictProbe {
                provider: provider_kind.as_ref(),
                channel_id: channel.get(),
                row_output_path: session
                    .inflight
                    .as_ref()
                    .and_then(|state| state.output_path.as_deref()),
                registry_output_path: session.watcher_output_path.as_deref(),
                pane_idle_confirmed: operands.pane_idle_confirmed,
                rowless_active_turn: operands.rowless_active_turn,
                placeholder_present: operands.placeholder_present,
                now_epoch_ms: operands.now_epoch_ms,
                process_started_at_epoch_ms: operands.process_started_at_epoch_ms,
            })
            .in_band()
            .clone(),
            operands.now_epoch_ms.min(i64::MAX as u64) as i64,
        ))
    };
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
        transcript_binding_stall: transcript_binding_stall.as_str(),
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
        #[cfg(unix)]
        reachability_observation,
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
    // #5071 T4-B6: read the 4987 §5.1 switch once per snapshot, so every entry
    // in one response answers under the same authority even if the live config
    // is edited mid-poll.
    #[cfg(unix)]
    let composite_governs_polarity = relay_verdict_source().governs_health_polarity();

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
                // #5071 relay-tail S1 (I-4): the counterpart's coordinate is
                // read here, where the axis is already resolved, and published
                // beside this channel's own pair as raw evidence. It feeds NO
                // hypothesis — H3 is deferred past S1 (design §1.4, §9's S1
                // row) and the r1 review measured the cost of letting it decide
                // early. One more lock-free `.get()` on the same in-memory map;
                // `None` when this channel is not part of a parent/thread pair.
                let counterpart_coord_observation = relay_thread_proof
                    .counterpart_channel_id(channel.get())
                    .map(|counterpart| {
                        session_enrichment::observe_coord_frontier(
                            &entry.shared,
                            ChannelId::new(counterpart),
                        )
                    });
                let frontier_provenance = FrontierProvenanceReport::of(
                    session.frontier_provenance,
                    counterpart_coord_observation,
                );
                let active_turn = relay_active_turn_from_inflight(
                    mailbox_has_cancel_token,
                    session.inflight.as_ref(),
                );
                let unpaired_active_token_reconfirmed = unpaired_active_token::reconfirm(
                    &entry.shared,
                    provider_kind.as_ref(),
                    channel,
                    snapshot,
                    session.inflight_state_present,
                )
                .await;
                let relay_health = build_relay_health_snapshot(RelayHealthBuildInput {
                    provider: entry.name.clone(),
                    channel_id: channel.get(),
                    mailbox_has_cancel_token,
                    mailbox_active_user_msg_id,
                    mailbox_turn_started_at_ms: snapshot
                        .turn_started_at
                        .map(|started_at| started_at.timestamp_millis()),
                    unpaired_active_token_reconfirmed,
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
                // #5071 T4-B6 (4987 §4.3/§4.4): compose the relay verdict from
                // the durable T4-B2c ledger, the T4-B3 receipt projection and
                // the T4-B5 sidecar, and publish it. Whether it may also change
                // this entry's polarity is the `RelayVerdictSource` switch,
                // read once per poll below. The row's `output_path` is passed
                // to the divergence comparison only, matching the descriptive
                // call above it; nothing here resolves or tails through it.
                #[cfg(unix)]
                let relay_verdict = {
                    // The detail poll's `tmux_present` witness is intentionally
                    // weaker than the recovery snapshot's has-session probe. The
                    // explicit helper operand keeps that semantic difference
                    // visible instead of silently forking the remaining inputs.
                    let operands = relay_verdict_probe_operands(
                        tmux_present,
                        &relay_health,
                        unpaired_active_token_reconfirmed,
                        registry.started_at_unix(),
                    );
                    observe_relay_verdict(RelayVerdictProbe {
                        provider: provider_kind.as_ref(),
                        channel_id: channel.get(),
                        row_output_path: session
                            .inflight
                            .as_ref()
                            .and_then(|state| state.output_path.as_deref()),
                        registry_output_path: session.watcher_output_path.as_deref(),
                        pane_idle_confirmed: operands.pane_idle_confirmed,
                        rowless_active_turn: operands.rowless_active_turn,
                        placeholder_present: operands.placeholder_present,
                        now_epoch_ms: operands.now_epoch_ms,
                        process_started_at_epoch_ms: operands.process_started_at_epoch_ms,
                    })
                };
                #[cfg(unix)]
                apply_relay_verdict_polarity(
                    composite_governs_polarity,
                    &relay_verdict,
                    &entry.name,
                    channel.get(),
                    &mut degraded_reasons,
                    &mut status,
                );
                #[cfg(unix)]
                let reachability =
                    RelayVerdictReport::of(&relay_verdict, composite_governs_polarity);
                mailbox_entries.push(MailboxHealthSnapshot {
                    provider: entry.name.clone(),
                    channel_id: channel.get(),
                    has_cancel_token: mailbox_has_cancel_token,
                    queue_depth,
                    recovery_started: snapshot.recovery_started_at.is_some(),
                    active_request_owner: snapshot.active_request_owner.map(|id| id.get()),
                    active_user_message_id: mailbox_active_user_msg_id,
                    agent_turn_status: super::mailbox::mailbox_agent_turn_status(
                        mailbox_has_cancel_token,
                        super::mailbox::residual_occupancy(&entry.shared, channel, &snapshot),
                    ),
                    watcher_attached: session.watcher_attached,
                    inflight_state_present: session.inflight_state_present,
                    tmux_present,
                    process_present,
                    active_dispatch_present: session.active_dispatch_present(),
                    stall_shadow_verdict,
                    #[cfg(unix)]
                    reachability,
                    frontier_provenance,
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
        relay_authority_rollout: include_mailbox_details.then(cohort::rollout_report),
        relay_authority_observation: include_mailbox_details
            .then(authority_observation::observation_report),
        #[cfg(unix)]
        axis_b_observation: include_mailbox_details
            .then(crate::services::discord::relay_recovery::axis_b_observation_report),
    }
}

/// The only place 4987 §5.1's switch changes a snapshot's polarity (#5071 T4-B6).
///
/// The switch is also visible in the response as
/// `RelayVerdictReport::governs_health_polarity`, published on the mailbox entry
/// in both modes; what it does NOT do anywhere else is change the aggregate the
/// caller reads as the process's health. Under `Structural` this returns having
/// touched neither output — that is what makes the shadow mode a shadow.
///
/// One reason per non-green CHANNEL, not per provider: the channel is what an
/// operator has to look at, and the same response already carries one mailbox
/// entry per channel. `Degraded`, never `Unhealthy`: 4987 §4.4 asks a
/// non-`Reachable` relay to set the degraded flag, and taking the process out of
/// HTTP readiness is authority this switch was not given.
///
/// Split out of the per-channel loop so the switch has a seam a test can call.
/// The polarity is the whole of what T4-B6 granted the composed verdict, and
/// before this it was reachable only by building a full `HealthRegistry`, which
/// left both the `composite_governs_polarity` conjunct and the direction of the
/// `permits_health` test unpinned.
#[cfg(unix)]
fn apply_relay_verdict_polarity(
    composite_governs_polarity: bool,
    relay_verdict: &RelayVerdict,
    provider: &str,
    channel_id: u64,
    degraded_reasons: &mut Vec<String>,
    status: &mut HealthStatus,
) {
    if composite_governs_polarity && !relay_verdict.permits_health() {
        degraded_reasons.push(format!(
            "relay_verdict_{}_{provider}_{channel_id}",
            relay_verdict.label(),
        ));
        *status = status.worsen(HealthStatus::Degraded);
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
        build_public_health_snapshot, rebind_origin_inflight_is_idle,
        relay_active_turn_from_inflight, resolve_bound_selector,
    };
    // #5071 T4-B6: the polarity tests below name the `#[cfg(unix)]` reachability
    // tree, so they are gated with the seam they exercise. `HealthStatus` rides
    // the same gate because those tests are its only readers here.
    #[cfg(unix)]
    use super::{
        HealthStatus, RelayVerdict, apply_relay_verdict_polarity, relay_verdict_probe_operands,
    };
    use crate::services::agent_protocol::RuntimeHandoffKind;
    #[cfg(unix)]
    use crate::services::discord::health::reachability::composite::compose_relay_verdict;
    #[cfg(unix)]
    use crate::services::discord::health::reachability::external_verdict::ExternalRelayVerdict;
    #[cfg(unix)]
    use crate::services::discord::health::reachability::verdict::{
        ReachabilityUnknownReason, ReachabilityVerdict,
    };
    use crate::services::discord::inflight::InflightTurnState;
    use crate::services::discord::relay_health::{RelayActiveTurn, RelayHealthSnapshot};
    use crate::services::provider::{CancelToken, ProviderKind};
    use crate::services::tui_prompt_dedupe::TuiRuntimeBinding;
    use chrono::TimeZone;

    const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";
    static NEXT_ABSENT_MAILBOX_CHANNEL: AtomicU64 = AtomicU64::new(9_406_800_000_000);

    struct EnvGuard;

    #[cfg(unix)]
    #[test]
    fn relay_verdict_probe_operands_preserve_process_start_and_pane_semantics() {
        let relay_health = RelayHealthSnapshot {
            provider: "codex".to_string(),
            channel_id: 54_640,
            active_turn: RelayActiveTurn::None,
            tmux_session: None,
            tmux_alive: None,
            watcher_attached: false,
            watcher_attached_stale: false,
            watcher_owner_channel_id: None,
            watcher_owns_live_relay: false,
            bridge_inflight_present: false,
            bridge_current_msg_id: None,
            mailbox_has_cancel_token: false,
            mailbox_active_user_msg_id: None,
            mailbox_turn_started_at_ms: None,
            mailbox_turn_age_secs: None,
            queue_depth: 0,
            pending_discord_callback_msg_id: Some(7),
            pending_thread_proof: false,
            parent_channel_id: None,
            thread_channel_id: None,
            last_relay_ts_ms: None,
            last_relay_age_secs: None,
            last_outbound_activity_ms: None,
            last_capture_offset: None,
            last_relay_offset: 0,
            unread_bytes: None,
            desynced: false,
            stale_thread_proof: false,
            unpaired_active_token_reconfirmed: false,
        };
        let operands = relay_verdict_probe_operands(true, &relay_health, true, 1_725_000_123);
        assert_eq!(operands.process_started_at_epoch_ms, 1_725_000_123_000);
        assert!(operands.pane_idle_confirmed);
        assert!(operands.rowless_active_turn);
        assert!(operands.placeholder_present);
        assert!(!relay_verdict_probe_operands(false, &relay_health, false, 1).pane_idle_confirmed);
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
        }
    }

    /// #5464 T5 S1: the rollout dial rides the detail axis only, and it reports
    /// the dormant position with no operator config loaded.
    ///
    /// Both halves matter. Publishing on the public build would put a rollout
    /// knob into the allowlisted payload an unauthenticated dashboard reads;
    /// omitting it from the detail build would leave the S2 observation slice
    /// with no live way to confirm which dial a node is answering under.
    #[tokio::test]
    async fn relay_authority_rollout_is_published_on_the_detail_build_only() {
        let registry = HealthRegistry::new();

        let public = serde_json::to_value(build_public_health_snapshot(&registry).await)
            .expect("serialize public snapshot");
        assert!(
            public.get("relay_authority_rollout").is_none(),
            "the rollout dial must not reach the public health allowlist"
        );

        let detail = serde_json::to_value(build_health_snapshot(&registry).await)
            .expect("serialize detail snapshot");
        let rollout = detail
            .get("relay_authority_rollout")
            .expect("detail health publishes the rollout dial");
        assert_eq!(rollout.get("mode").and_then(|v| v.as_str()), Some("legacy"));
        assert_eq!(
            rollout.get("cohort_percent").and_then(|v| v.as_u64()),
            Some(0)
        );
        assert_eq!(
            rollout.get("cohort_fingerprint").and_then(|v| v.as_str()),
            Some(
                crate::services::discord::relay_recovery::cohort::cohort_fingerprint(
                    crate::config::RelayAuthorityMode::Legacy,
                    0,
                )
                .as_str()
            )
        );
    }

    /// #5464 T5 S2: the axis-A observation block rides the same detail axis as
    /// the S1 dial above, and publishes the whole triage shape.
    ///
    /// The counters are process-cumulative, so this pins the shape and the
    /// detail gate rather than the values — any other test in this binary that
    /// records an observation would move them, and a value assertion here would
    /// turn that into a spurious ordering dependency.
    #[tokio::test]
    async fn relay_authority_observation_is_published_on_the_detail_build_only() {
        let registry = HealthRegistry::new();

        let public = serde_json::to_value(build_public_health_snapshot(&registry).await)
            .expect("serialize public snapshot");
        assert!(
            public.get("relay_authority_observation").is_none(),
            "observation triage must not reach the public health allowlist"
        );
        #[cfg(unix)]
        assert!(
            public.get("axis_b_observation").is_none(),
            "axis-B triage must not reach the public health allowlist"
        );

        let detail = serde_json::to_value(build_health_snapshot(&registry).await)
            .expect("serialize detail snapshot");
        let observation = detail
            .get("relay_authority_observation")
            .expect("detail health publishes the observation block");
        let mut keys: Vec<&str> = observation
            .as_object()
            .expect("the observation block is a JSON object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "channels",
                "completion_scopes",
                "completion_sink_dropped_records",
                "completion_suppressions",
                "new_stricter_verdicts",
                "resident_buffers",
                "rowless_continuations",
                "sink_dropped_records",
                "stream_diff_ticks",
                "turns_recorded",
            ]
        );
        assert_eq!(
            observation
                .get("new_stricter_verdicts")
                .and_then(serde_json::Value::as_u64),
            Some(0),
            "AC2-R's monotone-relaxing alarm counter must be zero in this process"
        );
        #[cfg(unix)]
        {
            let axis_b = detail
                .get("axis_b_observation")
                .and_then(serde_json::Value::as_object)
                .expect("detail health publishes the independent axis-B producer block");
            assert!(axis_b.contains_key("dropped_records"));
            assert!(axis_b.contains_key("write_failures"));
        }
    }

    #[cfg(unix)]
    const POLARITY_PROVIDER: &str = "claude";
    #[cfg(unix)]
    const POLARITY_CHANNEL: u64 = 4_987_000_000_071;

    /// An `Unknown{TranscriptUnresolved}` composed with a sidecar that said
    /// nothing: 4987 §4.1's "an unobservable relay is not a healthy one", and
    /// the plainest verdict that does not permit health.
    #[cfg(unix)]
    fn not_green() -> RelayVerdict {
        let composed = compose_relay_verdict(
            ReachabilityVerdict::unknown(ReachabilityUnknownReason::TranscriptUnresolved, 0),
            ExternalRelayVerdict::Unknown,
        );
        assert!(!composed.permits_health());
        composed
    }

    /// #5071 T4-B6: the switch's one effect, locked from both sides.
    ///
    /// The same verdict is pushed through `apply_relay_verdict_polarity` in both
    /// modes and only `Composite` may move the snapshot. Dropping the
    /// `composite_governs_polarity` conjunct makes the `Structural` half fail —
    /// which is the mutation the r1 review ran green against every gate before
    /// this test existed.
    #[cfg(unix)]
    #[test]
    fn only_composite_mode_degrades_the_snapshot_for_a_non_green_relay_verdict() {
        let verdict = not_green();

        let mut composite_reasons = Vec::new();
        let mut composite_status = HealthStatus::Healthy;
        apply_relay_verdict_polarity(
            true,
            &verdict,
            POLARITY_PROVIDER,
            POLARITY_CHANNEL,
            &mut composite_reasons,
            &mut composite_status,
        );
        assert_eq!(
            composite_reasons,
            vec![format!(
                "relay_verdict_unknown_{POLARITY_PROVIDER}_{POLARITY_CHANNEL}"
            )],
            "Composite must name the non-green channel in the degraded reasons"
        );
        assert_eq!(composite_status, HealthStatus::Degraded);

        let mut shadow_reasons = Vec::new();
        let mut shadow_status = HealthStatus::Healthy;
        apply_relay_verdict_polarity(
            false,
            &verdict,
            POLARITY_PROVIDER,
            POLARITY_CHANNEL,
            &mut shadow_reasons,
            &mut shadow_status,
        );
        assert!(
            shadow_reasons.is_empty(),
            "Structural is a shadow: the same verdict must move no reason, got {shadow_reasons:?}"
        );
        assert_eq!(shadow_status, HealthStatus::Healthy);
    }

    /// The other direction of the same conditional: `Composite` degrades on a
    /// verdict that does NOT permit health, so a verdict that does must leave
    /// the snapshot alone. Without this, inverting the `permits_health` test
    /// still passes the case above.
    #[cfg(unix)]
    #[test]
    fn a_green_relay_verdict_degrades_the_snapshot_in_neither_mode() {
        let green =
            compose_relay_verdict(ReachabilityVerdict::Reachable, ExternalRelayVerdict::NoLoss);
        assert!(green.permits_health());

        for composite_governs_polarity in [false, true] {
            let mut reasons = Vec::new();
            let mut status = HealthStatus::Healthy;
            apply_relay_verdict_polarity(
                composite_governs_polarity,
                &green,
                POLARITY_PROVIDER,
                POLARITY_CHANNEL,
                &mut reasons,
                &mut status,
            );
            assert!(
                reasons.is_empty(),
                "a health-permitting verdict degraded nothing to report, got {reasons:?}"
            );
            assert_eq!(status, HealthStatus::Healthy);
        }
    }

    #[cfg(unix)]
    #[test]
    fn exact_unreachable_verdict_reaches_the_polarity_boundary() {
        let unreachable = compose_relay_verdict(
            ReachabilityVerdict::Unreachable {
                oldest_unsatisfied_age_secs: 900,
                uncovered_ranges: 4,
            },
            ExternalRelayVerdict::NoLoss,
        );
        assert!(!unreachable.permits_health());

        for (composite_governs_polarity, expected_status) in [
            (false, HealthStatus::Healthy),
            (true, HealthStatus::Degraded),
        ] {
            let mut reasons = Vec::new();
            let mut status = HealthStatus::Healthy;
            apply_relay_verdict_polarity(
                composite_governs_polarity,
                &unreachable,
                POLARITY_PROVIDER,
                POLARITY_CHANNEL,
                &mut reasons,
                &mut status,
            );
            assert_eq!(status, expected_status);
            assert_eq!(reasons.is_empty(), !composite_governs_polarity);
        }
    }

    /// The worsen is a floor, not an assignment: a snapshot already `Unhealthy`
    /// for an unrelated reason (no providers registered, say) must not be
    /// improved to `Degraded` by a non-green relay verdict landing after it.
    #[cfg(unix)]
    #[test]
    fn a_non_green_relay_verdict_never_improves_an_unhealthy_snapshot() {
        let mut reasons = vec!["no_providers_registered".to_string()];
        let mut status = HealthStatus::Unhealthy;
        apply_relay_verdict_polarity(
            true,
            &not_green(),
            POLARITY_PROVIDER,
            POLARITY_CHANNEL,
            &mut reasons,
            &mut status,
        );
        assert_eq!(status, HealthStatus::Unhealthy);
        assert_eq!(
            reasons.len(),
            2,
            "the relay reason is added, not substituted"
        );
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

    /// #5071 relay-tail S2 (r4 review): the tail term's polarity as the LIVE
    /// health path answers it, read off the surface that path publishes.
    ///
    /// The r3 review measured the r2 version of this test evaluating a COPY of
    /// the call site's conjunction, so reverting the call site alone to the
    /// pre-S2 `unread_bytes.unwrap_or(0) == 0` fold left it green. This one
    /// calls `build_health_snapshot` and reads `reachability` off the mailbox
    /// entry, which is where `pane_idle_confirmed` becomes observable from
    /// outside the per-channel loop: it is `observe_relay_verdict`'s third alive
    /// witness, and with a bootstrapped ledger holding no obligation and a
    /// transcript no longer than that ledger's `last_observed_len`,
    /// `transcript_liveness`'s `alive` IS that term — so `reachable` versus
    /// `incarnation_not_alive_no_obligations` on the published surface reports
    /// the conjunction rather than restating it.
    ///
    /// Both channels build the shape the S2 split is about and that a rowless
    /// fixture cannot: a row IS present, so `bridge_inflight_present` is set,
    /// while `active_turn` still reads `None` (#3631 rebind-origin without a
    /// cancel token). They differ in whether that row carries an `output_path`
    /// for the tail measurement to land on — the first entry of the
    /// `seed_runtime` absence enumerated on `idle_witness_tail_is_not_waiting`.
    /// That also moves the descriptive divergence operand (`SameFile` versus
    /// `NoRowCoordinate`), and neither of those two produces an unknown reason,
    /// so the verdict this reads apart is still the alive question alone.
    #[cfg(unix)]
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn call_site_withholds_the_pane_idle_witness_for_an_unmeasured_tail() {
        use crate::services::discord::health::reachability::discovery::TranscriptFileId;
        use crate::services::discord::health::reachability::ledger::{
            LedgerIncarnation, bootstrap_ledger_at, ledger_path,
        };

        fn live_watcher_handle(
            tmux_session_name: &str,
            output_path: &str,
        ) -> crate::services::discord::TmuxWatcherHandle {
            crate::services::discord::TmuxWatcherHandle {
                tmux_session_name: tmux_session_name.to_string(),
                output_path: output_path.to_string(),
                paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                resume_offset: Arc::new(std::sync::Mutex::new(None)),
                cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                pause_epoch: Arc::new(AtomicU64::new(0)),
                turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(
                    crate::services::discord::tmux_watcher_now_ms(),
                )),
            }
        }

        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().expect("temp runtime root");
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;
        if !crate::services::platform::tmux::is_available() {
            eprintln!("skipping #5071 S2 call-site witness: tmux unavailable");
            return;
        }

        let provider = ProviderKind::Codex;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let registry = HealthRegistry::new();
        registry
            .register(provider.as_str().to_string(), shared.clone())
            .await;

        let mut fixtures = Vec::new();
        for (label, row_carries_output_path) in
            [("measured drained tail", true), ("unmeasured tail", false)]
        {
            let channel =
                ChannelId::new(NEXT_ABSENT_MAILBOX_CHANNEL.fetch_add(1, Ordering::Relaxed));
            // The witness's FIRST conjunct is a real live pane, so this fixture
            // owns one instead of granting the term.
            let tmux_session = format!(
                "AgentDesk-codex-s2r4-{}-{}",
                std::process::id(),
                channel.get()
            );
            let _ = crate::services::platform::tmux::kill_session(
                &tmux_session,
                "#5071 S2 call-site fixture reset",
            );
            assert!(
                crate::services::platform::tmux::create_session(&tmux_session, None, "sleep 60")
                    .expect("start tmux fixture")
                    .status
                    .success(),
                "{label}: the pane-idle witness needs a live pane to be about"
            );

            // The transcript the live watcher tails, which is the one
            // `transcript_liveness` stats. Empty, so its EOF cannot exceed the
            // ledger's `last_observed_len` and the growth half of the alive
            // question answers `false` — leaving `pane_idle_confirmed` as the
            // only thing that can still make the incarnation read alive.
            let transcript = tmp.path().join(format!("s2-r4-{}.jsonl", channel.get()));
            std::fs::write(&transcript, b"").expect("write transcript fixture");
            let transcript = transcript.to_str().expect("utf8 path").to_string();
            shared
                .tmux_watchers
                .insert(channel, live_watcher_handle(&tmux_session, &transcript));

            let mut row = InflightTurnState::new(
                provider.clone(),
                channel.get(),
                None,
                5_071_000_000_002_000,
                5_071_000_000_002_001,
                5_071_000_000_002_002,
                "s2 call-site fixture".to_string(),
                None,
                Some(tmux_session.clone()),
                row_carries_output_path.then(|| transcript.clone()),
                None,
                0,
            );
            // #3631: a rebind-origin row with no cancel token is idle. That is
            // how `active_turn` is `None` while a row is present here.
            row.rebind_origin = true;
            crate::services::discord::inflight::save_inflight_state(&row)
                .expect("persist row fixture");

            // A mailbox with no cancel token: what puts the channel into
            // `provider_probe`'s snapshots, so the per-channel loop runs for it,
            // without giving the row an active turn.
            shared.mailboxes.handle(channel);

            // No obligation and `last_observed_len == 0`: the sweep holds
            // nothing, so `classify_reachability` decides on `alive` alone.
            bootstrap_ledger_at(
                &ledger_path(&provider, channel.get()).expect("ledger path"),
                LedgerIncarnation::new(
                    tmux_session.clone(),
                    0,
                    None,
                    TranscriptFileId { dev: 0, ino: 0 },
                ),
                0,
            )
            .expect("bootstrap ledger fixture");

            fixtures.push((label, channel, tmux_session, row_carries_output_path));
        }

        let health = build_health_snapshot(&registry).await;

        let mut published = Vec::new();
        for (label, channel, _, row_carries_output_path) in &fixtures {
            let entry = health
                .mailboxes
                .iter()
                .find(|entry| entry.channel_id == channel.get())
                .unwrap_or_else(|| panic!("{label}: the fixture channel must reach the snapshot"));
            assert!(
                entry.tmux_present && entry.relay_health.tmux_alive == Some(true),
                "{label}: the fixture pane must be the one the snapshot probed"
            );
            assert!(
                entry.relay_health.bridge_inflight_present,
                "{label}: the fixture row must be the one the snapshot observed"
            );
            assert_eq!(
                entry.relay_health.active_turn,
                RelayActiveTurn::None,
                "{label}: a rebind-origin row without a cancel token must read idle"
            );
            assert_eq!(
                entry.relay_health.unread_bytes,
                row_carries_output_path.then_some(0),
                "{label}: enrichment must derive the tail reading this case is about"
            );
            published.push((
                *label,
                entry.reachability.verdict,
                entry.reachability.reason,
            ));
        }

        for (_, _, tmux_session, _) in &fixtures {
            let _ = crate::services::platform::tmux::kill_session(
                tmux_session,
                "#5071 S2 call-site fixture teardown",
            );
        }

        assert_eq!(
            published,
            vec![
                ("measured drained tail", "reachable", None),
                (
                    "unmeasured tail",
                    "unknown",
                    Some("incarnation_not_alive_no_obligations")
                ),
            ],
            "the live path's witness must survive a measured-empty tail and be withheld for an unmeasured one"
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

    #[test]
    fn health_detail_marks_same_episode_guarded_finish_occupancy_residual() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().expect("temp runtime root");
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("health detail test runtime")
            .block_on(async {
                let registry = HealthRegistry::new();
                let shared = crate::services::discord::make_shared_data_for_tests();
                registry
                    .register(ProviderKind::Claude.as_str().to_string(), shared.clone())
                    .await;
                let channel =
                    ChannelId::new(NEXT_ABSENT_MAILBOX_CHANNEL.fetch_add(1, Ordering::Relaxed));
                let active_user_msg_id = 5_068_301;
                let token = Arc::new(CancelToken::new());
                let nonce = token.turn_nonce().expect("fresh token nonce").to_string();
                assert!(
                    crate::services::discord::mailbox_try_start_turn(
                        &shared,
                        channel,
                        token,
                        UserId::new(7),
                        MessageId::new(active_user_msg_id),
                    )
                    .await
                );
                shared.turn_finalizer.guarded_finish_residues().insert(
                    channel,
                    crate::services::discord::turn_finalizer::GuardedFinishResidue {
                        expected_user_msg_id: 5_068_300,
                        active_user_msg_id,
                        generation: 0,
                        provider: ProviderKind::Claude,
                        terminal_turn_nonce: Some(nonce.clone()),
                        active_turn_nonce: Some(nonce),
                        observed_before: std::time::Instant::now(),
                        allow_completion_cleanup: true,
                        drain_voice: true,
                        terminal_was_cancel: false,
                    },
                );

                let json = serde_json::to_value(build_health_snapshot(&registry).await)
                    .expect("serialize residual mailbox health");
                assert_eq!(json["mailboxes"][0]["agent_turn_status"], "residual");
                assert_eq!(
                    json["mailboxes"][0]["active_user_message_id"],
                    active_user_msg_id
                );
            });
    }

    /// #5071 T4-B4 (4987 S4): the divergence observe call in
    /// `watcher_state_snapshot_for_shared` is severable without any of
    /// `reachability::divergence`'s own tests failing (they are pure/local),
    /// so this drives the real builder end to end — registry binding on a live
    /// file, in-flight row on a dead path — and asserts the record it emits.
    #[cfg(unix)]
    mod row_coordinate_divergence_wiring {
        use std::sync::Arc;
        use std::sync::atomic::Ordering;

        use poise::serenity_prelude::ChannelId;

        use crate::services::provider::ProviderKind;

        #[derive(Clone)]
        struct CapturingWriter(Arc<std::sync::Mutex<Vec<u8>>>);

        impl std::io::Write for CapturingWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturingWriter {
            type Writer = CapturingWriter;

            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        /// Install a WARN-level capturing subscriber for the duration of `run`.
        /// `set_default` is thread-local and the record is emitted on the
        /// polling thread, so callers must stay on `flavor = "current_thread"`.
        async fn capture_warn<F, R>(run: F) -> (R, String)
        where
            F: std::future::Future<Output = R>,
        {
            let buffer = Arc::new(std::sync::Mutex::new(Vec::new()));
            let subscriber = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::WARN)
                .with_ansi(false)
                .without_time()
                .with_writer(CapturingWriter(buffer.clone()))
                .finish();
            let _guard = tracing::subscriber::set_default(subscriber);
            let result = run.await;
            let output = String::from_utf8_lossy(
                &buffer.lock().unwrap_or_else(|poison| poison.into_inner()),
            )
            .into_owned();
            (result, output)
        }

        fn watcher_handle(
            tmux_session_name: &str,
            output_path: &str,
        ) -> crate::services::discord::TmuxWatcherHandle {
            crate::services::discord::TmuxWatcherHandle {
                tmux_session_name: tmux_session_name.to_string(),
                output_path: output_path.to_string(),
                paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                resume_offset: Arc::new(std::sync::Mutex::new(None)),
                cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(
                    crate::services::discord::tmux_watcher_now_ms(),
                )),
            }
        }

        #[tokio::test(flavor = "current_thread")]
        async fn watcher_state_snapshot_records_row_vs_registry_identity_divergence() {
            let tmp = tempfile::tempdir().expect("temp runtime root");
            let _env =
                crate::config::TestEnvVarGuard::set_path(super::AGENTDESK_ROOT_DIR_ENV, tmp.path());

            let provider = ProviderKind::Codex;
            let channel =
                ChannelId::new(super::NEXT_ABSENT_MAILBOX_CHANNEL.fetch_add(1, Ordering::Relaxed));
            let tmux_session_name = "AgentDesk-codex-adk-t4b4-wiring";
            let shared = crate::services::discord::make_shared_data_for_tests();

            // #4986 형상1 fixture: the registry tails a live native transcript
            // while the row still names a path that does not stat.
            let native = tmp.path().join("t4b4-registry-native.jsonl");
            std::fs::write(&native, "{\"type\":\"assistant\"}\n").expect("write registry fixture");
            shared.tmux_watchers.insert(
                channel,
                watcher_handle(tmux_session_name, native.to_str().expect("utf8 path")),
            );
            let missing_wrapper = tmp.path().join("t4b4-row-wrapper-missing.jsonl");
            let row = crate::services::discord::inflight::InflightTurnState::new(
                provider.clone(),
                channel.get(),
                None,
                5_071_000_000_004_000,
                5_071_000_000_004_001,
                5_071_000_000_004_002,
                "t4-b4 wiring fixture".to_string(),
                None,
                Some(tmux_session_name.to_string()),
                Some(missing_wrapper.to_str().expect("utf8 path").to_string()),
                None,
                0,
            );
            crate::services::discord::inflight::save_inflight_state(&row)
                .expect("persist row fixture");

            let (snapshot, logs) = capture_warn(super::super::watcher_state_snapshot_for_shared(
                provider.as_str(),
                shared,
                channel,
                0,
            ))
            .await;

            let snapshot = snapshot.expect("a channel with a live watcher and a row snapshots");
            assert_eq!(
                snapshot.inflight_output_path.as_deref(),
                missing_wrapper.to_str(),
                "the fixture row must be the one the builder observed"
            );
            let records: Vec<&str> = logs
                .lines()
                .filter(|line| line.contains("counter=\"reachability_row_coordinate_divergence\""))
                .collect();
            assert_eq!(
                records.len(),
                1,
                "one build must emit exactly one divergence record; got:\n{logs}"
            );
            assert!(
                records[0].contains("outcome=\"row_path_unresolvable_while_registry_live\""),
                "the record must carry the #4986 형상1 outcome; got:\n{}",
                records[0]
            );
        }
    }
}
