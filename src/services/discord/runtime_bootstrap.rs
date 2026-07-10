use super::*;

mod framework_setup;
mod gateway_lease;
mod gateway_runtime;
mod intake;
mod orphan_recovery;
mod queued_placeholders;
mod recovery_flush;
mod restored_state;
mod session_gc;
mod shared_data;
mod shutdown;
mod spawns;
mod startup_doctor;
mod voice;

use self::framework_setup::{run_bot_build_slash_commands, run_bot_framework_setup};
use self::gateway_lease::{
    GatewayLeaseOutcome, run_bot_acquire_gateway_lease, run_bot_spawn_gateway_lease_keepalive,
};
use self::gateway_runtime::run_bot_start_gateway_runtime;
use self::intake::run_bot_maybe_spawn_intake_worker;
#[allow(unused_imports)]
pub(in crate::services::discord) use self::queued_placeholders::{
    FilteredQueuedPlaceholders, StalePlaceholderDeleter, collect_live_queue_message_ids,
    delete_stale_queued_placeholder_cards, delete_stale_queued_placeholder_cards_with,
    filter_restored_queued_placeholders,
};
use self::shared_data::{
    ProcessLifecycleCounters, RestoredSessionState, RuntimeServices, UiFeatureFlags,
    run_bot_build_shared_data,
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
    /// #3805 P2: two-message panel rollout gate (default OFF). Scaffolding —
    /// threaded from config into shared UI state, not read by any path in PR-A.
    pub(crate) two_message_panel_enabled: bool,
}

