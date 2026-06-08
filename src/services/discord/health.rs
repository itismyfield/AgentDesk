use std::sync::Arc;
use std::time::{Duration, Instant};

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, CreateMessage};
use sqlx::PgPool;

use super::SharedData;
use super::formatting::{build_long_message_attachment, split_message};
use crate::db::Db;
use crate::services::discord::outbound::delivery::{
    deliver_outbound as deliver_v3_outbound, first_raw_message_id,
};
use crate::services::discord::outbound::message::{DiscordOutboundMessage, OutboundTarget};
use crate::services::discord::outbound::policy::DiscordOutboundPolicy;
use crate::services::discord::outbound::result::{DeliveryResult, FallbackUsed};
use crate::services::discord::outbound::{
    DISCORD_HARD_LIMIT_CHARS, DISCORD_SAFE_LIMIT_CHARS, DiscordOutboundClient, OutboundDedupClaim,
    OutboundDedupReservation, OutboundDedupWait, OutboundDeduper, shared_outbound_deduper,
};
use crate::services::dispatches::discord_delivery::{
    DispatchMessagePostError, DispatchMessagePostErrorKind,
};
use crate::services::provider::ProviderKind;

mod mailbox;
mod provider_probe;
mod recovery;
mod redaction;
mod session_enrichment;
mod snapshot;

pub(crate) use recovery::stop_provider_channel_runtime_with_policy;
#[allow(unused_imports)]
pub use recovery::{
    HardStopRuntimeResult, IdleTmuxStaleTurnRepairResult, PendingQueueSnapshot,
    PostCancelDrainOutcome, ProviderMailboxState, RuntimeTurnStopResult,
    clear_idle_tmux_stale_turn, clear_provider_channel_runtime,
    finish_cancelled_provider_channel_mailbox, force_kill_provider_channel_runtime,
    handle_rebind_inflight, handle_relay_recovery, hard_stop_runtime_turn,
    provider_channel_mailbox_state, resolve_tmux_session_for_cancel,
    schedule_pending_queue_drain_after_cancel, snapshot_pending_queue_state, spawn_stall_watchdog,
    spawn_watchdog, stop_provider_channel_runtime, stop_runtime_turn_preserving_watcher,
};
#[allow(unused_imports)]
pub use snapshot::{
    DiscordHealthSnapshot, HealthStatus, WatcherStateSnapshot, active_request_owner_for_channel,
    build_health_snapshot, build_public_health_snapshot,
};

/// Per-provider snapshot for the health response.
pub(super) struct ProviderEntry {
    pub(super) name: String,
    pub(super) shared: Arc<SharedData>,
}

