use super::*;

/// Build all owned `SharedData` fields and wrap in an `Arc`. Side-effecting
/// initializers (`TurnFinalizer::spawn`, `StatusPanelController::spawn`,
/// `runtime_store::load_generation`, `load_queue_exit_placeholder_clears`,
/// the `inflight_signals` broadcast channel) run here in the exact same order
/// as the original inline struct literal. `bot_settings`, `initial_skills`,
/// `global_active`, `global_finalizing`, `pg_pool`, and `engine` are consumed
/// by move; the `restored_*` slices are borrowed (they are reused later in
/// run_bot for logging and session-reset bootstrap).
#[allow(clippy::too_many_arguments)]
pub(super) fn run_bot_build_shared_data(
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
        tmux_watchers: crate::services::discord::TmuxWatcherRegistry::new(),
        tmux_relay_coords: dashmap::DashMap::new(),
        // #3038 S4: wrapped verbatim at the first-member position
        // (evaluation-order preserved).
        ui: PlaceholderState {
            placeholder_cleanup: Arc::new(
                crate::services::discord::placeholder_cleanup::PlaceholderCleanupRegistry::default(
                ),
            ),
            placeholder_controller: Arc::new(
                crate::services::discord::placeholder_controller::PlaceholderController::default(),
            ),
            placeholder_live_events: Arc::new(
                crate::services::discord::placeholder_live_events::PlaceholderLiveEvents::default(),
            ),
            placeholder_live_events_enabled,
            status_panel_v2_enabled,
        },
        // #3038 S1: wrapped verbatim at the first-member position (evaluation-order preserved).
        queued: QueuedPlaceholderState {
            queued_placeholders: dashmap::DashMap::new(),
            queue_exit_placeholder_clears: {
                let map = dashmap::DashMap::new();
                for (key, placeholder_msg_id) in
                    crate::services::discord::queued_placeholders_store::load_queue_exit_placeholder_clears(
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
            crate::services::discord::answer_flush_barrier::AnswerFlushBarrier::default(),
        ),
        // #3038 S3: wrapped at the first-member position with member
        // expressions byte-identical. The three trailing members
        // (`global_finalizing` / `shutdown_remaining` / `shutdown_counted`)
        // move textually above the TurnFinalizer/StatusPanelController spawn
        // calls, but all three are side-effect-free initializers (parameter
        // move, `Arc::clone`, const constructor), so the relative order of
        // every side-effecting initializer
        // (`load_queue_exit_placeholder_clears` ↔ `load_generation` ↔
        // `Instant::now` ↔ `TurnFinalizer::spawn` ↔
        // `StatusPanelController::spawn` ↔ `broadcast::channel`) is
        // preserved.
        restart: RestartLifecycle {
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
            global_finalizing,
            shutdown_remaining: shutdown_remaining.clone(),
            shutdown_counted: std::sync::atomic::AtomicBool::new(false),
        },
        turn_finalizer: crate::services::discord::turn_finalizer::TurnFinalizer::spawn(),
        status_panel_controller:
            crate::services::discord::status_panel_controller::StatusPanelController::spawn(
                status_panel_v2_enabled,
            ),
        intake_dedup: dashmap::DashMap::new(),
        dispatch_thread_parents: dashmap::DashMap::new(),
        bot_connected: std::sync::atomic::AtomicBool::new(false),
        last_turn_at: std::sync::Mutex::new(None),
        // #3038 S2: wrapped verbatim at the first-member position (evaluation-order preserved).
        overrides: SessionOverrideState {
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
        },
        dispatch_role_overrides: dashmap::DashMap::new(),
        voice_barge_in: voice_barge_in.clone(),
        voice_pairings: Arc::new(voice_routing::VoiceChannelPairingStore::load_default()),
        last_message_ids: dashmap::DashMap::new(),
        catch_up_retry_pending: dashmap::DashMap::new(),
        turn_start_times: dashmap::DashMap::new(),
        channel_rosters: dashmap::DashMap::new(),
        http: RuntimeHttpCache {
            cached_serenity_ctx: tokio::sync::OnceCell::new(),
            cached_bot_token: tokio::sync::OnceCell::new(),
        },
        token_hash: token_hash.to_string(),
        provider: provider.clone(),
        api_port,
        pg_pool,
        policy: PolicyRuntime { engine },
        health_registry: Arc::downgrade(health_registry),
        known_slash_commands: tokio::sync::OnceCell::new(),
        // #2448: capacity 256 gives ~hundreds of in-flight turns headroom
        // before a slow listener triggers `RecvError::Lagged`. The standby
        // relay subscriber falls back to file polling on lag.
        inflight_signals: tokio::sync::broadcast::channel(256).0,
    })
}