pub(super) fn discord_gateway_intents() -> serenity::GatewayIntents {
    serenity::GatewayIntents::GUILDS
        | serenity::GatewayIntents::GUILD_MESSAGES
        | serenity::GatewayIntents::GUILD_VOICE_STATES
        | serenity::GatewayIntents::DIRECT_MESSAGES
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
        two_message_panel_enabled,
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
    super::single_message_panel_enabled();

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
    let restored_node_overrides: Vec<(ChannelId, String)> = bot_settings
        .channel_node_overrides
        .iter()
        .filter_map(|(channel_id, instance_id)| {
            channel_id
                .parse::<u64>()
                .ok()
                .map(|id| (ChannelId::new(id), instance_id.clone()))
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
        &provider,
        RuntimeServices {
            initial_skills,
            token_hash: token_hash.clone(),
            api_port,
            voice_barge_in: voice_barge_in.clone(),
            health_registry: health_registry.clone(),
            pg_pool,
            engine,
        },
        ProcessLifecycleCounters {
            global_active,
            global_finalizing,
            shutdown_remaining: shutdown_remaining.clone(),
        },
        UiFeatureFlags {
            placeholder_live_events_enabled,
            status_panel_v2_enabled,
            two_message_panel_enabled,
        },
        RestoredSessionState {
            model_overrides: &restored_model_overrides,
            node_overrides: &restored_node_overrides,
            fast_mode_channels: &restored_fast_mode_channels,
            fast_mode_reset_entries: &restored_fast_mode_reset_entries,
            fast_mode_reset_channels: &restored_fast_mode_reset_channels,
            codex_goals_channels: &restored_codex_goals_channels,
            codex_goals_reset_channels: &restored_codex_goals_reset_channels,
        },
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
    let _ = shared.http.cached_bot_token.set(token.to_string());

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
    // Cancellation rides on `shared.restart.shutting_down`. On the leader, the
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

    run_bot_start_gateway_runtime(
        token,
        provider,
        provider_for_error,
        provider_for_framework,
        provider_for_shutdown,
        startup_reconcile_remaining,
        startup_doctor_started,
        health_registry,
        startup_reconcile_remaining_for_client_start,
        startup_doctor_started_for_client_start,
        health_registry_for_client_start,
        api_port,
        shared,
        voice_config,
        voice_receiver,
        gateway_lease,
        &restored_model_overrides,
        &restored_fast_mode_channels,
    )
    .await;
}

// ── run_bot startup-phase helpers (decomposition of the run_bot
// god-function, issue #3038). These are behavior-preserving extractions:
// each helper runs the exact statements it replaced, in the same order,
// and run_bot calls them in the same order with the same threaded state.
// INITIALIZATION/SPAWN ORDER IS LOAD-BEARING — do not reorder. ──

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
    fn reaction_status_removal_cannot_cancel_gateway_contract() {
        let intents = discord_gateway_intents();
        assert!(
            !intents.contains(serenity::GatewayIntents::GUILD_MESSAGE_REACTIONS),
            "guild reaction events must stay unsubscribed so status removal cannot cancel work"
        );
        assert!(
            !intents.contains(serenity::GatewayIntents::DIRECT_MESSAGE_REACTIONS),
            "DM reaction events must stay unsubscribed so status removal cannot cancel work"
        );

        let expected = serenity::GatewayIntents::GUILDS
            | serenity::GatewayIntents::GUILD_MESSAGES
            | serenity::GatewayIntents::GUILD_VOICE_STATES
            | serenity::GatewayIntents::DIRECT_MESSAGES
            | serenity::GatewayIntents::MESSAGE_CONTENT;
        assert_eq!(intents, expected);

        let intake_gate_source = include_str!("router/intake_gate.rs");
        assert!(
            !intake_gate_source.contains("FullEvent::ReactionRemove"),
            "intake must not dispatch reaction-removal events"
        );
        assert!(
            !intake_gate_source.contains("reaction_remove::handle_reaction_remove"),
            "the destructive reaction-removal handler must not be reachable from intake"
        );
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

#[cfg(test)]
mod restart_lifecycle_characterization_tests {
    //! #3038 S3-0 — characterization tests for the restart-lifecycle cluster
    //! (cluster E) BEFORE the thirteen fields are lifted into
    //! `shared_state::RestartLifecycle`. The same tests passing unchanged
    //! after the move is the behaviour-equivalence proof (the S1
    //! `QueuedPlaceholderState` / S2 `SessionOverrideState` characterization
    //! standard, #3294/#3295 lineage).
    //!
    //! The tests never read `SharedData` fields. State is observed through
    //! the test's own `Arc` handle on the injected process-global
    //! `shutdown_remaining` counter (the `run_bot_build_shared_data`
    //! injection seam used by the real `run_bot`), and behaviour is driven
    //! only through the production function surface:
    //! `run_bot_spawn_deferred_restart_poller` (the deferred-restart marker
    //! poll loop, historically the `restart_ctrl` path) and
    //! `check_deferred_restart`. This is what lets the extraction's
    //! field-path rewiring land without a single test edit.
    //!
    //! Branches that cannot be pinned here, and why:
    //! - the final-provider `std::process::exit(0)` arms (poller,
    //!   `check_deferred_restart`, SIGTERM handler) would kill the test
    //!   runner — every scenario below keeps `shutdown_remaining > 1`;
    //! - the SIGTERM handler itself needs a process signal — its
    //!   `shutdown_counted` CAS + `shutdown_remaining` decrement protocol is
    //!   byte-identical to the poller/`check_deferred_restart` protocol
    //!   pinned here, and the run_bot S0 precedent (word-diff 0 + compile
    //!   gate) covers the unseedable handler body;
    //! - `check_deferred_restart`'s fresh-token decrement branch requires
    //!   `restart_pending == true` with `shutdown_counted == false`, a state
    //!   only the unseedable SIGTERM path produces without writing fields
    //!   directly — it is pinned by a post-move regression test instead
    //!   (`shared_state.rs` S3), where the group path may be used freely.

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";

    struct EnvGuard;

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var(AGENTDESK_ROOT_DIR_ENV);
            }
        }
    }

    fn isolate_runtime_root(tmp: &std::path::Path) -> EnvGuard {
        unsafe {
            std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.to_str().unwrap());
        }
        EnvGuard
    }

    /// Build a `SharedData` through the production constructor
    /// (`run_bot_build_shared_data`) so the test keeps its own handle on the
    /// injected `shutdown_remaining` counter — exactly how `run_bot` shares
    /// the counter across providers — instead of reading `SharedData` fields.
    fn build_shared_with_injected_shutdown_remaining(
        shutdown_remaining: &Arc<AtomicUsize>,
    ) -> Arc<SharedData> {
        let voice = Arc::new(voice_barge_in::VoiceBargeInRuntime::disabled());
        let health_registry = Arc::new(health::HealthRegistry::new());
        run_bot_build_shared_data(
            DiscordBotSettings::default(),
            &ProviderKind::Claude,
            RuntimeServices {
                initial_skills: Vec::new(),
                token_hash: "s3-restart-characterization-token-hash".to_string(),
                api_port: 9,
                voice_barge_in: voice,
                health_registry,
                pg_pool: None,
                engine: None,
            },
            ProcessLifecycleCounters {
                global_active: Arc::new(AtomicUsize::new(0)),
                global_finalizing: Arc::new(AtomicUsize::new(0)),
                shutdown_remaining: shutdown_remaining.clone(),
            },
            UiFeatureFlags {
                placeholder_live_events_enabled: false,
                status_panel_v2_enabled: false,
                two_message_panel_enabled: false,
            },
            RestoredSessionState {
                model_overrides: &[],
                node_overrides: &[],
                fast_mode_channels: &[],
                fast_mode_reset_entries: &[],
                fast_mode_reset_channels: &[],
                codex_goals_channels: &[],
                codex_goals_reset_channels: &[],
            },
        )
    }

    // Current-thread runtime driven from a synchronous `#[test]` so the
    // `test_support` env lock is never held across an `.await` inside an
    // async context (await_holding_lock ratchet stays flat — S1/S2 pattern).
    // `start_paused` auto-advance fast-forwards the 10s deferred-restart
    // poll interval instead of sleeping through it in real time.
    fn paused_rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap()
    }

    #[test]
    fn check_deferred_restart_is_noop_without_restart_pending() {
        // #3167 B3: serialize process-global env mutation via the single
        // crate-wide `test_support` lock (no local per-module Mutex).
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env_guard = isolate_runtime_root(tmp.path());
        let rt = paused_rt();
        rt.block_on(async {
            let shutdown_remaining = Arc::new(AtomicUsize::new(5));
            let shared = build_shared_with_injected_shutdown_remaining(&shutdown_remaining);

            // Decision matrix row 1: no restart pending — the helper must
            // return before touching the shutdown token or the barrier.
            check_deferred_restart(&shared);
            assert_eq!(
                shutdown_remaining.load(Ordering::Acquire),
                5,
                "without restart_pending, check_deferred_restart must not consume the shutdown token"
            );

            // Idempotent: a second poll-loop tick is still a no-op.
            check_deferred_restart(&shared);
            assert_eq!(shutdown_remaining.load(Ordering::Acquire), 5);
        });
    }

    #[test]
    fn deferred_restart_poller_consumes_marker_token_and_decrements_exactly_once() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env_guard = isolate_runtime_root(tmp.path());
        let rt = paused_rt();
        rt.block_on(async {
            // Three providers outstanding: this provider's quick-exit pass
            // must take the count 3 → 2 and stop there (the exit(0) arm is
            // only reachable for the LAST provider, remaining == 1).
            let shutdown_remaining = Arc::new(AtomicUsize::new(3));
            let shared = build_shared_with_injected_shutdown_remaining(&shutdown_remaining);

            let root = crate::agentdesk_runtime_root().expect("runtime root override");
            std::fs::create_dir_all(&root).unwrap();
            let marker = root.join("restart_pending");
            std::fs::write(&marker, "v0.0.0-s3-characterization").unwrap();

            spawns::run_bot_spawn_deferred_restart_poller(&shared, &ProviderKind::Claude);

            // Marker branch: restart_pending + shutting_down are flipped,
            // the shutdown_counted CAS consumes this provider's token, local
            // state is persisted, and shutdown_remaining decrements ONCE.
            let mut decremented = false;
            for _ in 0..400 {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                if shutdown_remaining.load(Ordering::Acquire) == 2 {
                    decremented = true;
                    break;
                }
            }
            assert!(
                decremented,
                "deferred-restart poller must decrement the injected shutdown_remaining via the marker quick-exit path"
            );

            // Exactly-once: the consumed token blocks every later decrement
            // attempt — give the poller several more (auto-advanced) poll
            // intervals and re-drive the check_deferred_restart surface
            // directly. If the CAS guard regressed, remaining would hit 1.
            for _ in 0..400 {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            assert_eq!(
                shutdown_remaining.load(Ordering::Acquire),
                2,
                "shutdown_counted must hold the poller to a single shutdown_remaining decrement"
            );
            check_deferred_restart(&shared);
            assert_eq!(
                shutdown_remaining.load(Ordering::Acquire),
                2,
                "a consumed shutdown token must make check_deferred_restart a no-op (poll-loop + SIGTERM double-run guard)"
            );

            // Non-final provider must leave the marker on disk for the
            // remaining providers' own quick-exit passes.
            assert!(
                marker.exists(),
                "restart_pending marker is only removed by the final provider's exit arm"
            );
        });
    }
}
