//! Worker construction, startup staging, and supervision loops.

use super::*;

impl SupervisedWorkerRegistry {
    pub(crate) fn new(
        config: Config,
        engine: PolicyEngine,
        health_registry: Option<Arc<HealthRegistry>>,
        pg_pool: Option<Arc<PgPool>>,
        cluster_runtime: ClusterRuntime,
    ) -> Self {
        Self {
            config,
            engine,
            health_registry,
            pg_pool,
            cluster_runtime,
            shutdown: Arc::new(AtomicBool::new(false)),
            running: Vec::new(),
        }
    }

    pub(crate) async fn run_boot_only_steps(&self) -> Result<()> {
        for step in BOOT_ONLY_STEPS {
            tracing::info!(
                boot_step = step.name,
                order = step.order,
                responsibility = step.responsibility,
                "running boot-only server step"
            );
            match step.id {
                BootStepId::RefreshMemoryHealth => {
                    super::super::refresh_memory_health_for_startup().await;
                }
                BootStepId::DrainStartupHooks => {
                    self.engine.drain_startup_hooks();
                }
            }
        }
        Ok(())
    }

    pub(crate) fn start_after_boot_reconcile(&mut self) -> Result<()> {
        self.start_stage(WorkerStartStage::AfterBootReconcile, None)
            .map(|_| ())
    }

    pub(crate) fn start_after_websocket_broadcast(
        &mut self,
        broadcast_tx: BroadcastTx,
    ) -> Result<BatchBuffer> {
        self.start_stage(
            WorkerStartStage::AfterWebsocketBroadcast,
            Some(broadcast_tx),
        )?
        .ok_or_else(|| anyhow!("missing websocket batch flusher registration"))
    }

    pub(super) fn start_stage(
        &mut self,
        stage: WorkerStartStage,
        broadcast_tx: Option<BroadcastTx>,
    ) -> Result<Option<BatchBuffer>> {
        let mut batch_buffer = None;
        for spec in WORKER_SPECS {
            if spec.start_stage != stage || self.is_started(spec.id) {
                continue;
            }
            self.log_start(spec);
            batch_buffer = self
                .start_worker(spec, broadcast_tx.clone())?
                .or(batch_buffer);
        }
        tracing::info!(
            stage = stage.as_doc_str(),
            started = self
                .running
                .iter()
                .filter(|worker| worker.spec.start_stage == stage)
                .count(),
            "supervised worker stage complete"
        );
        Ok(batch_buffer)
    }

