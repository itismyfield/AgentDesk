use super::*;

/// #3479 Item 3: runtime services/handles/backends consumed by the SharedData
/// builder. Groups seven former positional args; field names match the original
/// argument names so the builder body is unchanged after destructuring.
pub(super) struct RuntimeServices {
    pub(super) initial_skills: Vec<(String, String)>,
    pub(super) token_hash: String,
    pub(super) api_port: u16,
    pub(super) voice_barge_in: Arc<voice_barge_in::VoiceBargeInRuntime>,
    pub(super) health_registry: Arc<health::HealthRegistry>,
    pub(super) pg_pool: Option<sqlx::PgPool>,
    pub(super) engine: Option<crate::engine::PolicyEngine>,
}

/// #3479 Item 3: process-wide lifecycle counters shared across all providers
/// (injected so every provider's `SharedData` shares the same atomics).
pub(super) struct ProcessLifecycleCounters {
    pub(super) global_active: Arc<std::sync::atomic::AtomicUsize>,
    pub(super) global_finalizing: Arc<std::sync::atomic::AtomicUsize>,
    pub(super) shutdown_remaining: Arc<std::sync::atomic::AtomicUsize>,
}

/// #3479 Item 3: UI feature gates for the live-placeholder / status-panel surface.
pub(super) struct UiFeatureFlags {
    pub(super) placeholder_live_events_enabled: bool,
    pub(super) status_panel_v2_enabled: bool,
    /// #3805 P2: two-message panel rollout gate (default OFF). Scaffolding —
    /// copied into shared UI state alongside `status_panel_v2_enabled` but not
    /// read by any path in PR-A.
    pub(super) two_message_panel_enabled: bool,
}

/// #3479 Item 3: session state restored from persisted bot settings. Borrowed
/// because run_bot reuses these slices for logging + session-reset bootstrap
/// after the builder returns.
pub(super) struct RestoredSessionState<'a> {
    pub(super) model_overrides: &'a [(ChannelId, String)],
    pub(super) node_overrides: &'a [(ChannelId, String)],
    pub(super) fast_mode_channels: &'a [ChannelId],
    pub(super) fast_mode_reset_entries: &'a [String],
    pub(super) fast_mode_reset_channels: &'a [ChannelId],
    pub(super) codex_goals_channels: &'a [ChannelId],
    pub(super) codex_goals_reset_channels: &'a [ChannelId],
}

