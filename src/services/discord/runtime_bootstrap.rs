use super::*;

mod framework_setup;
mod orphan_recovery;
mod queued_placeholders;
mod recovery_flush;
mod restored_state;
mod session_gc;
mod spawns;
mod startup_doctor;
mod voice;

use self::framework_setup::{run_bot_build_slash_commands, run_bot_framework_setup};
#[allow(unused_imports)]
pub(in crate::services::discord) use self::queued_placeholders::{
    FilteredQueuedPlaceholders, StalePlaceholderDeleter, collect_live_queue_message_ids,
    delete_stale_queued_placeholder_cards, delete_stale_queued_placeholder_cards_with,
    filter_restored_queued_placeholders,
};
#[cfg(test)]
use self::voice::voice_auto_join_provider_map;
use self::voice::{run_bot_init_voice_workers, run_bot_rehydrate_voice_handoffs};
#[allow(unused_imports)]
use self::{orphan_recovery::*, restored_state::*, session_gc::*, startup_doctor::*};

pub(crate) struct RunBotContext {
    pub(crate) global_active: Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) global_finalizing: Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) shutdown_remaining: Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) startup_reconcile_remaining: Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) startup_doctor_started: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) health_registry: Arc<health::HealthRegistry>,
    pub(crate) api_port: u16,
    pub(crate) pg_pool: Option<sqlx::PgPool>,
    pub(crate) engine: Option<crate::engine::PolicyEngine>,
    pub(crate) placeholder_live_events_enabled: bool,
    pub(crate) status_panel_v2_enabled: bool,
}

const DISCORD_GATEWAY_LEASE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
const DISCORD_GATEWAY_LOCK_PREFIX: u64 = 0x0443_0000_0000_0000;
fn discord_gateway_lock_id(token_hash: &str) -> i64 {
    // `discord_token_hash()` returns "discord_<16hex>". Strip the literal prefix
    // so the first 16 chars we sample are actual hex; otherwise the `is_ascii_hexdigit`
    // check fails on non-hex letters in the prefix and every bot collapses onto the
    // same fallback lock id, causing only one bot to acquire the singleton lease.
    let raw = token_hash.strip_prefix("discord_").unwrap_or(token_hash);
    let hex = raw
        .get(..16)
        .filter(|prefix| prefix.chars().all(|ch| ch.is_ascii_hexdigit()))
        .unwrap_or("0");
    let parsed = u64::from_str_radix(hex, 16).unwrap_or(0);
    let suffix = parsed & 0x0000_FFFF_FFFF_FFFF;
    (DISCORD_GATEWAY_LOCK_PREFIX | suffix) as i64
}

async fn try_acquire_discord_gateway_lease(
    pool: &sqlx::PgPool,
    token_hash: &str,
    provider: &ProviderKind,
) -> Result<Option<crate::db::postgres::AdvisoryLockLease>, String> {
    crate::db::postgres::AdvisoryLockLease::try_acquire(
        pool,
        discord_gateway_lock_id(token_hash),
        format!("discord gateway {}", provider.as_str()),
    )
    .await
}

pub(super) fn discord_gateway_intents() -> serenity::GatewayIntents {
    serenity::GatewayIntents::GUILDS
        | serenity::GatewayIntents::GUILD_MESSAGES
        | serenity::GatewayIntents::GUILD_MESSAGE_REACTIONS
        | serenity::GatewayIntents::GUILD_VOICE_STATES
        | serenity::GatewayIntents::DIRECT_MESSAGES
        | serenity::GatewayIntents::DIRECT_MESSAGE_REACTIONS
        | serenity::GatewayIntents::MESSAGE_CONTENT
}