    pub(super) fn start_worker(
        &mut self,
        spec: WorkerSpec,
        broadcast_tx: Option<BroadcastTx>,
    ) -> Result<Option<BatchBuffer>> {
        match spec.id {
            ServerWorkerId::GithubSync => {
                let sync_interval = self.config.github.sync_interval_minutes;
                if sync_interval <= 0 {
                    self.log_skip(spec, "github.sync_interval_minutes <= 0");
                    return Ok(None);
                }
                let Some(sync_pg_pool) = self.pg_pool.clone() else {
                    self.log_skip(spec, "postgres pool unavailable");
                    return Ok(None);
                };
                self.register_leader_tokio(spec, move || {
                    let sync_pg_pool = sync_pg_pool.clone();
                    async move {
                        super::super::github_sync_loop(sync_pg_pool, sync_interval).await;
                    }
                });
                Ok(None)
            }
            ServerWorkerId::PolicyTick => {
                let Some(tick_pg_pool) = self.pg_pool.clone() else {
                    self.log_skip(spec, "postgres pool unavailable");
                    return Ok(None);
                };
                let tick_config = self.config.clone();
                let tick_cluster_runtime = self.cluster_runtime.clone();
                let shutdown = self.shutdown.clone();
                // #5142 D-4: hand the tick loop the health registry so the
                // auto-queue cleanup replay can complete its runtime-side
                // teardown instead of silently skipping it. Starting the tick
                // with `None` while the process has a registry is silent: the
                // DB state still converges and only the in-memory provider
                // runtime is left behind. `policy_tick_captured_registry` records what the
                // spawned thread actually captured so that is not a matter of
                // reading this line.
                let tick_health_registry = self.health_registry.clone();
                self.register_thread(spec, "policy-tick", move || {
                    #[cfg(test)]
                    record_policy_tick_captured_registry(
                        tick_health_registry
                            .as_ref()
                            .map(|registry| Arc::as_ptr(registry) as usize),
                    );
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap_or_else(|e| {
                            tracing::warn!("Fatal: failed to create policy-tick runtime: {e}");
                            std::process::exit(1);
                        });
                    loop {
                        if !rt.block_on(wait_until_leader_or_shutdown(
                            &tick_cluster_runtime,
                            shutdown.clone(),
                        )) {
                            break;
                        }
                        // #747: build a dedicated tick engine so a stuck tick hook
                        // cannot back up the main engine's actor queue and starve
                        // HTTP/Discord hook paths. Recreate it per leader epoch
                        // because `policy_tick_loop` owns and consumes the engine.
                        let _epoch = LeaderOnlyWorkerEpoch::start(spec);
                        match PolicyEngine::new_for_tick(
                            &tick_config,
                            Some(tick_pg_pool.as_ref().clone()),
                        ) {
                            Ok(tick_engine) => {
                                rt.block_on(super::super::policy_tick_loop(
                                    tick_engine,
                                    Some(tick_pg_pool.clone()),
                                    Some(tick_cluster_runtime.clone()),
                                    Some(shutdown.clone()),
                                    tick_health_registry.clone(),
                                ));
                            }
                            Err(error) => {
                                tracing::warn!(
                                    "failed to initialize dedicated policy tick engine: {error}"
                                );
                            }
                        }
                        drop(_epoch);
                        if shutdown.load(Ordering::Acquire) {
                            break;
                        }
                        // Build the Sleep future inside `block_on` so the Tokio
                        // reactor handle is in scope. Constructing it outside
                        // panics with "there is no reactor running".
                        rt.block_on(async {
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        });
                    }
                })?;
                Ok(None)
            }
            ServerWorkerId::RateLimitSync => {
                let Some(rate_limit_pg_pool) = self.pg_pool.clone() else {
                    self.log_skip(spec, "postgres pool unavailable");
                    return Ok(None);
                };
                self.register_leader_tokio(spec, move || {
                    let rate_limit_pg_pool = rate_limit_pg_pool.clone();
                    async move {
                        super::super::rate_limit_sync_loop(rate_limit_pg_pool).await;
                    }
                });
                Ok(None)
            }
            ServerWorkerId::MaintenanceScheduler => {
                let Some(maintenance_pg_pool) = self.pg_pool.clone() else {
                    self.log_skip(spec, "postgres pool unavailable");
                    return Ok(None);
                };
                let prompt_manifest_retention = self.config.prompt_manifest_retention.clone();
                // #3909 — resolve the voice TTS cache/temp sweep dirs from the
                // loaded runtime VoiceConfig (the same source of truth the TTS
                // write path uses) so operator overrides of
                // `voice.tts.progress_cache_dir` / `voice.audio.temp_dir` are
                // swept, not the defaults.
                let voice_cache_sweep =
                    crate::services::maintenance::jobs::voice_cache_sweep::Config::from_voice_config(
                        &self.config.voice,
                    );
                self.register_leader_tokio(spec, move || {
                    let maintenance_pg_pool = maintenance_pg_pool.clone();
                    let prompt_manifest_retention = prompt_manifest_retention.clone();
                    let voice_cache_sweep = voice_cache_sweep.clone();
                    async move {
                        super::super::maintenance::scheduler_loop(
                            maintenance_pg_pool,
                            prompt_manifest_retention,
                            voice_cache_sweep,
                        )
                        .await;
                    }
                });
                Ok(None)
            }
            ServerWorkerId::MessageOutbox => {
                let Some(outbox_pg_pool) = self.pg_pool.clone() else {
                    self.log_skip(spec, "postgres pool unavailable");
                    return Ok(None);
                };
                let outbox_health_registry = self.health_registry.clone();
                self.register_leader_tokio(spec, move || {
                    let outbox_pg_pool = outbox_pg_pool.clone();
                    let outbox_health_registry = outbox_health_registry.clone();
                    async move {
                        super::super::message_outbox_loop(outbox_pg_pool, outbox_health_registry)
                            .await;
                    }
                });
                Ok(None)
            }
            ServerWorkerId::ScheduledMessages => {
                let Some(smsg_pg_pool) = self.pg_pool.clone() else {
                    self.log_skip(spec, "postgres pool unavailable");
                    return Ok(None);
                };
                let smsg_health_registry = self.health_registry.clone();
                self.register_leader_tokio(spec, move || {
                    let smsg_pg_pool = smsg_pg_pool.clone();
                    let smsg_health_registry = smsg_health_registry.clone();
                    async move {
                        crate::services::scheduled_messages::scheduled_message_loop(
                            smsg_pg_pool,
                            smsg_health_registry,
                        )
                        .await;
                    }
                });
                Ok(None)
            }
            ServerWorkerId::DispatchOutbox => {
                let Some(dispatch_outbox_pg_pool) = self.pg_pool.clone() else {
                    self.log_skip(spec, "postgres pool unavailable");
                    return Ok(None);
                };
                let claim_owner = self.cluster_runtime.instance_id().to_string();
                let cluster_runtime = self.cluster_runtime.clone();
                let cluster_config = self.config.cluster.clone();
                self.register_tokio(spec, move || {
                    let dispatch_outbox_pg_pool = dispatch_outbox_pg_pool.clone();
                    let claim_owner = claim_owner.clone();
                    let cluster_runtime = cluster_runtime.clone();
                    let cluster_config = cluster_config.clone();
                    async move {
                        super::super::routes::dispatches::dispatch_outbox_loop(
                            dispatch_outbox_pg_pool,
                            claim_owner,
                            cluster_runtime,
                            cluster_config,
                        )
                        .await;
                    }
                });
                Ok(None)
            }
            ServerWorkerId::DmReplyRetry => {
                let Some(dm_retry_pg_pool) = self.pg_pool.clone() else {
                    self.log_skip(spec, "postgres pool unavailable");
                    return Ok(None);
                };
                self.register_leader_tokio(spec, move || {
                    let dm_retry_pg_pool = dm_retry_pg_pool.clone();
                    async move {
                        super::super::dm_reply_retry_loop(dm_retry_pg_pool).await;
                    }
                });
                Ok(None)
            }
            ServerWorkerId::WsBatchFlusher => {
                let tx = broadcast_tx.ok_or_else(|| {
                    anyhow!(
                        "worker {} requires a websocket broadcast sender before startup",
                        spec.name
                    )
                })?;
                let buffer = super::super::ws::spawn_batch_flusher(tx);
                self.running.push(RunningWorker {
                    spec,
                    _handle: WorkerHandle::SpawnHelper,
                });
                Ok(Some(buffer))
            }
            ServerWorkerId::SessionDiscovery => {
                let Some(discovery_pg_pool) = self.pg_pool.clone() else {
                    self.log_skip(spec, "postgres pool unavailable");
                    return Ok(None);
                };
                let instance_id = Some(self.cluster_runtime.instance_id().to_string());
                let shutdown = self.shutdown.clone();
                // Worker-local (not register_leader_tokio): tmux is host-scoped,
                // so every node must enumerate its own sessions. The registry's
                // reconcile_for_node is instance_id-scoped to keep peers from
                // stomping each other's entries.
                self.register_tokio(spec, move || {
                    let instance_id = instance_id.clone();
                    let discovery_pg_pool = discovery_pg_pool.clone();
                    let shutdown = shutdown.clone();
                    async move {
                        crate::services::cluster::session_discovery::run_discovery_loop(
                            instance_id,
                            discovery_pg_pool,
                            crate::services::cluster::session_discovery::DiscoveryConfig::default(),
                            shutdown,
                        )
                        .await;
                    }
                });
                Ok(None)
            }
            ServerWorkerId::WatcherSupervisor => {
                #[cfg(not(unix))]
                {
                    self.log_skip(spec, "session-bound relay supervisor requires Unix tmux");
                    return Ok(None);
                }

                #[cfg(unix)]
                {
                    if !self.config.cluster.session_bound_relay_enabled {
                        self.log_skip(spec, "cluster.session_bound_relay_enabled=false");
                        return Ok(None);
                    }
                    let shutdown = self.shutdown.clone();
                    // Worker-local: tmux is host-scoped, so every node supervises
                    // its own relays. No leader gating — peer hosts can't observe
                    // each other's sessions anyway.
                    let health_registry = self.health_registry.clone();
                    self.register_tokio(spec, move || {
                        let health_registry = health_registry.clone();
                        let shutdown = shutdown.clone();
                        async move {
                            crate::services::discord::run_session_bound_discord_relay_supervisor(
                                health_registry,
                                shutdown,
                            )
                            .await;
                        }
                    });
                    Ok(None)
                }
            }
            ServerWorkerId::RoutineRuntime => {
                if !self.config.routines.enabled {
                    self.log_skip(spec, "routines.enabled=false");
                    return Ok(None);
                }
                let tick_secs = match validate_routine_runtime_config(&self.config.routines) {
                    Ok(value) => value,
                    Err(error) => {
                        self.log_skip(spec, error.message());
                        return Ok(None);
                    }
                };
                let Some(routine_pg_pool) = self.pg_pool.clone() else {
                    self.log_skip(
                        spec,
                        "postgres pool unavailable; routines require postgresql",
                    );
                    return Ok(None);
                };
                let routines_config = self.config.routines.clone();
                let routine_health_target = self
                    .config
                    .kanban
                    .human_alert_channel_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(|value| format!("channel:{value}"));
                let routine_health_registry = self.health_registry.clone();
                self.register_leader_tokio(spec, move || {
                    let routine_pg_pool = routine_pg_pool.clone();
                    let routine_health_registry = routine_health_registry.clone();
                    let routines_config = routines_config.clone();
                    let routine_health_target = routine_health_target.clone();
                    async move {
                        super::super::routine_runtime_loop(
                            routine_pg_pool,
                            routine_health_registry,
                            routines_config,
                            routine_health_target,
                            tick_secs,
                        )
                        .await;
                    }
                });
                Ok(None)
            }
        }
    }

