use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use poise::serenity_prelude as serenity;
use serde::Serialize;
use serenity::{ChannelId, CreateMessage};
use sqlx::PgPool;

use super::formatting::{build_long_message_attachment, split_message};
use super::{
    SharedData, clear_inflight_state, mailbox_cancel_active_turn, mailbox_clear_channel,
    mailbox_clear_recovery_marker, mailbox_finish_turn,
};
use crate::db::Db;
use crate::server::routes::dispatches::discord_delivery::{
    DispatchMessagePostError, DispatchMessagePostErrorKind,
};
use crate::services::discord::outbound::{
    DISCORD_HARD_LIMIT_CHARS, DISCORD_SAFE_LIMIT_CHARS, DeliveryResult, DiscordOutboundClient,
    DiscordOutboundMessage, DiscordOutboundPolicy, FallbackKind, OutboundDeduper, deliver_outbound,
};
use crate::services::provider::ProviderKind;

/// Per-provider snapshot for the health response.
struct ProviderEntry {
    name: String,
    shared: Arc<SharedData>,
}

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

/// Registry that providers register with so the unified axum API can query all of them.
/// Also holds Discord HTTP clients for agent-to-agent message routing.
pub struct HealthRegistry {
    providers: tokio::sync::Mutex<Vec<ProviderEntry>>,
    started_at: Instant,
    /// Discord HTTP clients keyed by provider name (for sending messages via correct bot)
    discord_http: tokio::sync::Mutex<Vec<(String, Arc<serenity::Http>)>>,
    /// Dedicated HTTP client for the announce bot (agent-to-agent routing).
    /// This bot's messages are accepted by all agents' allowed_bot_ids.
    announce_http: tokio::sync::Mutex<Option<Arc<serenity::Http>>>,
    /// Cached Discord user id for the announce bot.
    announce_user_id: tokio::sync::Mutex<Option<u64>>,
    /// Dedicated HTTP client for the notify bot (info-only notifications).
    /// Agents do NOT process notify bot messages — use for non-actionable alerts.
    notify_http: tokio::sync::Mutex<Option<Arc<serenity::Http>>>,
    /// Cached Discord user id for the notify bot.
    notify_user_id: tokio::sync::Mutex<Option<u64>>,
}

impl HealthRegistry {
    pub fn new() -> Self {
        Self {
            providers: tokio::sync::Mutex::new(Vec::new()),
            started_at: Instant::now(),
            discord_http: tokio::sync::Mutex::new(Vec::new()),
            announce_http: tokio::sync::Mutex::new(None),
            announce_user_id: tokio::sync::Mutex::new(None),
            notify_http: tokio::sync::Mutex::new(None),
            notify_user_id: tokio::sync::Mutex::new(None),
        }
    }

    pub(super) async fn register(&self, name: String, shared: Arc<SharedData>) {
        let mut providers = self.providers.lock().await;
        if providers.iter().any(|entry| entry.name == name) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ duplicate health provider registration ignored: {}",
                name
            );
            return;
        }
        providers.push(ProviderEntry { name, shared });
    }

    pub(super) async fn registered_provider_count(&self) -> usize {
        self.providers.lock().await.len()
    }

    pub(super) async fn register_http(&self, provider: String, http: Arc<serenity::Http>) {
        self.discord_http.lock().await.push((provider, http));
    }

    /// #896: Rebind a live tmux session to a freshly-created inflight state
    /// for the given provider/channel, routing through the provider's
    /// registered `SharedData` and Discord HTTP. Returns `None` when the
    /// provider is not registered with this dcserver (standalone mode or
    /// cross-runtime target); the HTTP handler maps that to 503. The inner
    /// `Result` carries typed failures from `rebind_inflight_for_channel`
    /// so the handler can pick the right HTTP status.
    ///
    /// Kept on the registry (rather than exposing `SharedData` directly via
    /// an accessor) so this crate does not leak the `pub(in crate::services)`
    /// `SharedData` type across the service boundary.
    pub(crate) async fn rebind_inflight(
        &self,
        provider: &crate::services::provider::ProviderKind,
        channel_id: u64,
        tmux_override: Option<String>,
    ) -> Option<Result<super::recovery_engine::RebindOutcome, super::recovery_engine::RebindError>>
    {
        let provider_name = provider.as_str().to_string();
        let shared = self
            .providers
            .lock()
            .await
            .iter()
            .find(|entry| entry.name == provider_name)
            .map(|entry| entry.shared.clone())?;
        let http = self
            .discord_http
            .lock()
            .await
            .iter()
            .find(|(name, _)| name == &provider_name)
            .map(|(_, http)| http.clone())?;
        Some(
            super::recovery_engine::rebind_inflight_for_channel(
                &http,
                &shared,
                provider,
                channel_id,
                tmux_override,
            )
            .await,
        )
    }

    /// Load announce + notify bot tokens from the canonical runtime credential path.
    /// Call once at startup before the axum server begins accepting requests.
    pub async fn init_bot_tokens(&self) {
        if super::runtime_store::agentdesk_root().is_some() {
            for (bot_name, field) in [
                ("announce", &self.announce_http),
                ("notify", &self.notify_http),
            ] {
                if let Some(token) = crate::credential::read_bot_token(bot_name) {
                    let http = Arc::new(serenity::Http::new(&format!("Bot {token}")));
                    *field.lock().await = Some(http);
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    let emoji = if bot_name == "announce" {
                        "📢"
                    } else {
                        "🔔"
                    };
                    tracing::info!("  [{ts}] {emoji} {bot_name} bot loaded for /api/send routing");
                }
            }
        }
    }

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
        let channel = ChannelId::new(channel_id);
        let providers = self.providers.lock().await;
        for entry in providers.iter() {
            let shared = entry.shared.clone();
            let watcher_binding = shared.tmux_watchers.channel_binding(&channel);
            let inflight = ProviderKind::from_str(&entry.name)
                .and_then(|pk| super::inflight::load_inflight_state(&pk, channel_id));
            let inflight_tmux_session = inflight
                .as_ref()
                .and_then(|state| state.tmux_session_name.clone());
            let inflight_owner_channel_id = inflight_tmux_session
                .as_deref()
                .and_then(|tmux| shared.tmux_watchers.owner_channel_for_tmux_session(tmux));
            let attached = watcher_binding.is_some() || inflight_owner_channel_id.is_some();
            let has_relay_coord = shared.tmux_relay_coords.contains_key(&channel);
            let inflight_state_present = inflight.is_some();
            let mailbox_snapshot = super::mailbox_snapshot(&shared, channel).await;
            let mailbox_active_user_msg_id =
                mailbox_snapshot.active_user_message_id.map(|id| id.get());
            let has_pending_queue = !mailbox_snapshot.intervention_queue.is_empty();
            let mailbox_engaged = mailbox_active_user_msg_id.is_some() || has_pending_queue;
            if !attached && !has_relay_coord && !inflight_state_present && !mailbox_engaged {
                continue;
            }
            let (last_relay_offset, last_relay_ts_ms) = shared
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
            return Some(WatcherStateSnapshot {
                provider: entry.name.clone(),
                attached,
                tmux_session,
                watcher_owner_channel_id,
                last_relay_offset,
                inflight_state_present,
                last_relay_ts_ms,
                inflight_started_at,
                inflight_updated_at,
                inflight_user_msg_id,
                inflight_current_msg_id,
                tmux_session_alive,
                has_pending_queue,
                mailbox_active_user_msg_id,
            });
        }
        None
    }

    pub async fn utility_bot_user_id(&self, bot_name: &str) -> Option<u64> {
        match bot_name {
            "announce" => {
                if let Some(id) = *self.announce_user_id.lock().await {
                    return Some(id);
                }
                let http = { self.announce_http.lock().await.clone()? };
                let user = http.get_current_user().await.ok()?;
                let id = user.id.get();
                *self.announce_user_id.lock().await = Some(id);
                Some(id)
            }
            "notify" => {
                if let Some(id) = *self.notify_user_id.lock().await {
                    return Some(id);
                }
                let http = { self.notify_http.lock().await.clone()? };
                let user = http.get_current_user().await.ok()?;
                let id = user.id.get();
                *self.notify_user_id.lock().await = Some(id);
                Some(id)
            }
            _ => None,
        }
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
    let providers = registry.providers.lock().await;
    providers
        .iter()
        .find(|entry| entry.name.eq_ignore_ascii_case(provider.as_str()))
        .map(|entry| entry.shared.clone())
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
    #[cfg(test)]
    {
        std::time::Duration::from_millis(150)
    }
    #[cfg(not(test))]
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
        && !super::inflight::inflight_state_file_exists(provider, channel_id.get());
    removed_now || disappeared_during_stop
}