/// Registry that providers register with so the unified axum API can query all of them.
/// Also holds Discord HTTP clients for agent-to-agent message routing.
pub struct HealthRegistry {
    providers: tokio::sync::Mutex<Vec<ProviderEntry>>,
    started_at: Instant,
    /// Wall-clock (Unix seconds) at which this dcserver process booted.
    /// `started_at` is a monotonic `Instant` and cannot be compared against
    /// the Unix timestamps parsed from inflight `updated_at` strings, so the
    /// stall watchdog uses this field to grant a post-restart grace window:
    /// an inflight row that went stale *before* the restart must not be
    /// force-cleaned until the watcher has had a full staleness window after
    /// boot to re-sync (#3041).
    started_at_unix: i64,
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
            started_at_unix: chrono::Utc::now().timestamp(),
            discord_http: tokio::sync::Mutex::new(Vec::new()),
            announce_http: tokio::sync::Mutex::new(None),
            announce_user_id: tokio::sync::Mutex::new(None),
            notify_http: tokio::sync::Mutex::new(None),
            notify_user_id: tokio::sync::Mutex::new(None),
        }
    }

    /// Wall-clock Unix seconds at which this dcserver process booted. Used by
    /// the stall watchdog to anchor its post-restart grace window (#3041).
    pub(crate) fn started_at_unix(&self) -> i64 {
        self.started_at_unix
    }

    /// Snapshot the notify-bot HTTP client (for non-actionable side channels
    /// like the idle-recap renderer). Returns `None` when the notify bot
    /// hasn't been registered yet — caller treats that as "skip the post
    /// this cycle".
    pub(crate) async fn notify_http_clone(&self) -> Option<Arc<serenity::Http>> {
        self.notify_http.lock().await.clone()
    }

    /// Snapshot the announce-bot HTTP client. The announce bot is where
    /// `Manage Messages` (and other channel-mod) permissions are concentrated
    /// in this deployment, so pin/unpin lifecycle code prefers it over the
    /// per-provider bot http to avoid the `Missing Permissions` 403 storm we
    /// otherwise see on terminal-relay placeholder cleanup.
    pub(crate) async fn announce_http_clone(&self) -> Option<Arc<serenity::Http>> {
        self.announce_http.lock().await.clone()
    }

    pub(super) async fn register(&self, name: String, shared: Arc<SharedData>) {
        let mut providers = self.providers.lock().await;
        if providers
            .iter()
            .any(|entry| std::sync::Arc::ptr_eq(&entry.shared, &shared))
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ duplicate health runtime registration ignored: {}",
                name
            );
            return;
        }
        if providers.iter().any(|entry| entry.name == name) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🩺 registering additional health runtime for provider: {}",
                name
            );
        }
        providers.push(ProviderEntry { name, shared });
    }

    async fn dm_default_agent_authorizes_private_channel(
        &self,
        channel_id: ChannelId,
        is_private_channel: bool,
        source: &str,
    ) -> bool {
        if !is_private_channel {
            return false;
        }

        let shared_runtimes: Vec<Arc<SharedData>> = self
            .providers
            .lock()
            .await
            .iter()
            .map(|entry| entry.shared.clone())
            .collect();

        for shared in shared_runtimes {
            let provider = { shared.settings.read().await.provider.clone() };
            let session_bound = {
                let data = shared.core.lock().await;
                data.sessions.contains_key(&channel_id)
            };
            if dm_default_agent_authorizes_unmapped_private_channel(
                is_private_channel,
                source,
                &provider,
                session_bound,
            ) {
                return true;
            }
        }

        false
    }

    pub(super) async fn registered_provider_count(&self) -> usize {
        self.providers.lock().await.len()
    }

    pub(in crate::services::discord) async fn shared_for_provider(
        &self,
        provider: &ProviderKind,
    ) -> Option<Arc<SharedData>> {
        self.providers
            .lock()
            .await
            .iter()
            .find(|entry| entry.name.eq_ignore_ascii_case(provider.as_str()))
            .map(|entry| entry.shared.clone())
    }

    /// Channel-aware variant of [`Self::shared_for_provider`].
    ///
    /// Once `register` stopped deduping by provider name (multi-bot
    /// deployments register several runtimes under the same provider),
    /// the name-only lookup above resolves whichever runtime registered
    /// first. Recovery/relay paths that are scoped to a single channel
    /// would then stop, drain, or relay against the *wrong* runtime's
    /// mailbox/inflight for that channel — the turn looks cut off and
    /// progress stops updating for the other bot.
    ///
    /// This disambiguates by the runtime's allowed/live channel set via
    /// the same selection logic `resolve_direct_meeting_shared` uses. For
    /// a single registered runtime it returns that runtime regardless of
    /// channel, so single-bot deployments behave exactly as before.
    pub(in crate::services::discord) async fn shared_for_provider_on_channel(
        &self,
        provider: &ProviderKind,
        channel_id: ChannelId,
    ) -> Option<Arc<SharedData>> {
        resolve_direct_meeting_shared(self, channel_id, provider)
            .await
            .ok()
    }

    /// Every runtime registered under `provider`'s name.
    ///
    /// `shared_for_provider` returns only the first-registered runtime,
    /// which is correct for channel-scoped lookups (paired with
    /// `shared_for_provider_on_channel`) but wrong for provider-global
    /// sweeps like the stall watchdog: in a multi-bot deployment the
    /// later-registered runtime's channels would never be visited, so its
    /// stalled turns would never be force-cleaned (turn looks cut off,
    /// progress stops updating). Callers that must touch every runtime use
    /// this and then resolve the owning runtime per channel.
    pub(in crate::services::discord) async fn all_shared_for_provider(
        &self,
        provider: &ProviderKind,
    ) -> Vec<Arc<SharedData>> {
        self.providers
            .lock()
            .await
            .iter()
            .filter(|entry| entry.name.eq_ignore_ascii_case(provider.as_str()))
            .map(|entry| entry.shared.clone())
            .collect()
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
        // Channel-aware: multi-bot deployments register several runtimes
        // under one provider name, so a first-match-by-name lookup would
        // rebind whichever runtime registered first instead of the one
        // that actually owns `channel_id`, leaving the real runtime's
        // orphan inflight untouched (turn stuck, no progress). This reuses
        // the same selection logic as the direct-meeting resolver and
        // falls back to the single registered runtime when only one
        // exists, so single-bot behaviour is unchanged.
        let (http, shared) =
            resolve_direct_meeting_runtime(self, ChannelId::new(channel_id), provider)
                .await
                .ok()?;
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
        self.reload_bot_tokens_inner(false).await;
    }

    /// Issue #2047 Finding 11 — operator-triggered token rotation.
    ///
    /// Re-read the announce/notify credential files and rebuild the
    /// `serenity::Http` clients in place. The previous tokens cached in
    /// `announce_http` / `notify_http` are replaced atomically (per-mutex)
    /// and the cached user ids are cleared so the next call to
    /// `utility_bot_user_id` re-derives them against the new token.
    ///
    /// Returns a tuple `(announce_loaded, notify_loaded)` so callers can
    /// surface a clean status. Tokens that fail [`crate::credential::is_valid_bot_name`]
    /// or whose credential file is absent leave the corresponding HTTP slot
    /// untouched (caller can decide whether to treat that as an error).
    pub async fn reload_bot_tokens(&self) -> (bool, bool) {
        self.reload_bot_tokens_inner(true).await
    }

    async fn reload_bot_tokens_inner(&self, rotation: bool) -> (bool, bool) {
        let mut announce_loaded = false;
        let mut notify_loaded = false;
        if super::runtime_store::agentdesk_root().is_some() {
            for (bot_name, http_field, user_id_field, loaded_flag) in [
                (
                    "announce",
                    &self.announce_http,
                    &self.announce_user_id,
                    &mut announce_loaded,
                ),
                (
                    "notify",
                    &self.notify_http,
                    &self.notify_user_id,
                    &mut notify_loaded,
                ),
            ] {
                if let Some(token) = crate::credential::read_bot_token(bot_name) {
                    let http = Arc::new(serenity::Http::new(&format!("Bot {token}")));
                    *http_field.lock().await = Some(http);
                    // Invalidate the cached user-id so the next utility call
                    // re-resolves it via the rotated token; otherwise a stale
                    // id from a revoked bot account could leak into routing.
                    *user_id_field.lock().await = None;
                    *loaded_flag = true;
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    let emoji = if bot_name == "announce" {
                        "📢"
                    } else {
                        "🔔"
                    };
                    let action = if rotation { "reloaded" } else { "loaded" };
                    tracing::info!(
                        "  [{ts}] {emoji} {bot_name} bot {action} for /api/discord/send routing"
                    );
                } else if rotation {
                    tracing::warn!(
                        bot = bot_name,
                        "reload_bot_tokens: credential file missing or invalid; keeping previous client"
                    );
                }
            }
        } else if rotation {
            tracing::warn!("reload_bot_tokens called before agentdesk runtime root is initialised");
        }
        (announce_loaded, notify_loaded)
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
                if bot_names_match(name, provider) {
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

fn bot_names_match(registered: &str, requested: &str) -> bool {
    let registered = registered.trim();
    let requested = requested.trim();
    if registered == requested || registered.eq_ignore_ascii_case(requested) {
        return true;
    }

    match (
        ProviderKind::from_str(registered),
        ProviderKind::from_str(requested),
    ) {
        (Some(left), Some(right)) => left == right,
        _ => false,
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
    let reservation = reserve_headless_agent_turn(channel_id);
    start_reserved_headless_agent_turn(
        registry,
        channel_id,
        owner_provider,
        prompt,
        source,
        metadata,
        channel_name_hint,
        reservation,
    )
    .await
}

#[derive(Debug, Clone)]
pub struct HeadlessAgentTurnReservation {
    channel_id: ChannelId,
    turn_id: String,
    inner: super::router::HeadlessTurnReservation,
}

impl HeadlessAgentTurnReservation {
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }
}

pub fn reserve_headless_agent_turn(channel_id: ChannelId) -> HeadlessAgentTurnReservation {
    let inner = super::router::reserve_headless_turn();
    HeadlessAgentTurnReservation {
        channel_id,
        turn_id: inner.turn_id(channel_id),
        inner,
    }
}

pub async fn start_reserved_headless_agent_turn(
    registry: &HealthRegistry,
    channel_id: ChannelId,
    owner_provider: ProviderKind,
    prompt: String,
    source: Option<String>,
    metadata: Option<serde_json::Value>,
    channel_name_hint: Option<String>,
    reservation: HeadlessAgentTurnReservation,
) -> Result<super::router::HeadlessTurnStartOutcome, super::router::HeadlessTurnStartError> {
    if reservation.channel_id != channel_id {
        return Err(super::router::HeadlessTurnStartError::Internal(format!(
            "headless turn reservation channel mismatch: reserved {} but starting {}",
            reservation.channel_id.get(),
            channel_id.get()
        )));
    }

    let expected_turn_id = reservation.turn_id.clone();
    let shared = resolve_direct_meeting_shared(registry, channel_id, &owner_provider)
        .await
        .map_err(super::router::HeadlessTurnStartError::Internal)?;

    start_reserved_headless_agent_turn_with_shared(
        shared,
        channel_id,
        owner_provider,
        prompt,
        source,
        metadata,
        channel_name_hint,
        None,
        reservation,
        expected_turn_id,
    )
    .await
}

pub async fn start_headless_agent_turn_in_dm(
    registry: &HealthRegistry,
    owner_channel_id: ChannelId,
    dm_user_id: u64,
    owner_provider: ProviderKind,
    prompt: String,
    source: Option<String>,
    metadata: Option<serde_json::Value>,
) -> Result<super::router::HeadlessTurnStartOutcome, super::router::HeadlessTurnStartError> {
    let (_, shared) = resolve_direct_meeting_runtime(registry, owner_channel_id, &owner_provider)
        .await
        .map_err(super::router::HeadlessTurnStartError::Internal)?;
    let ctx = shared.cached_serenity_ctx.get().cloned().ok_or_else(|| {
        super::router::HeadlessTurnStartError::Internal(format!(
            "provider runtime is not ready for channel {}",
            owner_channel_id.get()
        ))
    })?;
    let dm_channel = serenity::UserId::new(dm_user_id)
        .create_dm_channel(&ctx.http)
        .await
        .map_err(|error| {
            super::router::HeadlessTurnStartError::Internal(format!(
                "DM channel creation failed for user {dm_user_id}: {error}"
            ))
        })?;
    let dm_channel_id = dm_channel.id;
    let reservation = reserve_headless_agent_turn(dm_channel_id);
    let expected_turn_id = reservation.turn_id.clone();
    let channel_name_hint = Some(format!("dm-{dm_user_id}"));

    start_reserved_headless_agent_turn_with_shared(
        shared,
        dm_channel_id,
        owner_provider,
        prompt,
        source,
        metadata,
        channel_name_hint,
        Some(true),
        reservation,
        expected_turn_id,
    )
    .await
}

pub async fn reserve_headless_agent_turn_in_dm(
    registry: &HealthRegistry,
    owner_channel_id: ChannelId,
    dm_user_id: u64,
    owner_provider: &ProviderKind,
) -> Result<(ChannelId, HeadlessAgentTurnReservation), super::router::HeadlessTurnStartError> {
    let (_, shared) = resolve_direct_meeting_runtime(registry, owner_channel_id, owner_provider)
        .await
        .map_err(super::router::HeadlessTurnStartError::Internal)?;
    let ctx = shared.cached_serenity_ctx.get().cloned().ok_or_else(|| {
        super::router::HeadlessTurnStartError::Internal(format!(
            "provider runtime is not ready for channel {}",
            owner_channel_id.get()
        ))
    })?;
    let dm_channel = serenity::UserId::new(dm_user_id)
        .create_dm_channel(&ctx.http)
        .await
        .map_err(|error| {
            super::router::HeadlessTurnStartError::Internal(format!(
                "DM channel creation failed for user {dm_user_id}: {error}"
            ))
        })?;
    let dm_channel_id = dm_channel.id;
    Ok((dm_channel_id, reserve_headless_agent_turn(dm_channel_id)))
}

pub async fn start_reserved_headless_agent_turn_in_dm(
    registry: &HealthRegistry,
    owner_channel_id: ChannelId,
    dm_channel_id: ChannelId,
    dm_user_id: u64,
    owner_provider: ProviderKind,
    prompt: String,
    source: Option<String>,
    metadata: Option<serde_json::Value>,
    reservation: HeadlessAgentTurnReservation,
) -> Result<super::router::HeadlessTurnStartOutcome, super::router::HeadlessTurnStartError> {
    if reservation.channel_id != dm_channel_id {
        return Err(super::router::HeadlessTurnStartError::Internal(format!(
            "headless turn reservation channel mismatch: reserved {} but starting {}",
            reservation.channel_id.get(),
            dm_channel_id.get()
        )));
    }

    let (_, shared) = resolve_direct_meeting_runtime(registry, owner_channel_id, &owner_provider)
        .await
        .map_err(super::router::HeadlessTurnStartError::Internal)?;
    let expected_turn_id = reservation.turn_id.clone();
    let channel_name_hint = Some(format!("dm-{dm_user_id}"));

    start_reserved_headless_agent_turn_with_shared(
        shared,
        dm_channel_id,
        owner_provider,
        prompt,
        source,
        metadata,
        channel_name_hint,
        Some(true),
        reservation,
        expected_turn_id,
    )
    .await
}

async fn start_reserved_headless_agent_turn_with_shared(
    shared: Arc<SharedData>,
    channel_id: ChannelId,
    _owner_provider: ProviderKind,
    prompt: String,
    source: Option<String>,
    metadata: Option<serde_json::Value>,
    channel_name_hint: Option<String>,
    is_dm_hint: Option<bool>,
    reservation: HeadlessAgentTurnReservation,
    expected_turn_id: String,
) -> Result<super::router::HeadlessTurnStartOutcome, super::router::HeadlessTurnStartError> {
    if reservation.channel_id != channel_id {
        return Err(super::router::HeadlessTurnStartError::Internal(format!(
            "headless turn reservation channel mismatch: reserved {} but starting {}",
            reservation.channel_id.get(),
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

    let outcome = super::router::start_reserved_headless_turn(
        &ctx,
        channel_id,
        &prompt,
        source.as_deref().unwrap_or("system"),
        &shared,
        &token,
        source.as_deref(),
        metadata,
        channel_name_hint,
        is_dm_hint,
        reservation.inner,
    )
    .await?;

    if outcome.turn_id != expected_turn_id {
        return Err(super::router::HeadlessTurnStartError::Internal(format!(
            "reserved headless turn id mismatch: expected {} but started {}",
            expected_turn_id, outcome.turn_id
        )));
    }

    Ok(outcome)
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
        .or_else(|| crate::services::dispatches::outbox_route::resolve_channel_alias_pub(trimmed))
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

async fn routine_thread_parent_hint(
    pg_pool: Option<&PgPool>,
    thread_channel_id: ChannelId,
) -> Option<ChannelId> {
    let Some(pg_pool) = pg_pool else {
        return None;
    };

    let agent_id = match sqlx::query_scalar::<_, String>(
        r#"
        SELECT agent_id
          FROM routines
         WHERE discord_thread_id = $1
           AND agent_id IS NOT NULL
           AND status <> 'detached'
         ORDER BY updated_at DESC
         LIMIT 1
        "#,
    )
    .bind(thread_channel_id.get().to_string())
    .fetch_optional(pg_pool)
    .await
    {
        Ok(Some(agent_id)) => agent_id,
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!(
                "routine thread auth lookup failed for {}: {}",
                thread_channel_id.get(),
                error
            );
            return None;
        }
    };

    let bindings = match crate::db::agents::load_agent_channel_bindings_pg(pg_pool, &agent_id).await
    {
        Ok(Some(bindings)) => bindings,
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!(
                "routine thread auth failed to load agent bindings for {agent_id}: {error}"
            );
            return None;
        }
    };

    let Some(primary_channel) = bindings.primary_channel() else {
        return None;
    };
    let Some(parent_channel_id) = parse_channel_target_value(&primary_channel) else {
        tracing::warn!(
            "routine thread auth found invalid primary channel for {agent_id}: {primary_channel}"
        );
        return None;
    };
    Some(ChannelId::new(parent_channel_id))
}

fn resolve_channel_target(target: &str) -> Result<u64, SendTargetResolutionError> {
    let channel_target = target.strip_prefix("channel:").unwrap_or(target);
    parse_channel_target_value(channel_target).ok_or(SendTargetResolutionError::BadRequest(
        "invalid target format (use channel:<id>, channel:<name>, or agent:<roleId>)",
    ))
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

            {
                let _ = db;
                Err(SendTargetResolutionError::Internal(
                    "postgres pool unavailable during agent lookup".to_string(),
                ))
            }
        }
        None => resolve_channel_target(target),
    }
}

/// Handle POST /api/discord/send — agent-to-agent native routing.
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
    send_message_with_backends_and_delivery_options(
        registry,
        db,
        pg_pool,
        target,
        content,
        source,
        bot,
        summary,
        delivery_id,
        ManualOutboundOptions::default(),
    )
    .await
}

const HEADLESS_TURN_OUTBOX_SOURCE: &str = "headless_turn";

fn dm_default_agent_authorizes_unmapped_private_channel(
    is_private_channel: bool,
    source: &str,
    provider: &ProviderKind,
    session_bound_to_provider: bool,
) -> bool {
    if !is_private_channel {
        return false;
    }

    let source = source.trim();
    super::agentdesk_config::dm_default_agent_allows_outbound_source(provider, source)
        || (source == HEADLESS_TURN_OUTBOX_SOURCE
            && session_bound_to_provider
            && super::agentdesk_config::resolve_dm_default_agent(provider).is_some())
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ManualOutboundOptions {
    pub(crate) allow_unbound_internal_channel: bool,
}

pub(crate) async fn send_message_with_backends_and_delivery_options(
    registry: &HealthRegistry,
    db: Option<&Db>,
    pg_pool: Option<&PgPool>,
    target: &str,
    content: &str,
    source: &str,
    bot: &str,
    summary: Option<&str>,
    delivery_id: Option<ManualOutboundDeliveryId<'_>>,
    options: ManualOutboundOptions,
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

    // Validate source is a known agent role_id or internal system source.
    // Issue #2047 Finding 9 — don't echo the caller-supplied label back in the
    // response body. That made enumerating the whitelist trivial and gave a
    // log-injection assist. The full label is preserved in `tracing::warn!`
    // for operators.
    if !is_allowed_send_source(source) {
        tracing::warn!(
            source,
            bot,
            "/api/discord/send rejected: source label not allowed for caller class"
        );
        return (
            "403 Forbidden",
            r#"{"ok":false,"error":"source not allowed for this caller"}"#.to_string(),
        );
    }

    // Verify target channel exists in role-map (authorization check).
    // If the target is a thread, resolve its parent channel and check that instead.
    // Pass channel name so byChannelName-style configs can match.
    if super::settings::resolve_role_binding(channel_id, None).is_none() {
        let routine_parent_hint = routine_thread_parent_hint(pg_pool, channel_id).await;
        let mut authorized = false;
        let mut target_channel_accessible = false;
        // Try resolving as a thread: fetch channel info and check parent_id.
        //
        // Issue #2047 Finding 10 — also use the fetched channel name to retry
        // the byChannelName fallback for the target channel itself. The first
        // `resolve_role_binding(channel_id, None)` above can only match
        // `byChannelId` entries; a channel registered with `byChannelName`
        // only was previously blocked even though it is legitimately mapped.
        if let Ok(http) = resolve_bot_http(registry, bot).await {
            if let Ok(channel) = channel_id.to_channel(&*http).await {
                target_channel_accessible = true;
                let is_private_channel = matches!(&channel, serenity::Channel::Private(_));
                if !authorized
                    && registry
                        .dm_default_agent_authorizes_private_channel(
                            channel_id,
                            is_private_channel,
                            source,
                        )
                        .await
                {
                    authorized = true;
                    tracing::info!(
                        target_channel_id = channel_id.get(),
                        source,
                        bot,
                        "allowing outbound delivery to dm_default_agent-bound private channel"
                    );
                }
                // `Channel::guild` consumes the value, so derive the target
                // channel name first via `clone()`; the original `channel`
                // is then consumed by the thread/parent walk below.
                let target_name = channel.clone().guild().map(|gc| gc.name.clone());
                // First: byChannelName retry on the *target* channel itself.
                if !authorized
                    && super::settings::resolve_role_binding(channel_id, target_name.as_deref())
                        .is_some()
                {
                    authorized = true;
                }
                if let Some(guild_channel) = channel.guild() {
                    if let Some(parent_id) = guild_channel.parent_id {
                        if let Some(expected_parent) = routine_parent_hint {
                            if expected_parent != parent_id {
                                tracing::warn!(
                                    target_channel_id = channel_id.get(),
                                    actual_parent_id = parent_id.get(),
                                    expected_parent_id = expected_parent.get(),
                                    "routine thread parent hint did not match Discord parent"
                                );
                            }
                        }
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
        if !authorized
            && options.allow_unbound_internal_channel
            && is_allowed_send_source(source)
            && target.trim_start().starts_with("channel:")
            && target_channel_accessible
        {
            authorized = true;
            tracing::warn!(
                target_channel_id = channel_id.get(),
                source,
                bot,
                "allowing trusted internal Discord relay to unbound but accessible channel"
            );
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
        shared_outbound_deduper(),
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

/// Caller-class for `/api/discord/send` and friends.
///
/// Issue #2047 Finding 7 — the `source` JSON label was previously a free-form
/// string that any authenticated caller could set to e.g. `"system"` and have
/// observability dashboards treat the message as if it were emitted by the
/// internal automation plane. We now require callers to attest their class via
/// the `X-AgentDesk-Source` header (or, for backwards compat, by being on the
/// loopback interface) and only honour `source` labels that match that class.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SendCallerClass {
    /// In-process dcserver call (loopback peer + matching bearer, or no
    /// bearer when auth is disabled). Allowed to use any internal label.
    LoopbackInternal,
    /// External CLI/agent presenting `X-AgentDesk-Source: cli` and a valid
    /// bearer token. Restricted to a small set of labels.
    Cli,
    /// Browser dashboard with same-origin loopback. Can only attribute
    /// messages to `dashboard` or to a known agent role id.
    Dashboard,
    /// Fallback when no header is provided and the request isn't loopback —
    /// most restrictive bucket.
    Unknown,
}

impl SendCallerClass {
    /// Parse the `X-AgentDesk-Source` header value (case-insensitive). Returns
    /// `None` for unknown / empty values so the caller can fall back to
    /// peer-based inference.
    pub fn from_header(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "loopback" | "dcserver" | "internal" => Some(Self::LoopbackInternal),
            "cli" | "agentdesk-cli" => Some(Self::Cli),
            "dashboard" | "browser" => Some(Self::Dashboard),
            _ => None,
        }
    }

    fn allowed_internal_labels(self) -> &'static [&'static str] {
        // The 강력 라벨 (`system`, `headless_turn`, …) are loopback-only —
        // dashboard/cli must not be able to impersonate them.
        const LOOPBACK_ONLY: &[&str] = &[
            "kanban-rules",
            "triage-rules",
            "review-automation",
            "auto-queue",
            "pipeline",
            "system",
            "timeouts",
            "merge-automation",
            "lifecycle_notifier",
            "routine-runtime",
            "headless_turn",
            "slo_alerter",
            "quality_regression_alerter",
            "auto-queue-monitor",
            "inventory",
            "voice",
        ];
        const CLI_ALLOWED: &[&str] = &["agentdesk-cli", "operator"];
        const DASHBOARD_ALLOWED: &[&str] = &["dashboard"];
        match self {
            SendCallerClass::LoopbackInternal => LOOPBACK_ONLY,
            SendCallerClass::Cli => CLI_ALLOWED,
            SendCallerClass::Dashboard => DASHBOARD_ALLOWED,
            SendCallerClass::Unknown => &[],
        }
    }
}

/// Backward-compatible label gate. New code paths should call
/// [`is_allowed_send_source_for`] with an explicit caller-class. This wrapper
/// behaves as if the call came from `LoopbackInternal` so existing in-process
/// publishers (lifecycle notifier, headless turn, …) keep working without
/// surface-level rewrites.
fn is_allowed_send_source(source: &str) -> bool {
    is_allowed_send_source_for(source, SendCallerClass::LoopbackInternal)
}

/// Issue #2047 Finding 7 — gate the `source` label by caller-class.
pub fn is_allowed_send_source_for(source: &str, caller: SendCallerClass) -> bool {
    if super::settings::is_known_agent(source) {
        return true;
    }
    caller.allowed_internal_labels().contains(&source)
}

#[cfg(test)]
mod send_source_tests {
    use super::{SendCallerClass, is_allowed_send_source, is_allowed_send_source_for};

    #[test]
    fn headless_turn_is_allowed_internal_send_source() {
        assert!(is_allowed_send_source("headless_turn"));
        assert!(is_allowed_send_source("lifecycle_notifier"));
        assert!(is_allowed_send_source("routine-runtime"));
        assert!(is_allowed_send_source("slo_alerter"));
        assert!(is_allowed_send_source("quality_regression_alerter"));
        assert!(is_allowed_send_source("auto-queue-monitor"));
        assert!(is_allowed_send_source("inventory"));
        assert!(is_allowed_send_source("voice"));
        assert!(!is_allowed_send_source("not-a-real-source"));
    }

    #[test]
    fn dashboard_cannot_impersonate_system_or_headless_turn() {
        // Issue #2047 Finding 7 — dashboards / browser callers must not be
        // able to claim 강력 internal labels.
        assert!(!is_allowed_send_source_for(
            "system",
            SendCallerClass::Dashboard
        ));
        assert!(!is_allowed_send_source_for(
            "headless_turn",
            SendCallerClass::Dashboard
        ));
        assert!(!is_allowed_send_source_for(
            "auto-queue",
            SendCallerClass::Dashboard
        ));
    }

    #[test]
    fn cli_cannot_impersonate_loopback_only_labels() {
        assert!(!is_allowed_send_source_for("system", SendCallerClass::Cli));
        assert!(!is_allowed_send_source_for(
            "kanban-rules",
            SendCallerClass::Cli
        ));
        assert!(is_allowed_send_source_for(
            "agentdesk-cli",
            SendCallerClass::Cli
        ));
        assert!(is_allowed_send_source_for("operator", SendCallerClass::Cli));
    }

    #[test]
    fn unknown_caller_class_only_allows_known_agents() {
        // Without a verified caller class we still let messages through when
        // the source matches a registered agent role id (the agent identity
        // itself is the attestation). Strong internal labels are denied.
        assert!(!is_allowed_send_source_for(
            "system",
            SendCallerClass::Unknown
        ));
        assert!(!is_allowed_send_source_for(
            "dashboard",
            SendCallerClass::Unknown
        ));
    }

    #[test]
    fn loopback_internal_keeps_existing_internal_label_acceptance() {
        for label in [
            "kanban-rules",
            "triage-rules",
            "review-automation",
            "auto-queue",
            "system",
            "headless_turn",
            "lifecycle_notifier",
            "routine-runtime",
        ] {
            assert!(
                is_allowed_send_source_for(label, SendCallerClass::LoopbackInternal),
                "loopback caller must keep accepting `{label}`"
            );
        }
    }

    #[test]
    fn from_header_parses_known_caller_classes() {
        assert_eq!(
            SendCallerClass::from_header("cli"),
            Some(SendCallerClass::Cli)
        );
        assert_eq!(
            SendCallerClass::from_header("AgentDesk-CLI"),
            Some(SendCallerClass::Cli)
        );
        assert_eq!(
            SendCallerClass::from_header("dashboard"),
            Some(SendCallerClass::Dashboard)
        );
        assert_eq!(
            SendCallerClass::from_header("loopback"),
            Some(SendCallerClass::LoopbackInternal)
        );
        assert_eq!(
            SendCallerClass::from_header("dcserver"),
            Some(SendCallerClass::LoopbackInternal)
        );
        assert_eq!(SendCallerClass::from_header(""), None);
        assert_eq!(SendCallerClass::from_header("attacker"), None);
    }
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

fn is_reserved_voice_correlation_namespace(delivery_id: ManualOutboundDeliveryId<'_>) -> bool {
    delivery_id
        .correlation_id
        .trim_start()
        .starts_with("voice:")
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

    async fn resolve_dm_channel(&self, user_id: &str) -> Result<String, DispatchMessagePostError> {
        let user_id = user_id
            .parse::<u64>()
            .map(serenity::UserId::new)
            .map_err(|error| {
                DispatchMessagePostError::new(
                    DispatchMessagePostErrorKind::Other,
                    format!("invalid Discord user id {user_id}: {error}"),
                )
            })?;
        user_id
            .create_dm_channel(&*self.http)
            .await
            .map(|channel| channel.id.get().to_string())
            .map_err(|error| {
                DispatchMessagePostError::new(
                    DispatchMessagePostErrorKind::Other,
                    format!("DM channel creation failed: {error}"),
                )
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
    // Issue #2363: the manual dedupe key must include the resolved target
    // channel AND the sending `bot` identity. Voice announce delivery ids
    // encode (guild, voice_channel, utterance, generation) in
    // correlation+semantic, but the routed **target** channel and the
    // producer bot can still differ (announce vs notify), and external
    // `/api/discord/send` callers can set delivery ids freely — so without
    // bot+target scoping a notify send could poison a later announce
    // send and report "duplicate" while the announce bot never actually
    // delivered the voice transcript trigger.
    let dedup_key = delivery_id.map(|delivery_id| manual_dedup_key(bot, channel_id, delivery_id));
    let reservation = if let Some(key) = dedup_key.as_deref() {
        match reserve_manual_delivery(dedup, key).await {
            ManualDedupReservation::Duplicate(existing_message_id) => {
                return ManualDeliveryOutcome::Sent {
                    message_id: existing_message_id,
                    delivery: Some("duplicate"),
                };
            }
            ManualDedupReservation::InFlight => {
                return ManualDeliveryOutcome::Sent {
                    message_id: String::new(),
                    delivery: Some("in_flight"),
                };
            }
            ManualDedupReservation::Reserved(reservation) => Some(reservation),
        }
    } else {
        None
    };

    let content_len = content.chars().count();
    if content_len > DISCORD_HARD_LIMIT_CHARS {
        // Compatibility shim: v3 text delivery does not yet own attachment
        // upload or manual chunk-posting for over-2k `/api/discord/send` payloads.
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
            if let Some(mut reservation) = reservation {
                reservation.record(message_id);
            }
        }
        return result;
    }

    let target_channel = match parse_channel_id_for_manual(channel_id) {
        Ok(channel_id) => channel_id,
        Err(outcome) => return outcome,
    };
    let result = deliver_manual_v3_text(
        client,
        dedup,
        OutboundTarget::Channel(target_channel),
        channel_id,
        bot,
        content,
        summary,
        delivery_id,
        content_len > DISCORD_SAFE_LIMIT_CHARS,
    )
    .await;
    record_manual_delivery_success(dedup, reservation, dedup_key.as_deref(), &result);
    result
}

async fn deliver_manual_dm_notification<C: ManualOutboundClient>(
    client: &C,
    dedup: &OutboundDeduper,
    user_id: u64,
    content: &str,
    bot: &str,
    summary: Option<&str>,
    delivery_id: Option<ManualOutboundDeliveryId<'_>>,
) -> ManualDeliveryOutcome {
    // Issue #2363: scope the dedupe key by sending bot + DM target so the
    // same delivery id can't be silently suppressed across different
    // recipients or across producer bots.
    let dm_target_label = format!("dm:{user_id}");
    let dedup_key =
        delivery_id.map(|delivery_id| manual_dedup_key(bot, &dm_target_label, delivery_id));
    let reservation = if let Some(key) = dedup_key.as_deref() {
        match reserve_manual_delivery(dedup, key).await {
            ManualDedupReservation::Duplicate(existing_message_id) => {
                return ManualDeliveryOutcome::Sent {
                    message_id: existing_message_id,
                    delivery: Some("duplicate"),
                };
            }
            ManualDedupReservation::InFlight => {
                return ManualDeliveryOutcome::Sent {
                    message_id: String::new(),
                    delivery: Some("in_flight"),
                };
            }
            ManualDedupReservation::Reserved(reservation) => Some(reservation),
        }
    } else {
        None
    };

    let content_len = content.chars().count();
    if content_len > DISCORD_HARD_LIMIT_CHARS {
        // Compatibility shim: keep the existing attachment/chunk behavior for
        // oversize DM payloads while v3 owns the DM channel resolution.
        let dm_channel = match client.resolve_dm_channel(&user_id.to_string()).await {
            Ok(channel_id) => channel_id,
            Err(error) => {
                return ManualDeliveryOutcome::Failed {
                    detail: error.to_string(),
                };
            }
        };
        let result = match if bot == "announce" {
            client
                .post_text_attachment(&dm_channel, content, summary)
                .await
                .map(|message_id| ManualDeliveryOutcome::Sent {
                    message_id,
                    delivery: Some("summary+txt"),
                })
        } else {
            deliver_chunked_manual_notification(client, &dm_channel, content).await
        } {
            Ok(outcome) => outcome,
            Err(error) => ManualDeliveryOutcome::Failed {
                detail: error.to_string(),
            },
        };
        record_manual_delivery_success(dedup, reservation, dedup_key.as_deref(), &result);
        return result;
    }

    let result = deliver_manual_v3_text(
        client,
        dedup,
        OutboundTarget::DmUser(serenity::UserId::new(user_id)),
        &format!("dm:{user_id}"),
        bot,
        content,
        summary,
        delivery_id,
        content_len > DISCORD_SAFE_LIMIT_CHARS,
    )
    .await;
    record_manual_delivery_success(dedup, reservation, dedup_key.as_deref(), &result);
    result
}

enum ManualDedupReservation {
    Reserved(OutboundDedupReservation),
    Duplicate(String),
    InFlight,
}

async fn reserve_manual_delivery(dedup: &OutboundDeduper, key: &str) -> ManualDedupReservation {
    loop {
        match dedup.reserve(key) {
            OutboundDedupClaim::Reserved(reservation) => {
                return ManualDedupReservation::Reserved(reservation);
            }
            OutboundDedupClaim::Duplicate(message_id) => {
                return ManualDedupReservation::Duplicate(message_id);
            }
            OutboundDedupClaim::InFlight(in_flight) => {
                match in_flight.wait_for_delivery(Duration::from_secs(5)).await {
                    OutboundDedupWait::Delivered(message_id) => {
                        return ManualDedupReservation::Duplicate(message_id);
                    }
                    OutboundDedupWait::Released => continue,
                    OutboundDedupWait::TimedOut => return ManualDedupReservation::InFlight,
                }
            }
        }
    }
}

async fn deliver_manual_v3_text<C: DiscordOutboundClient>(
    client: &C,
    dedup: &OutboundDeduper,
    target: OutboundTarget,
    target_label: &str,
    bot: &str,
    content: &str,
    summary: Option<&str>,
    delivery_id: Option<ManualOutboundDeliveryId<'_>>,
    preserve_inline_content: bool,
) -> ManualDeliveryOutcome {
    let mut policy = if preserve_inline_content {
        DiscordOutboundPolicy::preserve_inline_content()
    } else {
        DiscordOutboundPolicy::review_notification()
    };
    if delivery_id.is_none() {
        policy = policy.without_idempotency();
    }
    let (correlation_id, semantic_event_id) = delivery_id
        .map(|delivery_id| {
            // Issue #2363: prefix the v3 correlation_id with the bot
            // identity so structurally-equal external delivery ids cannot
            // poison sends across different producer bots. The v3 dedup
            // key already includes target, but not bot, and external
            // `/api/discord/send` callers can supply arbitrary
            // (correlation_id, semantic_event_id) pairs.
            (
                format!("bot:{bot}::{}", delivery_id.correlation_id),
                delivery_id.semantic_event_id.to_string(),
            )
        })
        .unwrap_or_else(|| {
            (
                format!("manual:no-idempotency:bot:{bot}:{target_label}"),
                "manual:no-idempotency".to_string(),
            )
        });
    let mut outbound_msg =
        DiscordOutboundMessage::new(correlation_id, semantic_event_id, content, target, policy);
    if let Some(summary) = summary.map(str::trim).filter(|value| !value.is_empty()) {
        outbound_msg = outbound_msg.with_summary(summary.to_string());
    }

    match deliver_v3_outbound(client, dedup, outbound_msg, None).await {
        DeliveryResult::Sent { messages, .. } => ManualDeliveryOutcome::Sent {
            message_id: first_raw_message_id(&messages).unwrap_or_default(),
            delivery: None,
        },
        DeliveryResult::Fallback {
            messages,
            fallback_used,
            ..
        } => ManualDeliveryOutcome::Sent {
            message_id: first_raw_message_id(&messages).unwrap_or_default(),
            delivery: Some(match fallback_used {
                FallbackUsed::LengthCompacted => "truncated",
                FallbackUsed::MinimalFallback => "minimal_fallback",
                FallbackUsed::LengthSplit => "chunked",
                FallbackUsed::FileAttachment => "summary+txt",
                FallbackUsed::ParentChannel => "parent_channel",
            }),
        },
        DeliveryResult::Duplicate {
            existing_messages, ..
        } => ManualDeliveryOutcome::Sent {
            // Issue #2363: surface the prior message id so retry callers
            // (e.g. announce-bot transcript driver) don't fail on an empty
            // numeric body when the v3 layer dedupes structurally.
            message_id: first_raw_message_id(&existing_messages).unwrap_or_default(),
            delivery: Some("duplicate"),
        },
        DeliveryResult::Skip { .. } => ManualDeliveryOutcome::Sent {
            message_id: String::new(),
            delivery: Some("skipped"),
        },
        DeliveryResult::PermanentFailure { reason } => {
            ManualDeliveryOutcome::Failed { detail: reason }
        }
    }
}

fn record_manual_delivery_success(
    dedup: &OutboundDeduper,
    reservation: Option<OutboundDedupReservation>,
    dedup_key: Option<&str>,
    result: &ManualDeliveryOutcome,
) {
    let mut reservation = reservation;
    let ManualDeliveryOutcome::Sent {
        message_id,
        delivery,
    } = result
    else {
        return;
    };
    if message_id.is_empty() {
        return;
    }
    // Don't overwrite the stored entry when we're just replaying a known
    // duplicate — `dedup.record` would re-insert the same id but at a
    // refreshed timestamp on backends that gain TTLs later.
    if matches!(delivery, Some("duplicate")) {
        if let Some(reservation) = reservation.as_mut() {
            reservation.record(message_id);
        }
        return;
    }
    if let Some(key) = dedup_key {
        if let Some(reservation) = reservation.as_mut() {
            reservation.record(message_id);
        } else {
            dedup.record(key, message_id);
        }
    }
}

/// Build a manual-delivery dedupe key scoped to the producer bot and the
/// resolved target so that the same (correlation_id, semantic_event_id)
/// cannot collide across different Discord channels, DM recipients, or
/// producer bots. External callers of `/api/discord/send` may supply
/// arbitrary `correlation_id` / `semantic_event_id`; scoping by `bot`
/// blocks a notify-bot send from poisoning a later announce-bot send to
/// the same target.
fn manual_dedup_key(
    bot: &str,
    target_label: &str,
    delivery_id: ManualOutboundDeliveryId<'_>,
) -> String {
    format!(
        "manual::{}::{}::{}::{}",
        bot, target_label, delivery_id.correlation_id, delivery_id.semantic_event_id
    )
}

fn parse_channel_id_for_manual(channel_id: &str) -> Result<ChannelId, ManualDeliveryOutcome> {
    channel_id
        .parse::<u64>()
        .map(ChannelId::new)
        .map_err(|error| ManualDeliveryOutcome::Failed {
            detail: format!("invalid discord channel id {channel_id}: {error}"),
        })
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
    sqlite: Option<&Db>,
    pg_pool: Option<&PgPool>,
    body: &str,
) -> (&'a str, String) {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
        return (
            "400 Bad Request",
            r#"{"ok":false,"error":"invalid JSON"}"#.to_string(),
        );
    };

    let raw_target = json
        .get("target")
        .and_then(|v| v.as_str())
        .or_else(|| json.get("channel_id").and_then(|v| v.as_str()))
        .unwrap_or("");
    let target = if json.get("target").and_then(|v| v.as_str()).is_none()
        && !raw_target.trim().is_empty()
        && !raw_target.trim_start().starts_with("channel:")
    {
        format!("channel:{raw_target}")
    } else {
        raw_target.to_string()
    };
    let content = json
        .get("content")
        .and_then(|v| v.as_str())
        .or_else(|| json.get("message").and_then(|v| v.as_str()))
        .unwrap_or("");
    let source = json
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("system");
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
    if delivery_id.is_some_and(is_reserved_voice_correlation_namespace) {
        return (
            "400 Bad Request",
            serde_json::json!({
                "ok": false,
                "error": "delivery_id correlation namespace is reserved"
            })
            .to_string(),
        );
    }

    send_message_with_backends_and_delivery_id(
        registry,
        sqlite,
        pg_pool,
        &target,
        content,
        source,
        bot,
        summary,
        delivery_id,
    )
    .await
}

/// Handle POST /api/discord/send-dm — send a DM to a Discord user.
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
    let dm_delivery_id = request.delivery_id();

    match deliver_manual_dm_notification(
        &SerenityManualOutboundClient { http },
        shared_outbound_deduper(),
        request.user_id,
        &request.content,
        &request.bot,
        None,
        dm_delivery_id
            .as_ref()
            .map(|delivery_id| ManualOutboundDeliveryId {
                correlation_id: &delivery_id.0,
                semantic_event_id: &delivery_id.1,
            }),
    )
    .await
    {
        ManualDeliveryOutcome::Sent {
            message_id,
            delivery,
        } => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 📨 DM: → user {} via shared outbound",
                request.user_id
            );
            let mut response = serde_json::json!({
                "ok": true,
                "user_id": user_id_text,
                "message_id": message_id,
            });
            if let Some(delivery) = delivery {
                response["delivery"] = serde_json::Value::String(delivery.to_string());
            }
            ("200 OK", response.to_string())
        }
        ManualDeliveryOutcome::Failed { detail } => (
            "500 Internal Server Error",
            format!(r#"{{"ok":false,"error":"DM send failed: {}"}}"#, detail),
        ),
    }
}

#[derive(Debug, PartialEq)]
struct SendDmRequest {
    user_id: u64,
    content: String,
    bot: String,
    correlation_id: Option<String>,
    semantic_event_id: Option<String>,
    idempotency_key: Option<String>,
}

impl SendDmRequest {
    fn delivery_id(&self) -> Option<(String, String)> {
        let correlation_id = self
            .correlation_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("senddm:{}", self.user_id));
        let semantic_event_id = self
            .semantic_event_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| {
                self.idempotency_key
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(|key| format!("senddm:{}:{}", self.user_id, normalize_senddm_key(key)))
            });
        semantic_event_id.map(|semantic_event_id| (correlation_id, semantic_event_id))
    }
}

fn normalize_senddm_key(value: &str) -> String {
    let normalized: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, ':' | '_' | '-' | '.') {
                ch
            } else {
                '_'
            }
        })
        .take(160)
        .collect();
    if normalized.is_empty() {
        "message".to_string()
    } else {
        normalized
    }
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
    let correlation_id = parsed["correlation_id"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let semantic_event_id = parsed["semantic_event_id"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let idempotency_key = parsed["idempotency_key"]
        .as_str()
        .or_else(|| parsed["idempotency_id"].as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    Ok(SendDmRequest {
        user_id,
        content,
        bot,
        correlation_id,
        semantic_event_id,
        idempotency_key,
    })
}

/// Parse a /api/discord/send JSON body and extract (target, content, source).
/// Returns Err with an error message on invalid input.
/// Factored out of handle_send for testability.
#[cfg_attr(not(test), allow(dead_code))]
fn parse_send_body(body: &str) -> Result<(String, String, String), &'static str> {
    let json: serde_json::Value = serde_json::from_str(body).map_err(|_| "invalid JSON")?;
    let content = json
        .get("content")
        .and_then(|v| v.as_str())
        .or_else(|| json.get("message").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();
    if content.is_empty() {
        return Err("content is required");
    }
    let mut target = json
        .get("target")
        .and_then(|v| v.as_str())
        .or_else(|| json.get("channel_id").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();
    if !target.trim().is_empty()
        && json.get("target").and_then(|v| v.as_str()).is_none()
        && !target.trim_start().starts_with("channel:")
    {
        target = format!("channel:{target}");
    }
    let source = json
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("system")
        .to_string();
    Ok((target, content, source))
}

#[cfg(test)]
mod manual_v3_delivery_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct MockManualOutboundClient {
        posts: Arc<Mutex<Vec<String>>>,
        post_targets: Arc<Mutex<Vec<String>>>,
        dm_resolutions: Arc<Mutex<Vec<String>>>,
    }

    impl DiscordOutboundClient for MockManualOutboundClient {
        async fn post_message(
            &self,
            target_channel: &str,
            content: &str,
        ) -> Result<String, DispatchMessagePostError> {
            let mut posts = self.posts.lock().unwrap();
            self.post_targets
                .lock()
                .unwrap()
                .push(target_channel.to_string());
            posts.push(content.to_string());
            Ok(format!("message-{}", posts.len()))
        }

        async fn resolve_dm_channel(
            &self,
            user_id: &str,
        ) -> Result<String, DispatchMessagePostError> {
            self.dm_resolutions
                .lock()
                .unwrap()
                .push(user_id.to_string());
            Ok("9876".to_string())
        }
    }

    impl ManualOutboundClient for MockManualOutboundClient {
        async fn post_text_attachment(
            &self,
            _target_channel: &str,
            _content: &str,
            _summary: Option<&str>,
        ) -> Result<String, DispatchMessagePostError> {
            Ok("attachment-message-1".to_string())
        }
    }

    #[tokio::test]
    async fn manual_dm_notification_uses_v3_dm_target_and_dedupes_before_resolve() {
        let client = MockManualOutboundClient::default();
        let dedup = OutboundDeduper::new();
        let delivery_id = ManualOutboundDeliveryId {
            correlation_id: "senddm:42",
            semantic_event_id: "senddm:42:hello",
        };

        let first = deliver_manual_dm_notification(
            &client,
            &dedup,
            42,
            "hello",
            "announce",
            None,
            Some(delivery_id),
        )
        .await;
        let second = deliver_manual_dm_notification(
            &client,
            &dedup,
            42,
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
                message_id: "message-1".to_string(),
                delivery: Some("duplicate")
            }
        );
        assert_eq!(
            client.dm_resolutions.lock().unwrap().clone(),
            vec!["42".to_string()]
        );
        assert_eq!(
            client.post_targets.lock().unwrap().clone(),
            vec!["9876".to_string()]
        );
        assert_eq!(
            client.posts.lock().unwrap().clone(),
            vec!["hello".to_string()]
        );
    }

    #[tokio::test]
    async fn voice_announce_same_utterance_and_generation_dedupes_at_health_layer() {
        use crate::services::discord::voice_background_driver::{
            default_voice_announce_generation, voice_announce_delivery_id,
        };
        use poise::serenity_prelude::{ChannelId, GuildId};

        let client = MockManualOutboundClient::default();
        let dedup = OutboundDeduper::new();
        let voice_id = voice_announce_delivery_id(
            GuildId::new(7001),
            ChannelId::new(8002),
            "utt-2363-a",
            default_voice_announce_generation(),
        );
        let delivery_id = ManualOutboundDeliveryId {
            correlation_id: &voice_id.correlation_id,
            semantic_event_id: &voice_id.semantic_event_id,
        };

        // Issue #2363: announce send retried with identical
        // (guild, voice_channel, utterance, generation) must hit the dedupe
        // path and not produce a second Discord call.
        let first = deliver_manual_notification(
            &client,
            &dedup,
            "9000",
            "voice transcript",
            "announce",
            None,
            Some(delivery_id),
        )
        .await;
        let second = deliver_manual_notification(
            &client,
            &dedup,
            "9000",
            "voice transcript",
            "announce",
            None,
            Some(delivery_id),
        )
        .await;

        assert_eq!(
            first,
            ManualDeliveryOutcome::Sent {
                message_id: "message-1".to_string(),
                delivery: None,
            }
        );
        assert_eq!(
            second,
            ManualDeliveryOutcome::Sent {
                message_id: "message-1".to_string(),
                delivery: Some("duplicate"),
            }
        );
        assert_eq!(client.posts.lock().unwrap().len(), 1);
        assert_eq!(
            voice_id.correlation_id,
            "voice:7001:8002:utt-2363-a".to_string()
        );
        assert_eq!(
            voice_id.semantic_event_id,
            "announce:generation:1".to_string()
        );
    }

    #[tokio::test]
    async fn voice_announce_new_utterance_does_not_dedupe_against_prior_send() {
        use crate::services::discord::voice_background_driver::{
            default_voice_announce_generation, voice_announce_delivery_id,
        };
        use poise::serenity_prelude::{ChannelId, GuildId};

        let client = MockManualOutboundClient::default();
        let dedup = OutboundDeduper::new();
        let first_id = voice_announce_delivery_id(
            GuildId::new(7001),
            ChannelId::new(8002),
            "utt-A",
            default_voice_announce_generation(),
        );
        let second_id = voice_announce_delivery_id(
            GuildId::new(7001),
            ChannelId::new(8002),
            "utt-B",
            default_voice_announce_generation(),
        );

        let first = deliver_manual_notification(
            &client,
            &dedup,
            "9000",
            "first transcript",
            "announce",
            None,
            Some(ManualOutboundDeliveryId {
                correlation_id: &first_id.correlation_id,
                semantic_event_id: &first_id.semantic_event_id,
            }),
        )
        .await;
        let second = deliver_manual_notification(
            &client,
            &dedup,
            "9000",
            "second transcript",
            "announce",
            None,
            Some(ManualOutboundDeliveryId {
                correlation_id: &second_id.correlation_id,
                semantic_event_id: &second_id.semantic_event_id,
            }),
        )
        .await;

        assert_eq!(
            first,
            ManualDeliveryOutcome::Sent {
                message_id: "message-1".to_string(),
                delivery: None,
            }
        );
        assert_eq!(
            second,
            ManualDeliveryOutcome::Sent {
                message_id: "message-2".to_string(),
                delivery: None,
            }
        );
        assert_eq!(client.posts.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn voice_announce_generation_bump_breaks_dedupe_for_same_utterance() {
        use crate::services::discord::voice_background_driver::{
            default_voice_announce_generation, voice_announce_delivery_id,
        };
        use poise::serenity_prelude::{ChannelId, GuildId};

        // Same (guild, voice_channel, utterance) but a higher generation
        // (e.g. a barge-in follow-up) must NOT dedupe against the original
        // announce — different `semantic_event_id = announce:generation:{n}`.
        let client = MockManualOutboundClient::default();
        let dedup = OutboundDeduper::new();
        let gen_one = voice_announce_delivery_id(
            GuildId::new(7001),
            ChannelId::new(8002),
            "utt-shared",
            default_voice_announce_generation(),
        );
        let gen_two = voice_announce_delivery_id(
            GuildId::new(7001),
            ChannelId::new(8002),
            "utt-shared",
            default_voice_announce_generation() + 1,
        );
        assert_eq!(gen_one.correlation_id, gen_two.correlation_id);
        assert_ne!(gen_one.semantic_event_id, gen_two.semantic_event_id);

        let first = deliver_manual_notification(
            &client,
            &dedup,
            "9000",
            "transcript",
            "announce",
            None,
            Some(ManualOutboundDeliveryId {
                correlation_id: &gen_one.correlation_id,
                semantic_event_id: &gen_one.semantic_event_id,
            }),
        )
        .await;
        let second = deliver_manual_notification(
            &client,
            &dedup,
            "9000",
            "transcript with barge-in follow-up",
            "announce",
            None,
            Some(ManualOutboundDeliveryId {
                correlation_id: &gen_two.correlation_id,
                semantic_event_id: &gen_two.semantic_event_id,
            }),
        )
        .await;

        assert!(matches!(
            first,
            ManualDeliveryOutcome::Sent { delivery: None, .. }
        ));
        assert!(matches!(
            second,
            ManualDeliveryOutcome::Sent { delivery: None, .. }
        ));
        assert_eq!(client.posts.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn voice_announce_cross_guild_isolation_prevents_dedupe_collision() {
        use crate::services::discord::voice_background_driver::{
            default_voice_announce_generation, voice_announce_delivery_id,
        };
        use poise::serenity_prelude::{ChannelId, GuildId};

        // Two different guilds happen to use the same utterance_id (they're
        // independent generators) — must NOT dedupe against each other.
        let client = MockManualOutboundClient::default();
        let dedup = OutboundDeduper::new();
        let guild_a = voice_announce_delivery_id(
            GuildId::new(1),
            ChannelId::new(10),
            "utt-collide",
            default_voice_announce_generation(),
        );
        let guild_b = voice_announce_delivery_id(
            GuildId::new(2),
            ChannelId::new(10),
            "utt-collide",
            default_voice_announce_generation(),
        );
        assert_ne!(guild_a.correlation_id, guild_b.correlation_id);

        let first = deliver_manual_notification(
            &client,
            &dedup,
            "9000",
            "from guild 1",
            "announce",
            None,
            Some(ManualOutboundDeliveryId {
                correlation_id: &guild_a.correlation_id,
                semantic_event_id: &guild_a.semantic_event_id,
            }),
        )
        .await;
        let second = deliver_manual_notification(
            &client,
            &dedup,
            "9000",
            "from guild 2",
            "announce",
            None,
            Some(ManualOutboundDeliveryId {
                correlation_id: &guild_b.correlation_id,
                semantic_event_id: &guild_b.semantic_event_id,
            }),
        )
        .await;

        assert!(matches!(
            first,
            ManualDeliveryOutcome::Sent { delivery: None, .. }
        ));
        assert!(matches!(
            second,
            ManualDeliveryOutcome::Sent { delivery: None, .. }
        ));
        assert_eq!(client.posts.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn voice_announce_same_delivery_id_different_target_channel_does_not_dedupe() {
        use crate::services::discord::voice_background_driver::{
            default_voice_announce_generation, voice_announce_delivery_id,
        };
        use poise::serenity_prelude::{ChannelId, GuildId};

        // Issue #2363 (Codex high-severity finding): the dedupe key must
        // include the resolved target channel. Otherwise an announce queued
        // first to a transcript channel and later re-routed to a different
        // target channel for the same (guild, voice_channel, utterance,
        // generation) tuple would be silently suppressed as a duplicate.
        let client = MockManualOutboundClient::default();
        let dedup = OutboundDeduper::new();
        let voice_id = voice_announce_delivery_id(
            GuildId::new(7001),
            ChannelId::new(8002),
            "utt-cross-target",
            default_voice_announce_generation(),
        );
        let delivery_id = ManualOutboundDeliveryId {
            correlation_id: &voice_id.correlation_id,
            semantic_event_id: &voice_id.semantic_event_id,
        };

        let to_channel_a = deliver_manual_notification(
            &client,
            &dedup,
            "9000",
            "voice transcript",
            "announce",
            None,
            Some(delivery_id),
        )
        .await;
        let to_channel_b = deliver_manual_notification(
            &client,
            &dedup,
            "9001",
            "voice transcript",
            "announce",
            None,
            Some(delivery_id),
        )
        .await;

        assert_eq!(
            to_channel_a,
            ManualDeliveryOutcome::Sent {
                message_id: "message-1".to_string(),
                delivery: None,
            }
        );
        assert_eq!(
            to_channel_b,
            ManualDeliveryOutcome::Sent {
                message_id: "message-2".to_string(),
                delivery: None,
            }
        );
        assert_eq!(client.posts.lock().unwrap().len(), 2);
        assert_eq!(
            client.post_targets.lock().unwrap().clone(),
            vec!["9000".to_string(), "9001".to_string()]
        );
    }

    #[tokio::test]
    async fn voice_announce_different_bot_does_not_dedupe_against_announce() {
        use crate::services::discord::voice_background_driver::{
            default_voice_announce_generation, voice_announce_delivery_id,
        };
        use poise::serenity_prelude::{ChannelId, GuildId};

        // Issue #2363 (Codex high-severity finding round 2): scoping must
        // also include the producer `bot`. Without this an external
        // `/api/discord/send` caller could send through `notify` with a
        // crafted `voice:{guild}:{voice_channel}:{utterance}` delivery id
        // and silently poison the voice announce path.
        let client = MockManualOutboundClient::default();
        let dedup = OutboundDeduper::new();
        let voice_id = voice_announce_delivery_id(
            GuildId::new(7001),
            ChannelId::new(8002),
            "utt-cross-bot",
            default_voice_announce_generation(),
        );
        let delivery_id = ManualOutboundDeliveryId {
            correlation_id: &voice_id.correlation_id,
            semantic_event_id: &voice_id.semantic_event_id,
        };

        let from_notify = deliver_manual_notification(
            &client,
            &dedup,
            "9000",
            "notify payload",
            "notify",
            None,
            Some(delivery_id),
        )
        .await;
        let from_announce = deliver_manual_notification(
            &client,
            &dedup,
            "9000",
            "voice transcript",
            "announce",
            None,
            Some(delivery_id),
        )
        .await;

        assert_eq!(
            from_notify,
            ManualDeliveryOutcome::Sent {
                message_id: "message-1".to_string(),
                delivery: None,
            }
        );
        assert_eq!(
            from_announce,
            ManualDeliveryOutcome::Sent {
                message_id: "message-2".to_string(),
                delivery: None,
            }
        );
        assert_eq!(client.posts.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn api_send_rejects_user_supplied_voice_delivery_id_namespace() {
        let registry = HealthRegistry::new();

        let (status, body) = handle_send(
            &registry,
            None,
            None,
            r#"{
                "target": "channel:9000",
                "content": "forged voice transcript",
                "source": "system",
                "bot": "announce",
                "correlation_id": "voice:7001:8002:utt-forged",
                "semantic_event_id": "announce:generation:1"
            }"#,
        )
        .await;
        let response: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "400 Bad Request");
        assert_eq!(response["ok"], false);
        assert_eq!(
            response["error"],
            "delivery_id correlation namespace is reserved"
        );
    }

    #[tokio::test]
    async fn voice_announce_duplicate_surfaces_prior_message_id_not_empty() {
        use crate::services::discord::voice_background_driver::{
            default_voice_announce_generation, voice_announce_delivery_id,
        };
        use poise::serenity_prelude::{ChannelId, GuildId};

        // Issue #2363 (Codex high-severity finding): the duplicate path must
        // return the prior delivered message id. The announce-bot driver
        // (`AnnounceBotTranscriptDriver::start`) parses `message_id` from
        // the response body and errors out on empty / non-numeric values.
        let client = MockManualOutboundClient::default();
        let dedup = OutboundDeduper::new();
        let voice_id = voice_announce_delivery_id(
            GuildId::new(7001),
            ChannelId::new(8002),
            "utt-known-id",
            default_voice_announce_generation(),
        );
        let delivery_id = ManualOutboundDeliveryId {
            correlation_id: &voice_id.correlation_id,
            semantic_event_id: &voice_id.semantic_event_id,
        };

        let _ = deliver_manual_notification(
            &client,
            &dedup,
            "9000",
            "voice transcript",
            "announce",
            None,
            Some(delivery_id),
        )
        .await;
        let dup = deliver_manual_notification(
            &client,
            &dedup,
            "9000",
            "voice transcript",
            "announce",
            None,
            Some(delivery_id),
        )
        .await;

        match dup {
            ManualDeliveryOutcome::Sent {
                message_id,
                delivery,
            } => {
                assert!(
                    !message_id.is_empty(),
                    "duplicate must return prior message id, not empty"
                );
                assert!(
                    message_id
                        .chars()
                        .all(|c| c == '-' || c.is_ascii_alphanumeric()),
                    "message_id should be parseable by callers"
                );
                assert_eq!(delivery, Some("duplicate"));
            }
            other => panic!("expected Sent(duplicate), got {other:?}"),
        }
    }
}