/// Entry point: start the Discord bot
pub(crate) async fn run_bot(token: &str, provider: ProviderKind, context: RunBotContext) {
    let RunBotContext {
        global_active,
        global_finalizing,
        shutdown_remaining,
        startup_reconcile_remaining,
        startup_doctor_started,
        health_registry,
        api_port,
        pg_pool,
        engine,
        placeholder_live_events_enabled,
        status_panel_v2_enabled,
    } = context;

    if let Some(bot_name) = should_skip_agent_runtime_launch(token) {
        run_startup_diagnostic_after_reconcile_barrier(
            startup_reconcile_remaining,
            startup_doctor_started,
            health_registry,
            api_port,
        )
        .await;
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ⏭ BOT-LAUNCH: skipping utility bot '{}' in run_bot() — not mapped to any agent channel",
            bot_name
        );
        shutdown_remaining.fetch_sub(1, Ordering::AcqRel);
        return;
    }

    let token_hash = settings::discord_token_hash(token);

    // Phase 5.1 of intake-node-routing (issue #2007): build SharedData and
    // spawn the intake_worker poll loop BEFORE the gateway lease check.
    // Standby nodes (lease held elsewhere) still need a live worker to
    // claim `intake_outbox` rows targeted at this `instance_id` — that is
    // the entire point of routing intake to a worker node. Previously the
    // worker spawn lived inside the poise setup callback, which only
    // executes on the lease-holding leader, so standby workers never
    // started.
    super::internal_api::init(api_port, pg_pool.clone());

    // Initialize debug logging from environment variable
    claude::init_debug_from_env();

    let mut bot_settings = load_bot_settings(token);
    bot_settings.provider = provider.clone();

    match bot_settings.owner_user_id {
        Some(owner_id) => tracing::info!("  ✓ Owner: {owner_id}"),
        None => tracing::info!(
            "  ⚠ No owner registered — configure discord.owner_id (or allow_all_users) before use"
        ),
    }

    let initial_skills = scan_skills(&provider, None);
    let skill_count = initial_skills.len();
    tracing::info!(
        "  ✓ {} bot ready — Skills loaded: {}",
        provider.display_name(),
        skill_count
    );

    let voice_config = crate::config::load_graceful().voice;
    let voice_barge_in = Arc::new(voice_barge_in::VoiceBargeInRuntime::from_voice_config(
        &voice_config,
    ));

    run_bot_rehydrate_voice_handoffs(&pg_pool).await;

    // Cleanup stale Discord uploads on process start
    cleanup_old_uploads(UPLOAD_MAX_AGE);

    let provider_for_shutdown = provider.clone();
    let provider_for_error = provider.clone();
    let provider_for_framework = provider.clone();
    let startup_reconcile_remaining_for_client_start = startup_reconcile_remaining.clone();
    let startup_doctor_started_for_client_start = startup_doctor_started.clone();
    let health_registry_for_client_start = health_registry.clone();

    let restored_model_overrides: Vec<(ChannelId, String)> = bot_settings
        .channel_model_overrides
        .iter()
        .filter_map(|(channel_id, model)| {
            channel_id
                .parse::<u64>()
                .ok()
                .map(|id| (ChannelId::new(id), model.clone()))
        })
        .collect();
    let restored_fast_mode_channels =
        restored_fast_mode_enabled_channels_for_provider(&bot_settings, &provider);
    let restored_fast_mode_reset_entries = restored_fast_mode_reset_entries(&bot_settings);
    let restored_fast_mode_reset_channels = restored_fast_mode_reset_channels(&bot_settings);
    let restored_codex_goals_channels = restored_codex_goals_enabled_channels(&bot_settings);
    let restored_codex_goals_reset_channels = restored_codex_goals_reset_channels(&bot_settings);

    let shared = run_bot_build_shared_data(
        bot_settings,
        initial_skills,
        &provider,
        &token_hash,
        &voice_barge_in,
        global_active,
        global_finalizing,
        &shutdown_remaining,
        &health_registry,
        pg_pool,
        engine,
        api_port,
        placeholder_live_events_enabled,
        status_panel_v2_enabled,
        &restored_model_overrides,
        &restored_fast_mode_channels,
        &restored_fast_mode_reset_entries,
        &restored_fast_mode_reset_channels,
        &restored_codex_goals_channels,
        &restored_codex_goals_reset_channels,
    );
    super::tui_prompt_relay::spawn_tui_prompt_relay(shared.clone(), provider.clone());

    // Phase 5.2 of intake-node-routing (issue #2009): populate
    // `cached_bot_token` BEFORE the gateway lease check so the
    // standby-side response path (`turn_bridge` tmux watcher,
    // placeholder edits) can build a REST `Arc<Http>` via
    // `shared.serenity_http_or_token_fallback()` even when
    // `cached_serenity_ctx` stays empty (no gateway runtime).
    //
    // On the leader the OnceCell is also set later inside the poise
    // setup callback — that second `set` is a no-op (`OnceCell::set`
    // returns Err on already-set), preserving the leader's existing
    // semantics.
    let _ = shared.cached_bot_token.set(token.to_string());

    let voice_receiver =
        run_bot_init_voice_workers(&voice_config, &voice_barge_in, &shared, &provider);

    // Phase 5.1 of intake-node-routing (issue #2007): only spawn the
    // intake_worker poll loop when routing is explicitly enforced. In the
    // default disabled/observe modes there are no owned rows to drain, and
    // starting one poller per configured Discord agent can exhaust the shared
    // Postgres pool before the HTTP server finishes booting.
    //
    // The worker uses `serenity::http::Http::new(token)` (REST-only,
    // no IDENTIFY) so it never contends for the gateway lease. It
    // only touches `shared.{core, settings, pg_pool, dispatch_thread_parents}`,
    // none of which depend on a live gateway shard.
    //
    // Cancellation rides on `shared.shutting_down`. On the leader, the
    // gateway-lease loss handler and SIGTERM handler flip that flag.
    // On standby today no signal handler is wired; the worker exits
    // when launchd kills the process during deploy. A follow-up could
    // add SIGTERM handling on the standby path for graceful drain.
    run_bot_maybe_spawn_intake_worker(&shared, token, &provider);

    // After optional worker setup, do the gateway lease check. Standby nodes
    // (lease held elsewhere) early-return below; when intake routing is
    // enforced, the detached worker task keeps polling using `Arc<SharedData>`.
    let gateway_lease = match run_bot_acquire_gateway_lease(
        &shared,
        &token_hash,
        &provider,
        &startup_reconcile_remaining,
        &startup_doctor_started,
        &health_registry,
        api_port,
    )
    .await
    {
        GatewayLeaseOutcome::Proceed(lease) => lease,
        GatewayLeaseOutcome::Skip => {
            // Standby / lease-held-elsewhere / acquire-error: the diagnostic
            // already ran inside the helper. Decrement the shutdown barrier
            // and abort startup exactly as the original early-returns did.
            shutdown_remaining.fetch_sub(1, Ordering::AcqRel);
            return;
        }
    };

    {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🔑 dcserver generation: {}",
            shared.current_generation
        );
        if !restored_model_overrides.is_empty() {
            tracing::info!(
                "  [{ts}] 🧩 restored model overrides: {} channel(s)",
                restored_model_overrides.len()
            );
        }
        if !restored_fast_mode_channels.is_empty() {
            tracing::info!(
                "  [{ts}] ⚡ restored fast mode channels: {} channel(s)",
                restored_fast_mode_channels.len()
            );
        }
    }

    // Register this provider with the health check registry
    health_registry
        .register(provider.as_str().to_string(), shared.clone())
        .await;

    let token_owned = token.to_string();
    let shared_clone = shared.clone();
    let voice_config_for_setup = voice_config.clone();
    let voice_receiver_for_setup = voice_receiver.clone();

    let slash_commands = run_bot_build_slash_commands();

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: slash_commands,
            command_check: Some(|ctx| {
                Box::pin(async move {
                    let settings_snapshot = { ctx.data().shared.settings.read().await.clone() };
                    let allowed = provider_handles_channel(
                        ctx.serenity_context(),
                        &ctx.data().provider,
                        &settings_snapshot,
                        ctx.channel_id(),
                    )
                    .await;
                    if !allowed {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] ⏭ CMD-GUARD: skipping /{} in channel {} for provider {}",
                            ctx.command().name,
                            ctx.channel_id(),
                            ctx.data().provider.as_str()
                        );
                    }
                    Ok(allowed)
                })
            }),
            event_handler: |ctx, event, _framework, data| Box::pin(handle_event(ctx, event, data)),
            ..Default::default()
        })
        .setup(move |ctx, _ready, framework| {
            let shared_for_migrate = shared_clone.clone();
            let health_registry_for_setup = health_registry.clone();
            let provider_for_setup = provider_for_framework.clone();
            let token_for_ready = token_owned.clone();
            let voice_config_for_setup = voice_config_for_setup.clone();
            let voice_receiver_for_setup = voice_receiver_for_setup.clone();
            Box::pin(run_bot_framework_setup(
                ctx,
                _ready,
                framework,
                shared_for_migrate,
                shared_clone,
                health_registry_for_setup,
                provider_for_setup,
                token_for_ready,
                token_owned,
                voice_config_for_setup,
                voice_receiver_for_setup,
                startup_reconcile_remaining,
                startup_doctor_started,
                api_port,
            ))
        })
        .build();

    let intents = discord_gateway_intents();

    let client = commands::register_songbird(serenity::ClientBuilder::new(token, intents))
        .framework(framework)
        .await
        .expect("Failed to create Discord client");

    let gateway_lease_task = gateway_lease.map(|lease| {
        run_bot_spawn_gateway_lease_keepalive(
            lease,
            &shared,
            &provider,
            client.shard_manager.clone(),
        )
    });

    // Graceful shutdown: on SIGTERM, persist queue/inflight/last_message state
    // and quick-exit. tmux/TUI processes survive — the next dcserver instance
    // rehydrates the channel bindings (see rehydrate_existing_claude_tui_bindings;
    // polled every CLAUDE_IDLE_REHYDRATE_POLL_INTERVAL ≈ 5s) and resumes transcript
    // tailing from the persisted last_offset.
    run_bot_spawn_sigterm_handler(&shared, provider_for_shutdown);

    run_bot_run_gateway_backend(
        client,
        &provider_for_error,
        gateway_lease_task,
        startup_reconcile_remaining_for_client_start,
        startup_doctor_started_for_client_start,
        health_registry_for_client_start,
        api_port,
    )
    .await;
}