pub(crate) async fn stop_provider_channel_runtime_with_policy(
    registry: &HealthRegistry,
    provider_name: &str,
    channel_id: ChannelId,
    reason: &str,
    cleanup_policy: super::TmuxCleanupPolicy,
) -> Option<RuntimeTurnStopResult> {
    let provider = ProviderKind::from_str(provider_name)?;
    let shared = shared_for_provider(registry, &provider).await?;
    let cleanup_requested = cleanup_policy.should_cleanup_tmux();
    let should_clear_persistent_inflight = cleanup_policy.should_clear_inflight();
    let persistent_inflight_was_present = should_clear_persistent_inflight
        && super::inflight::inflight_state_file_exists(&provider, channel_id.get());
    let result = mailbox_cancel_active_turn(&shared, channel_id).await;

    if let Some(token) = result.token.as_ref() {
        let termination_recorded = if !result.already_stopping || cleanup_requested {
            super::turn_bridge::cancel_active_token(token, cleanup_policy, reason)
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
            super::turn_bridge::cancel_active_token(token, cleanup_policy, reason);
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
        super::TmuxCleanupPolicy::PreserveSession,
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
        super::TmuxCleanupPolicy::CleanupSession {
            termination_reason_code: Some(termination_reason_code),
        },
    )
    .await
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
            if has_session || super::ChannelMailboxRegistry::global_handle(channel_id).is_some() {
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
    finish: &super::FinishTurnResult,
    stop_source: &'static str,
    stop_watcher: bool,
) -> bool {
    if let Some(token) = finish.removed_token.as_ref() {
        token.cancelled.store(true, Ordering::Relaxed);
        shared.global_active.fetch_sub(1, Ordering::Relaxed);
    }

    super::clear_watchdog_deadline_override(channel_id.get()).await;
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
        super::schedule_deferred_idle_queue_kickoff(
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
            super::ChannelMailboxRegistry::global_handle(runtime.channel_id)
        {
            handle
                .finish_turn(super::queue_persistence_context(
                    &runtime.shared,
                    &runtime.provider,
                    runtime.channel_id,
                ))
                .await
        } else {
            super::FinishTurnResult {
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
        && let Some(handle) = super::ChannelMailboxRegistry::global_handle(channel_id)
    {
        let finish = handle.hard_stop().await;
        super::clear_watchdog_deadline_override(channel_id.get()).await;
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
        super::turn_bridge::cancel_active_token(
            &token,
            super::TmuxCleanupPolicy::PreserveSession,
            "auto-queue slot clear",
        );
        decrement_counter(shared.global_active.as_ref());
    }

    {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            super::settings::cleanup_channel_uploads(channel_id);
            session.clear_provider_session();
            session.history.clear();
            session.pending_uploads.clear();
            session.cleared = true;
        }
    }

    #[cfg(unix)]
    if let Some(name) = tmux_name {
        if provider == ProviderKind::Claude {
            let _ = tokio::task::spawn_blocking(move || {
                crate::services::platform::tmux::send_keys(&name, &["/clear", "Enter"])
            })
            .await;
        } else if provider.uses_managed_tmux_backend() {
            super::commands::reset_managed_process_session(&name);
        }
    }

    true
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
                let inflight_state = provider_kind
                    .as_ref()
                    .and_then(|pk| super::inflight::load_inflight_state(pk, channel_id.get()));
                let tmux_session_name = inflight_state
                    .as_ref()
                    .and_then(|state| state.tmux_session_name.as_deref());
                let tmux_present =
                    tmux_session_name.is_some_and(crate::services::platform::tmux::has_session);
                let process_present = tmux_session_name
                    .is_some_and(|name| crate::services::platform::tmux::pane_pid(name).is_some());
                mailbox_entries.push(MailboxHealthSnapshot {
                    provider: entry.name.clone(),
                    channel_id: channel_id.get(),
                    has_cancel_token: snapshot.cancel_token.is_some(),
                    queue_depth: snapshot.intervention_queue.len(),
                    recovery_started: snapshot.recovery_started_at.is_some(),
                    active_request_owner: snapshot.active_request_owner.map(|id| id.get()),
                    active_user_message_id: snapshot.active_user_message_id.map(|id| id.get()),
                    agent_turn_status: if snapshot.cancel_token.is_some() {
                        "active"
                    } else {
                        "idle"
                    },
                    watcher_attached: entry.shared.tmux_watchers.contains_key(channel_id),
                    inflight_state_present: inflight_state.is_some(),
                    tmux_present,
                    process_present,
                    active_dispatch_present: inflight_state
                        .as_ref()
                        .and_then(|state| state.dispatch_id.as_deref())
                        .is_some(),
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

#[cfg(test)]
pub(crate) struct TestHealthHarness {
    provider: ProviderKind,
    registry: Arc<HealthRegistry>,
    shared: Arc<SharedData>,
}

#[cfg(test)]
impl TestHealthHarness {
    pub(crate) async fn new() -> Self {
        Self::new_with_provider(ProviderKind::Claude).await
    }

    pub(crate) async fn new_with_provider(provider: ProviderKind) -> Self {
        let registry = Arc::new(HealthRegistry::new());
        let global_active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let global_finalizing = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let shutdown_remaining = Arc::new(std::sync::atomic::AtomicUsize::new(1));
        let mut settings = super::DiscordBotSettings::default();
        settings.provider = provider.clone();
        let shared = Arc::new(SharedData {
            core: tokio::sync::Mutex::new(super::CoreState {
                sessions: std::collections::HashMap::new(),
                active_meetings: std::collections::HashMap::new(),
            }),
            mailboxes: super::ChannelMailboxRegistry::default(),
            settings: tokio::sync::RwLock::new(settings),
            api_timestamps: dashmap::DashMap::new(),
            skills_cache: tokio::sync::RwLock::new(Vec::new()),
            tmux_watchers: super::TmuxWatcherRegistry::new(),
            tmux_relay_coords: dashmap::DashMap::new(),
            recovering_channels: dashmap::DashMap::new(),
            shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            finalizing_turns: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            current_generation: 0,
            restart_pending: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            reconcile_done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            deferred_hook_backlog: std::sync::atomic::AtomicUsize::new(0),
            recovery_started_at: Instant::now(),
            recovery_duration_ms: std::sync::atomic::AtomicU64::new(0),
            global_active,
            global_finalizing,
            shutdown_remaining,
            shutdown_counted: std::sync::atomic::AtomicBool::new(false),
            intake_dedup: dashmap::DashMap::new(),
            dispatch_thread_parents: dashmap::DashMap::new(),
            bot_connected: std::sync::atomic::AtomicBool::new(true),
            last_turn_at: std::sync::Mutex::new(None),
            model_overrides: dashmap::DashMap::new(),
            fast_mode_channels: dashmap::DashSet::new(),
            fast_mode_session_reset_pending: dashmap::DashSet::new(),
            model_session_reset_pending: dashmap::DashSet::new(),
            session_reset_pending: dashmap::DashSet::new(),
            model_picker_pending: dashmap::DashMap::new(),
            dispatch_role_overrides: dashmap::DashMap::new(),
            last_message_ids: dashmap::DashMap::new(),
            turn_start_times: dashmap::DashMap::new(),
            channel_rosters: dashmap::DashMap::new(),
            cached_serenity_ctx: tokio::sync::OnceCell::new(),
            cached_bot_token: tokio::sync::OnceCell::new(),
            token_hash: super::settings::discord_token_hash("test-token"),
            api_port: 8791,
            sqlite: None,
            pg_pool: None,
            engine: None,
            health_registry: Arc::downgrade(&registry),
            known_slash_commands: tokio::sync::OnceCell::new(),
        });
        super::mark_reconcile_complete(&shared);
        registry
            .register(provider.as_str().to_string(), shared.clone())
            .await;
        Self {
            provider,
            registry,
            shared,
        }
    }

    pub(crate) fn registry(&self) -> Arc<HealthRegistry> {
        self.registry.clone()
    }

    pub(crate) fn set_reconcile_done(&self, done: bool) {
        self.shared
            .reconcile_done
            .store(done, std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) fn set_connected(&self, connected: bool) {
        self.shared
            .bot_connected
            .store(connected, std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) fn set_restart_pending(&self, restart_pending: bool) {
        self.shared
            .restart_pending
            .store(restart_pending, std::sync::atomic::Ordering::Relaxed);
    }

    fn shared(&self) -> Arc<SharedData> {
        self.shared.clone()
    }

    pub(crate) fn set_deferred_hooks(&self, count: usize) {
        self.shared
            .deferred_hook_backlog
            .store(count, std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) fn set_recovery_duration_ms(&self, duration_ms: u64) {
        self.shared
            .recovery_duration_ms
            .store(duration_ms, std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) async fn set_queue_depth(&self, depth: usize) {
        self.set_queue_depth_for_channel(1, ProviderKind::Claude, depth)
            .await;
    }

    pub(crate) async fn set_queue_depth_for_channel(
        &self,
        channel_id: u64,
        provider: ProviderKind,
        depth: usize,
    ) {
        super::mailbox_replace_queue(
            &self.shared,
            &provider,
            ChannelId::new(channel_id),
            Vec::new(),
        )
        .await;
        if depth == 0 {
            return;
        }
        let queue = (0..depth)
            .map(|idx| super::Intervention {
                author_id: serenity::UserId::new(idx as u64 + 1),
                message_id: serenity::MessageId::new(idx as u64 + 1),
                source_message_ids: vec![serenity::MessageId::new(idx as u64 + 1)],
                text: format!("queued-{idx}"),
                mode: super::InterventionMode::Soft,
                created_at: Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
            })
            .collect::<Vec<_>>();
        super::mailbox_replace_queue(&self.shared, &provider, ChannelId::new(channel_id), queue)
            .await;
    }

    pub(crate) async fn queue_depth_for_channel(&self, channel_id: u64) -> usize {
        self.shared
            .mailbox(ChannelId::new(channel_id))
            .snapshot()
            .await
            .intervention_queue
            .len()
    }

    pub(crate) async fn seed_channel_session(
        &self,
        channel_id: u64,
        channel_name: &str,
        session_id: Option<&str>,
    ) {
        let mut data = self.shared.core.lock().await;
        data.sessions.insert(
            ChannelId::new(channel_id),
            super::DiscordSession {
                session_id: session_id.map(str::to_string),
                memento_context_loaded: super::session_runtime::restored_memento_context_loaded(
                    false, None, session_id,
                ),
                memento_reflected: false,
                current_path: None,
                history: Vec::new(),
                pending_uploads: Vec::new(),
                cleared: false,
                remote_profile_name: None,
                channel_id: Some(channel_id),
                channel_name: Some(channel_name.to_string()),
                category_name: None,
                last_active: tokio::time::Instant::now(),
                worktree: None,
                born_generation: 0,
                assistant_turns: 0,
            },
        );
    }

    pub(crate) async fn start_active_turn(
        &self,
        channel_id: u64,
        user_id: u64,
        message_id: u64,
        tmux_name: Option<&str>,
    ) -> Arc<crate::services::provider::CancelToken> {
        let token = Arc::new(crate::services::provider::CancelToken::new());
        if let Some(tmux_name) = tmux_name {
            *token.tmux_session.lock().unwrap() = Some(tmux_name.to_string());
        }
        let started = self
            .shared
            .mailbox(ChannelId::new(channel_id))
            .try_start_turn(
                token.clone(),
                serenity::UserId::new(user_id),
                serenity::MessageId::new(message_id),
            )
            .await;
        assert!(started, "test active turn should start");
        self.shared.global_active.fetch_add(1, Ordering::Relaxed);
        token
    }

    pub(crate) async fn seed_active_turn(
        &self,
        channel_id: u64,
        request_owner: u64,
        user_message_id: u64,
    ) {
        let started = self
            .shared
            .mailbox(ChannelId::new(channel_id))
            .try_start_turn(
                Arc::new(crate::services::provider::CancelToken::new()),
                serenity::UserId::new(request_owner),
                serenity::MessageId::new(user_message_id),
            )
            .await;
        assert!(started, "test harness expected an idle mailbox");
        self.shared.global_active.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) async fn seed_queue(&self, channel_id: u64, queue_items: &[(u64, &str)]) {
        let queue = queue_items
            .iter()
            .map(|(message_id, text)| super::Intervention {
                author_id: serenity::UserId::new(1),
                message_id: serenity::MessageId::new(*message_id),
                source_message_ids: vec![serenity::MessageId::new(*message_id)],
                text: (*text).to_string(),
                mode: super::InterventionMode::Soft,
                created_at: Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
            })
            .collect::<Vec<_>>();
        super::mailbox_replace_queue(
            &self.shared,
            &self.provider,
            ChannelId::new(channel_id),
            queue,
        )
        .await;
    }

    pub(crate) async fn mailbox_state(&self, channel_id: u64) -> (bool, usize, Option<String>) {
        let snapshot = super::mailbox_snapshot(&self.shared, ChannelId::new(channel_id)).await;
        let session_id = {
            let data = self.shared.core.lock().await;
            data.sessions
                .get(&ChannelId::new(channel_id))
                .and_then(|session| session.session_id.clone())
        };
        (
            snapshot.cancel_token.is_some(),
            snapshot.intervention_queue.len(),
            session_id,
        )
    }

    pub(crate) fn has_dispatch_role_override(&self, channel_id: u64) -> bool {
        self.shared
            .dispatch_role_overrides
            .contains_key(&ChannelId::new(channel_id))
    }

    pub(crate) fn insert_dispatch_role_override(&self, channel_id: u64, override_channel_id: u64) {
        self.shared.dispatch_role_overrides.insert(
            ChannelId::new(channel_id),
            ChannelId::new(override_channel_id),
        );
    }

    pub(crate) fn seed_watcher(&self, channel_id: u64) -> Arc<std::sync::atomic::AtomicBool> {
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.shared.tmux_watchers.insert(
            ChannelId::new(channel_id),
            super::TmuxWatcherHandle {
                tmux_session_name: format!("test-seeded-watcher-{channel_id}"),
                paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                resume_offset: Arc::new(std::sync::Mutex::new(None)),
                cancel: cancel.clone(),
                pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        );
        cancel
    }

    pub(crate) fn has_watcher(&self, channel_id: u64) -> bool {
        self.shared
            .tmux_watchers
            .contains_key(&ChannelId::new(channel_id))
    }
}

/// Resolve the bot HTTP client by name.
/// Supported: "announce", "notify", or a provider name like "claude"/"codex".
pub async fn resolve_bot_http(
    registry: &HealthRegistry,
    bot: &str,
) -> Result<Arc<serenity::Http>, (&'static str, String)> {
    match bot {
        "notify" => {
            let guard = registry.notify_http.lock().await;
            match guard.as_ref() {
                Some(http) => Ok(http.clone()),
                None => Err((
                    "503 Service Unavailable",
                    r#"{"ok":false,"error":"notify bot not configured (missing credential/notify_bot_token)"}"#.to_string(),
                )),
            }
        }
        "announce" => {
            let guard = registry.announce_http.lock().await;
            match guard.as_ref() {
                Some(http) => Ok(http.clone()),
                None => Err((
                    "503 Service Unavailable",
                    r#"{"ok":false,"error":"announce bot not configured (missing credential/announce_bot_token)"}"#.to_string(),
                )),
            }
        }
        provider => {
            // Look up provider bot (e.g. "claude", "codex")
            let clients = registry.discord_http.lock().await;
            for (name, http) in clients.iter() {
                if name == provider {
                    return Ok(http.clone());
                }
            }
            Err((
                "400 Bad Request",
                format!(r#"{{"ok":false,"error":"unknown bot: {provider}"}}"#),
            ))
        }
    }
}

pub async fn fetch_channel_name(
    registry: &HealthRegistry,
    channel_id: ChannelId,
    provider: &ProviderKind,
) -> Option<String> {
    let http = resolve_bot_http(registry, provider.as_str()).await.ok()?;
    let channel = channel_id.to_channel(&*http).await.ok()?;
    channel.guild().map(|guild_channel| guild_channel.name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DirectMeetingRuntimeCandidate {
    index: usize,
    explicit_channel_match: bool,
    live_channel_match: bool,
}

fn select_direct_meeting_runtime_candidate(
    provider_name: &str,
    channel_id: ChannelId,
    candidates: &[DirectMeetingRuntimeCandidate],
) -> Result<Option<usize>, String> {
    let explicit_matches = candidates
        .iter()
        .filter(|candidate| candidate.explicit_channel_match)
        .map(|candidate| candidate.index)
        .collect::<Vec<_>>();
    if explicit_matches.len() > 1 {
        return Err(serde_json::json!({
            "ok": false,
            "error": format!(
                "multiple runtimes explicitly allow channel {} for provider {}",
                channel_id.get(),
                provider_name
            ),
        })
        .to_string());
    }
    if let Some(index) = explicit_matches.first().copied() {
        return Ok(Some(index));
    }

    let live_matches = candidates
        .iter()
        .filter(|candidate| candidate.live_channel_match)
        .map(|candidate| candidate.index)
        .collect::<Vec<_>>();
    if live_matches.len() > 1 {
        return Err(serde_json::json!({
            "ok": false,
            "error": format!(
                "multiple runtimes can handle channel {} for provider {}",
                channel_id.get(),
                provider_name
            ),
        })
        .to_string());
    }
    Ok(live_matches.first().copied())
}

async fn resolve_direct_meeting_runtime(
    registry: &HealthRegistry,
    channel_id: ChannelId,
    owner_provider: &ProviderKind,
) -> Result<(Arc<serenity::Http>, Arc<SharedData>), String> {
    let provider_name = owner_provider.as_str();
    let shared_candidates = {
        let providers = registry.providers.lock().await;
        providers
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.name.eq_ignore_ascii_case(provider_name))
            .map(|(index, entry)| (index, entry.shared.clone()))
            .collect::<Vec<_>>()
    };

    if shared_candidates.is_empty() {
        return Err(serde_json::json!({
            "ok": false,
            "error": format!("provider runtime not registered: {}", provider_name),
        })
        .to_string());
    }

    let mut candidate_matches = Vec::with_capacity(shared_candidates.len());
    for (index, shared) in &shared_candidates {
        let settings = shared.settings.read().await.clone();
        let explicit_channel_match = settings.allowed_channel_ids.contains(&channel_id.get());
        let live_channel_match = match shared.cached_serenity_ctx.get() {
            Some(ctx) => {
                super::provider_handles_channel(ctx, owner_provider, &settings, channel_id).await
            }
            None => false,
        };
        candidate_matches.push(DirectMeetingRuntimeCandidate {
            index: *index,
            explicit_channel_match,
            live_channel_match,
        });
    }

    if let Some(selected_index) =
        select_direct_meeting_runtime_candidate(provider_name, channel_id, &candidate_matches)?
    {
        let (_, shared) = shared_candidates
            .iter()
            .find(|(index, _)| *index == selected_index)
            .cloned()
            .ok_or_else(|| {
                serde_json::json!({
                    "ok": false,
                    "error": format!(
                        "selected runtime index vanished for provider {} on channel {}",
                        provider_name,
                        channel_id.get()
                    ),
                })
                .to_string()
            })?;
        let http = shared
            .cached_serenity_ctx
            .get()
            .map(|ctx| ctx.http.clone())
            .ok_or_else(|| {
                serde_json::json!({
                    "ok": false,
                    "error": format!(
                        "matched runtime is not ready for provider {} on channel {}",
                        provider_name,
                        channel_id.get()
                    ),
                })
                .to_string()
            })?;
        return Ok((http, shared));
    }

    if shared_candidates.len() == 1 {
        let (_, shared) = shared_candidates[0].clone();
        if let Some(ctx) = shared.cached_serenity_ctx.get() {
            return Ok((ctx.http.clone(), shared));
        }
        let http = resolve_bot_http(registry, provider_name)
            .await
            .map_err(|(_, body)| body)?;
        return Ok((http, shared));
    }

    Err(serde_json::json!({
        "ok": false,
        "error": format!(
            "could not resolve a unique runtime for provider {} on channel {}",
            provider_name,
            channel_id.get()
        ),
    })
    .to_string())
}

async fn resolve_direct_meeting_shared(
    registry: &HealthRegistry,
    channel_id: ChannelId,
    owner_provider: &ProviderKind,
) -> Result<Arc<SharedData>, String> {
    let provider_name = owner_provider.as_str();
    let shared_candidates = {
        let providers = registry.providers.lock().await;
        providers
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.name.eq_ignore_ascii_case(provider_name))
            .map(|(index, entry)| (index, entry.shared.clone()))
            .collect::<Vec<_>>()
    };

    if shared_candidates.is_empty() {
        return Err(serde_json::json!({
            "ok": false,
            "error": format!("provider runtime not registered: {}", provider_name),
        })
        .to_string());
    }

    let mut candidate_matches = Vec::with_capacity(shared_candidates.len());
    for (index, shared) in &shared_candidates {
        let settings = shared.settings.read().await.clone();
        let explicit_channel_match = settings.allowed_channel_ids.contains(&channel_id.get());
        let live_channel_match = match shared.cached_serenity_ctx.get() {
            Some(ctx) => {
                super::provider_handles_channel(ctx, owner_provider, &settings, channel_id).await
            }
            None => false,
        };
        candidate_matches.push(DirectMeetingRuntimeCandidate {
            index: *index,
            explicit_channel_match,
            live_channel_match,
        });
    }

    if let Some(selected_index) =
        select_direct_meeting_runtime_candidate(provider_name, channel_id, &candidate_matches)?
    {
        let (_, shared) = shared_candidates
            .iter()
            .find(|(index, _)| *index == selected_index)
            .cloned()
            .ok_or_else(|| {
                serde_json::json!({
                    "ok": false,
                    "error": format!(
                        "selected runtime index vanished for provider {} on channel {}",
                        provider_name,
                        channel_id.get()
                    ),
                })
                .to_string()
            })?;
        return Ok(shared);
    }

    if shared_candidates.len() == 1 {
        return Ok(shared_candidates[0].1.clone());
    }

    Err(serde_json::json!({
        "ok": false,
        "error": format!(
            "could not resolve a unique runtime for provider {} on channel {}",
            provider_name,
            channel_id.get()
        ),
    })
    .to_string())
}

pub async fn start_headless_agent_turn(
    registry: &HealthRegistry,
    channel_id: ChannelId,
    owner_provider: ProviderKind,
    prompt: String,
    source: Option<String>,
    metadata: Option<serde_json::Value>,
    channel_name_hint: Option<String>,
) -> Result<super::router::HeadlessTurnStartOutcome, super::router::HeadlessTurnStartError> {
    let shared = resolve_direct_meeting_shared(registry, channel_id, &owner_provider)
        .await
        .map_err(super::router::HeadlessTurnStartError::Internal)?;

    if shared.mailbox(channel_id).has_active_turn().await {
        return Err(super::router::HeadlessTurnStartError::Conflict(format!(
            "agent mailbox is busy for channel {}",
            channel_id.get()
        )));
    }

    let ctx = shared.cached_serenity_ctx.get().cloned().ok_or_else(|| {
        super::router::HeadlessTurnStartError::Internal(format!(
            "provider runtime is not ready for channel {}",
            channel_id.get()
        ))
    })?;
    let token = shared
        .cached_bot_token
        .get()
        .cloned()
        .or_else(|| super::resolve_discord_token_by_hash(&shared.token_hash))
        .ok_or_else(|| {
            super::router::HeadlessTurnStartError::Internal(format!(
                "provider token unavailable for channel {}",
                channel_id.get()
            ))
        })?;

    super::router::start_headless_turn(
        &ctx,
        channel_id,
        &prompt,
        source.as_deref().unwrap_or("system"),
        &shared,
        &token,
        source.as_deref(),
        metadata,
        channel_name_hint,
    )
    .await
}

pub async fn start_direct_meeting(
    registry: &HealthRegistry,
    channel_id: ChannelId,
    owner_provider: ProviderKind,
    primary_provider: ProviderKind,
    reviewer_provider: ProviderKind,
    agenda: String,
    fixed_participants: Vec<String>,
) -> Result<(), String> {
    let (http, shared) =
        resolve_direct_meeting_runtime(registry, channel_id, &owner_provider).await?;

    super::meeting::spawn_direct_start(
        http,
        channel_id,
        agenda,
        primary_provider,
        reviewer_provider,
        fixed_participants,
        shared,
    )
    .await
}

#[derive(Debug, PartialEq, Eq)]
enum SendTargetResolutionError {
    BadRequest(&'static str),
    NotFound(String),
    Internal(String),
}

fn parse_channel_target_value(target: &str) -> Option<u64> {
    let trimmed = target.trim();
    trimmed
        .parse::<u64>()
        .ok()
        .or_else(|| crate::server::routes::dispatches::resolve_channel_alias_pub(trimmed))
}

fn parse_agent_target(target: &str) -> Result<Option<&str>, SendTargetResolutionError> {
    let Some(agent_id_raw) = target.strip_prefix("agent:") else {
        return Ok(None);
    };
    let agent_id = agent_id_raw.trim();
    if agent_id.is_empty() {
        return Err(SendTargetResolutionError::BadRequest(
            "invalid target format (use channel:<id>, channel:<name>, or agent:<roleId>)",
        ));
    }
    Ok(Some(agent_id))
}

fn resolve_agent_target_channel_id_sqlite(
    sqlite: &Db,
    agent_id: &str,
) -> Result<u64, SendTargetResolutionError> {
    let conn = sqlite.lock().map_err(|e| {
        SendTargetResolutionError::Internal(format!("db lock failed during agent lookup: {e}"))
    })?;
    let bindings = crate::db::agents::load_agent_channel_bindings(&conn, agent_id)
        .map_err(|e| {
            SendTargetResolutionError::Internal(format!("agent lookup failed for {agent_id}: {e}"))
        })?
        .ok_or_else(|| {
            SendTargetResolutionError::NotFound(format!("unknown agent target: {agent_id}"))
        })?;
    let channel_target = bindings.primary_channel().ok_or_else(|| {
        SendTargetResolutionError::NotFound(format!(
            "agent target has no primary channel: {agent_id}"
        ))
    })?;

    parse_channel_target_value(&channel_target).ok_or_else(|| {
        SendTargetResolutionError::Internal(format!(
            "agent target resolved to invalid channel: {channel_target}"
        ))
    })
}

async fn resolve_agent_target_channel_id_pg(
    pg_pool: &PgPool,
    agent_id: &str,
) -> Result<u64, SendTargetResolutionError> {
    let bindings = crate::db::agents::load_agent_channel_bindings_pg(pg_pool, agent_id)
        .await
        .map_err(|e| {
            SendTargetResolutionError::Internal(format!("agent lookup failed for {agent_id}: {e}"))
        })?
        .ok_or_else(|| {
            SendTargetResolutionError::NotFound(format!("unknown agent target: {agent_id}"))
        })?;
    let channel_target = bindings.primary_channel().ok_or_else(|| {
        SendTargetResolutionError::NotFound(format!(
            "agent target has no primary channel: {agent_id}"
        ))
    })?;

    parse_channel_target_value(&channel_target).ok_or_else(|| {
        SendTargetResolutionError::Internal(format!(
            "agent target resolved to invalid channel: {channel_target}"
        ))
    })
}

fn resolve_channel_target(target: &str) -> Result<u64, SendTargetResolutionError> {
    let channel_target = target.strip_prefix("channel:").unwrap_or(target);
    parse_channel_target_value(channel_target).ok_or(SendTargetResolutionError::BadRequest(
        "invalid target format (use channel:<id>, channel:<name>, or agent:<roleId>)",
    ))
}

fn resolve_send_target_channel_id(
    sqlite: &Db,
    target: &str,
) -> Result<u64, SendTargetResolutionError> {
    match parse_agent_target(target)? {
        Some(agent_id) => resolve_agent_target_channel_id_sqlite(sqlite, agent_id),
        None => resolve_channel_target(target),
    }
}

async fn resolve_send_target_channel_id_with_backends(
    db: Option<&Db>,
    pg_pool: Option<&PgPool>,
    target: &str,
) -> Result<u64, SendTargetResolutionError> {
    match parse_agent_target(target)? {
        Some(agent_id) => {
            if let Some(pg_pool) = pg_pool {
                return resolve_agent_target_channel_id_pg(pg_pool, agent_id).await;
            }
            let db = db.ok_or_else(|| {
                SendTargetResolutionError::Internal(
                    "sqlite db unavailable during agent lookup".to_string(),
                )
            })?;
            resolve_agent_target_channel_id_sqlite(db, agent_id)
        }
        None => resolve_channel_target(target),
    }
}

/// Handle POST /api/send — agent-to-agent native routing.
/// Accepts JSON: {"target":"channel:<id>|channel:<name>|agent:<roleId>", "content":"...", "source":"role-id", "bot":"announce|notify", "summary":"..."}
///
/// `summary` is optional minimal fallback content if Discord rejects the
/// length-truncated primary send.
pub(crate) async fn send_message_with_backends(
    registry: &HealthRegistry,
    db: Option<&Db>,
    pg_pool: Option<&PgPool>,
    target: &str,
    content: &str,
    source: &str,
    bot: &str,
    summary: Option<&str>,
) -> (&'static str, String) {
    send_message_with_backends_and_delivery_id(
        registry, db, pg_pool, target, content, source, bot, summary, None,
    )
    .await
}

pub(crate) async fn send_message_with_backends_and_delivery_id(
    registry: &HealthRegistry,
    db: Option<&Db>,
    pg_pool: Option<&PgPool>,
    target: &str,
    content: &str,
    source: &str,
    bot: &str,
    summary: Option<&str>,
    delivery_id: Option<ManualOutboundDeliveryId<'_>>,
) -> (&'static str, String) {
    if content.is_empty() {
        return (
            "400 Bad Request",
            r#"{"ok":false,"error":"content is required"}"#.to_string(),
        );
    }

    let channel_id_raw =
        match resolve_send_target_channel_id_with_backends(db, pg_pool, target).await {
            Ok(id) => id,
            Err(SendTargetResolutionError::BadRequest(message)) => {
                return (
                    "400 Bad Request",
                    serde_json::json!({"ok": false, "error": message}).to_string(),
                );
            }
            Err(SendTargetResolutionError::NotFound(message)) => {
                return (
                    "404 Not Found",
                    serde_json::json!({"ok": false, "error": message}).to_string(),
                );
            }
            Err(SendTargetResolutionError::Internal(message)) => {
                return (
                    "500 Internal Server Error",
                    serde_json::json!({"ok": false, "error": message}).to_string(),
                );
            }
        };

    let channel_id = ChannelId::new(channel_id_raw);

    // Validate source is a known agent role_id or internal system source
    const INTERNAL_SOURCES: &[&str] = &[
        "kanban-rules",
        "triage-rules",
        "review-automation",
        "auto-queue",
        "pipeline",
        "system",
        "timeouts",
        "merge-automation",
        "dashboard",
    ];
    if !INTERNAL_SOURCES.contains(&source) && !super::settings::is_known_agent(source) {
        return (
            "403 Forbidden",
            format!(
                r#"{{"ok":false,"error":"unknown source role: {}"}}"#,
                source
            ),
        );
    }

    // Verify target channel exists in role-map (authorization check).
    // If the target is a thread, resolve its parent channel and check that instead.
    // Pass channel name so byChannelName-style configs can match.
    if super::settings::resolve_role_binding(channel_id, None).is_none() {
        let mut authorized = false;
        // Try resolving as a thread: fetch channel info and check parent_id
        if let Ok(http) = resolve_bot_http(registry, bot).await {
            if let Ok(channel) = channel_id.to_channel(&*http).await {
                if let Some(guild_channel) = channel.guild() {
                    if let Some(parent_id) = guild_channel.parent_id {
                        // Resolve parent channel name for byChannelName configs
                        let parent_name = if let Ok(parent_ch) = parent_id.to_channel(&*http).await
                        {
                            parent_ch.guild().map(|pg| pg.name.clone())
                        } else {
                            None
                        };
                        if super::settings::resolve_role_binding(parent_id, parent_name.as_deref())
                            .is_some()
                        {
                            authorized = true;
                        }
                    }
                }
            }
        }
        if !authorized {
            return (
                "403 Forbidden",
                r#"{"ok":false,"error":"channel not in role-map"}"#.to_string(),
            );
        }
    }

    // Select bot: "announce" (default, agents respond) or "notify" (info-only, agents ignore)
    let http = match resolve_bot_http(registry, bot).await {
        Ok(h) => h,
        Err(resp) => return resp,
    };

    let outbound_client = SerenityManualOutboundClient { http };
    send_resolved_manual_message_with_client(
        &outbound_client,
        manual_notification_deduper(),
        channel_id_raw,
        target,
        content,
        source,
        bot,
        summary,
        delivery_id,
    )
    .await
}

async fn send_resolved_manual_message_with_client<C: ManualOutboundClient>(
    client: &C,
    dedup: &OutboundDeduper,
    channel_id_raw: u64,
    target: &str,
    content: &str,
    source: &str,
    bot: &str,
    summary: Option<&str>,
    delivery_id: Option<ManualOutboundDeliveryId<'_>>,
) -> (&'static str, String) {
    let channel_id = ChannelId::new(channel_id_raw);
    let send_result = deliver_manual_notification(
        client,
        dedup,
        &channel_id_raw.to_string(),
        content,
        bot,
        summary,
        delivery_id,
    )
    .await;
    match send_result {
        ManualDeliveryOutcome::Sent {
            message_id,
            delivery,
        } => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            let emoji = if bot == "notify" { "🔔" } else { "📨" };
            let delivery_tag = delivery
                .map(|value| format!(" +{value}"))
                .unwrap_or_default();
            tracing::info!(
                "  [{ts}] {emoji} ROUTE: [{source}] → channel {channel_id} (bot={bot}{delivery_tag})"
            );
            let mut response = serde_json::json!({
                "ok": true,
                "target": format!("channel:{channel_id}"),
                "channel_id": channel_id.get().to_string(),
                "message_id": message_id,
                "source": source,
                "bot": bot,
                "sent_at": chrono::Utc::now().to_rfc3339(),
            });
            if let Some(delivery) = delivery {
                response["delivery"] = serde_json::Value::String(delivery.to_string());
            }
            if target != format!("channel:{channel_id}") {
                response["requested_target"] = serde_json::Value::String(target.to_string());
            }
            ("200 OK", response.to_string())
        }
        ManualDeliveryOutcome::Failed { detail } => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!("  [{ts}] ⚠ ROUTE: failed to send to channel {channel_id}: {detail}");
            (
                "500 Internal Server Error",
                format!(
                    r#"{{"ok":false,"error":"Discord send failed: {}"}}"#,
                    detail
                ),
            )
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ManualOutboundDeliveryId<'a> {
    pub(crate) correlation_id: &'a str,
    pub(crate) semantic_event_id: &'a str,
}

#[derive(Debug, PartialEq, Eq)]
enum ManualDeliveryOutcome {
    Sent {
        message_id: String,
        delivery: Option<&'static str>,
    },
    Failed {
        detail: String,
    },
}

#[derive(Clone)]
struct SerenityManualOutboundClient {
    http: Arc<serenity::Http>,
}

impl DiscordOutboundClient for SerenityManualOutboundClient {
    async fn post_message(
        &self,
        target_channel: &str,
        content: &str,
    ) -> Result<String, DispatchMessagePostError> {
        let channel_id = target_channel
            .parse::<u64>()
            .map(ChannelId::new)
            .map_err(|error| {
                DispatchMessagePostError::new(
                    DispatchMessagePostErrorKind::Other,
                    format!("invalid discord channel id {target_channel}: {error}"),
                )
            })?;
        channel_id
            .send_message(&*self.http, CreateMessage::new().content(content))
            .await
            .map(|message| message.id.get().to_string())
            .map_err(|error| {
                let detail = error.to_string();
                let lowered = detail.to_ascii_lowercase();
                let kind = if detail.contains("BASE_TYPE_MAX_LENGTH")
                    || lowered.contains("2000 or fewer in length")
                    || lowered.contains("length")
                {
                    DispatchMessagePostErrorKind::MessageTooLong
                } else {
                    DispatchMessagePostErrorKind::Other
                };
                DispatchMessagePostError::new(kind, detail)
            })
    }
}

trait ManualOutboundClient: DiscordOutboundClient {
    async fn post_text_attachment(
        &self,
        target_channel: &str,
        content: &str,
        summary: Option<&str>,
    ) -> Result<String, DispatchMessagePostError>;
}

impl ManualOutboundClient for SerenityManualOutboundClient {
    async fn post_text_attachment(
        &self,
        target_channel: &str,
        content: &str,
        summary: Option<&str>,
    ) -> Result<String, DispatchMessagePostError> {
        let channel_id = target_channel
            .parse::<u64>()
            .map(ChannelId::new)
            .map_err(|error| {
                DispatchMessagePostError::new(
                    DispatchMessagePostErrorKind::Other,
                    format!("invalid discord channel id {target_channel}: {error}"),
                )
            })?;
        let (inline, attachment) = build_long_message_attachment(content, summary);
        channel_id
            .send_message(
                &*self.http,
                CreateMessage::new().content(inline).add_file(attachment),
            )
            .await
            .map(|message| message.id.get().to_string())
            .map_err(|error| {
                DispatchMessagePostError::new(
                    DispatchMessagePostErrorKind::Other,
                    error.to_string(),
                )
            })
    }
}

async fn deliver_manual_notification<C: ManualOutboundClient>(
    client: &C,
    dedup: &OutboundDeduper,
    channel_id: &str,
    content: &str,
    bot: &str,
    summary: Option<&str>,
    delivery_id: Option<ManualOutboundDeliveryId<'_>>,
) -> ManualDeliveryOutcome {
    let dedup_key = delivery_id.map(|delivery_id| {
        format!(
            "{}::{}",
            delivery_id.correlation_id, delivery_id.semantic_event_id
        )
    });
    if let Some(key) = dedup_key.as_deref() {
        if dedup.lookup(key).is_some() {
            return ManualDeliveryOutcome::Sent {
                message_id: String::new(),
                delivery: Some("duplicate"),
            };
        }
    }

    let content_len = content.chars().count();
    if content_len > DISCORD_HARD_LIMIT_CHARS {
        let result = match if bot == "announce" {
            client
                .post_text_attachment(channel_id, content, summary)
                .await
                .map(|message_id| ManualDeliveryOutcome::Sent {
                    message_id,
                    delivery: Some("summary+txt"),
                })
        } else {
            deliver_chunked_manual_notification(client, channel_id, content).await
        } {
            Ok(outcome) => outcome,
            Err(error) => ManualDeliveryOutcome::Failed {
                detail: error.to_string(),
            },
        };
        if let ManualDeliveryOutcome::Sent { message_id, .. } = &result {
            if let Some(key) = dedup_key.as_deref() {
                dedup.record(key, message_id);
            }
        }
        return result;
    }

    if content_len > DISCORD_SAFE_LIMIT_CHARS {
        let result = match client.post_message(channel_id, content).await {
            Ok(message_id) => ManualDeliveryOutcome::Sent {
                message_id,
                delivery: None,
            },
            Err(error) => ManualDeliveryOutcome::Failed {
                detail: error.to_string(),
            },
        };
        if let ManualDeliveryOutcome::Sent { message_id, .. } = &result {
            if let Some(key) = dedup_key.as_deref() {
                dedup.record(key, message_id);
            }
        }
        return result;
    }

    let mut outbound_msg = DiscordOutboundMessage::new(channel_id, content);
    if let Some(delivery_id) = delivery_id {
        outbound_msg = outbound_msg
            .with_correlation(delivery_id.correlation_id, delivery_id.semantic_event_id);
    }
    let policy = DiscordOutboundPolicy::review_notification(
        summary
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
    );

    match deliver_outbound(client, dedup, outbound_msg, policy).await {
        DeliveryResult::Success { message_id } => ManualDeliveryOutcome::Sent {
            message_id,
            delivery: None,
        },
        DeliveryResult::Fallback { message_id, kind } => ManualDeliveryOutcome::Sent {
            message_id,
            delivery: Some(match kind {
                FallbackKind::Truncated => "truncated",
                FallbackKind::MinimalFallback => "minimal_fallback",
            }),
        },
        DeliveryResult::Duplicate { .. } => ManualDeliveryOutcome::Sent {
            message_id: String::new(),
            delivery: Some("duplicate"),
        },
        DeliveryResult::Skipped { .. } => ManualDeliveryOutcome::Sent {
            message_id: String::new(),
            delivery: Some("skipped"),
        },
        DeliveryResult::PermanentFailure { detail } => ManualDeliveryOutcome::Failed { detail },
    }
}

async fn deliver_chunked_manual_notification<C: ManualOutboundClient>(
    client: &C,
    channel_id: &str,
    content: &str,
) -> Result<ManualDeliveryOutcome, DispatchMessagePostError> {
    let mut last_message_id = None;
    for chunk in split_message(content) {
        let message_id = client.post_message(channel_id, &chunk).await?;
        last_message_id = Some(message_id);
    }
    Ok(ManualDeliveryOutcome::Sent {
        message_id: last_message_id.unwrap_or_default(),
        delivery: Some("chunked"),
    })
}

fn manual_notification_deduper() -> &'static OutboundDeduper {
    static DEDUPER: OnceLock<OutboundDeduper> = OnceLock::new();
    DEDUPER.get_or_init(OutboundDeduper::new)
}

pub async fn send_message(
    registry: &HealthRegistry,
    sqlite: &Db,
    target: &str,
    content: &str,
    source: &str,
    bot: &str,
    summary: Option<&str>,
) -> (&'static str, String) {
    send_message_with_backends(
        registry,
        Some(sqlite),
        None,
        target,
        content,
        source,
        bot,
        summary,
    )
    .await
}

pub async fn handle_send<'a>(
    registry: &HealthRegistry,
    sqlite: &Db,
    body: &str,
) -> (&'a str, String) {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
        return (
            "400 Bad Request",
            r#"{"ok":false,"error":"invalid JSON"}"#.to_string(),
        );
    };

    let target = json.get("target").and_then(|v| v.as_str()).unwrap_or("");
    let content = json.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let source = json
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let bot = json
        .get("bot")
        .and_then(|v| v.as_str())
        .unwrap_or("announce");
    let summary = json.get("summary").and_then(|v| v.as_str());
    let delivery_id = match (
        json.get("correlation_id").and_then(|v| v.as_str()),
        json.get("semantic_event_id").and_then(|v| v.as_str()),
    ) {
        (Some(correlation_id), Some(semantic_event_id))
            if !correlation_id.trim().is_empty() && !semantic_event_id.trim().is_empty() =>
        {
            Some(ManualOutboundDeliveryId {
                correlation_id,
                semantic_event_id,
            })
        }
        _ => None,
    };

    send_message_with_backends_and_delivery_id(
        registry,
        Some(sqlite),
        None,
        target,
        content,
        source,
        bot,
        summary,
        delivery_id,
    )
    .await
}

/// #896: Parsed `/api/inflight/rebind` body, extracted for unit-test
/// coverage of input validation without spinning up a `HealthRegistry`.
#[derive(Debug, PartialEq, Eq)]
struct ParsedRebindRequest {
    provider: crate::services::provider::ProviderKind,
    channel_id: u64,
    tmux_session: Option<String>,
}

/// #896: Parse and validate the rebind request body. Returns a status-tuple
/// error on malformed input so the caller can surface it verbatim.
fn parse_rebind_body(body: &str) -> Result<ParsedRebindRequest, (&'static str, String)> {
    let json: serde_json::Value = serde_json::from_str(body).map_err(|_| {
        (
            "400 Bad Request",
            r#"{"ok":false,"error":"invalid JSON"}"#.to_string(),
        )
    })?;

    let provider_raw = json
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let provider =
        crate::services::provider::ProviderKind::from_str(provider_raw).ok_or_else(|| {
            (
                "400 Bad Request",
                r#"{"ok":false,"error":"provider must be one of: claude, codex"}"#.to_string(),
            )
        })?;

    // Accept channel_id as either a JSON number or a decimal string so
    // callers can forward snowflake IDs without precision loss.
    let channel_id: u64 = match json.get("channel_id") {
        Some(v) if v.is_u64() => v.as_u64().unwrap_or(0),
        Some(v) if v.is_string() => v.as_str().unwrap_or("").trim().parse::<u64>().unwrap_or(0),
        _ => 0,
    };
    if channel_id == 0 {
        return Err((
            "400 Bad Request",
            r#"{"ok":false,"error":"channel_id is required (non-zero u64)"}"#.to_string(),
        ));
    }

    let tmux_session = json
        .get("tmux_session")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    Ok(ParsedRebindRequest {
        provider,
        channel_id,
        tmux_session,
    })
}

/// #896: Handle `POST /api/inflight/rebind` — rebind a live tmux session to
/// a freshly-created inflight state and respawn the watcher. Used to recover
/// orphan states where tmux is still running but turn_bridge has no inflight
/// to track against (e.g. after a prior turn's cleanup cleared the state and
/// the agent continued work via internal auto-triggers).
///
/// Body shape:
/// ```json
/// { "provider": "codex" | "claude",
///   "channel_id": "1234567890",
///   "tmux_session": "AgentDesk-codex-foo"   // optional — derived otherwise
/// }
/// ```
pub async fn handle_rebind_inflight<'a>(
    registry: &HealthRegistry,
    body: &str,
) -> (&'a str, String) {
    let parsed = match parse_rebind_body(body) {
        Ok(p) => p,
        Err((status, body)) => return (status, body),
    };
    let ParsedRebindRequest {
        provider,
        channel_id,
        tmux_session: tmux_override,
    } = parsed;

    let Some(result) = registry
        .rebind_inflight(&provider, channel_id, tmux_override)
        .await
    else {
        // #897 counter-model review: dcserver bootstrap registers the
        // `ProviderEntry` before the provider's Discord HTTP client, so a
        // lookup miss here can mean EITHER permanent misconfiguration OR a
        // transient warmup window. The error text now tells operators to
        // retry instead of assuming the provider is permanently absent.
        return (
            "503 Service Unavailable",
            format!(
                r#"{{"ok":false,"error":"provider {} is not yet available in this dcserver (still warming up or not registered) — retry in a few seconds"}}"#,
                provider.as_str()
            ),
        );
    };

    match result {
        Ok(outcome) => (
            "200 OK",
            serde_json::json!({
                "ok": true,
                "tmux_session": outcome.tmux_session,
                "channel_id": outcome.channel_id.to_string(),
                "initial_offset": outcome.initial_offset,
                "watcher_spawned": outcome.watcher_spawned,
                "watcher_replaced": outcome.watcher_replaced,
            })
            .to_string(),
        ),
        Err(err) => {
            let (status, message) = rebind_error_status_and_message(&err);
            (
                status,
                serde_json::json!({ "ok": false, "error": message }).to_string(),
            )
        }
    }
}

fn rebind_error_status_and_message(
    err: &super::recovery_engine::RebindError,
) -> (&'static str, String) {
    let status = match err {
        super::recovery_engine::RebindError::TmuxNotAlive { .. } => "404 Not Found",
        super::recovery_engine::RebindError::InflightAlreadyExists
        | super::recovery_engine::RebindError::StaleOutputPath { .. } => "409 Conflict",
        super::recovery_engine::RebindError::ChannelNotBound
        | super::recovery_engine::RebindError::ChannelNameMissing => "400 Bad Request",
        super::recovery_engine::RebindError::Internal(_) => "500 Internal Server Error",
    };
    (status, err.to_string())
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedSendToAgentRequest {
    role_id: String,
    message: String,
    mode: String,
}

#[cfg_attr(not(test), allow(dead_code))]
fn parse_send_to_agent_body(body: &str) -> Result<ParsedSendToAgentRequest, &'static str> {
    let json: serde_json::Value = serde_json::from_str(body).map_err(|_| "invalid JSON")?;
    let role_id = json
        .get("role_id")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    if role_id.is_empty() {
        return Err("role_id is required");
    }

    let message = json
        .get("message")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    if message.is_empty() {
        return Err("message is required");
    }

    let mode = json
        .get("mode")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("announce");
    if !matches!(mode, "announce" | "notify") {
        return Err("mode must be announce or notify");
    }

    Ok(ParsedSendToAgentRequest {
        role_id,
        message,
        mode: mode.to_string(),
    })
}

pub async fn handle_send_to_agent(
    registry: &HealthRegistry,
    sqlite: &Db,
    body: &str,
) -> (&'static str, String) {
    let request = match parse_send_to_agent_body(body) {
        Ok(request) => request,
        Err(error) => {
            return (
                "400 Bad Request",
                serde_json::json!({"ok": false, "error": error}).to_string(),
            );
        }
    };

    let target = format!("agent:{}", request.role_id);
    send_message(
        registry,
        sqlite,
        &target,
        &request.message,
        "system",
        &request.mode,
        None,
    )
    .await
}

/// Handle POST /api/senddm — send a DM to a Discord user.
/// Accepts JSON:
/// {"user_id":"...", "content":"...", "bot":"announce|notify|claude|codex"}
pub async fn handle_senddm(registry: &HealthRegistry, body: &str) -> (&'static str, String) {
    let request = match parse_senddm_body(body) {
        Ok(request) => request,
        Err(error) => {
            return (
                "400 Bad Request",
                serde_json::json!({"ok": false, "error": error}).to_string(),
            );
        }
    };

    let http = match resolve_bot_http(registry, &request.bot).await {
        Ok(h) => h,
        Err(resp) => return resp,
    };
    let user_id_text = request.user_id.to_string();

    use poise::serenity_prelude::{CreateMessage, UserId};
    let user_id = UserId::new(request.user_id);
    match user_id.create_dm_channel(&*http).await {
        Ok(dm_channel) => {
            match dm_channel
                .id
                .send_message(&*http, CreateMessage::new().content(&request.content))
                .await
            {
                Ok(_) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!("  [{ts}] 📨 DM: → user {}", request.user_id);
                    (
                        "200 OK",
                        serde_json::json!({
                            "ok": true,
                            "user_id": user_id_text,
                        })
                        .to_string(),
                    )
                }
                Err(e) => (
                    "500 Internal Server Error",
                    format!(r#"{{"ok":false,"error":"DM send failed: {}"}}"#, e),
                ),
            }
        }
        Err(e) => (
            "500 Internal Server Error",
            format!(
                r#"{{"ok":false,"error":"DM channel creation failed: {}"}}"#,
                e
            ),
        ),
    }
}

#[derive(Debug, PartialEq)]
struct SendDmRequest {
    user_id: u64,
    content: String,
    bot: String,
}

fn parse_senddm_body(body: &str) -> Result<SendDmRequest, String> {
    let parsed: serde_json::Value = serde_json::from_str(body).map_err(|_| "invalid JSON")?;
    let user_id = parsed["user_id"]
        .as_str()
        .and_then(|value| value.parse().ok())
        .or_else(|| parsed["user_id"].as_u64())
        .ok_or("user_id required (string or number)")?;
    if user_id == 0 {
        return Err("user_id required (string or number)".to_string());
    }

    let content = parsed["content"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or("content required")?
        .to_string();
    let bot = parsed["bot"].as_str().unwrap_or("announce").to_string();

    Ok(SendDmRequest {
        user_id,
        content,
        bot,
    })
}

/// Self-watchdog: runs on a dedicated OS thread (not tokio) to detect
/// runtime hangs.  Periodically opens a raw TCP connection to the server
/// port and expects a response within a few seconds.  If the check fails
/// `max_failures` times in a row the process is force-killed so launchd
/// (or systemd) can restart it.
pub fn spawn_watchdog(port: u16) {
    const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
    const TCP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    const MAX_FAILURES: u32 = 3;
    // Grace period: skip checks for the first 30s after startup so the
    // runtime has time to initialise Discord bots and register providers.
    const STARTUP_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

    std::thread::Builder::new()
        .name("health-watchdog".into())
        .spawn(move || {
            std::thread::sleep(STARTUP_GRACE);

            let mut consecutive_failures: u32 = 0;

            loop {
                std::thread::sleep(CHECK_INTERVAL);

                let ok = (|| -> bool {
                    use std::io::{Read, Write};
                    let loopback = crate::config::loopback();
                    let addr = format!("{loopback}:{port}");
                    let mut stream =
                        match std::net::TcpStream::connect_timeout(
                            &addr.parse().unwrap(),
                            TCP_TIMEOUT,
                        ) {
                            Ok(s) => s,
                            Err(_) => return false,
                        };
                    let _ = stream.set_read_timeout(Some(TCP_TIMEOUT));
                    let _ = stream.set_write_timeout(Some(TCP_TIMEOUT));
                    let req = format!("GET /api/health HTTP/1.1\r\nHost: {loopback}\r\nConnection: close\r\n\r\n");
                    if stream.write_all(req.as_bytes()).is_err() {
                        return false;
                    }
                    let mut buf = [0u8; 512];
                    match stream.read(&mut buf) {
                        Ok(n) if n > 0 => {
                            // Any HTTP response means the process is alive and serving.
                            // Only TCP failure (Err/_) indicates a true hang/deadlock.
                            // A 503 (degraded/unhealthy state) still means the runtime is
                            // responsive — killing it would create an infinite crash loop
                            // when a provider is temporarily disconnected.
                            true
                        }
                        _ => false,
                    }
                })();

                if ok {
                    if consecutive_failures > 0 {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] 🩺 watchdog: health recovered after {consecutive_failures} failure(s)"
                        );
                    }
                    consecutive_failures = 0;
                } else {
                    consecutive_failures += 1;
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] 🩺 watchdog: health check failed ({consecutive_failures}/{MAX_FAILURES})"
                    );
                    if consecutive_failures >= MAX_FAILURES {
                        tracing::warn!(
                            "  [{ts}] 🩺 watchdog: runtime unresponsive — capturing diagnostics before exit"
                        );
                        // Capture process dump for post-mortem analysis (platform-aware)
                        // Write to runtime root's logs/ dir so dumps survive /tmp cleanup
                        let pid = std::process::id();
                        let dump_dir = crate::agentdesk_runtime_root()
                            .map(|r| r.join("logs"))
                            .unwrap_or_else(|| std::env::temp_dir());
                        let _ = std::fs::create_dir_all(&dump_dir);
                        let dump_path = format!(
                            "{}/adk-hang-{}-{}.txt",
                            dump_dir.display(),
                            pid,
                            chrono::Local::now().format("%Y%m%d-%H%M%S")
                        );
                        match crate::services::platform::capture_process_dump(pid, &dump_path) {
                            Ok(()) => tracing::warn!(
                                "  [{ts}] 🩺 watchdog: dump saved to {dump_path} — forcing exit"
                            ),
                            Err(e) => tracing::warn!(
                                "  [{ts}] 🩺 watchdog: dump capture failed ({e}) — forcing exit without diagnostics"
                            ),
                        }
                        std::process::exit(1);
                    }
                }
            }
        })
        .expect("Failed to spawn watchdog thread");
}

/// Parse a /api/send JSON body and extract (target, content, source).
/// Returns Err with an error message on invalid input.
/// Factored out of handle_send for testability.
#[cfg_attr(not(test), allow(dead_code))]
fn parse_send_body(body: &str) -> Result<(String, String, String), &'static str> {
    let json: serde_json::Value = serde_json::from_str(body).map_err(|_| "invalid JSON")?;
    let content = json
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if content.is_empty() {
        return Err("content is required");
    }
    let target = json
        .get("target")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let source = json
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    Ok((target, content, source))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::DISCORD_MSG_LIMIT;
    use poise::serenity_prelude::{MessageId, UserId};

    fn test_db() -> Db {
        crate::db::test_db()
    }

    #[test]
    fn test_parse_send_request_valid_json() {
        let body = r#"{"target":"channel:123","content":"hello","source":"agent-a"}"#;
        let result = parse_send_body(body);
        assert!(result.is_ok(), "Valid JSON should parse successfully");
        let (target, content, source) = result.unwrap();
        assert_eq!(target, "channel:123");
        assert_eq!(content, "hello");
        assert_eq!(source, "agent-a");
    }

    #[test]
    fn test_parse_send_request_missing_content() {
        let body = r#"{"target":"channel:123"}"#;
        let result = parse_send_body(body);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "content is required");
    }

    #[test]
    fn test_parse_send_request_empty_content() {
        let body = r#"{"target":"channel:123","content":""}"#;
        let result = parse_send_body(body);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "content is required");
    }

    #[test]
    fn test_parse_send_request_invalid_json() {
        let body = "not json at all";
        let result = parse_send_body(body);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "invalid JSON");
    }

    #[test]
    fn test_parse_send_request_missing_target_defaults_empty() {
        let body = r#"{"content":"hello world"}"#;
        let result = parse_send_body(body);
        assert!(result.is_ok());
        let (target, content, source) = result.unwrap();
        assert_eq!(target, "");
        assert_eq!(content, "hello world");
        assert_eq!(source, "unknown");
    }

    #[test]
    fn test_parse_send_request_missing_source_defaults_unknown() {
        let body = r#"{"target":"channel:999","content":"msg"}"#;
        let result = parse_send_body(body);
        assert!(result.is_ok());
        let (_, _, source) = result.unwrap();
        assert_eq!(source, "unknown");
    }

    #[derive(Clone, Default)]
    struct MockManualOutboundClient {
        posts: Arc<std::sync::Mutex<Vec<String>>>,
        attachments: Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>,
    }

    impl DiscordOutboundClient for MockManualOutboundClient {
        async fn post_message(
            &self,
            _target_channel: &str,
            content: &str,
        ) -> Result<String, DispatchMessagePostError> {
            let mut posts = self.posts.lock().unwrap();
            posts.push(content.to_string());
            Ok(format!("message-{}", posts.len()))
        }
    }

    impl ManualOutboundClient for MockManualOutboundClient {
        async fn post_text_attachment(
            &self,
            _target_channel: &str,
            content: &str,
            summary: Option<&str>,
        ) -> Result<String, DispatchMessagePostError> {
            let mut attachments = self.attachments.lock().unwrap();
            attachments.push((content.to_string(), summary.map(str::to_string)));
            Ok(format!("attachment-message-{}", attachments.len()))
        }
    }

    fn boundary_payload(len: usize, fill: char) -> String {
        let tail = format!("tail-{len}");
        format!(
            "{}{tail}",
            fill.to_string().repeat(len.saturating_sub(tail.len()))
        )
    }

    #[tokio::test]
    async fn manual_notification_delivery_skips_duplicate_semantic_event() {
        let client = MockManualOutboundClient::default();
        let dedup = OutboundDeduper::new();
        let delivery_id = ManualOutboundDeliveryId {
            correlation_id: "manual:test",
            semantic_event_id: "manual:test:duplicate",
        };

        let first = deliver_manual_notification(
            &client,
            &dedup,
            "123",
            "hello",
            "announce",
            None,
            Some(delivery_id),
        )
        .await;
        let second = deliver_manual_notification(
            &client,
            &dedup,
            "123",
            "hello",
            "announce",
            None,
            Some(delivery_id),
        )
        .await;

        assert_eq!(
            first,
            ManualDeliveryOutcome::Sent {
                message_id: "message-1".to_string(),
                delivery: None
            }
        );
        assert_eq!(
            second,
            ManualDeliveryOutcome::Sent {
                message_id: String::new(),
                delivery: Some("duplicate")
            }
        );
        assert_eq!(client.posts.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn api_send_announce_long_content_preserves_full_payload_as_attachment() {
        let client = MockManualOutboundClient::default();
        let dedup = OutboundDeduper::new();
        let long = "A".repeat(DISCORD_MSG_LIMIT + 500);

        let (status, body) = send_resolved_manual_message_with_client(
            &client,
            &dedup,
            123,
            "channel:123",
            &long,
            "system",
            "announce",
            None,
            None,
        )
        .await;
        let response: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "200 OK");
        assert_eq!(response["ok"], true);
        assert_eq!(response["bot"], "announce");
        assert_eq!(response["delivery"], "summary+txt");
        assert_eq!(response["message_id"], "attachment-message-1");
        assert!(client.posts.lock().unwrap().is_empty());
        let attachments = client.attachments.lock().unwrap();
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].0, long);
    }

    #[tokio::test]
    async fn api_send_notify_long_content_preserves_full_payload_as_chunks() {
        let client = MockManualOutboundClient::default();
        let dedup = OutboundDeduper::new();
        let long = "B".repeat(DISCORD_MSG_LIMIT + 500);

        let (status, body) = send_resolved_manual_message_with_client(
            &client,
            &dedup,
            123,
            "channel:123",
            &long,
            "system",
            "notify",
            None,
            None,
        )
        .await;
        let response: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "200 OK");
        assert_eq!(response["ok"], true);
        assert_eq!(response["bot"], "notify");
        assert_eq!(response["delivery"], "chunked");
        assert_eq!(response["message_id"], "message-2");
        assert!(client.attachments.lock().unwrap().is_empty());
        let posts = client.posts.lock().unwrap();
        assert_eq!(posts.len(), 2);
        assert!(posts.iter().all(|chunk| chunk.len() <= DISCORD_MSG_LIMIT));
        assert_eq!(posts.concat(), long);
    }

    #[tokio::test]
    async fn api_send_announce_1901_to_2000_preserves_full_payload_without_truncation() {
        for len in [DISCORD_SAFE_LIMIT_CHARS + 1, DISCORD_HARD_LIMIT_CHARS] {
            let client = MockManualOutboundClient::default();
            let dedup = OutboundDeduper::new();
            let content = boundary_payload(len, 'C');

            let (status, body) = send_resolved_manual_message_with_client(
                &client,
                &dedup,
                123,
                "channel:123",
                &content,
                "system",
                "announce",
                None,
                None,
            )
            .await;
            let response: serde_json::Value = serde_json::from_str(&body).unwrap();

            assert_eq!(status, "200 OK");
            assert_eq!(response["bot"], "announce");
            assert!(response.get("delivery").is_none());
            assert!(client.attachments.lock().unwrap().is_empty());
            let posts = client.posts.lock().unwrap();
            assert_eq!(posts.len(), 1);
            assert_eq!(posts[0], content);
            assert!(posts[0].ends_with(&format!("tail-{len}")));
        }
    }

    #[tokio::test]
    async fn api_send_notify_1901_to_2000_preserves_full_payload_without_truncation() {
        for len in [DISCORD_SAFE_LIMIT_CHARS + 1, DISCORD_HARD_LIMIT_CHARS] {
            let client = MockManualOutboundClient::default();
            let dedup = OutboundDeduper::new();
            let content = boundary_payload(len, 'D');

            let (status, body) = send_resolved_manual_message_with_client(
                &client,
                &dedup,
                123,
                "channel:123",
                &content,
                "system",
                "notify",
                None,
                None,
            )
            .await;
            let response: serde_json::Value = serde_json::from_str(&body).unwrap();

            assert_eq!(status, "200 OK");
            assert_eq!(response["bot"], "notify");
            assert!(response.get("delivery").is_none());
            assert!(client.attachments.lock().unwrap().is_empty());
            let posts = client.posts.lock().unwrap();
            assert_eq!(posts.len(), 1);
            assert_eq!(posts[0], content);
            assert!(posts[0].ends_with(&format!("tail-{len}")));
        }
    }

    #[test]
    fn test_parse_send_to_agent_body_defaults_mode_to_announce() {
        let body = r#"{"role_id":"ch-pd","message":"hello"}"#;
        let parsed = parse_send_to_agent_body(body).expect("send_to_agent body should parse");
        assert_eq!(
            parsed,
            ParsedSendToAgentRequest {
                role_id: "ch-pd".to_string(),
                message: "hello".to_string(),
                mode: "announce".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_send_to_agent_body_accepts_notify_mode() {
        let body = r#"{"role_id":"ch-pd","message":"hello","mode":"notify"}"#;
        let parsed = parse_send_to_agent_body(body).expect("send_to_agent body should parse");
        assert_eq!(parsed.mode, "notify");
    }

    #[test]
    fn test_parse_send_to_agent_body_rejects_invalid_mode() {
        let body = r#"{"role_id":"ch-pd","message":"hello","mode":"codex"}"#;
        let error = parse_send_to_agent_body(body).unwrap_err();
        assert_eq!(error, "mode must be announce or notify");
    }

    #[test]
    fn test_parse_senddm_body_without_reply_tracking() {
        let body = r#"{"user_id":"123","content":"hello","bot":"claude"}"#;
        let parsed = parse_senddm_body(body).expect("senddm body should parse");
        assert_eq!(
            parsed,
            SendDmRequest {
                user_id: 123,
                content: "hello".to_string(),
                bot: "claude".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_senddm_body_ignores_reply_tracking_fields() {
        let body = r#"{
            "user_id":"123",
            "content":"건강검진 요즘 했어?",
            "bot":"claude",
            "source_agent":"family-counsel",
            "channel_id":"1473922824350601297",
            "ttl_seconds":86400,
            "context":{"topicKey":"obujang.health_checkup","targetKey":"obujang"}
        }"#;
        let parsed = parse_senddm_body(body).expect("senddm body should parse");
        assert_eq!(parsed.user_id, 123);
        assert_eq!(parsed.content, "건강검진 요즘 했어?");
        assert_eq!(parsed.bot, "claude");
    }

    #[test]
    fn test_resolve_send_target_channel_id_supports_channel_target() {
        let db = test_db();
        let resolved = resolve_send_target_channel_id(&db, "channel:123").unwrap();
        assert_eq!(resolved, 123);
    }

    #[test]
    fn test_resolve_send_target_channel_id_uses_agent_primary_channel_for_claude() {
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, provider, discord_channel_id, discord_channel_alt)
                 VALUES ('agent-claude', 'Claude Agent', 'claude', '111', '222')",
                [],
            )
            .unwrap();
        }

        let resolved = resolve_send_target_channel_id(&db, "agent:agent-claude").unwrap();
        assert_eq!(resolved, 111);
    }

    #[test]
    fn test_resolve_send_target_channel_id_uses_agent_primary_channel_for_codex() {
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, provider, discord_channel_id, discord_channel_alt)
                 VALUES ('agent-codex', 'Codex Agent', 'codex', '111', '222')",
                [],
            )
            .unwrap();
        }

        let resolved = resolve_send_target_channel_id(&db, "agent:agent-codex").unwrap();
        assert_eq!(resolved, 222);
    }

    #[test]
    fn test_resolve_send_target_channel_id_rejects_unknown_agent_target() {
        let db = test_db();
        let err = resolve_send_target_channel_id(&db, "agent:missing").unwrap_err();
        assert_eq!(
            err,
            SendTargetResolutionError::NotFound("unknown agent target: missing".to_string())
        );
    }

    #[tokio::test]
    async fn handle_send_to_agent_returns_not_found_for_unknown_role() {
        let registry = HealthRegistry::new();
        let db = test_db();
        let (status, body) =
            handle_send_to_agent(&registry, &db, r#"{"role_id":"missing","message":"hello"}"#)
                .await;
        assert_eq!(status, "404 Not Found");
        assert!(body.contains("unknown agent target: missing"));
    }

    // #964: `snapshot_watcher_state` powers `GET /api/channels/:id/watcher-state`.
    // #1133 enriched the response with inflight timing/IDs, tmux liveness,
    // mailbox queue depth, and active-turn anchor. The endpoint MUST always
    // return the stable core fields, and `attached` MUST reflect the
    // DashMap entry (not just the relay-coord). When no watcher, no
    // inflight, and no mailbox engagement exist for the channel, the
    // snapshot returns None so the handler can emit 404.
    #[tokio::test]
    async fn snapshot_watcher_state_returns_attached_shape_for_seeded_watcher() {
        let harness = TestHealthHarness::new_with_provider(ProviderKind::Codex).await;
        let channel_id = 523_456_789_012_345_678;
        harness
            .seed_channel_session(channel_id, "watcher-state-snapshot", Some("session-ws"))
            .await;
        let _cancel = harness.seed_watcher(channel_id);

        // Advance the relay-coord watermark so the snapshot surfaces a
        // non-zero offset + timestamp. This exercises the full readout path
        // including the atomic load of last_relay_ts_ms added in #964.
        let coord = harness.shared.tmux_relay_coord(ChannelId::new(channel_id));
        coord
            .confirmed_end_offset
            .store(2048, std::sync::atomic::Ordering::Release);
        coord
            .last_relay_ts_ms
            .store(1_700_000_000_000, std::sync::atomic::Ordering::Release);

        let registry = harness.registry();
        let snapshot = registry
            .snapshot_watcher_state(channel_id)
            .await
            .expect("watcher state snapshot must be present for seeded channel");

        assert_eq!(snapshot.provider, "codex");
        assert!(snapshot.attached);
        assert_eq!(snapshot.watcher_owner_channel_id, Some(channel_id));
        assert_eq!(snapshot.last_relay_offset, 2048);
        assert_eq!(snapshot.last_relay_ts_ms, 1_700_000_000_000);
        // No inflight JSON written to disk → field is false and #1133 inflight
        // diagnostics are absent.
        assert!(!snapshot.inflight_state_present);
        assert!(snapshot.inflight_started_at.is_none());
        assert!(snapshot.inflight_updated_at.is_none());
        assert!(snapshot.inflight_user_msg_id.is_none());
        assert!(snapshot.inflight_current_msg_id.is_none());
        assert_eq!(
            snapshot.tmux_session.as_deref(),
            Some("test-seeded-watcher-523456789012345678")
        );
        assert_eq!(snapshot.tmux_session_alive, Some(false));
        // Idle mailbox: no active turn, no queued interventions.
        assert!(!snapshot.has_pending_queue);
        assert!(snapshot.mailbox_active_user_msg_id.is_none());

        // Serialization shape matches the HTTP response contract. Fields
        // marked `skip_serializing_if = "Option::is_none"` are intentionally
        // omitted when absent — assert only the stable required keys.
        let serialized = serde_json::to_value(&snapshot).expect("serialize watcher snapshot");
        let obj = serialized.as_object().expect("snapshot is a JSON object");
        for field in [
            "provider",
            "attached",
            "last_relay_offset",
            "inflight_state_present",
            "last_relay_ts_ms",
            "has_pending_queue",
            "watcher_owner_channel_id",
        ] {
            assert!(
                obj.contains_key(field),
                "snapshot must expose `{field}` in HTTP response",
            );
        }
        // Optional fields must be absent (skip_serializing_if = Option::is_none)
        // when their underlying source is missing — keeps the JSON tight.
        for absent in [
            "inflight_started_at",
            "inflight_updated_at",
            "inflight_user_msg_id",
            "inflight_current_msg_id",
            "mailbox_active_user_msg_id",
        ] {
            assert!(
                !obj.contains_key(absent),
                "snapshot must omit `{absent}` when no underlying state is present",
            );
        }
    }

    #[tokio::test]
    async fn snapshot_watcher_state_returns_none_for_unknown_channel() {
        let harness = TestHealthHarness::new_with_provider(ProviderKind::Codex).await;
        let registry = harness.registry();
        // No seed: neither watcher, relay-coord, inflight, nor mailbox entry.
        assert!(
            registry
                .snapshot_watcher_state(999_999_999_999_999_999)
                .await
                .is_none(),
            "unknown channel must return None so the HTTP handler can emit 404",
        );
    }

    #[tokio::test]
    async fn snapshot_watcher_state_reports_tmux_owner_for_inflight_channel() {
        let temp = tempfile::tempdir().expect("create temp runtime root");
        let prev_override = crate::config::current_test_runtime_root_override();
        crate::config::set_test_runtime_root_override(Some(temp.path().to_path_buf()));
        struct OverrideGuard(Option<std::path::PathBuf>);
        impl Drop for OverrideGuard {
            fn drop(&mut self) {
                crate::config::set_test_runtime_root_override(self.0.clone());
            }
        }
        let _guard = OverrideGuard(prev_override);

        let harness = TestHealthHarness::new_with_provider(ProviderKind::Codex).await;
        let owner_channel_id = 523_456_789_012_345_679;
        let inflight_channel_id = 523_456_789_012_345_680;
        let tmux_session_name = format!("test-seeded-watcher-{owner_channel_id}");
        harness.seed_watcher(owner_channel_id);

        let inflight = super::super::inflight::InflightTurnState::new(
            ProviderKind::Codex,
            inflight_channel_id,
            Some("watcher-state-owner-diagnostic".to_string()),
            /* request_owner_user_id */ 24,
            /* user_msg_id */ 9_101,
            /* current_msg_id */ 9_102,
            /* user_text */ String::new(),
            /* session_id */ Some("session-owner-diagnostic".to_string()),
            Some(tmux_session_name.clone()),
            /* output_path */ None,
            /* input_fifo_path */ None,
            /* last_offset */ 0,
        );
        super::super::inflight::save_inflight_state(&inflight).expect("write seeded inflight JSON");

        let snapshot = harness
            .registry()
            .snapshot_watcher_state(inflight_channel_id)
            .await
            .expect("inflight tmux owner should surface channel diagnostics");

        assert!(snapshot.attached);
        assert_eq!(snapshot.watcher_owner_channel_id, Some(owner_channel_id));
        assert_eq!(
            snapshot.tmux_session.as_deref(),
            Some(tmux_session_name.as_str())
        );
        assert!(snapshot.inflight_state_present);
    }

    /// #1133: when the mailbox holds an active turn (no watcher attached,
    /// no inflight on disk, no relay-coord), the snapshot must still
    /// surface so operators can see the queue/active-turn state during
    /// pre-watcher windows. The active-turn message ID and queue flag must
    /// reflect the mailbox.
    #[tokio::test]
    async fn snapshot_watcher_state_surfaces_mailbox_active_turn_without_watcher() {
        let harness = TestHealthHarness::new_with_provider(ProviderKind::Codex).await;
        let channel_id = 511_222_333_444_555_666;
        harness
            .seed_channel_session(channel_id, "watcher-state-mailbox", Some("session-mb"))
            .await;
        // Active turn with user_message_id=8675309, plus one queued msg.
        harness.seed_active_turn(channel_id, 42, 8_675_309).await;
        harness
            .seed_queue(channel_id, &[(7777, "queued intervention")])
            .await;

        let registry = harness.registry();
        let snapshot = registry
            .snapshot_watcher_state(channel_id)
            .await
            .expect("mailbox engagement alone must keep the channel visible");

        assert_eq!(snapshot.provider, "codex");
        assert!(!snapshot.attached);
        assert!(!snapshot.inflight_state_present);
        assert!(snapshot.has_pending_queue);
        assert_eq!(snapshot.mailbox_active_user_msg_id, Some(8_675_309));

        let serialized = serde_json::to_value(&snapshot).expect("serialize watcher snapshot");
        let obj = serialized.as_object().expect("snapshot is a JSON object");
        assert_eq!(
            obj.get("mailbox_active_user_msg_id"),
            Some(&serde_json::json!(8_675_309)),
        );
        assert_eq!(obj.get("has_pending_queue"), Some(&serde_json::json!(true)));
    }

    /// #1133: when an inflight JSON is present on disk, the snapshot must
    /// surface its `started_at`, `updated_at`, `user_msg_id`, and
    /// `current_msg_id` (PII-free scalars). Rebind-origin inflights with
    /// zero-valued IDs MUST be filtered out — they do not represent a real
    /// user-authored turn so exposing them would mislead operators.
    #[tokio::test]
    async fn snapshot_watcher_state_includes_inflight_diagnostics_when_persisted() {
        let temp = tempfile::tempdir().expect("create temp runtime root");
        let prev_override = crate::config::current_test_runtime_root_override();
        crate::config::set_test_runtime_root_override(Some(temp.path().to_path_buf()));
        struct OverrideGuard(Option<std::path::PathBuf>);
        impl Drop for OverrideGuard {
            fn drop(&mut self) {
                crate::config::set_test_runtime_root_override(self.0.clone());
            }
        }
        let _guard = OverrideGuard(prev_override);

        let harness = TestHealthHarness::new_with_provider(ProviderKind::Codex).await;
        let channel_id = 600_111_222_333_444_555;

        let mut inflight = super::super::inflight::InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("watcher-state-inflight".to_string()),
            /* request_owner_user_id */ 24,
            /* user_msg_id */ 9_001,
            /* current_msg_id */ 9_002,
            /* user_text */ String::new(),
            /* session_id */ Some("session-inflight-1133".to_string()),
            /* tmux_session_name */
            Some("nonexistent-session-1133-test".to_string()),
            /* output_path */ None,
            /* input_fifo_path */ None,
            /* last_offset */ 0,
        );
        // Pin `started_at` so the assertion is deterministic. `updated_at`
        // is rewritten to `now()` by `save_inflight_state` on every write
        // (see `save_inflight_state_in_root`), so we only check it is
        // surfaced as a non-empty `Some(_)` rather than asserting an exact
        // string.
        inflight.started_at = "2026-04-25 03:00:00".to_string();
        super::super::inflight::save_inflight_state(&inflight).expect("write seeded inflight JSON");

        let registry = harness.registry();
        let snapshot = registry
            .snapshot_watcher_state(channel_id)
            .await
            .expect("inflight on disk must surface a snapshot");

        assert!(snapshot.inflight_state_present);
        assert_eq!(
            snapshot.inflight_started_at.as_deref(),
            Some("2026-04-25 03:00:00")
        );
        let updated_at = snapshot
            .inflight_updated_at
            .as_deref()
            .expect("updated_at must be surfaced when inflight is on disk");
        assert!(
            !updated_at.is_empty(),
            "updated_at must be a non-empty timestamp string"
        );
        assert_eq!(snapshot.inflight_user_msg_id, Some(9_001));
        assert_eq!(snapshot.inflight_current_msg_id, Some(9_002));
        assert_eq!(
            snapshot.tmux_session.as_deref(),
            Some("nonexistent-session-1133-test")
        );
        // Session was never created, so tmux::has_session returns false. The
        // field is Some(false), not None, because we DID know a session name
        // to probe.
        assert_eq!(snapshot.tmux_session_alive, Some(false));
    }

    #[tokio::test]
    async fn hard_stop_runtime_turn_uses_mailbox_canonical_cleanup_when_runtime_online() {
        let harness = TestHealthHarness::new().await;
        let channel_id = 123_456_789_012_345_678;
        harness
            .seed_channel_session(channel_id, "hard-stop-runtime", Some("session-live"))
            .await;
        harness.seed_active_turn(channel_id, 99, 101).await;
        harness
            .seed_queue(channel_id, &[(7001, "preserve me")])
            .await;
        harness.insert_dispatch_role_override(channel_id, 987_654_321_098_765_432);

        let registry = harness.registry();
        let result = hard_stop_runtime_turn(
            Some(registry.as_ref()),
            Some("claude"),
            Some(channel_id),
            None,
            "test hard stop",
        )
        .await;

        assert_eq!(result.cleanup_path, "mailbox_canonical");
        assert!(result.had_active_turn);
        assert!(result.has_pending_queue);
        assert!(result.runtime_session_cleared);

        let (has_active_turn, queue_depth, session_id) = harness.mailbox_state(channel_id).await;
        assert!(!has_active_turn);
        assert_eq!(queue_depth, 1);
        assert_eq!(session_id, None);
        assert!(harness.has_dispatch_role_override(channel_id));
    }

    #[tokio::test]
    async fn hard_stop_runtime_turn_removes_dispatch_override_when_queue_is_empty() {
        let harness = TestHealthHarness::new().await;
        let channel_id = 223_456_789_012_345_678;
        harness
            .seed_channel_session(channel_id, "hard-stop-empty", Some("session-empty"))
            .await;
        harness.seed_active_turn(channel_id, 77, 88).await;
        harness.insert_dispatch_role_override(channel_id, 887_654_321_098_765_432);

        let registry = harness.registry();
        let result = hard_stop_runtime_turn(
            Some(registry.as_ref()),
            Some("claude"),
            Some(channel_id),
            None,
            "test hard stop",
        )
        .await;

        assert_eq!(result.cleanup_path, "mailbox_canonical");
        assert!(result.had_active_turn);
        assert!(!result.has_pending_queue);
        assert!(result.runtime_session_cleared);

        let (has_active_turn, queue_depth, session_id) = harness.mailbox_state(channel_id).await;
        assert!(!has_active_turn);
        assert_eq!(queue_depth, 0);
        assert_eq!(session_id, None);
        assert!(!harness.has_dispatch_role_override(channel_id));
    }

    #[tokio::test]
    async fn health_snapshot_reports_observability_metrics_and_degraded_queue_state() {
        let harness = TestHealthHarness::new().await;
        harness.set_deferred_hooks(2);
        harness.set_recovery_duration_ms(4_250);
        harness.set_queue_depth(3).await;

        let snapshot = build_health_snapshot(&harness.registry()).await;
        let json = serde_json::to_value(&snapshot).unwrap();

        assert_eq!(snapshot.status(), HealthStatus::Degraded);
        assert_eq!(json["deferred_hooks"], 2);
        assert_eq!(json["queue_depth"], 3);
        assert_eq!(json["watcher_count"], 0);
        assert_eq!(json["recovery_duration"], 4.25);
        assert!(
            json["degraded_reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|reason| reason == "provider:claude:deferred_hooks_backlog:2")
        );
        assert!(
            json["degraded_reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|reason| reason == "provider:claude:pending_queue_depth:3")
        );
    }

    #[tokio::test]
    async fn public_health_snapshot_omits_detail_mailbox_entries() {
        let harness = TestHealthHarness::new().await;
        let channel_id = 713_000_000_000_000_001u64;
        harness.seed_active_turn(channel_id, 42, 9_001).await;

        let public_snapshot = build_public_health_snapshot(&harness.registry()).await;
        let detail_snapshot = build_health_snapshot(&harness.registry()).await;
        let public_json = serde_json::to_value(&public_snapshot).unwrap();
        let detail_json = serde_json::to_value(&detail_snapshot).unwrap();

        assert_eq!(public_json["providers"][0]["active_turns"], 1);
        assert_eq!(public_json["mailboxes"].as_array().unwrap().len(), 0);
        assert_eq!(detail_json["mailboxes"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn health_snapshot_keeps_fully_recovered_true_for_disconnected_provider_after_startup() {
        let harness = TestHealthHarness::new().await;
        harness.set_connected(false);

        let snapshot = build_health_snapshot(&harness.registry()).await;
        let json = serde_json::to_value(&snapshot).unwrap();

        assert_eq!(snapshot.status(), HealthStatus::Unhealthy);
        assert_eq!(json["fully_recovered"], true);
        assert!(
            json["degraded_reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|reason| reason == "provider:claude:disconnected")
        );
    }

    #[tokio::test]
    async fn health_snapshot_keeps_fully_recovered_true_for_restart_pending_after_startup() {
        let harness = TestHealthHarness::new().await;
        harness.set_restart_pending(true);

        let snapshot = build_health_snapshot(&harness.registry()).await;
        let json = serde_json::to_value(&snapshot).unwrap();

        assert_eq!(snapshot.status(), HealthStatus::Unhealthy);
        assert_eq!(json["fully_recovered"], true);
        assert!(
            json["degraded_reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|reason| reason == "provider:claude:restart_pending")
        );
    }

    #[tokio::test]
    async fn runtime_stop_fallback_preserves_mailbox_queue() {
        let harness = TestHealthHarness::new().await;
        let channel_id = 777_000_000_000_000_001u64;
        harness
            .set_queue_depth_for_channel(channel_id, ProviderKind::Claude, 2)
            .await;
        harness
            .start_active_turn(channel_id, 7, 70, Some("missing-runtime-stop"))
            .await;

        let result = stop_provider_channel_runtime(
            harness.registry().as_ref(),
            "claude",
            ChannelId::new(channel_id),
            "test runtime fallback",
        )
        .await
        .expect("runtime stop should resolve provider");

        assert_eq!(result.lifecycle_path, "runtime-fallback");
        assert_eq!(result.queue_depth, 2);
        assert_eq!(harness.queue_depth_for_channel(channel_id).await, 2);
        assert!(
            !harness
                .shared()
                .mailbox(ChannelId::new(channel_id))
                .has_active_turn()
                .await,
            "fallback cleanup should clear the active turn",
        );
    }

    #[tokio::test]
    async fn direct_meeting_mailboxes_allow_parallel_turns_on_different_provider_channels() {
        let registry = Arc::new(HealthRegistry::new());
        let codex = TestHealthHarness::new_with_provider(ProviderKind::Codex).await;
        let claude = TestHealthHarness::new_with_provider(ProviderKind::Claude).await;
        registry
            .register(ProviderKind::Codex.as_str().to_string(), codex.shared())
            .await;
        registry
            .register(ProviderKind::Claude.as_str().to_string(), claude.shared())
            .await;

        let codex_channel = ChannelId::new(777_000_000_000_000_101);
        let claude_channel = ChannelId::new(777_000_000_000_000_102);
        let codex_shared =
            resolve_direct_meeting_shared(registry.as_ref(), codex_channel, &ProviderKind::Codex)
                .await
                .expect("codex runtime should resolve");
        let claude_shared =
            resolve_direct_meeting_shared(registry.as_ref(), claude_channel, &ProviderKind::Claude)
                .await
                .expect("claude runtime should resolve");

        let codex_started = codex_shared
            .mailbox(codex_channel)
            .try_start_turn(
                Arc::new(crate::services::provider::CancelToken::new()),
                UserId::new(7),
                MessageId::new(70),
            )
            .await;
        let claude_started = claude_shared
            .mailbox(claude_channel)
            .try_start_turn(
                Arc::new(crate::services::provider::CancelToken::new()),
                UserId::new(8),
                MessageId::new(80),
            )
            .await;

        assert!(
            codex_started,
            "first provider/channel mailbox should accept a turn"
        );
        assert!(
            claude_started,
            "a different provider/channel mailbox should not be blocked by the first turn"
        );
    }

    #[tokio::test]
    async fn resolve_bot_http_reports_missing_notify_bot_token() {
        let harness = TestHealthHarness::new().await;

        let err = resolve_bot_http(harness.registry().as_ref(), "notify")
            .await
            .unwrap_err();

        assert_eq!(err.0, "503 Service Unavailable");
        assert!(err.1.contains("notify bot not configured"));
    }

    #[test]
    fn select_direct_meeting_runtime_candidate_prefers_explicit_channel_match() {
        let selected = select_direct_meeting_runtime_candidate(
            "claude",
            ChannelId::new(123),
            &[
                DirectMeetingRuntimeCandidate {
                    index: 0,
                    explicit_channel_match: false,
                    live_channel_match: true,
                },
                DirectMeetingRuntimeCandidate {
                    index: 1,
                    explicit_channel_match: true,
                    live_channel_match: true,
                },
            ],
        )
        .expect("selection should succeed");

        assert_eq!(selected, Some(1));
    }

    #[test]
    fn select_direct_meeting_runtime_candidate_rejects_ambiguous_explicit_matches() {
        let err = select_direct_meeting_runtime_candidate(
            "claude",
            ChannelId::new(123),
            &[
                DirectMeetingRuntimeCandidate {
                    index: 0,
                    explicit_channel_match: true,
                    live_channel_match: true,
                },
                DirectMeetingRuntimeCandidate {
                    index: 1,
                    explicit_channel_match: true,
                    live_channel_match: false,
                },
            ],
        )
        .expect_err("ambiguous explicit matches must fail");

        assert!(err.contains("multiple runtimes explicitly allow channel 123"));
    }

    /// #896: `parse_rebind_body` must reject malformed inputs with
    /// 400 statuses before we touch any runtime state. Each case here is
    /// an operator footgun that would otherwise raise an opaque error
    /// deeper in the rebind flow.
    #[test]
    fn parse_rebind_body_rejects_invalid_json() {
        let err = parse_rebind_body("{not-json").expect_err("malformed JSON must fail");
        assert_eq!(err.0, "400 Bad Request");
        assert!(err.1.contains("invalid JSON"));
    }

    #[test]
    fn parse_rebind_body_rejects_unknown_provider() {
        let body = r#"{"provider":"gpt","channel_id":"123"}"#;
        let err = parse_rebind_body(body).expect_err("unknown provider must fail");
        assert_eq!(err.0, "400 Bad Request");
        assert!(err.1.contains("provider"));
    }

    #[test]
    fn parse_rebind_body_rejects_missing_channel_id() {
        let body = r#"{"provider":"codex"}"#;
        let err = parse_rebind_body(body).expect_err("missing channel_id must fail");
        assert_eq!(err.0, "400 Bad Request");
        assert!(err.1.contains("channel_id"));
    }

    #[test]
    fn parse_rebind_body_rejects_zero_channel_id() {
        let body = r#"{"provider":"codex","channel_id":"0"}"#;
        let err = parse_rebind_body(body).expect_err("channel_id=0 must fail");
        assert_eq!(err.0, "400 Bad Request");
        assert!(err.1.contains("channel_id"));
    }

    #[test]
    fn parse_rebind_body_accepts_numeric_and_string_channel_id() {
        // Snowflakes exceed i32::MAX so both numeric and string forms
        // must be accepted to avoid precision loss on the caller side.
        for body in [
            r#"{"provider":"codex","channel_id":1490141485167808532}"#,
            r#"{"provider":"codex","channel_id":"1490141485167808532"}"#,
        ] {
            let parsed = parse_rebind_body(body).expect("snowflake channel_id must parse");
            assert_eq!(parsed.channel_id, 1490141485167808532);
            assert_eq!(parsed.provider.as_str(), "codex");
            assert!(parsed.tmux_session.is_none());
        }
    }

    #[test]
    fn parse_rebind_body_keeps_explicit_tmux_session_override() {
        let body =
            r#"{"provider":"claude","channel_id":"42","tmux_session":"AgentDesk-claude-foo"}"#;
        let parsed = parse_rebind_body(body).expect("body should parse");
        assert_eq!(parsed.provider.as_str(), "claude");
        assert_eq!(parsed.channel_id, 42);
        assert_eq!(
            parsed.tmux_session.as_deref(),
            Some("AgentDesk-claude-foo"),
            "explicit tmux_session must survive into rebind call"
        );
    }

    #[test]
    fn parse_rebind_body_treats_blank_tmux_session_as_absent() {
        let body = r#"{"provider":"claude","channel_id":"42","tmux_session":"  "}"#;
        let parsed = parse_rebind_body(body).expect("body should parse");
        assert!(
            parsed.tmux_session.is_none(),
            "whitespace-only override must fall back to auto-derivation"
        );
    }

    #[test]
    fn rebind_stale_output_path_surfaces_as_conflict() {
        let err = crate::services::discord::recovery_engine::RebindError::StaleOutputPath {
            tmux_session: "AgentDesk-codex-adk-cdx".to_string(),
            output_path: "/tmp/current.jsonl".to_string(),
            live_fd: "5w".to_string(),
            live_inode: Some(4_242),
            live_path: "/tmp/current.jsonl (deleted)".to_string(),
        };

        let (status, message) = rebind_error_status_and_message(&err);

        assert_eq!(status, "409 Conflict");
        assert!(message.contains("StaleOutputPath"));
        assert!(message.contains("fd 5w"));
    }
}
