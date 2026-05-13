#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, CreateMessage};
use sqlx::PgPool;

use super::SharedData;
use super::formatting::{build_long_message_attachment, split_message};
use crate::db::Db;
use crate::server::routes::dispatches::discord_delivery::{
    DispatchMessagePostError, DispatchMessagePostErrorKind,
};
use crate::services::discord::outbound::delivery::{
    deliver_outbound as deliver_v3_outbound, first_raw_message_id,
};
use crate::services::discord::outbound::message::{DiscordOutboundMessage, OutboundTarget};
use crate::services::discord::outbound::policy::DiscordOutboundPolicy;
use crate::services::discord::outbound::result::{DeliveryResult, FallbackUsed};
use crate::services::discord::outbound::{
    DISCORD_HARD_LIMIT_CHARS, DISCORD_SAFE_LIMIT_CHARS, DiscordOutboundClient, OutboundDeduper,
};
use crate::services::provider::ProviderKind;

mod mailbox;
mod provider_probe;
mod recovery;
mod redaction;
mod session_enrichment;
mod snapshot;

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use crate::services::discord::relay_health::{RelayActiveTurn, RelayStallState};
pub(crate) use recovery::stop_provider_channel_runtime_with_policy;
#[allow(unused_imports)]
pub use recovery::{
    HardStopRuntimeResult, PendingQueueSnapshot, PostCancelDrainOutcome, RuntimeTurnStopResult,
    clear_provider_channel_runtime, force_kill_provider_channel_runtime, handle_rebind_inflight,
    handle_relay_recovery, hard_stop_runtime_turn, resolve_tmux_session_for_cancel,
    schedule_pending_queue_drain_after_cancel, snapshot_pending_queue_state, spawn_stall_watchdog,
    spawn_watchdog, stop_provider_channel_runtime, stop_runtime_turn_preserving_watcher,
};
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use recovery::{
    STALL_WATCHDOG_THRESHOLD_SECS, parse_rebind_body, rebind_error_status_and_message,
    run_stall_watchdog_pass,
};
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use session_enrichment::WATCHER_STATE_DESYNC_STALE_MS;
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use snapshot::normalize_global_active_counter;
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

    /// Snapshot the notify-bot HTTP client (for non-actionable side channels
    /// like the idle-recap renderer). Returns `None` when the notify bot
    /// hasn't been registered yet — caller treats that as "skip the post
    /// this cycle".
    pub(crate) async fn notify_http_clone(&self) -> Option<Arc<serenity::Http>> {
        self.notify_http.lock().await.clone()
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) struct TestHealthHarness {
    provider: ProviderKind,
    registry: Arc<HealthRegistry>,
    shared: Arc<SharedData>,
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
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
            placeholder_cleanup: Arc::new(
                super::placeholder_cleanup::PlaceholderCleanupRegistry::default(),
            ),
            placeholder_controller: Arc::new(
                super::placeholder_controller::PlaceholderController::default(),
            ),
            placeholder_live_events: Arc::new(
                super::placeholder_live_events::PlaceholderLiveEvents::default(),
            ),
            placeholder_live_events_enabled: false,
            status_panel_v2_enabled: false,
            queued_placeholders: dashmap::DashMap::new(),
            queue_exit_placeholder_clears: dashmap::DashMap::new(),
            queued_placeholders_persist_locks: dashmap::DashMap::new(),
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
            voice_barge_in: Arc::new(super::voice_barge_in::VoiceBargeInRuntime::disabled()),
            voice_pairings: Arc::new(
                super::voice_routing::VoiceChannelPairingStore::load_default(),
            ),
            bot_connected: std::sync::atomic::AtomicBool::new(true),
            last_turn_at: std::sync::Mutex::new(None),
            model_overrides: dashmap::DashMap::new(),
            fast_mode_channels: dashmap::DashSet::new(),
            fast_mode_session_reset_pending: dashmap::DashSet::new(),
            codex_goals_channels: dashmap::DashSet::new(),
            codex_goals_session_reset_pending: dashmap::DashSet::new(),
            model_session_reset_pending: dashmap::DashSet::new(),
            session_reset_pending: dashmap::DashSet::new(),
            model_picker_pending: dashmap::DashMap::new(),
            dispatch_role_overrides: dashmap::DashMap::new(),
            last_message_ids: dashmap::DashMap::new(),
            catch_up_retry_pending: dashmap::DashMap::new(),
            turn_start_times: dashmap::DashMap::new(),
            channel_rosters: dashmap::DashMap::new(),
            cached_serenity_ctx: tokio::sync::OnceCell::new(),
            cached_bot_token: tokio::sync::OnceCell::new(),
            token_hash: super::settings::discord_token_hash("test-token"),
            provider: provider.clone(),
            api_port: 8791,
            #[cfg(all(test, feature = "legacy-sqlite-tests"))]
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

    /// #1446 — expose the inner `SharedData` so router-level intake tests
    /// can drive `dispatch_thread_parents` / mailbox state directly. Test-
    /// only accessor; the harness retains its own `Arc` clone, so the
    /// returned handle is safe to operate on after the harness is dropped.
    pub(crate) fn shared(&self) -> Arc<SharedData> {
        self.shared.clone()
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

    pub(crate) fn set_deferred_hooks(&self, count: usize) {
        self.shared
            .deferred_hook_backlog
            .store(count, std::sync::atomic::Ordering::Relaxed);
    }

    /// #1672 P2: read-only view of `deferred_hook_backlog` so route
    /// tests can verify that a cancel surface scheduled a deferred
    /// idle-queue drain. The `SharedData` field is `pub(super)`, which
    /// is invisible from `server::routes::routes_tests` — this getter
    /// keeps that boundary intact while still letting tests assert on
    /// the post-cancel drain contract.
    pub(crate) fn deferred_hook_backlog(&self) -> usize {
        self.shared
            .deferred_hook_backlog
            .load(std::sync::atomic::Ordering::Relaxed)
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
                last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(
                    super::tmux_watcher_now_ms(),
                )),
                mailbox_finalize_owed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        );
        cancel
    }

    pub(crate) fn seed_watcher_for_tmux(
        &self,
        channel_id: u64,
        tmux_session_name: &str,
    ) -> Arc<std::sync::atomic::AtomicBool> {
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.shared.tmux_watchers.insert(
            ChannelId::new(channel_id),
            super::TmuxWatcherHandle {
                tmux_session_name: tmux_session_name.to_string(),
                paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                resume_offset: Arc::new(std::sync::Mutex::new(None)),
                cancel: cancel.clone(),
                pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(
                    super::tmux_watcher_now_ms(),
                )),
                mailbox_finalize_owed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
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

            #[cfg(all(test, feature = "legacy-sqlite-tests"))]
            {
                let db = db.ok_or_else(|| {
                    SendTargetResolutionError::Internal(
                        "sqlite db unavailable during test agent lookup".to_string(),
                    )
                })?;
                return resolve_agent_target_channel_id_sqlite(db, agent_id);
            }

            #[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
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
            "auto-queue-monitor",
            "inventory",
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
        assert!(is_allowed_send_source("auto-queue-monitor"));
        assert!(is_allowed_send_source("inventory"));
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
            if let Some(key) = dedup_key.as_deref() {
                dedup.record(key, message_id);
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
        content,
        summary,
        delivery_id,
        content_len > DISCORD_SAFE_LIMIT_CHARS,
    )
    .await;
    record_manual_delivery_success(dedup, dedup_key.as_deref(), &result);
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
        record_manual_delivery_success(dedup, dedup_key.as_deref(), &result);
        return result;
    }

    let result = deliver_manual_v3_text(
        client,
        dedup,
        OutboundTarget::DmUser(serenity::UserId::new(user_id)),
        &format!("dm:{user_id}"),
        content,
        summary,
        delivery_id,
        content_len > DISCORD_SAFE_LIMIT_CHARS,
    )
    .await;
    record_manual_delivery_success(dedup, dedup_key.as_deref(), &result);
    result
}

