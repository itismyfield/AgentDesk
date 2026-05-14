use std::future::Future;

use anyhow::{Result, anyhow};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};

use crate::config::Config;
use crate::engine::PolicyEngine;
use crate::services::discord::health::HealthRegistry;
use crate::services::routines::validate_routine_runtime_config;
use sqlx::PgPool;

use super::cluster::ClusterRuntime;
use super::ws::{BatchBuffer, BroadcastTx};

static LEADER_ONLY_WORKERS_STARTED: AtomicBool = AtomicBool::new(false);
static LEADER_ONLY_WORKER_ACTIVE_COUNT: AtomicUsize = AtomicUsize::new(0);
static LEADER_ONLY_WORKER_LAST_SPAWN_UNIX_MS: AtomicI64 = AtomicI64::new(0);

pub(crate) fn leader_only_worker_status_json() -> serde_json::Value {
    let last_spawn_unix_ms = LEADER_ONLY_WORKER_LAST_SPAWN_UNIX_MS.load(Ordering::Acquire);
    serde_json::json!({
        "leader_only_workers_started": LEADER_ONLY_WORKERS_STARTED.load(Ordering::Acquire),
        "leader_only_workers_active_count": LEADER_ONLY_WORKER_ACTIVE_COUNT.load(Ordering::Acquire),
        "last_leader_only_worker_spawn_at": if last_spawn_unix_ms > 0 {
            chrono::DateTime::<chrono::Utc>::from_timestamp_millis(last_spawn_unix_ms)
        } else {
            None
        },
    })
}

fn record_leader_only_worker_started(spec: WorkerSpec) {
    LEADER_ONLY_WORKERS_STARTED.store(true, Ordering::Release);
    LEADER_ONLY_WORKER_ACTIVE_COUNT.fetch_add(1, Ordering::AcqRel);
    LEADER_ONLY_WORKER_LAST_SPAWN_UNIX_MS
        .store(chrono::Utc::now().timestamp_millis(), Ordering::Release);
    tracing::info!(
        worker = spec.name,
        execution_scope = spec.execution_scope.as_doc_str(),
        "leader-only worker epoch started"
    );
}

fn record_leader_only_worker_stopped(spec: WorkerSpec, reason: &str) {
    let _ = LEADER_ONLY_WORKER_ACTIVE_COUNT.fetch_update(
        Ordering::AcqRel,
        Ordering::Acquire,
        |count| Some(count.saturating_sub(1)),
    );
    tracing::warn!(
        worker = spec.name,
        reason,
        "leader-only worker epoch stopped"
    );
}

struct LeaderOnlyWorkerEpoch {
    spec: WorkerSpec,
}

impl LeaderOnlyWorkerEpoch {
    fn start(spec: WorkerSpec) -> Self {
        record_leader_only_worker_started(spec);
        Self { spec }
    }
}

impl Drop for LeaderOnlyWorkerEpoch {
    fn drop(&mut self) {
        record_leader_only_worker_stopped(self.spec, "leader worker epoch ended");
    }
}