    pub(super) fn register_tokio<MakeFuture, Fut>(
        &mut self,
        spec: WorkerSpec,
        make_future: MakeFuture,
    ) where
        MakeFuture: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let future = super::super::worker_recovery::supervise_worker_local(
            spec,
            self.shutdown.clone(),
            make_future,
            move |reason, expected_shutdown, auto_restart, restart_attempt| {
                record_worker_local_terminal_signal(
                    spec,
                    reason,
                    expected_shutdown,
                    auto_restart,
                    restart_attempt,
                );
            },
            // #4515 PR3: restart-budget exhaustion completes the recovery circuit
            // — readiness down (via the Exhausted recovery state) plus process
            // exit so launchd KeepAlive restarts a clean process.
            super::super::worker_recovery::production_fatal_hook(self.shutdown.clone()),
        );
        self.running.push(RunningWorker {
            spec,
            _handle: WorkerHandle::Tokio {
                _handle: tokio::spawn(future),
            },
        });
    }

    pub(super) fn register_leader_tokio<MakeFuture, Fut>(
        &mut self,
        spec: WorkerSpec,
        make_future: MakeFuture,
    ) where
        MakeFuture: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let future = Self::supervise_leader_tokio_worker(
            spec,
            self.cluster_runtime.clone(),
            self.shutdown.clone(),
            make_future,
        );
        self.running.push(RunningWorker {
            spec,
            _handle: WorkerHandle::Tokio {
                _handle: tokio::spawn(future),
            },
        });
    }

    pub(super) async fn supervise_leader_tokio_worker<MakeFuture, Fut>(
        spec: WorkerSpec,
        cluster_runtime: ClusterRuntime,
        shutdown: Arc<AtomicBool>,
        make_future: MakeFuture,
    ) where
        MakeFuture: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        loop {
            if !wait_until_leader_or_shutdown(&cluster_runtime, shutdown.clone()).await {
                break;
            }
            let _epoch = LeaderOnlyWorkerEpoch::start(spec);
            let future = make_future();
            tokio::pin!(future);
            tokio::select! {
                _ = &mut future => {
                    tracing::warn!(
                        worker = spec.name,
                        target = spec.target,
                        kind = spec.kind.as_doc_str(),
                        stage = spec.start_stage.as_doc_str(),
                        order = spec.start_order,
                        restart = spec.restart_policy.as_doc_str(),
                        shutdown = spec.shutdown_policy.as_doc_str(),
                        execution_scope = spec.execution_scope.as_doc_str(),
                        owner = spec.owner,
                        health = spec.health_owner,
                        responsibility = spec.responsibility,
                        notes = spec.notes,
                        "leader-only worker future exited"
                    );
                }
                _ = cluster_runtime.wait_until_not_leader() => {
                    tracing::warn!(
                        worker = spec.name,
                        target = spec.target,
                        kind = spec.kind.as_doc_str(),
                        stage = spec.start_stage.as_doc_str(),
                        order = spec.start_order,
                        restart = spec.restart_policy.as_doc_str(),
                        shutdown = spec.shutdown_policy.as_doc_str(),
                        execution_scope = spec.execution_scope.as_doc_str(),
                        owner = spec.owner,
                        health = spec.health_owner,
                        responsibility = spec.responsibility,
                        notes = spec.notes,
                        instance_id = cluster_runtime.instance_id(),
                        "leader-only worker self-fenced after cluster leadership was lost"
                    );
                }
                _ = wait_until_shutdown(shutdown.clone()) => {
                    tracing::info!(
                        worker = spec.name,
                        target = spec.target,
                        kind = spec.kind.as_doc_str(),
                        stage = spec.start_stage.as_doc_str(),
                        order = spec.start_order,
                        restart = spec.restart_policy.as_doc_str(),
                        shutdown = spec.shutdown_policy.as_doc_str(),
                        execution_scope = spec.execution_scope.as_doc_str(),
                        owner = spec.owner,
                        health = spec.health_owner,
                        responsibility = spec.responsibility,
                        notes = spec.notes,
                        "leader-only worker supervisor shutting down"
                    );
                    break;
                }
            }
            drop(_epoch);
            if shutdown.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    }

    pub(super) fn register_thread<F>(&mut self, spec: WorkerSpec, name: &str, body: F) -> Result<()>
    where
        F: FnOnce() + Send + 'static,
    {
        let handle = std::thread::Builder::new()
            .name(name.to_string())
            .spawn(body)
            .map_err(|e| anyhow!("Failed to spawn {} thread: {e}", spec.name))?;
        self.running.push(RunningWorker {
            spec,
            _handle: WorkerHandle::Thread { _handle: handle },
        });
        Ok(())
    }

    pub(super) fn is_started(&self, id: ServerWorkerId) -> bool {
        self.running.iter().any(|worker| worker.spec.id == id)
    }

    pub(super) fn log_start(&self, spec: WorkerSpec) {
        tracing::info!(
            worker = spec.name,
            target = spec.target,
            kind = spec.kind.as_doc_str(),
            stage = spec.start_stage.as_doc_str(),
            order = spec.start_order,
            restart = spec.restart_policy.as_doc_str(),
            shutdown = spec.shutdown_policy.as_doc_str(),
            execution_scope = spec.execution_scope.as_doc_str(),
            owner = spec.owner,
            health = spec.health_owner,
            responsibility = spec.responsibility,
            notes = spec.notes,
            "starting supervised worker"
        );
    }

    pub(super) fn log_skip(&self, spec: WorkerSpec, reason: &str) {
        tracing::info!(
            worker = spec.name,
            target = spec.target,
            kind = spec.kind.as_doc_str(),
            stage = spec.start_stage.as_doc_str(),
            order = spec.start_order,
            restart = spec.restart_policy.as_doc_str(),
            shutdown = spec.shutdown_policy.as_doc_str(),
            execution_scope = spec.execution_scope.as_doc_str(),
            owner = spec.owner,
            health = spec.health_owner,
            responsibility = spec.responsibility,
            notes = spec.notes,
            reason,
            "skipping supervised worker"
        );
    }
}