async fn deliver_manual_v3_text<C: DiscordOutboundClient>(
    client: &C,
    dedup: &OutboundDeduper,
    target: OutboundTarget,
    target_label: &str,
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
            (
                delivery_id.correlation_id.to_string(),
                delivery_id.semantic_event_id.to_string(),
            )
        })
        .unwrap_or_else(|| {
            (
                format!("manual:no-idempotency:{target_label}"),
                "manual:no-idempotency".to_string(),
            )
        });
    let mut outbound_msg =
        DiscordOutboundMessage::new(correlation_id, semantic_event_id, content, target, policy);
    if let Some(summary) = summary.map(str::trim).filter(|value| !value.is_empty()) {
        outbound_msg = outbound_msg.with_summary(summary.to_string());
    }

    match deliver_v3_outbound(client, dedup, outbound_msg).await {
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
        DeliveryResult::Duplicate { .. } => ManualDeliveryOutcome::Sent {
            message_id: String::new(),
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
    dedup_key: Option<&str>,
    result: &ManualDeliveryOutcome,
) {
    let ManualDeliveryOutcome::Sent { message_id, .. } = result else {
        return;
    };
    if message_id.is_empty() {
        return;
    }
    if let Some(key) = dedup_key {
        dedup.record(key, message_id);
    }
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
    sqlite: Option<&Db>,
    pg_pool: Option<&PgPool>,
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
    send_message_with_backends(
        registry,
        sqlite,
        pg_pool,
        &target,
        &request.message,
        "system",
        &request.mode,
        None,
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
        manual_notification_deduper(),
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
                message_id: String::new(),
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
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use crate::services::discord::DISCORD_MSG_LIMIT;
    use chrono::TimeZone;
    use poise::serenity_prelude::{MessageId, UserId};

    struct TestTmuxSession {
        name: String,
    }

    impl Drop for TestTmuxSession {
        fn drop(&mut self) {
            let _ = std::process::Command::new("tmux")
                .args(["kill-session", "-t", &self.name])
                .status();
        }
    }

    fn start_test_tmux_session(label: &str) -> Option<TestTmuxSession> {
        if !crate::services::claude::is_tmux_available() {
            eprintln!("skipping watcher-state live tmux assertion: tmux is unavailable");
            return None;
        }
        let name = format!(
            "AgentDesk-health-{label}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        );
        let created = std::process::Command::new("tmux")
            .args(["new-session", "-d", "-s", &name, "sleep 600"])
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        assert!(
            created,
            "failed to create tmux session for watcher-state test"
        );
        Some(TestTmuxSession { name })
    }

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
        assert_eq!(source, "system");
    }

    #[test]
    fn test_parse_send_request_missing_source_defaults_system() {
        let body = r#"{"target":"channel:999","content":"msg"}"#;
        let result = parse_send_body(body);
        assert!(result.is_ok());
        let (_, _, source) = result.unwrap();
        assert_eq!(source, "system");
    }

    #[test]
    fn test_parse_send_request_accepts_documented_aliases() {
        let body = r#"{"channel_id":"999","message":"msg"}"#;
        let result = parse_send_body(body);
        assert!(result.is_ok());
        let (target, content, source) = result.unwrap();
        assert_eq!(target, "channel:999");
        assert_eq!(content, "msg");
        assert_eq!(source, "system");
    }

    #[test]
    fn headless_turn_is_allowed_internal_send_source() {
        assert!(is_allowed_send_source("headless_turn"));
        assert!(is_allowed_send_source("lifecycle_notifier"));
        assert!(is_allowed_send_source("routine-runtime"));
        assert!(is_allowed_send_source("slo_alerter"));
        assert!(is_allowed_send_source("auto-queue-monitor"));
        assert!(is_allowed_send_source("inventory"));
        assert!(!is_allowed_send_source("not-a-real-source"));
    }

    #[derive(Clone, Default)]
    struct MockManualOutboundClient {
        posts: Arc<std::sync::Mutex<Vec<String>>>,
        post_targets: Arc<std::sync::Mutex<Vec<String>>>,
        dm_resolutions: Arc<std::sync::Mutex<Vec<String>>>,
        attachments: Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>,
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
    async fn manual_notification_without_delivery_id_does_not_dedupe_same_content() {
        let client = MockManualOutboundClient::default();
        let dedup = OutboundDeduper::new();

        let first =
            deliver_manual_notification(&client, &dedup, "123", "hello", "announce", None, None)
                .await;
        let second =
            deliver_manual_notification(&client, &dedup, "123", "hello", "announce", None, None)
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
                message_id: "message-2".to_string(),
                delivery: None
            }
        );
        assert_eq!(client.posts.lock().unwrap().len(), 2);
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
                message_id: String::new(),
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
                correlation_id: None,
                semantic_event_id: None,
                idempotency_key: None,
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
    fn test_parse_senddm_body_accepts_delivery_ids() {
        let body = r#"{
            "user_id":"123",
            "content":"hello",
            "bot":"claude",
            "correlation_id":"senddm:custom",
            "semantic_event_id":"senddm:custom:event"
        }"#;
        let parsed = parse_senddm_body(body).expect("senddm body should parse");
        assert_eq!(
            parsed.delivery_id(),
            Some((
                "senddm:custom".to_string(),
                "senddm:custom:event".to_string()
            ))
        );
    }

    #[test]
    fn test_senddm_delivery_id_is_absent_without_explicit_ids() {
        let first =
            parse_senddm_body(r#"{"user_id":"123","content":"hello","bot":"claude"}"#).unwrap();
        let second =
            parse_senddm_body(r#"{"user_id":"123","content":"hello","bot":"claude"}"#).unwrap();

        assert_eq!(first.delivery_id(), None);
        assert_eq!(second.delivery_id(), None);
    }

    #[test]
    fn test_senddm_delivery_id_uses_idempotency_key() {
        let parsed = parse_senddm_body(
            r#"{"user_id":"123","content":"hello","bot":"claude","idempotency_key":"family/checkup 1"}"#,
        )
        .unwrap();
        assert_eq!(
            parsed.delivery_id(),
            Some((
                "senddm:123".to_string(),
                "senddm:123:family_checkup_1".to_string()
            ))
        );
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
        let (status, body) = handle_send_to_agent(
            &registry,
            Some(&db),
            None,
            r#"{"role_id":"missing","message":"hello"}"#,
        )
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
        coord
            .reconnect_count
            .store(2, std::sync::atomic::Ordering::Release);

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
        assert_eq!(snapshot.last_capture_offset, None);
        assert_eq!(snapshot.unread_bytes, None);
        assert!(!snapshot.desynced);
        assert_eq!(snapshot.reconnect_count, 2);
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
        assert_eq!(snapshot.relay_stall_state, RelayStallState::Healthy);
        assert_eq!(snapshot.relay_health.active_turn, RelayActiveTurn::None);
        assert!(snapshot.relay_health.watcher_attached);

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
            "last_capture_offset",
            "unread_bytes",
            "desynced",
            "reconnect_count",
            "has_pending_queue",
            "watcher_owner_channel_id",
            "relay_stall_state",
            "relay_health",
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
    async fn snapshot_watcher_state_reports_cross_owner_tmux_as_desynced() {
        let Some(tmux_guard) = start_test_tmux_session("cross-owner") else {
            return;
        };
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
        let tmux_session_name = tmux_guard.name.clone();
        harness.seed_watcher_for_tmux(owner_channel_id, &tmux_session_name);

        let mut inflight = super::super::inflight::InflightTurnState::new(
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
        inflight.started_at = "2026-04-25 03:00:00".to_string();
        super::super::inflight::save_inflight_state(&inflight).expect("write seeded inflight JSON");

        let owner_snapshot = harness
            .registry()
            .snapshot_watcher_state(owner_channel_id)
            .await
            .expect("owner channel should still report its attached watcher");
        assert!(owner_snapshot.attached);
        assert_eq!(
            owner_snapshot.tmux_session.as_deref(),
            Some(tmux_session_name.as_str())
        );

        let snapshot = harness
            .registry()
            .snapshot_watcher_state(inflight_channel_id)
            .await
            .expect("inflight tmux owner should surface channel diagnostics");

        assert_eq!(snapshot.watcher_owner_channel_id, Some(owner_channel_id));
        assert_eq!(
            snapshot.tmux_session.as_deref(),
            Some(tmux_session_name.as_str())
        );
        assert!(
            !snapshot.attached,
            "a tmux watcher owned by another channel must not make this channel attached"
        );
        assert_eq!(snapshot.tmux_session_alive, Some(true));
        assert!(
            snapshot.desynced,
            "live cross-owner inflight must surface as desynced for the inflight channel"
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
        let output_path = temp.path().join("watcher-state-inflight-output.jsonl");
        std::fs::write(&output_path, vec![b'x'; 96]).expect("seed output capture");

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
            /* output_path */ Some(output_path.to_string_lossy().into_owned()),
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
        let coord = harness.shared.tmux_relay_coord(ChannelId::new(channel_id));
        coord
            .confirmed_end_offset
            .store(32, std::sync::atomic::Ordering::Release);
        coord.last_relay_ts_ms.store(
            chrono::Utc::now().timestamp_millis(),
            std::sync::atomic::Ordering::Release,
        );

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
        assert_eq!(snapshot.last_capture_offset, Some(96));
        assert_eq!(snapshot.unread_bytes, Some(64));
        assert!(
            !snapshot.desynced,
            "non-live tmux session must not be marked desynced solely because a temp output file has unread bytes"
        );
        assert_eq!(snapshot.reconnect_count, 0);
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
    async fn snapshot_watcher_state_marks_capture_lag_desynced_for_live_tmux() {
        let Some(tmux_guard) = start_test_tmux_session("capture-lag") else {
            return;
        };
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
        let channel_id = 600_111_222_333_444_556;
        harness.seed_watcher_for_tmux(channel_id, &tmux_guard.name);
        let output_path = temp.path().join("watcher-state-desynced-output.jsonl");
        std::fs::write(&output_path, vec![b'x'; 96]).expect("seed output capture");

        let mut inflight = super::super::inflight::InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("watcher-state-desync".to_string()),
            24,
            9_011,
            9_012,
            String::new(),
            Some("session-inflight-desync".to_string()),
            Some(tmux_guard.name.clone()),
            Some(output_path.to_string_lossy().into_owned()),
            None,
            0,
        );
        inflight.started_at = "2026-04-25 03:00:00".to_string();
        super::super::inflight::save_inflight_state(&inflight).expect("write seeded inflight JSON");
        let coord = harness.shared.tmux_relay_coord(ChannelId::new(channel_id));
        coord
            .confirmed_end_offset
            .store(32, std::sync::atomic::Ordering::Release);
        coord.last_relay_ts_ms.store(
            chrono::Utc::now().timestamp_millis() - WATCHER_STATE_DESYNC_STALE_MS - 1_000,
            std::sync::atomic::Ordering::Release,
        );

        let snapshot = harness
            .registry()
            .snapshot_watcher_state(channel_id)
            .await
            .expect("live tmux watcher should surface a snapshot");

        assert!(snapshot.attached);
        assert_eq!(snapshot.tmux_session_alive, Some(true));
        assert_eq!(snapshot.last_capture_offset, Some(96));
        assert_eq!(snapshot.unread_bytes, Some(64));
        assert!(snapshot.desynced);
    }

    #[tokio::test]
    async fn snapshot_watcher_state_marks_never_relayed_stale_capture_desynced() {
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
        let channel_id = 600_111_222_333_444_558;
        let output_path = temp.path().join("watcher-state-never-relayed-output.jsonl");
        std::fs::write(&output_path, vec![b'x'; 96]).expect("seed output capture");

        let mut inflight = super::super::inflight::InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("watcher-state-never-relayed".to_string()),
            24,
            9_031,
            9_032,
            String::new(),
            Some("session-inflight-never-relayed".to_string()),
            Some("nonexistent-session-never-relayed".to_string()),
            Some(output_path.to_string_lossy().into_owned()),
            None,
            0,
        );
        inflight.started_at = "2026-04-25 03:00:00".to_string();
        super::super::inflight::save_inflight_state(&inflight).expect("write seeded inflight JSON");

        let snapshot = harness
            .registry()
            .snapshot_watcher_state(channel_id)
            .await
            .expect("stale never-relayed inflight should surface a snapshot");

        assert_eq!(snapshot.last_relay_ts_ms, 0);
        assert_eq!(snapshot.last_capture_offset, Some(96));
        assert_eq!(snapshot.unread_bytes, Some(96));
        assert!(
            snapshot.desynced,
            "a stale inflight with capture bytes and no relay must be marked desynced"
        );
    }

    #[tokio::test]
    async fn snapshot_watcher_state_marks_capture_regression_desynced() {
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
        let channel_id = 600_111_222_333_444_559;
        let output_path = temp.path().join("watcher-state-regressed-output.jsonl");
        std::fs::write(&output_path, vec![b'x'; 16]).expect("seed truncated output capture");

        let mut inflight = super::super::inflight::InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("watcher-state-regressed".to_string()),
            24,
            9_041,
            9_042,
            String::new(),
            Some("session-inflight-regressed".to_string()),
            Some("nonexistent-session-regressed".to_string()),
            Some(output_path.to_string_lossy().into_owned()),
            None,
            0,
        );
        inflight.started_at = "2026-04-25 03:00:00".to_string();
        super::super::inflight::save_inflight_state(&inflight).expect("write seeded inflight JSON");
        let coord = harness.shared.tmux_relay_coord(ChannelId::new(channel_id));
        coord
            .confirmed_end_offset
            .store(32, std::sync::atomic::Ordering::Release);
        coord.last_relay_ts_ms.store(
            chrono::Utc::now().timestamp_millis() - WATCHER_STATE_DESYNC_STALE_MS - 1_000,
            std::sync::atomic::Ordering::Release,
        );

        let snapshot = harness
            .registry()
            .snapshot_watcher_state(channel_id)
            .await
            .expect("capture regression should surface a snapshot");

        assert_eq!(snapshot.last_capture_offset, Some(16));
        assert_eq!(
            snapshot.unread_bytes,
            Some(0),
            "unread_bytes remains saturating for compatibility"
        );
        assert!(
            snapshot.desynced,
            "capture size behind relay offset should be marked desynced when stale"
        );
    }

    #[tokio::test]
    async fn snapshot_watcher_state_marks_live_tmux_inflight_without_watcher_desynced() {
        let Some(tmux_guard) = start_test_tmux_session("orphaned") else {
            return;
        };
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
        let channel_id = 600_111_222_333_444_557;
        let mut inflight = super::super::inflight::InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("watcher-state-orphan".to_string()),
            24,
            9_021,
            9_022,
            String::new(),
            Some("session-inflight-orphan".to_string()),
            Some(tmux_guard.name.clone()),
            None,
            None,
            0,
        );
        inflight.started_at = "2026-04-25 03:00:00".to_string();
        super::super::inflight::save_inflight_state(&inflight).expect("write seeded inflight JSON");

        let snapshot = harness
            .registry()
            .snapshot_watcher_state(channel_id)
            .await
            .expect("live orphaned tmux inflight should surface a snapshot");

        assert!(!snapshot.attached);
        assert_eq!(snapshot.tmux_session_alive, Some(true));
        assert!(snapshot.desynced);
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

    #[test]
    fn normalize_global_active_counter_preserves_ordinary_snapshot_races() {
        let (global_active, reason) = normalize_global_active_counter(3, 2, 0);

        assert_eq!(global_active, 3);
        assert_eq!(reason, None);
    }

    #[test]
    fn normalize_global_active_counter_bounds_wrapped_values_only() {
        let (global_active, reason) = normalize_global_active_counter(usize::MAX, 2, 1);

        assert_eq!(global_active, 2);
        assert!(reason.is_some_and(|reason| {
            reason.contains("raw=")
                && reason.contains("provider_active_turns=2")
                && reason.contains("global_finalizing=1")
        }));
    }

    #[tokio::test]
    async fn health_snapshot_bounds_wrapped_global_active_counter() {
        let harness = TestHealthHarness::new().await;
        harness
            .seed_active_turn(713_000_000_000_000_010, 42, 9_001)
            .await;
        harness
            .seed_active_turn(713_000_000_000_000_011, 43, 9_002)
            .await;
        harness
            .shared
            .global_active
            .store(usize::MAX, Ordering::Relaxed);

        let snapshot = build_health_snapshot(&harness.registry()).await;
        let json = serde_json::to_value(&snapshot).unwrap();

        assert_eq!(snapshot.status(), HealthStatus::Degraded);
        assert_eq!(json["global_active"], 2);
        assert_eq!(json["global_finalizing"], 0);
        assert_eq!(json["providers"][0]["active_turns"], 2);
        assert!(
            json["degraded_reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|reason| reason.as_str().is_some_and(
                    |reason| reason.starts_with("global_active_counter_out_of_bounds:raw=")
                ))
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

    /// #1446 Layer 3 (integration) — the watchdog must force-clean a
    /// channel whose snapshot satisfies the conjunction (attached +
    /// desynced + stale inflight). We construct that exact state via the
    /// existing `TestHealthHarness` + a tmux session so the snapshot's
    /// `live_tmux_orphaned` branch fires (`tmux_alive && inflight_state &&
    /// !attached && relay_stale` — by routing the watcher binding to a
    /// DIFFERENT channel than the inflight, the inflight channel reports
    /// `attached=false` from the binding view but the snapshot still
    /// classifies it as `attached` via `inflight_owner_matches_channel`
    /// when the watcher owner-map points back at it).
    ///
    /// We rely on the simpler `tmux_session_mismatch && relay_stale`
    /// desync path: seed two watcher bindings for the same tmux name from
    /// two different channels, write an inflight whose `tmux_session_name`
    /// disagrees with the binding, and pin the relay-coord to a stale
    /// timestamp.
    #[tokio::test]
    async fn stall_watchdog_force_cleans_desynced_attached_watcher() {
        let Some(tmux_guard) = start_test_tmux_session("watchdog-clean") else {
            return;
        };
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
        let channel_id: u64 = 700_111_222_333_444_001;
        // Bind the live tmux session to this channel — establishes
        // `attached=true` via the watcher owner-map.
        harness.seed_watcher_for_tmux(channel_id, &tmux_guard.name);

        // Write an inflight pointing at the SAME live tmux session and an
        // output capture file with bytes the relay never advanced past.
        let output_path = temp.path().join("watchdog-clean-output.jsonl");
        std::fs::write(&output_path, vec![b'x'; 256]).expect("seed output capture");
        let mut inflight = super::super::inflight::InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("watchdog-clean".to_string()),
            42,
            8_001,
            8_002,
            String::new(),
            Some("session-watchdog-clean".to_string()),
            Some(tmux_guard.name.clone()),
            Some(output_path.to_string_lossy().into_owned()),
            None,
            0,
        );
        // Backdate updated_at WAY beyond the watchdog threshold. The
        // snapshot's `parse_started_at_unix` reads the same encoding the
        // helper uses, so this also drives the staleness check.
        let stale_unix =
            chrono::Utc::now().timestamp() - (super::STALL_WATCHDOG_THRESHOLD_SECS as i64) - 60;
        let stale_local = chrono::Local
            .timestamp_opt(stale_unix, 0)
            .single()
            .expect("valid local time")
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        inflight.started_at = stale_local.clone();
        inflight.updated_at = stale_local.clone();
        super::super::inflight::save_inflight_state(&inflight)
            .expect("write seeded stale inflight JSON");
        // Re-stamp updated_at on disk (save_inflight_state rewrites it to
        // now) — write the JSON directly to bypass the auto-stamp.
        let json = serde_json::to_string_pretty(&inflight).expect("serialize stale inflight");
        let inflight_path = super::super::inflight::inflight_runtime_root()
            .expect("inflight root override")
            .join("codex")
            .join(format!("{channel_id}.json"));
        std::fs::write(&inflight_path, json).expect("rewrite stale inflight on disk");

        // Backdate the relay-coord too so `relay_stale` fires.
        let coord = harness.shared.tmux_relay_coord(ChannelId::new(channel_id));
        coord.last_relay_ts_ms.store(
            chrono::Utc::now().timestamp_millis() - WATCHER_STATE_DESYNC_STALE_MS - 5_000,
            std::sync::atomic::Ordering::Release,
        );
        coord
            .confirmed_end_offset
            .store(8, std::sync::atomic::Ordering::Release);

        // Seed dispatch_thread_parents so the watchdog's parent-mapping
        // cleanup path is also exercised.
        let parent_channel_id: u64 = 700_111_222_333_444_999;
        harness.shared.dispatch_thread_parents.insert(
            ChannelId::new(parent_channel_id),
            ChannelId::new(channel_id),
        );

        // Sanity: snapshot must classify this as desynced before the run.
        let pre_snapshot = harness
            .registry()
            .snapshot_watcher_state(channel_id)
            .await
            .expect("seeded inflight should surface a snapshot");
        assert!(pre_snapshot.attached, "watcher binding implies attached");
        assert!(
            pre_snapshot.desynced,
            "stale capture-lag must classify as desynced before watchdog runs"
        );
        assert!(
            pre_snapshot.inflight_state_present,
            "inflight file must be present pre-watchdog"
        );

        // Run the watchdog pass.
        let cleaned =
            super::run_stall_watchdog_pass(&harness.registry(), &ProviderKind::Codex).await;
        assert_eq!(
            cleaned, 1,
            "watchdog must report cleaning exactly 1 channel"
        );

        // Inflight file must be gone.
        assert!(
            super::super::inflight::load_inflight_state(&ProviderKind::Codex, channel_id).is_none(),
            "watchdog must delete the stale inflight state file"
        );
        // Parent → thread mapping must be cleared.
        assert!(
            !harness
                .shared
                .dispatch_thread_parents
                .contains_key(&ChannelId::new(parent_channel_id)),
            "watchdog must drop the dispatch_thread_parents entry pointing at the cleaned channel"
        );
    }

    /// #1446 Layer 3 — the watchdog must NOT touch a channel whose
    /// inflight `updated_at` is fresh, even if it happens to look
    /// desynced for an unrelated reason. This is the false-positive guard
    /// that protects healthy long-running turns.
    #[tokio::test]
    async fn stall_watchdog_skips_fresh_inflight_even_if_desynced() {
        let Some(tmux_guard) = start_test_tmux_session("watchdog-skip-fresh") else {
            return;
        };
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
        let channel_id: u64 = 700_111_222_333_555_001;
        harness.seed_watcher_for_tmux(channel_id, &tmux_guard.name);

        let output_path = temp.path().join("watchdog-skip-fresh-output.jsonl");
        std::fs::write(&output_path, vec![b'x'; 256]).expect("seed output capture");
        let mut inflight = super::super::inflight::InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("watchdog-skip-fresh".to_string()),
            42,
            8_011,
            8_012,
            String::new(),
            Some("session-watchdog-skip-fresh".to_string()),
            Some(tmux_guard.name.clone()),
            Some(output_path.to_string_lossy().into_owned()),
            None,
            0,
        );
        // FRESH updated_at — save_inflight_state will stamp `now()` and
        // we leave it as-is.
        inflight.started_at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        super::super::inflight::save_inflight_state(&inflight)
            .expect("write seeded fresh inflight JSON");

        let coord = harness.shared.tmux_relay_coord(ChannelId::new(channel_id));
        coord.last_relay_ts_ms.store(
            chrono::Utc::now().timestamp_millis() - WATCHER_STATE_DESYNC_STALE_MS - 5_000,
            std::sync::atomic::Ordering::Release,
        );

        let cleaned =
            super::run_stall_watchdog_pass(&harness.registry(), &ProviderKind::Codex).await;
        assert_eq!(
            cleaned, 0,
            "watchdog must NOT clean a fresh-updated_at channel even if desynced"
        );
        assert!(
            super::super::inflight::load_inflight_state(&ProviderKind::Codex, channel_id).is_some(),
            "fresh inflight must survive the watchdog"
        );
    }

    /// codex review round-3 P2 (#1672): when the in-memory mailbox is
    /// empty but the disk-backed `discord_pending_queue/<provider>/<token>/<channel>.json`
    /// is still present, `schedule_pending_queue_drain_after_cancel` must
    /// hydrate the mailbox from disk and schedule the deferred drain.
    /// Otherwise the cancel response truthfully reports
    /// `queue_disk_present_after=true` but the queued items remain
    /// stranded — they only get absorbed when the next user message
    /// arrives, and the next `mailbox_enqueue_intervention` may
    /// overwrite the disk file before that happens.
    #[tokio::test]
    async fn schedule_pending_queue_drain_hydrates_mailbox_when_only_disk_queue_exists() {
        use crate::services::discord::runtime_store::lock_test_env;
        use crate::services::turn_orchestrator::save_channel_queue;

        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().expect("create temp runtime root");
        let prev_override = crate::config::current_test_runtime_root_override();
        crate::config::set_test_runtime_root_override(Some(tmp.path().to_path_buf()));
        struct OverrideGuard(Option<std::path::PathBuf>);
        impl Drop for OverrideGuard {
            fn drop(&mut self) {
                crate::config::set_test_runtime_root_override(self.0.clone());
            }
        }
        let _guard = OverrideGuard(prev_override);

        let harness = TestHealthHarness::new_with_provider(ProviderKind::Codex).await;
        let channel_id_u64: u64 = 660_111_222_333_444_001;
        let channel_id = ChannelId::new(channel_id_u64);

        // Seed disk-only queue: write a pending queue file *without*
        // touching the in-memory mailbox so we recreate the production
        // failure mode where a previous restart left the file behind.
        let intervention = super::super::Intervention {
            author_id: UserId::new(123),
            message_id: MessageId::new(987_654_321),
            source_message_ids: vec![MessageId::new(987_654_321)],
            text: "stranded-on-disk".to_string(),
            mode: super::super::InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
        };
        save_channel_queue(
            &ProviderKind::Codex,
            &harness.shared.token_hash,
            channel_id,
            &[intervention],
            None,
        );

        // Pre-condition: mailbox is empty, disk file is present.
        let pre_snapshot = snapshot_pending_queue_state(&harness.registry(), "codex", channel_id)
            .await
            .expect("snapshot must resolve registered runtime");
        assert_eq!(
            pre_snapshot.queue_depth, 0,
            "in-memory mailbox must start empty"
        );
        assert!(
            pre_snapshot.disk_present,
            "disk-backed queue file must be seeded for the test"
        );

        // Drive the helper. With the round-3 P2 fix it must hydrate the
        // mailbox from disk and schedule the deferred drain
        // (`scheduled=true`, `queue_depth_after>0`); without the fix
        // it short-circuits on the empty mailbox, leaving the disk
        // queue stranded.
        let outcome = schedule_pending_queue_drain_after_cancel(
            &harness.registry(),
            "codex",
            channel_id,
            "test-disk-only-hydrate",
        )
        .await;
        assert!(
            outcome.scheduled,
            "drain must be scheduled when only the disk-backed queue is non-empty"
        );
        assert_eq!(
            outcome.queue_depth_after, 1,
            "post-hydrate depth must reflect the disk-backed item"
        );

        // Post-condition: mailbox is now hydrated from disk so the
        // deferred drain (and any subsequent `mailbox_enqueue_intervention`)
        // sees the surviving items.
        let post_depth = harness.queue_depth_for_channel(channel_id_u64).await;
        assert_eq!(
            post_depth, 1,
            "mailbox must be hydrated from disk after the drain helper runs"
        );

        // The deferred drain itself increments `deferred_hook_backlog`
        // synchronously before spawning the delayed kickoff task.
        assert!(
            harness.deferred_hook_backlog() >= 1,
            "post-cancel drain helper must register a deferred hook"
        );
    }

    /// codex review round-4 P2-1 (#1672): when a fresh user message
    /// races into the mailbox in between a cancel completing and the
    /// disk-backed pending queue being hydrated, the merge must
    /// preserve *both* the disk payload (chronologically older) and
    /// the live racer (chronologically newer) — not clobber the
    /// in-memory entry with the disk snapshot. The previous
    /// implementation called `mailbox_replace_queue`, which performed
    /// a wholesale overwrite and silently dropped the racer.
    #[tokio::test]
    async fn schedule_pending_queue_drain_merges_concurrent_enqueue_with_disk_payload() {
        use crate::services::discord::runtime_store::lock_test_env;
        use crate::services::turn_orchestrator::save_channel_queue;

        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().expect("create temp runtime root");
        let prev_override = crate::config::current_test_runtime_root_override();
        crate::config::set_test_runtime_root_override(Some(tmp.path().to_path_buf()));
        struct OverrideGuard(Option<std::path::PathBuf>);
        impl Drop for OverrideGuard {
            fn drop(&mut self) {
                crate::config::set_test_runtime_root_override(self.0.clone());
            }
        }
        let _guard = OverrideGuard(prev_override);

        let harness = TestHealthHarness::new_with_provider(ProviderKind::Codex).await;
        let channel_id_u64: u64 = 660_111_222_333_444_002;
        let channel_id = ChannelId::new(channel_id_u64);

        // Seed disk with two surviving interventions (came in before the cancel).
        let disk_items = vec![
            super::super::Intervention {
                author_id: UserId::new(7),
                message_id: MessageId::new(1001),
                source_message_ids: vec![MessageId::new(1001)],
                text: "disk-1".to_string(),
                mode: super::super::InterventionMode::Soft,
                created_at: Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
            },
            super::super::Intervention {
                author_id: UserId::new(7),
                message_id: MessageId::new(1002),
                source_message_ids: vec![MessageId::new(1002)],
                text: "disk-2".to_string(),
                mode: super::super::InterventionMode::Soft,
                created_at: Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
            },
        ];
        save_channel_queue(
            &ProviderKind::Codex,
            &harness.shared.token_hash,
            channel_id,
            &disk_items,
            None,
        );

        // Simulate the racer: a brand-new user message that landed in
        // the in-memory mailbox after the cancel emptied it but before
        // the drain helper got to hydrate.
        harness
            .seed_queue(channel_id_u64, &[(2003, "concurrent-enqueue")])
            .await;
        assert_eq!(
            harness.queue_depth_for_channel(channel_id_u64).await,
            1,
            "racer message must already be in the in-memory mailbox"
        );

        // Drive the post-cancel drain. The fix must merge disk+memory.
        let outcome = schedule_pending_queue_drain_after_cancel(
            &harness.registry(),
            "codex",
            channel_id,
            "test-merge-with-racer",
        )
        .await;
        assert!(
            outcome.scheduled,
            "drain must be scheduled when either source has items"
        );
        assert_eq!(
            outcome.queue_depth_after, 3,
            "post-hydrate depth must include both disk items and the racer"
        );

        // Verify ordering: disk items prepend (chronologically older),
        // racer stays at the tail (newest).
        let snapshot = harness
            .shared
            .mailbox(channel_id)
            .snapshot()
            .await
            .intervention_queue;
        assert_eq!(snapshot.len(), 3);
        assert_eq!(snapshot[0].message_id, MessageId::new(1001));
        assert_eq!(snapshot[1].message_id, MessageId::new(1002));
        assert_eq!(snapshot[2].message_id, MessageId::new(2003));
    }

    /// codex review round-4 P2-1 (#1672): hydration must be idempotent
    /// — if the same disk file is processed twice (e.g. retry after a
    /// transient error) the duplicate `message_id`s are skipped, not
    /// inserted twice.
    #[tokio::test]
    async fn hydrate_pending_queue_is_idempotent_on_repeated_disk_load() {
        use crate::services::discord::runtime_store::lock_test_env;
        use crate::services::turn_orchestrator::save_channel_queue;

        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().expect("create temp runtime root");
        let prev_override = crate::config::current_test_runtime_root_override();
        crate::config::set_test_runtime_root_override(Some(tmp.path().to_path_buf()));
        struct OverrideGuard(Option<std::path::PathBuf>);
        impl Drop for OverrideGuard {
            fn drop(&mut self) {
                crate::config::set_test_runtime_root_override(self.0.clone());
            }
        }
        let _guard = OverrideGuard(prev_override);

        let harness = TestHealthHarness::new_with_provider(ProviderKind::Codex).await;
        let channel_id_u64: u64 = 660_111_222_333_444_003;
        let channel_id = ChannelId::new(channel_id_u64);

        let intervention = super::super::Intervention {
            author_id: UserId::new(8),
            message_id: MessageId::new(3001),
            source_message_ids: vec![MessageId::new(3001)],
            text: "only-once".to_string(),
            mode: super::super::InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
        };
        save_channel_queue(
            &ProviderKind::Codex,
            &harness.shared.token_hash,
            channel_id,
            &[intervention],
            None,
        );

        let first = schedule_pending_queue_drain_after_cancel(
            &harness.registry(),
            "codex",
            channel_id,
            "test-idempotent-1",
        )
        .await;
        assert_eq!(first.queue_depth_after, 1);

        // Second invocation: the disk file is still present (the hydrate
        // helper only writes through the mailbox actor which re-persists
        // the same payload), but the in-memory entry already has
        // `message_id=3001` so the merge must be a no-op on count.
        let second = schedule_pending_queue_drain_after_cancel(
            &harness.registry(),
            "codex",
            channel_id,
            "test-idempotent-2",
        )
        .await;
        assert_eq!(
            second.queue_depth_after, 1,
            "duplicate disk entry must not double-count"
        );
    }

    /// #1671 — orphan ExplicitBackgroundWork safety net. The bridge left
    /// behind an inflight whose `task_notification_kind` is set so the relay
    /// classifier reports `ExplicitBackgroundWork`, the watcher view looks
    /// healthy (`desynced=false`), and `unread_bytes=0` because the relay
    /// caught up to the capture file. The classic desynced-only watchdog
    /// path missed this case (issue #1670) — the new safety net must
    /// recover it once the inflight has aged past the threshold.
    #[tokio::test]
    async fn stall_watchdog_force_cleans_orphan_explicit_background_work() {
        let Some(tmux_guard) = start_test_tmux_session("watchdog-orphan-bg") else {
            return;
        };
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
        let channel_id: u64 = 700_111_222_333_777_001;
        harness.seed_watcher_for_tmux(channel_id, &tmux_guard.name);

        // Capture file with N bytes; relay-coord will be advanced to the
        // SAME offset so unread_bytes == 0 (relay caught up).
        let output_path = temp.path().join("watchdog-orphan-bg-output.jsonl");
        let capture_bytes: u64 = 256;
        std::fs::write(&output_path, vec![b'x'; capture_bytes as usize])
            .expect("seed output capture");

        let mut inflight = super::super::inflight::InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("watchdog-orphan-bg".to_string()),
            42,
            9_001,
            9_002,
            String::new(),
            Some("session-watchdog-orphan-bg".to_string()),
            Some(tmux_guard.name.clone()),
            Some(output_path.to_string_lossy().into_owned()),
            None,
            0,
        );
        // Mark this as ExplicitBackground via `task_notification_kind` so
        // `relay_active_turn_from_inflight` returns ExplicitBackground and
        // the classifier emits `RelayStallState::ExplicitBackgroundWork`.
        inflight.task_notification_kind =
            Some(crate::services::agent_protocol::TaskNotificationKind::Background);
        // Advance offset so unread_bytes == 0 against the seeded capture.
        inflight.last_offset = capture_bytes;
        // Backdate `updated_at` past the orphan threshold (10 min).
        let stale_unix =
            chrono::Utc::now().timestamp() - (super::STALL_WATCHDOG_THRESHOLD_SECS as i64) - 60;
        let stale_local = chrono::Local
            .timestamp_opt(stale_unix, 0)
            .single()
            .expect("valid local time")
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        inflight.started_at = stale_local.clone();
        inflight.updated_at = stale_local.clone();
        super::super::inflight::save_inflight_state(&inflight)
            .expect("write seeded stale inflight JSON");
        // Re-stamp updated_at on disk (save_inflight_state rewrites it to
        // now()) — write the JSON directly to bypass the auto-stamp.
        let json = serde_json::to_string_pretty(&inflight).expect("serialize stale inflight");
        let inflight_path = super::super::inflight::inflight_runtime_root()
            .expect("inflight root override")
            .join("codex")
            .join(format!("{channel_id}.json"));
        std::fs::write(&inflight_path, json).expect("rewrite stale inflight on disk");

        // Match the relay-coord offset to capture so unread_bytes == 0.
        // Backdate last_relay_ts_ms beyond the desync stale window AND
        // beyond the 10-minute outbound threshold so the safety net's
        // `last_outbound_activity_ms` gate also fires.
        let coord = harness.shared.tmux_relay_coord(ChannelId::new(channel_id));
        coord
            .confirmed_end_offset
            .store(capture_bytes, std::sync::atomic::Ordering::Release);
        let stale_outbound_ms = chrono::Utc::now().timestamp_millis()
            - ((super::STALL_WATCHDOG_THRESHOLD_SECS as i64) * 1000)
            - 60_000;
        coord
            .last_relay_ts_ms
            .store(stale_outbound_ms, std::sync::atomic::Ordering::Release);

        // Sanity: snapshot must classify this as ExplicitBackgroundWork +
        // unread_bytes == 0 + desynced == false before the run.
        let pre_snapshot = harness
            .registry()
            .snapshot_watcher_state(channel_id)
            .await
            .expect("seeded inflight should surface a snapshot");
        assert!(pre_snapshot.attached, "watcher binding implies attached");
        assert_eq!(pre_snapshot.unread_bytes, Some(0));
        assert_eq!(
            pre_snapshot.relay_stall_state,
            super::super::relay_health::RelayStallState::ExplicitBackgroundWork
        );

        // codex P1 — `reregister_active_turn_from_inflight` re-creates a
        // mailbox cancel token after a dcserver restart WITHOUT touching
        // `global_active` (the previous parent's counter died with that
        // process). Mirror that here by leaving `global_active = 0` and
        // proving the orphan-recovery decrement does not wrap to
        // `usize::MAX`.
        let global_active_before = harness
            .shared
            .global_active
            .load(std::sync::atomic::Ordering::Acquire);
        assert_eq!(
            global_active_before, 0,
            "test setup: counter must start at 0 to exercise the wrap-protection branch",
        );

        // Run the watchdog pass — orphan ExplicitBackgroundWork must be
        // recovered.
        let cleaned =
            super::run_stall_watchdog_pass(&harness.registry(), &ProviderKind::Codex).await;
        assert_eq!(
            cleaned, 1,
            "watchdog must clean exactly 1 orphan ExplicitBackgroundWork channel"
        );
        assert!(
            super::super::inflight::load_inflight_state(&ProviderKind::Codex, channel_id).is_none(),
            "watchdog must delete the stale orphan inflight state file"
        );
        // codex P1 — saturating-decrement invariant: counter must remain
        // 0 (never wrap to `usize::MAX`). A regression that re-introduces
        // raw `fetch_sub(1)` here would surface as a wrapped value, which
        // would convince health and deferred-restart logic that a phantom
        // active turn lives forever.
        let global_active_after = harness
            .shared
            .global_active
            .load(std::sync::atomic::Ordering::Acquire);
        assert_eq!(
            global_active_after, 0,
            "global_active must not wrap when the orphan-recovery branch decrements an already-zero counter",
        );
    }

    /// #1671 — false-positive guard for the orphan ExplicitBackgroundWork
    /// path: a real long-running background turn whose
    /// `last_outbound_activity_ms` is fresh (the watcher is still streaming
    /// tool output) MUST NOT be flagged even if `task_notification_kind`
    /// has been set for hours.
    #[tokio::test]
    async fn stall_watchdog_skips_active_explicit_background_with_fresh_outbound() {
        let Some(tmux_guard) = start_test_tmux_session("watchdog-active-bg") else {
            return;
        };
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
        let channel_id: u64 = 700_111_222_333_777_002;
        harness.seed_watcher_for_tmux(channel_id, &tmux_guard.name);

        let output_path = temp.path().join("watchdog-active-bg-output.jsonl");
        let capture_bytes: u64 = 256;
        std::fs::write(&output_path, vec![b'x'; capture_bytes as usize])
            .expect("seed output capture");

        let mut inflight = super::super::inflight::InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("watchdog-active-bg".to_string()),
            42,
            9_011,
            9_012,
            String::new(),
            Some("session-watchdog-active-bg".to_string()),
            Some(tmux_guard.name.clone()),
            Some(output_path.to_string_lossy().into_owned()),
            None,
            0,
        );
        inflight.task_notification_kind =
            Some(crate::services::agent_protocol::TaskNotificationKind::Background);
        inflight.last_offset = capture_bytes;
        // Stale inflight updated_at — but we'll keep last_relay_ts_ms FRESH
        // to mimic an actively-streaming background turn.
        let stale_unix =
            chrono::Utc::now().timestamp() - (super::STALL_WATCHDOG_THRESHOLD_SECS as i64) - 60;
        let stale_local = chrono::Local
            .timestamp_opt(stale_unix, 0)
            .single()
            .expect("valid local time")
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        inflight.started_at = stale_local.clone();
        inflight.updated_at = stale_local.clone();
        super::super::inflight::save_inflight_state(&inflight)
            .expect("write seeded stale inflight JSON");
        let json = serde_json::to_string_pretty(&inflight).expect("serialize stale inflight");
        let inflight_path = super::super::inflight::inflight_runtime_root()
            .expect("inflight root override")
            .join("codex")
            .join(format!("{channel_id}.json"));
        std::fs::write(&inflight_path, json).expect("rewrite stale inflight on disk");

        let coord = harness.shared.tmux_relay_coord(ChannelId::new(channel_id));
        coord
            .confirmed_end_offset
            .store(capture_bytes, std::sync::atomic::Ordering::Release);
        // FRESH last_relay_ts_ms — within the outbound threshold.
        coord.last_relay_ts_ms.store(
            chrono::Utc::now().timestamp_millis() - 30_000,
            std::sync::atomic::Ordering::Release,
        );

        let cleaned =
            super::run_stall_watchdog_pass(&harness.registry(), &ProviderKind::Codex).await;
        assert_eq!(
            cleaned, 0,
            "watchdog must NOT clean an actively-streaming ExplicitBackgroundWork channel"
        );
        assert!(
            super::super::inflight::load_inflight_state(&ProviderKind::Codex, channel_id).is_some(),
            "active background inflight must survive the watchdog"
        );
    }

    /// #1671 codex re-review P2 — the orphan ExplicitBackgroundWork
    /// cleanup branch MUST preserve the channel's queued user
    /// interventions. The legacy implementation routed cleanup through
    /// `mailbox_clear_channel`, which drained `intervention_queue` as
    /// `Superseded`; any user message queued behind the stalled turn was
    /// silently dropped. The fix swaps to `mailbox_finish_turn`, which
    /// only releases the active-turn anchor + cancel token while leaving
    /// the queue intact, then schedules a deferred idle-queue kickoff so
    /// the survived items drain without waiting for a fresh user message.
    /// This test seeds the same orphan-recovery scenario as the parent
    /// test plus a non-empty mailbox queue, runs the watchdog, and
    /// asserts (1) the queue items survive in the mailbox snapshot and
    /// (2) the inflight cleanup still happens.
    #[tokio::test]
    async fn stall_watchdog_orphan_explicit_background_preserves_pending_queue() {
        let Some(tmux_guard) = start_test_tmux_session("watchdog-orphan-bg-queue") else {
            return;
        };
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
        let channel_id: u64 = 700_111_222_333_777_777;
        harness.seed_watcher_for_tmux(channel_id, &tmux_guard.name);

        // Seed two queued interventions BEFORE the watchdog runs. These
        // must survive the orphan-recovery cleanup; if the cleanup still
        // routes through `mailbox_clear_channel` they will be drained.
        harness
            .set_queue_depth_for_channel(channel_id, ProviderKind::Codex, 2)
            .await;
        let pre_queue_depth = harness.queue_depth_for_channel(channel_id).await;
        assert_eq!(
            pre_queue_depth, 2,
            "test setup must successfully seed 2 queued interventions"
        );

        let output_path = temp.path().join("watchdog-orphan-bg-queue-output.jsonl");
        let capture_bytes: u64 = 256;
        std::fs::write(&output_path, vec![b'x'; capture_bytes as usize])
            .expect("seed output capture");

        let mut inflight = super::super::inflight::InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("watchdog-orphan-bg-queue".to_string()),
            42,
            9_777,
            9_778,
            String::new(),
            Some("session-watchdog-orphan-bg-queue".to_string()),
            Some(tmux_guard.name.clone()),
            Some(output_path.to_string_lossy().into_owned()),
            None,
            0,
        );
        inflight.task_notification_kind =
            Some(crate::services::agent_protocol::TaskNotificationKind::Background);
        inflight.last_offset = capture_bytes;
        let stale_unix =
            chrono::Utc::now().timestamp() - (super::STALL_WATCHDOG_THRESHOLD_SECS as i64) - 60;
        let stale_local = chrono::Local
            .timestamp_opt(stale_unix, 0)
            .single()
            .expect("valid local time")
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        inflight.started_at = stale_local.clone();
        inflight.updated_at = stale_local.clone();
        super::super::inflight::save_inflight_state(&inflight)
            .expect("write seeded stale inflight JSON");
        let json = serde_json::to_string_pretty(&inflight).expect("serialize stale inflight");
        let inflight_path = super::super::inflight::inflight_runtime_root()
            .expect("inflight root override")
            .join("codex")
            .join(format!("{channel_id}.json"));
        std::fs::write(&inflight_path, json).expect("rewrite stale inflight on disk");

        let coord = harness.shared.tmux_relay_coord(ChannelId::new(channel_id));
        coord
            .confirmed_end_offset
            .store(capture_bytes, std::sync::atomic::Ordering::Release);
        let stale_outbound_ms = chrono::Utc::now().timestamp_millis()
            - ((super::STALL_WATCHDOG_THRESHOLD_SECS as i64) * 1000)
            - 60_000;
        coord
            .last_relay_ts_ms
            .store(stale_outbound_ms, std::sync::atomic::Ordering::Release);

        let cleaned =
            super::run_stall_watchdog_pass(&harness.registry(), &ProviderKind::Codex).await;
        assert_eq!(
            cleaned, 1,
            "watchdog must clean exactly 1 orphan ExplicitBackgroundWork channel"
        );
        assert!(
            super::super::inflight::load_inflight_state(&ProviderKind::Codex, channel_id).is_none(),
            "watchdog must delete the stale orphan inflight state file"
        );
        // CORE invariant for codex P2 — queued interventions survive the
        // watchdog cleanup. A regression that re-routes through
        // `mailbox_clear_channel` would surface here as queue_depth == 0.
        let post_queue_depth = harness.queue_depth_for_channel(channel_id).await;
        assert_eq!(
            post_queue_depth, 2,
            "queued interventions must survive orphan ExplicitBackgroundWork cleanup",
        );
    }
}
