use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Instant;

use poise::serenity_prelude as serenity;
use serde::Serialize;
use serenity::ChannelId;

use super::SharedData;
use super::bot_role::UtilityBotRole;
use crate::services::provider::ProviderKind;

// #3038 Phase A: `health.rs` is the module root — the `HealthRegistry` core
// (provider/bot-HTTP registration, token rotation, channel-aware runtime
// lookup). The former 2.9k-line monolith body lives in the submodules below
// as verbatim function moves:
//   * `runtime_resolve` — bot-HTTP resolution + direct-meeting runtime resolver pair
//   * `headless_turn` — headless agent-turn reserve/start API + direct-meeting starter
//   * `outbound::{send_target,send_gate,manual_delivery,send_api}` — manual
//     send-to-agent/outbound dispatch, re-exported here for compatibility
mod headless_turn;
pub(in crate::services::discord) mod liveness_authority;
mod mailbox;
mod provider_probe;
#[path = "health/rebind_request.rs"]
mod rebind_request;
mod recovery;
mod redaction;
mod relay_auto_heal;
mod relay_dead_reattach;
mod relay_progress;
mod runtime_resolve;
mod session_enrichment;
mod snapshot;
mod stall_liveness;
mod stall_verdict;
mod watcher_respawn;

// `HeadlessAgentTurnReservation` has no external referent today (callers
// destructure the reserve/start tuple); kept re-exported for the reserve→start
// API surface, same convention as the recovery/snapshot blocks below.
pub(crate) use crate::services::discord::outbound::manual_delivery::ManualOutboundDeliveryId;
pub use crate::services::discord::outbound::send_api::{handle_send, handle_senddm};
use crate::services::discord::outbound::send_gate::dm_default_agent_authorizes_unmapped_private_channel;
pub(crate) use crate::services::discord::outbound::send_gate::{
    ManualOutboundOptions, send_message_with_backends, send_message_with_backends_and_delivery_id,
    send_message_with_backends_and_delivery_options,
};
#[allow(unused_imports)]
pub use crate::services::discord::outbound::send_gate::{
    SendCallerClass, is_allowed_send_source_for,
};
#[allow(unused_imports)]
pub use headless_turn::HeadlessAgentTurnReservation;
pub use headless_turn::{
    reserve_headless_agent_turn, reserve_headless_agent_turn_in_dm, start_direct_meeting,
    start_headless_agent_turn, start_headless_agent_turn_in_dm,
    start_reserved_headless_agent_turn_in_dm,
    start_reserved_headless_agent_turn_with_owner_channel,
};
pub use mailbox::purge_idle_channel_mailbox_registry_entry;
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
    spawn_watchdog, stop_providerless_runtime_turn_preserving_watcher_strict_ownership,
    stop_runtime_turn_preserving_watcher,
};
pub(crate) use runtime_resolve::resolve_utility_bot_http;
pub use runtime_resolve::{fetch_channel_name, resolve_bot_http};
use runtime_resolve::{resolve_direct_meeting_runtime, resolve_direct_meeting_shared};
#[allow(unused_imports)]
pub use snapshot::{
    DiscordHealthSnapshot, HealthStatus, WatcherStateSnapshot, active_request_owner_for_channel,
    build_health_snapshot, build_public_health_snapshot,
};