// ── run_bot startup-phase helpers (decomposition of the run_bot
// god-function, issue #3038). These are behavior-preserving extractions:
// each helper runs the exact statements it replaced, in the same order,
// and run_bot calls them in the same order with the same threaded state.
// INITIALIZATION/SPAWN ORDER IS LOAD-BEARING — do not reorder. ──

/// Outcome of the gateway singleton-lease acquisition phase.
enum GatewayLeaseOutcome {
    /// Either the lease was acquired (`Some`) or there is no PG pool (`None`,
    /// the standalone/no-DB path). Either way, startup proceeds.
    Proceed(Option<crate::db::postgres::AdvisoryLockLease>),
    /// Lease is held elsewhere, or acquisition failed. The startup diagnostic
    /// has already run; run_bot must decrement the shutdown barrier and return.
    Skip,
}

/// Build all owned `SharedData` fields and wrap in an `Arc`. Side-effecting
/// initializers (`TurnFinalizer::spawn`, `StatusPanelController::spawn`,
/// `runtime_store::load_generation`, `load_queue_exit_placeholder_clears`,
/// the `inflight_signals` broadcast channel) run here in the exact same order
/// as the original inline struct literal. `bot_settings`, `initial_skills`,
/// `global_active`, `global_finalizing`, `pg_pool`, and `engine` are consumed
/// by move; the `restored_*` slices are borrowed (they are reused later in
/// run_bot for logging and session-reset bootstrap).
#[allow(clippy::too_many_arguments)]
fn run_bot_build_shared_data(
    bot_settings: DiscordBotSettings,
    initial_skills: Vec<(String, String)>,
    provider: &ProviderKind,
    token_hash: &str,
    voice_barge_in: &Arc<voice_barge_in::VoiceBargeInRuntime>,
    global_active: Arc<std::sync::atomic::AtomicUsize>,
    global_finalizing: Arc<std::sync::atomic::AtomicUsize>,
    shutdown_remaining: &Arc<std::sync::atomic::AtomicUsize>,
    health_registry: &Arc<health::HealthRegistry>,
    pg_pool: Option<sqlx::PgPool>,
    engine: Option<crate::engine::PolicyEngine>,
    api_port: u16,
    placeholder_live_events_enabled: bool,
    status_panel_v2_enabled: bool,
    restored_model_overrides: &[(ChannelId, String)],
    restored_fast_mode_channels: &[ChannelId],
    restored_fast_mode_reset_entries: &[String],
    restored_fast_mode_reset_channels: &[ChannelId],
    restored_codex_goals_channels: &[ChannelId],
    restored_codex_goals_reset_channels: &[ChannelId],
) -> Arc<SharedData> {
    Arc::new(SharedData {
        core: Mutex::new(CoreState {
            sessions: HashMap::new(),
            active_meetings: HashMap::new(),
        }),
        mailboxes: ChannelMailboxRegistry::default(),
        settings: tokio::sync::RwLock::new(bot_settings),
        api_timestamps: dashmap::DashMap::new(),
        skills_cache: tokio::sync::RwLock::new(initial_skills),
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
        placeholder_live_events_enabled,
        status_panel_v2_enabled,
        // #3038 S1: wrapped verbatim at the first-member position (evaluation-order preserved).
        queued: QueuedPlaceholderState {
            queued_placeholders: dashmap::DashMap::new(),
            queue_exit_placeholder_clears: {
                let map = dashmap::DashMap::new();
                for (key, placeholder_msg_id) in
                    super::queued_placeholders_store::load_queue_exit_placeholder_clears(
                        provider, token_hash,
                    )
                {
                    map.insert(key, placeholder_msg_id);
                }
                map
            },
            queued_placeholders_persist_locks: dashmap::DashMap::new(),
        },
        answer_flush_barrier: std::sync::Arc::new(
            super::answer_flush_barrier::AnswerFlushBarrier::default(),
        ),
        recovering_channels: dashmap::DashMap::new(),
        shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        finalizing_turns: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        current_generation: runtime_store::load_generation(),
        restart_pending: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        reconcile_done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        deferred_hook_backlog: std::sync::atomic::AtomicUsize::new(0),
        recovery_started_at: std::time::Instant::now(),
        recovery_duration_ms: std::sync::atomic::AtomicU64::new(0),
        global_active,
        turn_finalizer: super::turn_finalizer::TurnFinalizer::spawn(),
        status_panel_controller: super::status_panel_controller::StatusPanelController::spawn(
            status_panel_v2_enabled,
        ),
        global_finalizing,
        shutdown_remaining: shutdown_remaining.clone(),
        shutdown_counted: std::sync::atomic::AtomicBool::new(false),
        intake_dedup: dashmap::DashMap::new(),
        dispatch_thread_parents: dashmap::DashMap::new(),
        bot_connected: std::sync::atomic::AtomicBool::new(false),
        last_turn_at: std::sync::Mutex::new(None),
        model_overrides: {
            let map = dashmap::DashMap::new();
            for (channel_id, model) in restored_model_overrides {
                map.insert(*channel_id, model.clone());
            }
            map
        },
        fast_mode_channels: {
            let set = dashmap::DashSet::new();
            for channel_id in restored_fast_mode_channels {
                set.insert(*channel_id);
            }
            set
        },
        fast_mode_session_reset_pending: {
            let set = dashmap::DashSet::new();
            for entry in restored_fast_mode_reset_entries {
                set.insert(entry.clone());
            }
            set
        },
        codex_goals_channels: {
            let set = dashmap::DashSet::new();
            for channel_id in restored_codex_goals_channels {
                set.insert(*channel_id);
            }
            set
        },
        codex_goals_session_reset_pending: {
            let set = dashmap::DashSet::new();
            for channel_id in restored_codex_goals_reset_channels {
                set.insert(*channel_id);
            }
            set
        },
        model_session_reset_pending: dashmap::DashSet::new(),
        session_reset_pending: bootstrap_session_reset_pending_channels(
            restored_model_overrides,
            restored_fast_mode_reset_channels,
            restored_codex_goals_reset_channels,
        ),
        model_picker_pending: dashmap::DashMap::new(),
        dispatch_role_overrides: dashmap::DashMap::new(),
        voice_barge_in: voice_barge_in.clone(),
        voice_pairings: Arc::new(voice_routing::VoiceChannelPairingStore::load_default()),
        last_message_ids: dashmap::DashMap::new(),
        catch_up_retry_pending: dashmap::DashMap::new(),
        turn_start_times: dashmap::DashMap::new(),
        channel_rosters: dashmap::DashMap::new(),
        cached_serenity_ctx: tokio::sync::OnceCell::new(),
        cached_bot_token: tokio::sync::OnceCell::new(),
        token_hash: token_hash.to_string(),
        provider: provider.clone(),
        api_port,
        pg_pool,
        engine,
        health_registry: Arc::downgrade(health_registry),
        known_slash_commands: tokio::sync::OnceCell::new(),
        // #2448: capacity 256 gives ~hundreds of in-flight turns headroom
        // before a slow listener triggers `RecvError::Lagged`. The standby
        // relay subscriber falls back to file polling on lag.
        inflight_signals: tokio::sync::broadcast::channel(256).0,
    })
}

