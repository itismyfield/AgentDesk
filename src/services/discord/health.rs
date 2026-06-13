use std::sync::Arc;
use std::time::Instant;

use poise::serenity_prelude as serenity;
use serenity::ChannelId;

use super::SharedData;
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
mod mailbox;
mod provider_probe;
mod recovery;
mod redaction;
mod relay_auto_heal;
mod runtime_resolve;
mod session_enrichment;
mod snapshot;
mod stall_liveness;
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
    start_headless_agent_turn, start_headless_agent_turn_in_dm, start_reserved_headless_agent_turn,
    start_reserved_headless_agent_turn_in_dm,
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
    spawn_watchdog, stop_runtime_turn_preserving_watcher,
};
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
    // #3034: operator-triggered token-rotation entry point (#2047 Finding 11);
    // not yet wired to an HTTP/CLI route. Keep the rotation API live.
    #[allow(dead_code)]
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