/// Per-provider snapshot for the health response.
pub(super) struct ProviderEntry {
    pub(super) name: String,
    pub(super) shared: Arc<SharedData>,
    pub(super) role: ProviderRuntimeRole,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ProviderRuntimeRole {
    Gateway,
    Standby,
}

impl ProviderRuntimeRole {
    pub(super) fn requires_gateway_connection(self) -> bool {
        matches!(self, Self::Gateway)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BotTokenReloadStatus {
    Reloaded,
    MissingOrInvalid,
    RuntimeRootUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BotTokenReloadScopeStatus {
    ReloadSupported,
    RestartRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BotTokenReloadScope {
    pub scope: &'static str,
    pub status: BotTokenReloadScopeStatus,
    pub live_reload_supported: bool,
    pub restart_required: bool,
    pub token_source: &'static str,
    pub detail: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BotTokenReloadScopes {
    pub utility_rest_clients: BotTokenReloadScope,
    pub provider_runtime_cached_token: BotTokenReloadScope,
    pub provider_gateway_session: BotTokenReloadScope,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BotTokenReloadEntry {
    pub bot: &'static str,
    pub credential: &'static str,
    pub status: BotTokenReloadStatus,
    pub reloaded: bool,
    pub previous_client_kept: bool,
    pub user_id_cache_invalidated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BotTokenReloadReport {
    pub announce: BotTokenReloadEntry,
    pub notify: BotTokenReloadEntry,
    pub runtime_root_available: bool,
    pub any_reloaded: bool,
    pub utility_bot_user_ids_invalidated: bool,
    pub scopes: BotTokenReloadScopes,
    pub provider_cached_bot_token_scope: &'static str,
}

pub fn bot_token_reload_scopes() -> BotTokenReloadScopes {
    BotTokenReloadScopes {
        utility_rest_clients: BotTokenReloadScope {
            scope: "utility_rest_clients",
            status: BotTokenReloadScopeStatus::ReloadSupported,
            live_reload_supported: true,
            restart_required: false,
            token_source: "credential/announce_bot_token and credential/notify_bot_token",
            detail: "POST /api/discord/bot-tokens/reload rebuilds announce/notify HealthRegistry REST clients in place.",
        },
        provider_runtime_cached_token: BotTokenReloadScope {
            scope: "provider_runtime_cached_token",
            status: BotTokenReloadScopeStatus::RestartRequired,
            live_reload_supported: false,
            restart_required: true,
            token_source: "discord.bots.<name>.token or credential/<name>_bot_token selected at provider runtime startup",
            detail: "SharedData.cached_bot_token is a OnceCell per provider runtime, so rotated provider REST fallback credentials are not adopted until dcserver restarts.",
        },
        provider_gateway_session: BotTokenReloadScope {
            scope: "provider_gateway_session",
            status: BotTokenReloadScopeStatus::RestartRequired,
            live_reload_supported: false,
            restart_required: true,
            token_source: "discord.bots.<name>.token or credential/<name>_bot_token selected at provider runtime startup",
            detail: "Discord gateway sessions are created by provider runtimes at startup; reconnecting them with a rotated token requires a dcserver restart.",
        },
    }
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
    /// HTTP clients and identity caches keyed by stable utility-bot role.
    utility_bots: [UtilityBotRuntime; UtilityBotRole::ALL.len()],
}

struct UtilityBotRuntime {
    role: UtilityBotRole,
    http: tokio::sync::Mutex<Option<Arc<serenity::Http>>>,
    user_id: tokio::sync::Mutex<Option<(u64, u64)>>,
    token_generation: AtomicU64,
}

impl UtilityBotRuntime {
    fn new(role: UtilityBotRole) -> Self {
        Self {
            role,
            http: tokio::sync::Mutex::new(None),
            user_id: tokio::sync::Mutex::new(None),
            token_generation: AtomicU64::new(0),
        }
    }
}

/// Result of resolving one utility bot's Discord user id.
///
/// `Unconfigured` is a stable absence: there is no HTTP client for that bot, so
/// catch-up does not need to wait for an identity that this runtime cannot
/// produce. `Unavailable` is deliberately distinct: a configured/current HTTP
/// client exists, but Discord could not resolve its user id (or token rotation
/// kept changing underneath the lookup). Callers that would make an irreversible
/// sender-identity decision must defer rather than treating that transient miss
/// as "not this utility bot".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UtilityBotUserIdResolution {
    Resolved(u64),
    Unconfigured,
    Unavailable,
}

impl UtilityBotUserIdResolution {
    pub(crate) fn user_id(self) -> Option<u64> {
        match self {
            Self::Resolved(user_id) => Some(user_id),
            Self::Unconfigured | Self::Unavailable => None,
        }
    }
}

impl HealthRegistry {
    pub fn new() -> Self {
        Self {
            providers: tokio::sync::Mutex::new(Vec::new()),
            started_at: Instant::now(),
            started_at_unix: chrono::Utc::now().timestamp(),
            discord_http: tokio::sync::Mutex::new(Vec::new()),
            utility_bots: UtilityBotRole::ALL.map(UtilityBotRuntime::new),
        }
    }

    /// Wall-clock Unix seconds at which this dcserver process booted. Used by
    /// the stall watchdog to anchor its post-restart grace window (#3041).
    pub(crate) fn started_at_unix(&self) -> i64 {
        self.started_at_unix
    }

    fn utility_bot(&self, role: UtilityBotRole) -> &UtilityBotRuntime {
        &self.utility_bots[role as usize]
    }

    /// Snapshot one utility-bot HTTP client by semantic role.
    pub(crate) async fn utility_bot_http_clone(
        &self,
        role: UtilityBotRole,
    ) -> Option<Arc<serenity::Http>> {
        self.utility_bot(role).http.lock().await.clone()
    }

    /// Snapshot the announce-role HTTP client. This role is where `Manage
    /// Messages` permissions are concentrated, so pin/unpin lifecycle code
    /// prefers it over per-provider HTTP clients.
    pub(crate) async fn announce_http_clone(&self) -> Option<Arc<serenity::Http>> {
        self.utility_bot_http_clone(UtilityBotRole::Announce).await
    }

    pub(in crate::services::discord) async fn register_standby(
        &self,
        name: String,
        shared: Arc<SharedData>,
    ) {
        self.register_with_role(name, shared, ProviderRuntimeRole::Standby)
            .await;
    }

    pub(crate) async fn all_providers_are_standby(&self) -> bool {
        let providers = self.providers.lock().await;
        !providers.is_empty()
            && providers
                .iter()
                .all(|entry| entry.role == ProviderRuntimeRole::Standby)
    }

    pub(in crate::services::discord) async fn register(
        &self,
        name: String,
        shared: Arc<SharedData>,
    ) {
        self.register_with_role(name, shared, ProviderRuntimeRole::Gateway)
            .await;
    }

    async fn register_with_role(
        &self,
        name: String,
        shared: Arc<SharedData>,
        role: ProviderRuntimeRole,
    ) {
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
        providers.push(ProviderEntry { name, shared, role });
    }

    pub(in crate::services::discord) async fn dm_default_agent_authorizes_private_channel(
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

    /// #3293: every registered runtime regardless of provider. Used by the
    /// provider-unfiltered mailbox-registry purge, which must visit every
    /// instance registry because a bogus entry may live in any of them.
    pub(in crate::services::discord) async fn all_registered_shared(&self) -> Vec<Arc<SharedData>> {
        self.providers
            .lock()
            .await
            .iter()
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
    pub(in crate::services::discord) async fn rebind_inflight(
        &self,
        provider: &crate::services::provider::ProviderKind,
        channel_id: u64,
        tmux_override: Option<String>,
        overrides: super::recovery_engine::ManualRebindOverrides,
        expected_episode: Option<&super::inflight::InflightEpisodePin>,
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
                overrides,
                expected_episode,
            )
            .await,
        )
    }

    pub(crate) async fn rebind_inflight_after_force_clean(
        &self,
        provider: &crate::services::provider::ProviderKind,
        channel_id: u64,
        tmux_override: Option<String>,
        minimum_initial_offset: Option<u64>,
    ) -> Option<Result<super::recovery_engine::RebindOutcome, super::recovery_engine::RebindError>>
    {
        let (http, shared) =
            resolve_direct_meeting_runtime(self, ChannelId::new(channel_id), provider)
                .await
                .ok()?;
        Some(
            super::recovery_engine::rebind_inflight_for_channel_with_minimum_start_offset(
                &http,
                &shared,
                provider,
                channel_id,
                tmux_override,
                minimum_initial_offset,
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
    /// Re-read the utility-bot credential files and rebuild the role-keyed
    /// `serenity::Http` clients in place. Clients are replaced atomically
    /// (per-mutex), and cached user ids are cleared so the next
    /// `utility_bot_user_id` call re-derives them against the new token.
    ///
    /// Returns a structured report so operator surfaces can distinguish
    /// "reloaded", "credential missing/invalid, kept prior client", and
    /// "runtime root unavailable" without ever exposing token material.
    pub async fn reload_bot_tokens(&self) -> BotTokenReloadReport {
        self.reload_bot_tokens_inner(true).await
    }

    async fn reload_bot_tokens_inner(&self, rotation: bool) -> BotTokenReloadReport {
        let runtime_root_available = super::runtime_store::agentdesk_root().is_some();
        let announce_runtime = self.utility_bot(UtilityBotRole::Announce);
        let notify_runtime = self.utility_bot(UtilityBotRole::Notify);
        let (announce, notify) = if runtime_root_available {
            (
                self.reload_utility_bot_token(announce_runtime, rotation)
                    .await,
                self.reload_utility_bot_token(notify_runtime, rotation)
                    .await,
            )
        } else {
            let announce = self
                .runtime_root_unavailable_reload_entry(announce_runtime)
                .await;
            let notify = self
                .runtime_root_unavailable_reload_entry(notify_runtime)
                .await;
            if rotation {
                tracing::warn!(
                    "reload_bot_tokens called before agentdesk runtime root is initialised"
                );
            }
            (announce, notify)
        };

        BotTokenReloadReport {
            runtime_root_available,
            any_reloaded: announce.reloaded || notify.reloaded,
            utility_bot_user_ids_invalidated: announce.user_id_cache_invalidated
                || notify.user_id_cache_invalidated,
            scopes: bot_token_reload_scopes(),
            provider_cached_bot_token_scope: "announce/notify HealthRegistry clients are reloaded; provider runtime SharedData.cached_bot_token is restart-only",
            announce,
            notify,
        }
    }

    async fn reload_utility_bot_token(
        &self,
        runtime: &UtilityBotRuntime,
        rotation: bool,
    ) -> BotTokenReloadEntry {
        let role = runtime.role;
        let bot_name = role.alias();
        let credential = role.credential_label();
        if let Some(token) = crate::credential::read_bot_token(bot_name) {
            let http = Arc::new(serenity::Http::new(&format!("Bot {token}")));
            *runtime.http.lock().await = Some(http);
            // Invalidate the cached user-id so the next utility call re-resolves
            // it via the rotated token; otherwise a stale id from a revoked bot
            // account could leak into routing.
            let mut user_id = runtime.user_id.lock().await;
            *user_id = None;
            runtime.token_generation.fetch_add(1, Ordering::SeqCst);
            let ts = chrono::Local::now().format("%H:%M:%S");
            let emoji = role.log_emoji();
            let action = if rotation { "reloaded" } else { "loaded" };
            tracing::info!(
                "  [{ts}] {emoji} {bot_name} bot {action} for /api/discord/send routing"
            );
            return BotTokenReloadEntry {
                bot: bot_name,
                credential,
                status: BotTokenReloadStatus::Reloaded,
                reloaded: true,
                previous_client_kept: false,
                user_id_cache_invalidated: true,
            };
        }

        let previous_client_kept = runtime.http.lock().await.is_some();
        if rotation {
            tracing::warn!(
                bot = bot_name,
                "reload_bot_tokens: credential file missing or invalid; keeping previous client"
            );
        }
        BotTokenReloadEntry {
            bot: bot_name,
            credential,
            status: BotTokenReloadStatus::MissingOrInvalid,
            reloaded: false,
            previous_client_kept,
            user_id_cache_invalidated: false,
        }
    }

    async fn runtime_root_unavailable_reload_entry(
        &self,
        runtime: &UtilityBotRuntime,
    ) -> BotTokenReloadEntry {
        BotTokenReloadEntry {
            bot: runtime.role.alias(),
            credential: runtime.role.credential_label(),
            status: BotTokenReloadStatus::RuntimeRootUnavailable,
            reloaded: false,
            previous_client_kept: runtime.http.lock().await.is_some(),
            user_id_cache_invalidated: false,
        }
    }

    pub async fn utility_bot_user_id(&self, role: UtilityBotRole) -> Option<u64> {
        self.utility_bot_user_id_resolution(role).await.user_id()
    }

    pub(crate) async fn utility_bot_user_id_resolution(
        &self,
        role: UtilityBotRole,
    ) -> UtilityBotUserIdResolution {
        let runtime = self.utility_bot(role);
        Self::utility_bot_user_id_resolution_from(
            &runtime.http,
            &runtime.user_id,
            &runtime.token_generation,
        )
        .await
    }

    async fn utility_bot_user_id_resolution_from(
        http_field: &tokio::sync::Mutex<Option<Arc<serenity::Http>>>,
        user_id_field: &tokio::sync::Mutex<Option<(u64, u64)>>,
        token_generation: &AtomicU64,
    ) -> UtilityBotUserIdResolution {
        for _ in 0..3 {
            let current_generation = token_generation.load(Ordering::SeqCst);
            if let Some((id, cached_generation)) = *user_id_field.lock().await
                && cached_generation == current_generation
            {
                return UtilityBotUserIdResolution::Resolved(id);
            }
            let Some(http) = http_field.lock().await.clone() else {
                return UtilityBotUserIdResolution::Unconfigured;
            };
            let observed_generation = token_generation.load(Ordering::SeqCst);
            let user = match http.get_current_user().await {
                Ok(user) => user,
                Err(_) => {
                    if Self::utility_bot_http_matches_current(http_field, &http).await {
                        return UtilityBotUserIdResolution::Unavailable;
                    }
                    continue;
                }
            };
            let id = user.id.get();
            if Self::cache_utility_bot_user_id_if_current(
                http_field,
                user_id_field,
                token_generation,
                observed_generation,
                &http,
                id,
            )
            .await
            {
                return UtilityBotUserIdResolution::Resolved(id);
            }
        }
        UtilityBotUserIdResolution::Unavailable
    }

    async fn utility_bot_http_matches_current(
        http_field: &tokio::sync::Mutex<Option<Arc<serenity::Http>>>,
        expected_http: &Arc<serenity::Http>,
    ) -> bool {
        http_field
            .lock()
            .await
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, expected_http))
    }

    async fn cache_utility_bot_user_id_if_current(
        http_field: &tokio::sync::Mutex<Option<Arc<serenity::Http>>>,
        user_id_field: &tokio::sync::Mutex<Option<(u64, u64)>>,
        token_generation: &AtomicU64,
        expected_generation: u64,
        expected_http: &Arc<serenity::Http>,
        id: u64,
    ) -> bool {
        if token_generation.load(Ordering::SeqCst) != expected_generation {
            return false;
        }
        if !Self::utility_bot_http_matches_current(http_field, expected_http).await {
            return false;
        }
        let mut cached_user_id = user_id_field.lock().await;
        if token_generation.load(Ordering::SeqCst) != expected_generation {
            return false;
        }
        if cached_user_id
            .as_ref()
            .is_none_or(|(_, cached_generation)| *cached_generation != expected_generation)
        {
            *cached_user_id = Some((id, expected_generation));
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::Path, sync::MutexGuard};
    use tempfile::TempDir;

    struct EnvVarGuard {
        key: String,
        previous_value: Option<std::ffi::OsString>,
        _lock: MutexGuard<'static, ()>,
    }

    impl EnvVarGuard {
        fn set_path(key: &str, path: &Path) -> Self {
            let lock = crate::config::shared_test_env_lock()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let previous_value = std::env::var_os(key);
            unsafe { std::env::set_var(key, path) };
            Self {
                key: key.to_string(),
                previous_value,
                _lock: lock,
            }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous_value {
                Some(value) => unsafe { std::env::set_var(&self.key, value) },
                None => unsafe { std::env::remove_var(&self.key) },
            }
        }
    }

    fn write_test_bot_token(root: &Path, bot_name: &str, token: &str) {
        crate::runtime_layout::ensure_credential_layout(root).unwrap();
        let path = crate::runtime_layout::credential_token_path(root, bot_name);
        crate::utils::secret_file::write_secret_file(&path, format!("{token}\n"))
            .expect("write test bot token");
    }

    #[test]
    fn reload_bot_tokens_reports_success_and_invalidates_user_id_cache() {
        let temp = TempDir::new().expect("temp runtime root");
        let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
        write_test_bot_token(temp.path(), "announce", "announce-token");
        write_test_bot_token(temp.path(), "notify", "notify-token");

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        runtime.block_on(async {
            let registry = HealthRegistry::new();
            *registry
                .utility_bot(UtilityBotRole::Announce)
                .user_id
                .lock()
                .await = Some((11, 0));
            *registry
                .utility_bot(UtilityBotRole::Notify)
                .user_id
                .lock()
                .await = Some((22, 0));

            let report = registry.reload_bot_tokens().await;

            assert!(report.runtime_root_available);
            assert!(report.any_reloaded);
            assert!(report.utility_bot_user_ids_invalidated);
            assert_eq!(report.announce.status, BotTokenReloadStatus::Reloaded);
            assert!(report.announce.reloaded);
            assert!(report.announce.user_id_cache_invalidated);
            assert_eq!(
                *registry
                    .utility_bot(UtilityBotRole::Announce)
                    .user_id
                    .lock()
                    .await,
                None
            );
            assert_eq!(report.notify.status, BotTokenReloadStatus::Reloaded);
            assert!(report.notify.reloaded);
            assert!(report.notify.user_id_cache_invalidated);
            assert_eq!(
                *registry
                    .utility_bot(UtilityBotRole::Notify)
                    .user_id
                    .lock()
                    .await,
                None
            );
            assert!(resolve_bot_http(&registry, "announce").await.is_ok());
            assert!(resolve_bot_http(&registry, "notify").await.is_ok());
        });
    }

    #[test]
    fn reload_bot_tokens_keeps_previous_client_when_credential_is_missing() {
        let temp = TempDir::new().expect("temp runtime root");
        let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
        write_test_bot_token(temp.path(), "announce", "announce-token");

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        runtime.block_on(async {
            let registry = HealthRegistry::new();
            let first = registry.reload_bot_tokens().await;
            assert_eq!(first.announce.status, BotTokenReloadStatus::Reloaded);
            assert!(resolve_bot_http(&registry, "announce").await.is_ok());

            std::fs::remove_file(crate::runtime_layout::credential_token_path(
                temp.path(),
                "announce",
            ))
            .expect("remove announce token");
            let second = registry.reload_bot_tokens().await;

            assert_eq!(
                second.announce.status,
                BotTokenReloadStatus::MissingOrInvalid
            );
            assert!(!second.announce.reloaded);
            assert!(second.announce.previous_client_kept);
            assert!(resolve_bot_http(&registry, "announce").await.is_ok());
        });
    }

    #[tokio::test]
    async fn utility_bot_user_id_cache_rejects_stale_http_after_reload() {
        let registry = HealthRegistry::new();
        let runtime = registry.utility_bot(UtilityBotRole::Announce);
        let old_http = Arc::new(serenity::Http::new("Bot old-token"));
        let new_http = Arc::new(serenity::Http::new("Bot new-token"));

        *runtime.http.lock().await = Some(old_http.clone());
        let old_generation = runtime.token_generation.load(Ordering::SeqCst);
        assert!(
            HealthRegistry::cache_utility_bot_user_id_if_current(
                &runtime.http,
                &runtime.user_id,
                &runtime.token_generation,
                old_generation,
                &old_http,
                11,
            )
            .await
        );
        assert_eq!(*runtime.user_id.lock().await, Some((11, old_generation)));

        runtime.token_generation.fetch_add(1, Ordering::SeqCst);
        *runtime.http.lock().await = Some(new_http);
        *runtime.user_id.lock().await = None;
        assert!(
            !HealthRegistry::cache_utility_bot_user_id_if_current(
                &runtime.http,
                &runtime.user_id,
                &runtime.token_generation,
                old_generation,
                &old_http,
                22,
            )
            .await
        );
        assert_eq!(*runtime.user_id.lock().await, None);
    }
}