/// Phase 5.1 of intake-node-routing (issue #2007): when intake routing is in
/// Enforce mode and a PG pool exists, spawn the REST-only intake_worker poll
/// loop (resolves `target_instance_id` inside the task to avoid racing
/// `cluster::bootstrap`). No-op in disabled/observe modes. Spawned after the
/// voice workers and before the gateway lease check — order preserved.
fn run_bot_maybe_spawn_intake_worker(
    shared: &Arc<SharedData>,
    token: &str,
    provider: &ProviderKind,
) {
    if matches!(
        crate::services::cluster::intake_router_hook::IntakeRoutingMode::from_env(),
        crate::services::cluster::intake_router_hook::IntakeRoutingMode::Enforce
    ) {
        if let Some(pool_for_intake_worker) = shared.pg_pool.clone() {
            let intake_worker_http = std::sync::Arc::new(serenity::http::Http::new(token));
            let intake_worker_shared = shared.clone();
            let intake_worker_token = token.to_string();
            let intake_worker_provider = provider.as_str().to_string();
            let intake_worker_cancel = shared.shutting_down.clone();
            // The intake_worker spawn runs concurrently with `cluster::bootstrap`
            // which is the writer of `SELF_INSTANCE_ID`. Resolving
            // `target_instance_id` eagerly here would race and pick up the
            // hostname+PID fallback (e.g. `itismyfieldui-Macmini-46662`)
            // instead of the configured cluster id (e.g. `mac-mini-release`).
            // The leader hook (`intake_router_hook::try_route_intake`) resolves
            // the same function later, by which time bootstrap has populated
            // the OnceLock — the two ids must match or every claim misses.
            // Bridge the race by awaiting the OnceLock inside the spawned task
            // before the worker logs "poll loop started".
            tokio::spawn(async move {
                let resolved_target_id =
                    crate::services::cluster::node_registry::wait_for_self_instance_id(
                        std::time::Duration::from_secs(30),
                    )
                    .await;
                // claim_owner appends provider so multi-bot deployments
                // surface which token's worker holds a row in
                // observability dashboards.
                let resolved_claim_owner =
                    format!("{}:{}", resolved_target_id, intake_worker_provider);
                crate::services::cluster::intake_worker::run_intake_worker_loop(
                    pool_for_intake_worker,
                    intake_worker_http,
                    intake_worker_shared,
                    intake_worker_token,
                    resolved_target_id,
                    intake_worker_provider,
                    resolved_claim_owner,
                    crate::services::cluster::intake_worker::IntakeWorkerConfig::default(),
                    intake_worker_cancel,
                )
                .await;
            });
        } else {
            tracing::info!(
                "[intake_worker] postgres pool unavailable — intake-node-routing worker not started"
            );
        }
    }
}

