use super::*;

mod framework_setup;
mod gateway_lease;
mod intake;
mod orphan_recovery;
mod queued_placeholders;
mod recovery_flush;
mod restored_state;
mod session_gc;
mod shutdown;
mod spawns;
mod startup_doctor;
mod voice;

use self::framework_setup::{run_bot_build_slash_commands, run_bot_framework_setup};
use self::gateway_lease::{
    GatewayLeaseOutcome, run_bot_acquire_gateway_lease, run_bot_spawn_gateway_lease_keepalive,
};
use self::intake::run_bot_maybe_spawn_intake_worker;
#[allow(unused_imports)]
pub(in crate::services::discord) use self::queued_placeholders::{
    FilteredQueuedPlaceholders, StalePlaceholderDeleter, collect_live_queue_message_ids,
    delete_stale_queued_placeholder_cards, delete_stale_queued_placeholder_cards_with,
    filter_restored_queued_placeholders,
};
use self::shutdown::{run_bot_run_gateway_backend, run_bot_spawn_sigterm_handler};
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