async fn wait_until_shutdown(shutdown: Arc<AtomicBool>) {
    while !shutdown.load(Ordering::Acquire) {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

async fn wait_until_leader_or_shutdown(
    cluster_runtime: &ClusterRuntime,
    shutdown: Arc<AtomicBool>,
) -> bool {
    loop {
        if shutdown.load(Ordering::Acquire) {
            return false;
        }
        if cluster_runtime.is_leader() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BootStepId {
    RefreshMemoryHealth,
    DrainStartupHooks,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BootStepSpec {
    id: BootStepId,
    name: &'static str,
    responsibility: &'static str,
    order: u8,
}

const BOOT_ONLY_STEPS: [BootStepSpec; 2] = [
    BootStepSpec {
        id: BootStepId::RefreshMemoryHealth,
        name: "refresh_memory_health_for_startup",
        responsibility: "Prime runtime memory backend health before long-lived workers start",
        order: 10,
    },
    BootStepSpec {
        id: BootStepId::DrainStartupHooks,
        name: "drain_startup_hooks",
        responsibility: "Resume deferred startup hooks persisted before the previous shutdown",
        order: 20,
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerWorkerId {
    GithubSync,
    PolicyTick,
    RateLimitSync,
    MaintenanceScheduler,
    MessageOutbox,
    DispatchOutbox,
    DmReplyRetry,
    WsBatchFlusher,
    RoutineRuntime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerKind {
    TokioTask,
    DedicatedThread,
    SpawnHelper,
}

impl WorkerKind {
    pub(crate) const fn as_doc_str(self) -> &'static str {
        match self {
            Self::TokioTask => "tokio::spawn",
            Self::DedicatedThread => "std::thread::spawn",
            Self::SpawnHelper => "spawn helper",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerStartStage {
    AfterBootReconcile,
    AfterWebsocketBroadcast,
}

impl WorkerStartStage {
    pub(crate) const fn as_doc_str(self) -> &'static str {
        match self {
            Self::AfterBootReconcile => "after_boot_reconcile",
            Self::AfterWebsocketBroadcast => "after_websocket_broadcast",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerRestartPolicy {
    SkipWhenDisabled,
    LoopOwned,
    ManualProcessRestart,
}

impl WorkerRestartPolicy {
    pub(crate) const fn as_doc_str(self) -> &'static str {
        match self {
            Self::SkipWhenDisabled => "skip_when_disabled",
            Self::LoopOwned => "loop_owned",
            Self::ManualProcessRestart => "manual_process_restart",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerShutdownPolicy {
    RuntimeShutdown,
    ProcessExit,
}

impl WorkerShutdownPolicy {
    pub(crate) const fn as_doc_str(self) -> &'static str {
        match self {
            Self::RuntimeShutdown => "runtime_shutdown",
            Self::ProcessExit => "process_exit",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerExecutionScope {
    LeaderOnly,
    WorkerLocal,
}

impl WorkerExecutionScope {
    pub(crate) const fn as_doc_str(self) -> &'static str {
        match self {
            Self::LeaderOnly => "leader_only",
            Self::WorkerLocal => "worker_local",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WorkerSpec {
    id: ServerWorkerId,
    pub(crate) name: &'static str,
    pub(crate) kind: WorkerKind,
    pub(crate) target: &'static str,
    pub(crate) responsibility: &'static str,
    pub(crate) owner: &'static str,
    pub(crate) start_stage: WorkerStartStage,
    pub(crate) start_order: u8,
    pub(crate) restart_policy: WorkerRestartPolicy,
    pub(crate) shutdown_policy: WorkerShutdownPolicy,
    pub(crate) execution_scope: WorkerExecutionScope,
    pub(crate) health_owner: &'static str,
    pub(crate) notes: &'static str,
}

pub(crate) const WORKER_SPECS: [WorkerSpec; 9] = [
    WorkerSpec {
        id: ServerWorkerId::GithubSync,
        name: "github_sync_loop",
        kind: WorkerKind::TokioTask,
        target: "github_sync_loop",
        responsibility: "Periodically sync enabled GitHub repos into the local tracker",
        owner: "server::worker_registry",
        start_stage: WorkerStartStage::AfterBootReconcile,
        start_order: 10,
        restart_policy: WorkerRestartPolicy::SkipWhenDisabled,
        shutdown_policy: WorkerShutdownPolicy::RuntimeShutdown,
        execution_scope: WorkerExecutionScope::LeaderOnly,
        health_owner: "tracing logs and GitHub sync side effects",
        notes: "Skipped when github.sync_interval_minutes <= 0 or gh CLI is unavailable",
    },
    WorkerSpec {
        id: ServerWorkerId::PolicyTick,
        name: "policy-tick",
        kind: WorkerKind::DedicatedThread,
        target: "policy_tick_loop",
        responsibility: "Fire tiered policy hooks on a dedicated OS thread",
        owner: "server::worker_registry",
        start_stage: WorkerStartStage::AfterBootReconcile,
        start_order: 20,
        restart_policy: WorkerRestartPolicy::ManualProcessRestart,
        shutdown_policy: WorkerShutdownPolicy::ProcessExit,
        execution_scope: WorkerExecutionScope::LeaderOnly,
        health_owner: "kv_meta last_tick_* keys and memory health refresh",
        notes: "Uses a dedicated current-thread Tokio runtime to avoid engine lock deadlocks",
    },
    WorkerSpec {
        id: ServerWorkerId::RateLimitSync,
        name: "rate_limit_sync_loop",
        kind: WorkerKind::TokioTask,
        target: "rate_limit_sync_loop",
        responsibility: "Refresh cached provider rate-limit data for dashboard APIs",
        owner: "server::worker_registry",
        start_stage: WorkerStartStage::AfterBootReconcile,
        start_order: 30,
        restart_policy: WorkerRestartPolicy::LoopOwned,
        shutdown_policy: WorkerShutdownPolicy::RuntimeShutdown,
        execution_scope: WorkerExecutionScope::LeaderOnly,
        health_owner: "rate_limit_cache freshness and tracing logs",
        notes: "Runs immediately on startup and then every 120 seconds",
    },
    WorkerSpec {
        id: ServerWorkerId::MaintenanceScheduler,
        name: "maintenance_scheduler_loop",
        kind: WorkerKind::TokioTask,
        target: "maintenance::scheduler_loop",
        responsibility: "Run registered maintenance jobs on interval schedules",
        owner: "server::worker_registry",
        start_stage: WorkerStartStage::AfterBootReconcile,
        start_order: 35,
        restart_policy: WorkerRestartPolicy::LoopOwned,
        shutdown_policy: WorkerShutdownPolicy::RuntimeShutdown,
        execution_scope: WorkerExecutionScope::LeaderOnly,
        health_owner: "kv_meta maintenance_job:* keys and tracing logs",
        notes: "Static registry seeded with a noop heartbeat; first runs are staggered after startup",
    },
    WorkerSpec {
        id: ServerWorkerId::MessageOutbox,
        name: "message_outbox_loop",
        kind: WorkerKind::TokioTask,
        target: "message_outbox_loop",
        responsibility: "Drain queued message_outbox rows through the in-process Discord delivery path",
        owner: "server::worker_registry",
        start_stage: WorkerStartStage::AfterBootReconcile,
        start_order: 40,
        restart_policy: WorkerRestartPolicy::LoopOwned,
        shutdown_policy: WorkerShutdownPolicy::RuntimeShutdown,
        execution_scope: WorkerExecutionScope::LeaderOnly,
        health_owner: "message_outbox row state and delivery tracing",
        notes: "Waits three seconds for Discord runtime readiness before polling with adaptive backoff",
    },
    WorkerSpec {
        id: ServerWorkerId::DispatchOutbox,
        name: "dispatch_outbox_loop",
        kind: WorkerKind::TokioTask,
        target: "routes::dispatches::dispatch_outbox_loop",
        responsibility: "Deliver dispatch follow-ups and centralize Discord side effects",
        owner: "server::worker_registry",
        start_stage: WorkerStartStage::AfterBootReconcile,
        start_order: 50,
        restart_policy: WorkerRestartPolicy::LoopOwned,
        shutdown_policy: WorkerShutdownPolicy::RuntimeShutdown,
        execution_scope: WorkerExecutionScope::WorkerLocal,
        health_owner: "dispatch outbox tables and delivery tracing",
        notes: "Runs on each cluster node; PostgreSQL row claims and capability filters select the worker",
    },
    WorkerSpec {
        id: ServerWorkerId::RoutineRuntime,
        name: "routine-runtime",
        kind: WorkerKind::TokioTask,
        target: "routine_runtime_loop",
        responsibility: "Run scheduled JS routines independent of the policy-tick engine",
        owner: "server::worker_registry",
        start_stage: WorkerStartStage::AfterBootReconcile,
        start_order: 55,
        restart_policy: WorkerRestartPolicy::SkipWhenDisabled,
        shutdown_policy: WorkerShutdownPolicy::RuntimeShutdown,
        execution_scope: WorkerExecutionScope::LeaderOnly,
        health_owner: "routine_runs row state and tracing logs",
        notes: "Skipped when routines.enabled=false or postgres pool unavailable; \
                performs boot recovery of stale running runs before the tick loop starts",
    },
    WorkerSpec {
        id: ServerWorkerId::DmReplyRetry,
        name: "dm_reply_retry_loop",
        kind: WorkerKind::TokioTask,
        target: "dm_reply_retry_loop",
        responsibility: "Retry failed Discord DM notifications on a five-minute cadence",
        owner: "server::worker_registry",
        start_stage: WorkerStartStage::AfterBootReconcile,
        start_order: 60,
        restart_policy: WorkerRestartPolicy::LoopOwned,
        shutdown_policy: WorkerShutdownPolicy::RuntimeShutdown,
        execution_scope: WorkerExecutionScope::LeaderOnly,
        health_owner: "failed DM notification rows and retry tracing",
        notes: "Skips the immediate tick and only starts retries after the first interval",
    },
    WorkerSpec {
        id: ServerWorkerId::WsBatchFlusher,
        name: "spawn_batch_flusher",
        kind: WorkerKind::SpawnHelper,
        target: "ws::spawn_batch_flusher",
        responsibility: "Flush deduplicated websocket events into the shared broadcast channel",
        owner: "server::worker_registry",
        start_stage: WorkerStartStage::AfterWebsocketBroadcast,
        start_order: 70,
        restart_policy: WorkerRestartPolicy::LoopOwned,
        shutdown_policy: WorkerShutdownPolicy::RuntimeShutdown,
        execution_scope: WorkerExecutionScope::WorkerLocal,
        health_owner: "websocket broadcast throughput and tracing logs",
        notes: "Starts after the broadcast sender exists because it owns the shared batch buffer",
    },
];

enum WorkerHandle {
    Tokio {
        _handle: tokio::task::JoinHandle<()>,
    },
    Thread {
        _handle: std::thread::JoinHandle<()>,
    },
    SpawnHelper,
}

struct RunningWorker {
    spec: WorkerSpec,
    _handle: WorkerHandle,
}

pub(crate) struct SupervisedWorkerRegistry {
    config: Config,
    engine: PolicyEngine,
    health_registry: Option<Arc<HealthRegistry>>,
    pg_pool: Option<Arc<PgPool>>,
    cluster_runtime: ClusterRuntime,
    shutdown: Arc<AtomicBool>,
    running: Vec<RunningWorker>,
}

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
                    super::refresh_memory_health_for_startup().await;
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

    fn start_stage(
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

    fn start_worker(
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
                        super::github_sync_loop(sync_pg_pool, sync_interval).await;
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
                self.register_thread(spec, "policy-tick", move || {
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
                                rt.block_on(super::policy_tick_loop(
                                    tick_engine,
                                    Some(tick_pg_pool.clone()),
                                    Some(tick_cluster_runtime.clone()),
                                    Some(shutdown.clone()),
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
                        rt.block_on(tokio::time::sleep(std::time::Duration::from_secs(5)));
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
                        super::rate_limit_sync_loop(rate_limit_pg_pool).await;
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
                self.register_leader_tokio(spec, move || {
                    let maintenance_pg_pool = maintenance_pg_pool.clone();
                    let prompt_manifest_retention = prompt_manifest_retention.clone();
                    async move {
                        super::maintenance::scheduler_loop(
                            maintenance_pg_pool,
                            prompt_manifest_retention,
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
                        super::message_outbox_loop(outbox_pg_pool, outbox_health_registry).await;
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
                self.register_tokio(spec, async move {
                    super::routes::dispatches::dispatch_outbox_loop(
                        dispatch_outbox_pg_pool,
                        claim_owner,
                        cluster_runtime,
                        cluster_config,
                    )
                    .await;
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
                        super::dm_reply_retry_loop(dm_retry_pg_pool).await;
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
                let buffer = super::ws::spawn_batch_flusher(tx);
                self.running.push(RunningWorker {
                    spec,
                    _handle: WorkerHandle::SpawnHelper,
                });
                Ok(Some(buffer))
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
                        super::routine_runtime_loop(
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

    fn register_tokio<F>(&mut self, spec: WorkerSpec, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.running.push(RunningWorker {
            spec,
            _handle: WorkerHandle::Tokio {
                _handle: tokio::spawn(future),
            },
        });
    }

    fn register_leader_tokio<MakeFuture, Fut>(&mut self, spec: WorkerSpec, make_future: MakeFuture)
    where
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

    async fn supervise_leader_tokio_worker<MakeFuture, Fut>(
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
                    tracing::warn!(worker = spec.name, "leader-only worker future exited");
                }
                _ = cluster_runtime.wait_until_not_leader() => {
                    tracing::warn!(
                        worker = spec.name,
                        instance_id = cluster_runtime.instance_id(),
                        "leader-only worker self-fenced after cluster leadership was lost"
                    );
                }
                _ = wait_until_shutdown(shutdown.clone()) => {
                    tracing::info!(worker = spec.name, "leader-only worker supervisor shutting down");
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

    fn register_thread<F>(&mut self, spec: WorkerSpec, name: &str, body: F) -> Result<()>
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

    fn is_started(&self, id: ServerWorkerId) -> bool {
        self.running.iter().any(|worker| worker.spec.id == id)
    }

    fn log_start(&self, spec: WorkerSpec) {
        tracing::info!(
            worker = spec.name,
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

    fn log_skip(&self, spec: WorkerSpec, reason: &str) {
        tracing::info!(
            worker = spec.name,
            stage = spec.start_stage.as_doc_str(),
            execution_scope = spec.execution_scope.as_doc_str(),
            reason,
            "skipping supervised worker"
        );
    }
}

impl Drop for SupervisedWorkerRegistry {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::{
        BOOT_ONLY_STEPS, WORKER_SPECS, WorkerExecutionScope, WorkerShutdownPolicy, WorkerStartStage,
    };

    #[test]
    fn boot_steps_are_explicit_and_ordered() {
        assert_eq!(BOOT_ONLY_STEPS.len(), 2);
        assert!(
            BOOT_ONLY_STEPS
                .windows(2)
                .all(|pair| pair[0].order < pair[1].order)
        );
        assert_eq!(BOOT_ONLY_STEPS[0].name, "refresh_memory_health_for_startup");
        assert_eq!(BOOT_ONLY_STEPS[1].name, "drain_startup_hooks");
    }

    #[test]
    fn long_lived_workers_have_explicit_supervision_metadata() {
        assert_eq!(WORKER_SPECS.len(), 9);
        assert!(
            WORKER_SPECS
                .windows(2)
                .all(|pair| pair[0].start_order < pair[1].start_order)
        );
        assert_eq!(
            WORKER_SPECS
                .iter()
                .filter(|spec| spec.start_stage == WorkerStartStage::AfterBootReconcile)
                .count(),
            8
        );
        assert_eq!(
            WORKER_SPECS
                .iter()
                .filter(|spec| spec.start_stage == WorkerStartStage::AfterWebsocketBroadcast)
                .count(),
            1
        );
        assert_eq!(
            WORKER_SPECS
                .iter()
                .filter(|spec| spec.shutdown_policy == WorkerShutdownPolicy::ProcessExit)
                .count(),
            1
        );
        assert_eq!(
            WORKER_SPECS
                .iter()
                .filter(|spec| spec.execution_scope == WorkerExecutionScope::LeaderOnly)
                .count(),
            6
        );
        assert_eq!(
            WORKER_SPECS
                .iter()
                .filter(|spec| spec.execution_scope == WorkerExecutionScope::WorkerLocal)
                .count(),
            2
        );
        assert!(WORKER_SPECS.iter().all(|spec| !spec.owner.is_empty()));
        assert!(
            WORKER_SPECS
                .iter()
                .all(|spec| !spec.responsibility.is_empty())
        );
        assert!(
            WORKER_SPECS
                .iter()
                .all(|spec| !spec.health_owner.is_empty())
        );
        assert!(WORKER_SPECS.iter().all(|spec| !spec.notes.is_empty()));
    }
}