/// Acquire the Discord gateway singleton lease (advisory lock) when a PG pool
/// is present. Returns `Proceed(Some(lease))` on success, `Proceed(None)` when
/// there is no PG pool (standalone path), or `Skip` when the lease is held
/// elsewhere / acquisition failed. On the `Skip` paths this runs the
/// post-reconcile startup diagnostic exactly as the original early-returns did,
/// before returning; run_bot then decrements the shutdown barrier and returns.
#[allow(clippy::too_many_arguments)]
async fn run_bot_acquire_gateway_lease(
    shared: &Arc<SharedData>,
    token_hash: &str,
    provider: &ProviderKind,
    startup_reconcile_remaining: &Arc<std::sync::atomic::AtomicUsize>,
    startup_doctor_started: &Arc<std::sync::atomic::AtomicBool>,
    health_registry: &Arc<health::HealthRegistry>,
    api_port: u16,
) -> GatewayLeaseOutcome {
    match shared.pg_pool.as_ref() {
        Some(pool) => match try_acquire_discord_gateway_lease(pool, token_hash, provider).await {
            Ok(Some(lease)) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🔐 GATEWAY-LEASE: {} acquired singleton lease",
                    provider.display_name()
                );
                GatewayLeaseOutcome::Proceed(Some(lease))
            }
            Ok(None) => {
                run_startup_diagnostic_after_reconcile_barrier(
                    startup_reconcile_remaining.clone(),
                    startup_doctor_started.clone(),
                    health_registry.clone(),
                    api_port,
                )
                .await;
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⏭ GATEWAY-LEASE: {} launch skipped — singleton lease held elsewhere",
                    provider.display_name()
                );
                GatewayLeaseOutcome::Skip
            }
            Err(error) => {
                run_startup_diagnostic_after_reconcile_barrier(
                    startup_reconcile_remaining.clone(),
                    startup_doctor_started.clone(),
                    health_registry.clone(),
                    api_port,
                )
                .await;
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⏭ GATEWAY-LEASE: {} launch skipped — failed to acquire singleton lease: {}",
                    provider.display_name(),
                    error
                );
                GatewayLeaseOutcome::Skip
            }
        },
        None => GatewayLeaseOutcome::Proceed(None),
    }
}

/// Spawn the gateway singleton-lease keepalive loop. On lease loss this
/// self-fences: flips shutdown flags, cancels tmux watchers, drains pending
/// queues, persists last_message_ids, and shuts down all shards. Spawned
/// after the client is built (needs `shard_manager`) and before the gateway
/// backend run. Returns the JoinHandle so run_bot can abort it on backend exit.
fn run_bot_spawn_gateway_lease_keepalive(
    mut lease: crate::db::postgres::AdvisoryLockLease,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    shard_manager: Arc<serenity::gateway::ShardManager>,
) -> tokio::task::JoinHandle<()> {
    let shared_for_lease = shared.clone();
    let provider_for_lease = provider.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(DISCORD_GATEWAY_LEASE_KEEPALIVE_INTERVAL);
        interval.tick().await;
        loop {
            interval.tick().await;

            if shared_for_lease
                .shutting_down
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                let _ = lease.unlock().await;
                return;
            }

            if let Err(error) = lease.keepalive().await {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::error!(
                    "  [{ts}] ⛔ GATEWAY-LEASE: {} lost singleton lease: {} — self-fencing",
                    provider_for_lease.display_name(),
                    error
                );

                shared_for_lease
                    .bot_connected
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                shared_for_lease
                    .shutting_down
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                shared_for_lease
                    .restart_pending
                    .store(true, std::sync::atomic::Ordering::SeqCst);

                for entry in shared_for_lease.tmux_watchers.iter() {
                    entry
                        .value()
                        .cancel
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }

                let drain = mailbox_restart_drain_all(&shared_for_lease, &provider_for_lease).await;
                let queue_count = drain.queued_count;
                if !drain.persistence_errors.is_empty() {
                    tracing::error!(
                        failures = drain.persistence_errors.len(),
                        "gateway lease self-fence observed pending-queue persistence failure(s)"
                    );
                }
                if queue_count > 0 {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] 📋 GATEWAY-LEASE: persisted {queue_count} pending queue item(s) before self-fence"
                    );
                }

                let ids: std::collections::HashMap<u64, u64> = shared_for_lease
                    .last_message_ids
                    .iter()
                    .map(|entry| (entry.key().get(), *entry.value()))
                    .collect();
                if !ids.is_empty() {
                    runtime_store::save_all_last_message_ids(provider_for_lease.as_str(), &ids);
                }

                shard_manager.shutdown_all().await;
                return;
            }
        }
    })
}

