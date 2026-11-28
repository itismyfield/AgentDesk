use super::*;

/// #3479 Item 3: runtime services/handles/backends consumed by the SharedData
/// builder. Field names match the original arguments so the builder body stays
/// explicit at the ownership boundary.
pub(super) struct RuntimeServices {
    pub(super) initial_skills: Vec<(String, String)>,
    pub(super) token_hash: String,
    pub(super) api_port: u16,
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
    /// Two-message panel rollout gate (default OFF).
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
    // Keep supplied handles as owned inputs at the bootstrap boundary.
    let RuntimeServices {
        initial_skills,
        token_hash,
        api_port,
        health_registry,
        pg_pool,
        engine,
    } = services;
    crate::services::discord::health::runtime_registry::publish_runtime_registry(&health_registry);
    let ProcessLifecycleCounters {
        global_active,
        global_finalizing,
        shutdown_remaining,
    } = counters;
    let UiFeatureFlags {
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
    let tmux_relay_coords = Arc::new(dashmap::DashMap::new());
    let mut prepared_bot_state = None;
    let engine_runtime = EngineRuntime::new(
        provider.clone(),
        api_port,
        pg_pool,
        PolicyRuntime { engine },
        initial_skills,
        || {
            let core = Mutex::new(CoreState {
                sessions: HashMap::new(),
            });
            let mailboxes = ChannelMailboxRegistry::default();
            let settings = tokio::sync::RwLock::new(bot_settings);
            let api_timestamps = dashmap::DashMap::new();
            let registry =
                Arc::new(crate::services::engine_runtime::relay_state::RegistryState::new());
            prepared_bot_state = Some((settings, api_timestamps));
            (core, mailboxes, registry)
        },
    );
    let (settings, api_timestamps) =
        prepared_bot_state.expect("bootstrap storage initialized once");
    let tmux_watchers = crate::services::discord::TmuxWatcherRegistry::from_storage(
        Arc::clone(&engine_runtime.watcher_registry),
        Arc::clone(&tmux_relay_coords),
    );
    Arc::new(SharedData {
        engine: engine_runtime,
        settings,
        api_timestamps,
        tmux_watchers,
        tmux_relay_coords,
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
                crate::services::discord::placeholder_live_events::PlaceholderLiveEvents::with_tool_activity_display(crate::config::runtime_config().placeholder.tool_activity_display),
            ),
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
            replay_restart: std::sync::OnceLock::new(),
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
            gateway_waiter_cancellation: std::sync::Mutex::new(None),
            gateway_lease_diagnostics: Default::default(),
            gateway_handback_health: Arc::default(),
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
        // #3038 S2: wrapped verbatim at the first-member position (evaluation-order preserved).
        overrides: SessionOverrideState {
            model_overrides: {
                let map = dashmap::DashMap::new();
                restore_model_overrides(&map, restored_model_overrides);
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
        last_message_ids: dashmap::DashMap::new(),
        catch_up_retry_pending: dashmap::DashMap::new(),
        catch_up_scan_in_progress: dashmap::DashMap::new(),
        channel_rosters: dashmap::DashMap::new(),
        http: RuntimeHttpCache {
            cached_serenity_ctx: tokio::sync::OnceCell::new(),
            cached_bot_token: tokio::sync::OnceCell::new(),
        },
        token_hash,
        health_registry: (&health_registry).into(),
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
        router_ports: crate::services::discord::router::ROUTER_PORTS,
    })
}

#[cfg(test)]
pub(crate) fn shared_data_without_policy_for_tests(
    registry: &Arc<health::HealthRegistry>,
    pg_pool: Option<sqlx::PgPool>,
    #[cfg(unix)] owner_wake: Option<fn(&Arc<SharedData>, ChannelId)>,
) -> Arc<SharedData> {
    let shared = run_bot_build_shared_data(
        DiscordBotSettings::default(),
        &ProviderKind::Claude,
        RuntimeServices {
            initial_skills: Vec::new(),
            token_hash: "registry-bootstrap".to_owned(),
            api_port: 9,
            health_registry: registry.clone(),
            pg_pool,
            engine: None,
        },
        ProcessLifecycleCounters {
            global_active: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            global_finalizing: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            shutdown_remaining: Arc::new(std::sync::atomic::AtomicUsize::new(1)),
        },
        UiFeatureFlags {
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
    );
    #[cfg(unix)]
    let mut shared = shared;
    #[cfg(unix)]
    if let Some(wake) = owner_wake {
        Arc::get_mut(&mut shared).unwrap().router_ports.owner_wake = wake;
    }
    shared
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RestoreRegistry(Option<Arc<health::HealthRegistry>>);

    impl Drop for RestoreRegistry {
        fn drop(&mut self) {
            let restored = self
                .0
                .take()
                .unwrap_or_else(|| Arc::new(health::HealthRegistry::new()));
            crate::services::discord::health::runtime_registry::publish_runtime_registry(&restored);
            runtime_store::set_process_generation_for_tests(None);
        }
    }

    #[test]
    fn shared_data_publishes_runtime_registry_without_policy_engine() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let _restore =
            RestoreRegistry(crate::services::discord::health::runtime_registry::runtime_registry());
        let registry = Arc::new(health::HealthRegistry::new());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _entered = runtime.enter();
        let allocations = EngineRuntime::record_allocations_for_tests();
        let shared = shared_data_without_policy_for_tests(
            &registry,
            None,
            #[cfg(unix)]
            None,
        );
        assert_original_engine_allocation(allocations, &shared);
        assert!(shared.policy.engine.is_none());
        let published = crate::services::discord::health::runtime_registry::runtime_registry()
            .expect("runtime registry must be published without a policy engine");
        assert!(Arc::ptr_eq(&published, &registry));
        let shared_registry = health::registry_for(&shared)
            .expect("caller retains its registry while the runtime lives");
        assert!(Arc::ptr_eq(&shared_registry, &registry));
        use crate::services::engine_runtime::health_port::{
            RuntimeRegistryPort, published_runtime_registry,
        };
        let port = published_runtime_registry().expect("KE must see the published runtime");
        let expected: Arc<dyn RuntimeRegistryPort> = registry.clone();
        assert!(Arc::ptr_eq(&port, &expected));

        let replacement = Arc::new(health::HealthRegistry::new());
        crate::services::discord::health::runtime_registry::publish_runtime_registry(&replacement);
        let replacement_port: Arc<dyn RuntimeRegistryPort> = replacement.clone();
        assert!(Arc::ptr_eq(
            &published_runtime_registry().unwrap(),
            &replacement_port,
        ));
        drop(replacement_port);
        drop(replacement);
        assert!(published_runtime_registry().is_none());
    }

    fn assert_original_engine_allocation(
        allocations: crate::services::engine_runtime::AllocationRecorder,
        shared: &Arc<SharedData>,
    ) {
        let recorded = allocations.finish();
        assert_eq!(recorded.len(), 1, "one runtime allocation per constructor");
        assert!(std::ptr::eq(
            recorded[0].as_ptr(),
            Arc::as_ptr(&shared.engine)
        ));
    }

    fn assert_engine_runtime_identity(shared: &Arc<SharedData>) {
        use crate::services::engine_runtime::EngineRuntime;
        let through_shared: &EngineRuntime = shared.as_ref();
        assert!(std::ptr::eq(through_shared, shared.engine.as_ref()));
        let retained = Arc::clone(&shared.engine);
        assert!(Arc::ptr_eq(&retained, &shared.engine));
        assert!(std::ptr::eq(
            &shared.session_transition_locks,
            &retained.session_transition_locks
        ));
        assert!(std::ptr::eq(&shared.skills_cache, &retained.skills_cache));
        assert!(std::ptr::eq(&shared.last_turn_at, &retained.last_turn_at));
        assert!(std::ptr::eq(
            &shared.turn_start_times,
            &retained.turn_start_times
        ));
        assert!(std::ptr::eq(&shared.provider, &retained.provider));
        assert!(std::ptr::eq(&shared.api_port, &retained.api_port));
        assert!(std::ptr::eq(&shared.pg_pool, &retained.pg_pool));
        assert!(std::ptr::eq(&shared.policy, &retained.policy));
        assert!(std::ptr::eq(&shared.core, &retained.core));
        assert!(std::ptr::eq(&shared.mailboxes, &retained.mailboxes));
        let registry: &crate::services::engine_runtime::relay_state::RegistryState =
            &shared.tmux_watchers;
        assert!(std::ptr::eq(registry, retained.watcher_registry.as_ref()));
        assert_eq!(Arc::strong_count(&retained), 2);
    }

    #[tokio::test]
    async fn engine_runtime_fixture_keeps_one_object_and_none_pool() {
        let allocations = EngineRuntime::record_allocations_for_tests();
        let shared = make_shared_data_for_tests_with_storage(None);
        assert_original_engine_allocation(allocations, &shared);
        assert_engine_runtime_identity(&shared);
        assert_eq!(shared.provider, ProviderKind::Claude);
        assert_eq!(shared.api_port, 9);
        assert!(shared.engine.pg_pool.is_none());
        assert!(shared.engine.policy.engine.is_none());
        let channel = ChannelId::new(8192);
        let lock = shared.session_transition_locks.lock(channel);
        assert!(Arc::ptr_eq(
            &lock,
            &shared.engine.session_transition_locks.lock(channel)
        ));
        shared
            .skills_cache
            .write()
            .await
            .push(("name".into(), "description".into()));
        assert_eq!(shared.engine.skills_cache.read().await[0].0, "name");
        *shared.engine.last_turn_at.lock().unwrap() = Some("completed".into());
        assert_eq!(
            shared.last_turn_at.lock().unwrap().as_deref(),
            Some("completed")
        );
        let started = std::time::Instant::now();
        shared.turn_start_times.insert(channel, started);
        assert_eq!(
            *shared.engine.turn_start_times.get(&channel).unwrap(),
            started
        );
        let other_owner = Arc::clone(&shared);
        assert!(Arc::ptr_eq(&shared.engine, &other_owner.engine));
        let retained = Arc::clone(&shared.engine);
        drop(shared);
        drop(other_owner);
        assert_eq!(Arc::strong_count(&retained), 1);
        assert_eq!(retained.skills_cache.read().await.len(), 1);
    }

    #[tokio::test]
    async fn engine_runtime_fixture_customization_keeps_the_unique_arc() {
        let mut shared = make_shared_data_for_tests_with_storage(None);
        let original = Arc::as_ptr(&shared.engine);
        let unique = Arc::get_mut(&mut shared).unwrap();
        unique.provider = ProviderKind::Codex;
        unique.api_port = 8193;
        assert_eq!(shared.engine.provider, ProviderKind::Codex);
        assert_eq!(shared.engine.api_port, 8193);
        assert_eq!(original, Arc::as_ptr(&shared.engine));
        let retained = Arc::clone(&shared.engine);
        let mutation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Arc::get_mut(&mut shared).unwrap().api_port = 8194;
        }));
        assert!(
            mutation.is_err(),
            "shared runtime ownership must never clone on mutation"
        );
        assert_eq!(retained.api_port, 8193);
        assert_eq!(original, Arc::as_ptr(&shared.engine));
    }

    #[tokio::test]
    async fn engine_runtime_both_constructors_preserve_pool_and_policy_pg() {
        let root = crate::config::TestRuntimeRootGuard::new();
        let restore =
            RestoreRegistry(crate::services::discord::health::runtime_registry::runtime_registry());
        let Some(base) = crate::db::postgres::postgres_test_database_url_base() else {
            eprintln!("SKIP: PostgreSQL fixture is not configured");
            return;
        };
        let database =
            std::env::var("POSTGRES_TEST_ADMIN_DB").unwrap_or_else(|_| "postgres".into());
        let pool = crate::db::postgres::connect_test_pool_with_max_connections(
            &format!("{base}/{database}"),
            "engine runtime ownership",
            1,
        )
        .await
        .unwrap();
        let original_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&pool)
            .await
            .unwrap();
        let original_size = pool.size();
        for configured in [None, Some(pool.clone())] {
            let policy = configured.as_ref().map(|_| {
                let mut config = crate::config::Config::default();
                config.policies = crate::config::PoliciesConfig {
                    dir: root.path().join("empty-policies"),
                    hot_reload: false,
                    ..crate::config::PoliciesConfig::default()
                };
                crate::engine::PolicyEngine::new(&config).unwrap()
            });
            let witness = policy.as_ref().map(crate::engine::PolicyEngine::downgrade);
            if let Some(policy) = &policy {
                policy
                    .eval_js::<()>("globalThis.ownershipTrace = ['before-bootstrap']; undefined")
                    .unwrap();
            }
            let registry = Arc::new(health::HealthRegistry::new());
            let initial_skills = vec![("retained".into(), "prompt skill".into())];
            let allocations = EngineRuntime::record_allocations_for_tests();
            let shared = run_bot_build_shared_data(
                DiscordBotSettings::default(),
                &ProviderKind::Codex,
                RuntimeServices {
                    initial_skills: initial_skills.clone(),
                    token_hash: "engine-ownership".into(),
                    api_port: 8192,
                    health_registry: registry.clone(),
                    pg_pool: configured.clone(),
                    engine: policy,
                },
                ProcessLifecycleCounters {
                    global_active: Arc::new(0.into()),
                    global_finalizing: Arc::new(0.into()),
                    shutdown_remaining: Arc::new(1.into()),
                },
                UiFeatureFlags {
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
            );
            assert_original_engine_allocation(allocations, &shared);
            let allocations = EngineRuntime::record_allocations_for_tests();
            let fixture = make_shared_data_for_tests_with_storage(configured.clone());
            assert_original_engine_allocation(allocations, &fixture);
            assert_engine_runtime_identity(&shared);
            assert_engine_runtime_identity(&fixture);
            assert!(shared.engine.core.lock().await.sessions.is_empty());
            assert!(fixture.engine.core.lock().await.sessions.is_empty());
            assert!(shared.engine.mailboxes.snapshot_all().await.is_empty());
            assert!(fixture.engine.mailboxes.snapshot_all().await.is_empty());
            assert!(!Arc::ptr_eq(&shared.engine, &fixture.engine));
            assert_eq!(shared.engine.provider, ProviderKind::Codex);
            assert_eq!(shared.engine.api_port, 8192);
            assert_eq!(*shared.engine.skills_cache.read().await, initial_skills);
            assert_eq!(shared.pg_pool.is_some(), configured.is_some());
            assert_eq!(fixture.engine.pg_pool.is_some(), configured.is_some());
            if configured.is_some() {
                for stored in [
                    &shared.pg_pool,
                    &shared.engine.pg_pool,
                    &fixture.pg_pool,
                    &fixture.engine.pg_pool,
                ] {
                    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                        .fetch_one(stored.as_ref().unwrap())
                        .await
                        .unwrap();
                    assert_eq!(pid, original_pid);
                }
                shared
                    .policy
                    .engine
                    .as_ref()
                    .unwrap()
                    .eval_js::<()>("ownershipTrace.push('after-bootstrap'); undefined")
                    .unwrap();
                let trace: String = witness
                    .as_ref()
                    .unwrap()
                    .upgrade()
                    .unwrap()
                    .eval_js("ownershipTrace.join(',')")
                    .unwrap();
                assert_eq!(trace, "before-bootstrap,after-bootstrap");
            } else {
                assert!(shared.engine.policy.engine.is_none());
            }
            assert_eq!(pool.size(), original_size);
        }
        pool.close().await;
        drop(restore);
        drop(root);
    }

    #[tokio::test]
    async fn core_and_mailboxes_keep_the_same_lock_values_and_actor() {
        let root = crate::config::TestRuntimeRootGuard::new();
        let restore =
            RestoreRegistry(crate::services::discord::health::runtime_registry::runtime_registry());
        let shared = make_shared_data_for_tests_with_storage(None);
        assert_engine_runtime_identity(&shared);
        assert!(shared.engine.core.lock().await.sessions.is_empty());
        assert!(shared.engine.mailboxes.snapshot_all().await.is_empty());
        let channel = ChannelId::new(8_192_101);
        let uploads = vec![
            crate::services::cluster::attachment_transfer::uploads::Upload::Local(
                "retained-upload".to_owned(),
            ),
        ];
        let upload_storage = uploads.as_ptr();
        {
            let mut core = shared.core.lock().await;
            assert!(shared.engine.core.try_lock().is_err());
            core.sessions.insert(
                channel,
                DiscordSession {
                    session_id: Some("retained-session".to_owned()),
                    memento_context_loaded: true,
                    memento_reflected: false,
                    current_path: None,
                    history: Vec::new(),
                    pending_uploads: uploads,
                    cleared: false,
                    channel_id: Some(channel.get()),
                    channel_name: None,
                    category_name: None,
                    last_active: tokio::time::Instant::now(),
                    worktree: None,
                    born_generation: 81,
                },
            );
        }
        let through_shared = shared.mailboxes.handle(channel);
        let through_engine = shared.engine.mailboxes.handle(channel);
        assert!(through_shared.same_actor(&through_engine));
        let token = Arc::new(crate::services::provider::CancelToken::new());
        assert!(
            through_shared
                .try_start_turn(token.clone(), UserId::new(8192), MessageId::new(8192101),)
                .await
        );
        let observed = through_engine.snapshot().await;
        assert!(Arc::ptr_eq(observed.cancel_token.as_ref().unwrap(), &token));
        let retained = shared.engine.clone();
        drop(shared);
        let core = retained.core.lock().await;
        let session = core.sessions.get(&channel).unwrap();
        let canonical: &crate::services::engine_runtime::session::EngineSession = session;
        assert_eq!(canonical.session_id.as_deref(), Some("retained-session"));
        assert_eq!(canonical.pending_uploads.as_ptr(), upload_storage);
        assert_eq!(
            canonical.pending_uploads[0],
            crate::services::cluster::attachment_transfer::uploads::Upload::from("retained-upload")
        );
        assert_eq!(canonical.born_generation, 81);
        drop(core);
        assert!(through_engine.hard_stop().await.removed_token.is_some());
        drop(restore);
        drop(root);
    }

    #[tokio::test]
    async fn mailboxes_keep_one_registry_and_the_live_actor() {
        let root = crate::config::TestRuntimeRootGuard::new();
        let restore =
            RestoreRegistry(crate::services::discord::health::runtime_registry::runtime_registry());
        let shared = make_shared_data_for_tests_with_storage(None);
        assert!(std::ptr::eq(&shared.mailboxes, &shared.engine.mailboxes));
        assert!(shared.engine.mailboxes.snapshot_all().await.is_empty());
        let channel = ChannelId::new(8_192_102);
        let original = shared.mailboxes.handle(channel);
        let engine_view = shared.engine.mailboxes.handle(channel);
        assert!(original.same_actor(&engine_view));
        let token = Arc::new(crate::services::provider::CancelToken::new());
        assert!(
            original
                .try_start_turn(token.clone(), UserId::new(8192), MessageId::new(8192102),)
                .await
        );
        let snapshots = shared.engine.mailboxes.snapshot_all().await;
        assert_eq!(snapshots.len(), 1);
        let snapshot = snapshots.get(&channel).unwrap();
        assert!(Arc::ptr_eq(snapshot.cancel_token.as_ref().unwrap(), &token));
        assert_eq!(
            snapshot.active_user_message_id,
            Some(MessageId::new(8192102))
        );
        assert!(engine_view.hard_stop().await.removed_token.is_some());
        drop(restore);
        drop(root);
    }
    #[tokio::test]
    async fn watcher_registry_and_lease_keep_one_engine_identity() {
        let root = crate::config::TestRuntimeRootGuard::new();
        let restore =
            RestoreRegistry(crate::services::discord::health::runtime_registry::runtime_registry());
        let shared = make_shared_data_for_tests_with_storage(None);
        assert_engine_runtime_identity(&shared);
        let registry: &crate::services::engine_runtime::relay_state::RegistryState =
            &shared.tmux_watchers;
        assert!(std::ptr::eq(
            registry,
            shared.engine.watcher_registry.as_ref()
        ));
        assert_eq!(registry.len(), 0);
        let channel = ChannelId::new(8_192_301);
        let lease = shared.delivery_lease(channel);
        let canonical: &crate::services::engine_runtime::relay_state::lease::DeliveryLeaseCell =
            &lease;
        assert!(std::ptr::eq(
            canonical,
            shared.delivery_lease(channel).as_ref()
        ));
        let key = DeliveryLeaseKey::new(channel, 7, 31, None, None);
        let holder = LeaseHolder::Watcher { instance_id: 31 };
        assert!(lease.try_acquire(key.clone(), holder, 10, 20, lease_now_ms() + 1000));
        assert!(!lease.try_acquire(
            key.clone(),
            LeaseHolder::Sink,
            10,
            20,
            lease_now_ms() + 1000
        ));
        assert!(lease.release(holder, key, 10, 20));
        let retained = shared.engine.clone();
        drop(shared);
        assert_eq!(retained.watcher_registry.len(), 0);
        assert_eq!(canonical.channel_id(), channel);
        drop(restore);
        drop(root);
    }
}