/// Build all owned `SharedData` fields and wrap in an `Arc`. Side-effecting
/// initializers (`TurnFinalizer::spawn`,
/// `runtime_store::allocate_process_generation`, `load_queue_exit_placeholder_clears`,
/// the `inflight_signals` and `turn_completion_events` broadcast channels) run here in the exact same order
/// as the original inline struct literal. `bot_settings`, `services.initial_skills`,
/// `counters.global_active`, `counters.global_finalizing`, `services.pg_pool`, and
/// `services.engine` are consumed by move; the `restored.*` slices are borrowed
/// (they are reused later in run_bot for logging and session-reset bootstrap).
pub(super) fn run_bot_build_shared_data(
    bot_settings: DiscordBotSettings,
    provider: &ProviderKind,
    services: RuntimeServices,
    counters: ProcessLifecycleCounters,
    flags: UiFeatureFlags,
    restored: RestoredSessionState<'_>,
) -> Arc<SharedData> {
    // #3479 Item 3: destructure the grouped params back into the original
    // variable names so the construction body below is byte-identical.
    let RuntimeServices {
        initial_skills,
        token_hash,
        api_port,
        voice_barge_in,
        health_registry,
        pg_pool,
        engine,
    } = services;
    let ProcessLifecycleCounters {
        global_active,
        global_finalizing,
        shutdown_remaining,
    } = counters;
    let UiFeatureFlags {
        placeholder_live_events_enabled,
        status_panel_v2_enabled,
        two_message_panel_enabled,
    } = flags;
    let RestoredSessionState {
        model_overrides: restored_model_overrides,
        node_overrides: restored_node_overrides,
        fast_mode_channels: restored_fast_mode_channels,
        fast_mode_reset_entries: restored_fast_mode_reset_entries,
        fast_mode_reset_channels: restored_fast_mode_reset_channels,
        codex_goals_channels: restored_codex_goals_channels,
        codex_goals_reset_channels: restored_codex_goals_reset_channels,
    } = restored;
    let process_generation = runtime_store::allocate_process_generation();
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
            two_message_panel_enabled,
        },
        // #3038 S1: wrapped verbatim at the first-member position (evaluation-order preserved).
        queued: QueuedPlaceholderState {
            queued_placeholders: dashmap::DashMap::new(),
            queue_exit_placeholder_clears: {
                let map = dashmap::DashMap::new();
                for (key, placeholder_msg_id) in
                    crate::services::discord::queued_placeholders_store::load_queue_exit_placeholder_clears(
                        provider, &token_hash,
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
        // move textually above the TurnFinalizer spawn
        // calls, but all three are side-effect-free initializers (parameter
        // move, `Arc::clone`, const constructor), so the relative order of
        // every side-effecting initializer
        // (`load_queue_exit_placeholder_clears` ↔ `process_generation` ↔
        // `Instant::now` ↔ `TurnFinalizer::spawn` ↔ `broadcast::channel`) is
        // preserved.
        restart: RestartLifecycle {
            recovering_channels: dashmap::DashMap::new(),
            shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            intake_worker_lifecycle:
                crate::services::cluster::intake_worker::IntakeWorkerLifecycle::default(),
            finalizing_turns: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            current_generation: process_generation,
            restart_pending: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            reconcile_done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            deferred_hook_backlog: std::sync::atomic::AtomicUsize::new(0),
            deferred_hook_channels: dashmap::DashMap::new(),
            recovery_started_at: std::time::Instant::now(),
            recovery_duration_ms: std::sync::atomic::AtomicU64::new(0),
            global_active,
            global_finalizing,
            shutdown_remaining,
            shutdown_counted: std::sync::atomic::AtomicBool::new(false),
            shutdown_slot_consumed: std::sync::atomic::AtomicBool::new(false),
        },
        turn_finalizer: crate::services::discord::turn_finalizer::TurnFinalizer::spawn(),
        // #3479 Item 3: dispatch intake/routing cluster. All three members are
        // side-effect-free `DashMap::new()` inits, so grouping them at this
        // first-member position (dispatch_role_overrides moved up from below)
        // preserves the evaluation order of every side-effecting initializer.
        dispatch: DispatchRoutingState {
            intake_dedup: dashmap::DashMap::new(),
            thread_parents: dashmap::DashMap::new(),
            role_overrides: dashmap::DashMap::new(),
        },
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
            node_overrides: {
                let map = dashmap::DashMap::new();
                for (channel_id, instance_id) in restored_node_overrides {
                    map.insert(*channel_id, instance_id.clone());
                }
                map
            },
            model_session_reset_pending: dashmap::DashSet::new(),
            session_reset_pending: bootstrap_session_reset_pending_channels(
                restored_model_overrides,
                restored_fast_mode_reset_channels,
                restored_codex_goals_reset_channels,
            ),
            model_picker_pending: dashmap::DashMap::new(),
        },
        voice_barge_in,
        voice_pairings: Arc::new(voice_routing::VoiceChannelPairingStore::load_default()),
        last_message_ids: dashmap::DashMap::new(),
        catch_up_retry_pending: dashmap::DashMap::new(),
        turn_start_times: dashmap::DashMap::new(),
        channel_rosters: dashmap::DashMap::new(),
        http: RuntimeHttpCache {
            cached_serenity_ctx: tokio::sync::OnceCell::new(),
            cached_bot_token: tokio::sync::OnceCell::new(),
        },
        token_hash,
        provider: provider.clone(),
        api_port,
        pg_pool,
        policy: PolicyRuntime { engine },
        health_registry: Arc::downgrade(&health_registry),
        known_slash_commands: tokio::sync::OnceCell::new(),
        // #2448: capacity 256 gives ~hundreds of in-flight turns headroom
        // before a slow listener triggers `RecvError::Lagged`. The standby
        // relay subscriber falls back to file polling on lag.
        inflight_signals: tokio::sync::broadcast::channel(256).0,
        turn_completion_events: tokio::sync::broadcast::channel(
            crate::services::discord::turn_completion_events::TURN_COMPLETION_EVENT_BUS_CAPACITY,
        )
        .0,
        turn_view_reconciler:
            crate::services::discord::turn_view_reconciler::TurnViewReconciler::default(),
        readopted_mailbox_ledger:
            crate::services::discord::readopted_mailbox_ledger::ReadoptedMailboxLedger::default(),
    })
}