/// Spawn the SIGTERM graceful-shutdown handler. On SIGTERM it persists queue /
/// inflight / last_message state then quick-exits; tmux/TUI processes survive
/// for the next dcserver instance to rehydrate. Spawned after the lease
/// keepalive task and before the gateway backend run.
fn run_bot_spawn_sigterm_handler(shared: &Arc<SharedData>, provider_for_shutdown: ProviderKind) {
    let shared_for_signal = shared.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            if let Ok(mut sigterm) = signal(SignalKind::terminate()) {
                sigterm.recv().await;
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!("  [{ts}] 🛑 SIGTERM received — graceful shutdown");

                // Set global shutdown flag
                shared_for_signal
                    .shutting_down
                    .store(true, std::sync::atomic::Ordering::SeqCst);

                // Block dequeue and put router into drain mode so no new
                // queue/checkpoint mutations occur during shutdown.
                shared_for_signal
                    .restart_pending
                    .store(true, std::sync::atomic::Ordering::SeqCst);

                // ── Critical state persistence (MUST run before any I/O) ──
                // Save pending queues and last_message_ids FIRST, before any
                // network calls that might block/timeout and prevent saving.

                let drain =
                    mailbox_restart_drain_all(&shared_for_signal, &provider_for_shutdown).await;
                let queue_count = drain.queued_count;
                if !drain.persistence_errors.is_empty() {
                    tracing::error!(
                        failures = drain.persistence_errors.len(),
                        "SIGTERM initial drain observed pending-queue persistence failure(s)"
                    );
                }
                if queue_count > 0 {
                    let ts3 = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts3}] 📋 mailbox persisted {queue_count} pending queue item(s)"
                    );
                }

                // Persist last_message_ids for catch-up polling after restart
                {
                    let ids: std::collections::HashMap<u64, u64> = shared_for_signal
                        .last_message_ids
                        .iter()
                        .map(|entry| (entry.key().get(), *entry.value()))
                        .collect();
                    if !ids.is_empty() {
                        runtime_store::save_all_last_message_ids(
                            provider_for_shutdown.as_str(),
                            &ids,
                        );
                    }
                }

                // ── Inflight state preservation for silent re-attach ──
                let inflight_states = inflight::load_inflight_states(&provider_for_shutdown);
                if !inflight_states.is_empty() {
                    let ts2 = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts2}] 👁 preserving {} inflight turn(s) for restart recovery",
                        inflight_states.len()
                    );
                    let marked = inflight::mark_all_inflight_states_restart_mode(
                        &provider_for_shutdown,
                        crate::services::discord::InflightRestartMode::DrainRestart,
                    );
                    tracing::info!(
                        "  [{ts2}] 🔖 marked {marked} inflight turn(s) as drain_restart"
                    );
                }

                // ── Final state snapshot (belt-and-suspenders) ──
                // During the HTTP placeholder edits above, active turns may have
                // finished and mutated queues/last_message_ids. Re-save to capture
                // any changes that occurred after the initial save.
                {
                    let drain =
                        mailbox_restart_drain_all(&shared_for_signal, &provider_for_shutdown).await;
                    let queue_count = drain.queued_count;
                    if !drain.persistence_errors.is_empty() {
                        tracing::error!(
                            failures = drain.persistence_errors.len(),
                            "SIGTERM final drain observed pending-queue persistence failure(s)"
                        );
                    }
                    if queue_count > 0 {
                        let ts4 = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts4}] 📋 mailbox final drain: {queue_count} pending queue item(s)"
                        );
                    }
                }
                {
                    let ids: std::collections::HashMap<u64, u64> = shared_for_signal
                        .last_message_ids
                        .iter()
                        .map(|entry| (entry.key().get(), *entry.value()))
                        .collect();
                    if !ids.is_empty() {
                        runtime_store::save_all_last_message_ids(
                            provider_for_shutdown.as_str(),
                            &ids,
                        );
                    }
                }

                // Wait for all providers to finish saving before exiting.
                // CAS guard: skip if this provider already decremented via deferred restart path.
                if shared_for_signal
                    .shutdown_counted
                    .compare_exchange(
                        false,
                        true,
                        std::sync::atomic::Ordering::AcqRel,
                        std::sync::atomic::Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    if shared_for_signal
                        .shutdown_remaining
                        .fetch_sub(1, std::sync::atomic::Ordering::AcqRel)
                        == 1
                    {
                        std::process::exit(0);
                    }
                }
            }
        }
    });
}

/// Run the Discord gateway backend (`client.start()`) to completion, classify
/// the exit, run the post-reconcile startup diagnostic on failure, then abort
/// and join the gateway-lease keepalive task. This is the final event-loop
/// entry of run_bot. Consumes `client`.
#[allow(clippy::too_many_arguments)]
async fn run_bot_run_gateway_backend(
    mut client: serenity::Client,
    provider_for_error: &ProviderKind,
    gateway_lease_task: Option<tokio::task::JoinHandle<()>>,
    startup_reconcile_remaining_for_client_start: Arc<std::sync::atomic::AtomicUsize>,
    startup_doctor_started_for_client_start: Arc<std::sync::atomic::AtomicBool>,
    health_registry_for_client_start: Arc<health::HealthRegistry>,
    api_port: u16,
) {
    let gateway_backend_task = tokio::spawn(async move { client.start().await });
    let gateway_backend_failed = match gateway_backend_task.await {
        Ok(Ok(())) => {
            tracing::warn!(
                "  ✗ {} gateway backend exited without error",
                provider_for_error.display_name()
            );
            true
        }
        Ok(Err(error)) => {
            tracing::warn!(
                "  ✗ {} bot error: {error}",
                provider_for_error.display_name()
            );
            true
        }
        Err(join_error) if join_error.is_panic() => {
            tracing::error!(
                "  ✗ {} gateway backend task panicked: {join_error}",
                provider_for_error.display_name()
            );
            true
        }
        Err(join_error) => {
            tracing::warn!(
                "  ✗ {} gateway backend task ended unexpectedly: {join_error}",
                provider_for_error.display_name()
            );
            true
        }
    };
    if gateway_backend_failed {
        run_startup_diagnostic_after_reconcile_barrier(
            startup_reconcile_remaining_for_client_start,
            startup_doctor_started_for_client_start,
            health_registry_for_client_start,
            api_port,
        )
        .await;
    }

    if let Some(handle) = gateway_lease_task {
        handle.abort();
        let _ = handle.await;
    }
}


#[cfg(test)]
mod bootstrap_tests {
    use super::*;
    use std::collections::{HashMap, HashSet, VecDeque};

    fn sorted_channel_ids(channels: Vec<ChannelId>) -> Vec<u64> {
        channels
            .into_iter()
            .map(|channel_id| channel_id.get())
            .collect()
    }

    fn sorted_placeholder_pairs(
        pairs: Vec<((ChannelId, MessageId), MessageId)>,
    ) -> Vec<(u64, u64, u64)> {
        let mut pairs: Vec<(u64, u64, u64)> = pairs
            .into_iter()
            .map(|((channel_id, user_msg_id), placeholder_msg_id)| {
                (
                    channel_id.get(),
                    user_msg_id.get(),
                    placeholder_msg_id.get(),
                )
            })
            .collect();
        pairs.sort_unstable();
        pairs
    }

    fn sorted_stale_cards(cards: Vec<(ChannelId, MessageId, MessageId)>) -> Vec<(u64, u64, u64)> {
        let mut cards: Vec<(u64, u64, u64)> = cards
            .into_iter()
            .map(|(channel_id, user_msg_id, placeholder_msg_id)| {
                (
                    channel_id.get(),
                    user_msg_id.get(),
                    placeholder_msg_id.get(),
                )
            })
            .collect();
        cards.sort_unstable();
        cards
    }

    #[test]
    fn startup_doctor_barrier_arrive_decrements_once_until_release() {
        let remaining = std::sync::atomic::AtomicUsize::new(2);
        let started = std::sync::atomic::AtomicBool::new(false);

        assert_eq!(
            startup_doctor_barrier_arrive(&remaining, &started),
            StartupDoctorBarrier::Waiting(1)
        );
        assert_eq!(remaining.load(std::sync::atomic::Ordering::Acquire), 1);
        assert!(!started.load(std::sync::atomic::Ordering::Acquire));

        assert_eq!(
            startup_doctor_barrier_arrive(&remaining, &started),
            StartupDoctorBarrier::Released
        );
        assert_eq!(remaining.load(std::sync::atomic::Ordering::Acquire), 0);
        assert!(started.load(std::sync::atomic::Ordering::Acquire));

        assert_eq!(
            startup_doctor_barrier_arrive(&remaining, &started),
            StartupDoctorBarrier::AlreadyReleased
        );
        assert_eq!(
            remaining.load(std::sync::atomic::Ordering::Acquire),
            0,
            "arriving after release must not decrement below zero"
        );
    }

    #[test]
    fn startup_doctor_barrier_arrive_handles_prestarted_release_once() {
        let remaining = std::sync::atomic::AtomicUsize::new(1);
        let started = std::sync::atomic::AtomicBool::new(true);

        assert_eq!(
            startup_doctor_barrier_arrive(&remaining, &started),
            StartupDoctorBarrier::AlreadyReleased
        );
        assert_eq!(
            remaining.load(std::sync::atomic::Ordering::Acquire),
            0,
            "the final waiter still consumes exactly one remaining slot"
        );

        assert_eq!(
            startup_doctor_barrier_arrive(&remaining, &started),
            StartupDoctorBarrier::AlreadyReleased
        );
        assert_eq!(remaining.load(std::sync::atomic::Ordering::Acquire), 0);
    }

    #[test]
    fn restored_settings_filters_sort_parse_and_drop_disabled_entries() {
        let mut settings = DiscordBotSettings::default();
        settings.channel_fast_modes.insert("300".to_string(), true);
        settings.channel_fast_modes.insert("100".to_string(), true);
        settings.channel_fast_modes.insert("200".to_string(), false);
        settings
            .channel_fast_modes
            .insert("not-a-channel".to_string(), true);
        settings
            .channel_fast_mode_reset_pending
            .insert("codex:500".to_string());
        settings
            .channel_fast_mode_reset_pending
            .insert("400".to_string());
        settings
            .channel_fast_mode_reset_pending
            .insert("claude:400".to_string());
        settings
            .channel_fast_mode_reset_pending
            .insert("bad-reset-entry".to_string());
        settings.channel_codex_goals.insert("700".to_string(), true);
        settings.channel_codex_goals.insert("600".to_string(), true);
        settings
            .channel_codex_goals
            .insert("800".to_string(), false);
        settings
            .channel_codex_goals
            .insert("bad-goals".to_string(), true);
        settings
            .channel_codex_goals_reset_pending
            .insert("900".to_string());
        settings
            .channel_codex_goals_reset_pending
            .insert("850".to_string());
        settings
            .channel_codex_goals_reset_pending
            .insert("bad-reset".to_string());

        assert_eq!(
            sorted_channel_ids(restored_fast_mode_enabled_channels_for_provider(
                &settings,
                &ProviderKind::Codex,
            )),
            vec![100, 300]
        );
        assert_eq!(
            restored_fast_mode_reset_entries(&settings),
            vec![
                "400".to_string(),
                "bad-reset-entry".to_string(),
                "claude:400".to_string(),
                "codex:500".to_string(),
            ]
        );
        assert_eq!(
            sorted_channel_ids(restored_fast_mode_reset_channels(&settings)),
            vec![400, 500]
        );
        assert_eq!(
            sorted_channel_ids(restored_codex_goals_enabled_channels(&settings)),
            vec![600, 700]
        );
        assert_eq!(
            sorted_channel_ids(restored_codex_goals_reset_channels(&settings)),
            vec![850, 900]
        );
    }

    #[test]
    fn filter_restored_queued_placeholders_preserves_live_and_reports_stale() {
        let channel_live = ChannelId::new(10);
        let channel_stale = ChannelId::new(20);
        let mut loaded = HashMap::new();
        loaded.insert((channel_live, MessageId::new(100)), MessageId::new(1_000));
        loaded.insert((channel_live, MessageId::new(101)), MessageId::new(1_001));
        loaded.insert((channel_stale, MessageId::new(200)), MessageId::new(2_000));

        let mut live_queue_ids = HashMap::new();
        live_queue_ids.insert(channel_live, HashSet::from([100_u64]));

        let outcome = filter_restored_queued_placeholders(loaded, &live_queue_ids);

        assert_eq!(outcome.stale_count, 2);
        assert_eq!(
            outcome
                .channels_with_stale
                .iter()
                .map(|channel_id| channel_id.get())
                .collect::<HashSet<_>>(),
            HashSet::from([10, 20])
        );
        assert_eq!(
            sorted_placeholder_pairs(outcome.live),
            vec![(10, 100, 1_000)]
        );
        assert_eq!(
            sorted_stale_cards(outcome.stale_cards),
            vec![(10, 101, 1_001), (20, 200, 2_000)]
        );
    }

    #[test]
    fn discord_gateway_intents_snapshot_matches_bootstrap_contract() {
        let intents = discord_gateway_intents();
        let expected = serenity::GatewayIntents::GUILDS
            | serenity::GatewayIntents::GUILD_MESSAGES
            | serenity::GatewayIntents::GUILD_MESSAGE_REACTIONS
            | serenity::GatewayIntents::GUILD_VOICE_STATES
            | serenity::GatewayIntents::DIRECT_MESSAGES
            | serenity::GatewayIntents::DIRECT_MESSAGE_REACTIONS
            | serenity::GatewayIntents::MESSAGE_CONTENT;

        assert_eq!(intents, expected);
    }

    struct RecordingStalePlaceholderDeleter {
        calls: std::sync::Mutex<Vec<(u64, u64)>>,
        results: std::sync::Mutex<VecDeque<Result<(), String>>>,
    }

    impl RecordingStalePlaceholderDeleter {
        fn new(results: impl IntoIterator<Item = Result<(), String>>) -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                results: std::sync::Mutex::new(results.into_iter().collect()),
            }
        }

        fn calls(&self) -> Vec<(u64, u64)> {
            self.calls
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone()
        }
    }

    impl StalePlaceholderDeleter for RecordingStalePlaceholderDeleter {
        fn delete<'a>(
            &'a self,
            channel_id: ChannelId,
            placeholder_msg_id: MessageId,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>
        {
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .push((channel_id.get(), placeholder_msg_id.get()));
                self.results
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .pop_front()
                    .unwrap_or(Ok(()))
            })
        }
    }

    #[tokio::test]
    async fn delete_stale_queued_placeholder_cards_with_deletes_only_supplied_stale_cards() {
        let deleter = RecordingStalePlaceholderDeleter::new([Ok(()), Err("gone".to_string())]);
        let stale_cards = vec![
            (
                ChannelId::new(10),
                MessageId::new(100),
                MessageId::new(1_000),
            ),
            (
                ChannelId::new(20),
                MessageId::new(200),
                MessageId::new(2_000),
            ),
        ];

        delete_stale_queued_placeholder_cards_with(&deleter, &stale_cards).await;

        assert_eq!(deleter.calls(), vec![(10, 1_000), (20, 2_000)]);

        let empty_deleter = RecordingStalePlaceholderDeleter::new([]);
        delete_stale_queued_placeholder_cards_with(&empty_deleter, &[]).await;
        assert!(
            empty_deleter.calls().is_empty(),
            "empty stale-card input must preserve all visible cards"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = false)]
    async fn wait_for_local_http_bind_returns_quickly_when_port_is_bound() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let port = listener.local_addr().expect("local addr").port();
        let started = std::time::Instant::now();
        wait_for_local_http_bind(port).await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "bound port should resolve well under the {:?} bind timeout (elapsed {:?})",
            STARTUP_DOCTOR_HTTP_BIND_TIMEOUT,
            started.elapsed()
        );
        drop(listener);
    }

    #[test]
    fn bootstrap_session_reset_pending_excludes_restored_model_overrides() {
        let model_only_channel = ChannelId::new(123);
        let fast_mode_reset_channel = ChannelId::new(456);
        let goals_reset_channel = ChannelId::new(789);
        let restored_model_overrides = vec![(model_only_channel, "gpt-5.5".to_string())];

        let pending = bootstrap_session_reset_pending_channels(
            &restored_model_overrides,
            &[fast_mode_reset_channel],
            &[goals_reset_channel],
        );

        assert!(
            !pending.contains(&model_only_channel),
            "restoring a persisted model override must not force a fresh provider session"
        );
        assert!(pending.contains(&fast_mode_reset_channel));
        assert!(pending.contains(&goals_reset_channel));
    }

    #[test]
    fn voice_auto_join_provider_map_includes_agent_voice_channel() {
        let cfg: crate::config::Config = serde_yaml::from_str(
            r#"
server:
  port: 8791
agents:
- id: project-agentdesk
  name: AgentDesk
  provider: claude
  voice:
    channel_id: '999'
    foreground:
      provider: codex
  channels:
    codex:
      id: '123'
      name: adk-cdx
      provider: codex
"#,
        )
        .expect("config parses");

        let map = voice_auto_join_provider_map(&cfg);

        assert_eq!(map.get("123").map(|value| value.0.as_str()), Some("codex"));
        assert_eq!(map.get("123").and_then(|value| value.1.as_deref()), None);
        assert_eq!(map.get("999").map(|value| value.0.as_str()), Some("codex"));
        assert_eq!(
            map.get("999").and_then(|value| value.1.as_deref()),
            Some("project-agentdesk")
        );
    }

    #[test]
    fn legacy_durable_handoff_cleanup_removes_retired_json_tree() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
        struct EnvReset(Option<std::ffi::OsString>);
        impl Drop for EnvReset {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                    None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
                }
            }
        }
        let _reset = EnvReset(previous_root);

        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };
        let handoff_root = tmp
            .path()
            .join("runtime")
            .join("discord_handoff")
            .join("codex");
        std::fs::create_dir_all(&handoff_root).unwrap();
        std::fs::write(handoff_root.join("1486333430516945008.json"), "{}").unwrap();

        purge_legacy_durable_handoffs();

        assert!(
            !tmp.path().join("runtime").join("discord_handoff").exists(),
            "legacy handoff JSON must be removed without being parsed or consumed"
        );
    }
}
