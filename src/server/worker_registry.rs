use anyhow::{Result, anyhow};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::config::Config;
use crate::engine::PolicyEngine;
use crate::services::discord::health::HealthRegistry;
use crate::services::routines::validate_routine_runtime_config;
use sqlx::PgPool;

use super::cluster::ClusterRuntime;
use super::worker_recovery::WorkerLocalTerminalReason;
use super::ws::{BatchBuffer, BroadcastTx};

mod registry;
mod status;

#[cfg(test)]
use self::status::{
    LEADER_ONLY_WORKER_ACTIVE_COUNT, LEADER_ONLY_WORKER_LAST_SPAWN_UNIX_MS,
    LEADER_ONLY_WORKERS_STARTED,
};
use self::status::{
    LeaderOnlyWorkerEpoch, record_worker_local_terminal_signal, wait_until_leader_or_shutdown,
    wait_until_shutdown,
};
pub(crate) use self::status::{leader_only_worker_status_json, rate_limit_sync_active};

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
    ScheduledMessages,
    DispatchOutbox,
    DmReplyRetry,
    WsBatchFlusher,
    RoutineRuntime,
    SessionDiscovery,
    WatcherSupervisor,
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
pub(crate) struct WorkerRestartBudget {
    pub(crate) max_restarts: u32,
    pub(crate) window: Duration,
    pub(crate) initial_backoff: Duration,
    pub(crate) max_backoff: Duration,
}

pub(crate) const DEFAULT_WORKER_LOCAL_RESTART_BUDGET: WorkerRestartBudget = WorkerRestartBudget {
    max_restarts: 5,
    window: Duration::from_secs(600),
    initial_backoff: Duration::from_secs(1),
    max_backoff: Duration::from_secs(60),
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerRestartPolicy {
    SkipWhenDisabled,
    /// The worker future owns its retry/backoff loop and should only end during
    /// runtime shutdown. Leader-only Tokio workers re-enter on future exit after
    /// the next leader epoch; worker-local Tokio workers record a terminal
    /// supervision signal and do not auto-restart.
    LoopOwned,
    RestartableWithBudget(WorkerRestartBudget),
    ManualProcessRestart,
}

impl WorkerRestartPolicy {
    pub(crate) const fn as_doc_str(self) -> &'static str {
        match self {
            Self::SkipWhenDisabled => "skip_when_disabled",
            Self::LoopOwned => "loop_owned",
            Self::RestartableWithBudget(_) => "restartable_with_budget",
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

pub(crate) const WORKER_SPECS: [WorkerSpec; 12] = [
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
        id: ServerWorkerId::ScheduledMessages,
        name: "scheduled_message_loop",
        kind: WorkerKind::TokioTask,
        target: "services::scheduled_messages::scheduled_message_loop",
        responsibility: "Fire due scheduled-message reservations: hand push fires to message_outbox and drive agent fires through headless turns",
        owner: "server::worker_registry",
        start_stage: WorkerStartStage::AfterBootReconcile,
        start_order: 45,
        restart_policy: WorkerRestartPolicy::LoopOwned,
        shutdown_policy: WorkerShutdownPolicy::RuntimeShutdown,
        execution_scope: WorkerExecutionScope::LeaderOnly,
        health_owner: "scheduled_messages/scheduled_message_deliveries row state and tracing logs",
        notes: "Waits three seconds for Discord runtime readiness before polling with adaptive backoff; lease-based delivery claims keep firing at-most-once per slot",
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
        restart_policy: WorkerRestartPolicy::RestartableWithBudget(
            DEFAULT_WORKER_LOCAL_RESTART_BUDGET,
        ),
        shutdown_policy: WorkerShutdownPolicy::RuntimeShutdown,
        execution_scope: WorkerExecutionScope::WorkerLocal,
        health_owner: "dispatch outbox tables and delivery tracing",
        notes: "Runs on each cluster node; PostgreSQL row claims and capability filters select \
                the worker. Unexpected return/panic is restarted with a bounded local budget \
                and capped exponential backoff.",
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
        id: ServerWorkerId::SessionDiscovery,
        name: "session_discovery_loop",
        kind: WorkerKind::TokioTask,
        target: "services::cluster::session_discovery::run_discovery_loop",
        responsibility: "Enumerate tmux sessions, match to channel bindings, maintain SessionRegistry",
        owner: "server::worker_registry",
        start_stage: WorkerStartStage::AfterBootReconcile,
        start_order: 65,
        restart_policy: WorkerRestartPolicy::RestartableWithBudget(
            DEFAULT_WORKER_LOCAL_RESTART_BUDGET,
        ),
        shutdown_policy: WorkerShutdownPolicy::RuntimeShutdown,
        execution_scope: WorkerExecutionScope::WorkerLocal,
        health_owner: "SessionRegistry contents and /api/cluster/sessions diagnostic",
        notes: "Worker-local because tmux is host-scoped — every node must enumerate its own \
                sessions for the cluster registry. Reconcile is instance_id-scoped so peers \
                cannot stomp each other's entries. Boot reconcile runs immediately; subsequent \
                polls every 10s. External request_discovery_tick() nudges fire an immediate tick \
                for E3 event hooks. Unexpected return/panic is restarted with a bounded local \
                budget and capped exponential backoff.",
    },
    WorkerSpec {
        id: ServerWorkerId::WatcherSupervisor,
        name: "watcher_supervisor_loop",
        kind: WorkerKind::TokioTask,
        target: "services::discord::run_session_bound_discord_relay_supervisor",
        responsibility: "Spawn/teardown session-bound StreamRelay tasks in response to SessionRegistry events",
        owner: "server::worker_registry",
        start_stage: WorkerStartStage::AfterBootReconcile,
        start_order: 67,
        restart_policy: WorkerRestartPolicy::LoopOwned,
        shutdown_policy: WorkerShutdownPolicy::RuntimeShutdown,
        execution_scope: WorkerExecutionScope::WorkerLocal,
        health_owner: "watcher-supervisor tracing + per-relay metrics",
        notes: "Epic #2285 / E3 (#2345), wired through E4 (#2411) and E5 (#2412). Gated by \
                cluster.session_bound_relay_enabled (default true since E5); flipping the flag \
                off restores the legacy watcher as the sole terminal delivery path. \
                Worker-local because tmux is host-scoped — relays live next to the sessions \
                they observe. Production wires a Discord RelaySink that parses provider JSONL \
                frames and owns Discord terminal delivery for eligible session-bound inflight \
                shapes (rebind-origin/adopted sessions and watcher-owned relays). The legacy \
                watcher remains a fallback for bridge-owned/no-inflight envelopes and for \
                runtimes without a HealthRegistry. LoopOwned terminal semantics: unexpected \
                return/panic is recorded as a worker-local terminal supervision signal; registry \
                does not auto-restart.",
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

/// #5142 D-4 / r5: test-only record of the `HealthRegistry` the spawned
/// `policy-tick` thread actually captured.
///
/// The tick's auto-queue cleanup replay reaches `clear_provider_channel_runtime`
/// only through this handle, so starting the tick with `None` while the process
/// *has* a registry silently drops the runtime half of every replayed cleanup:
/// nothing fails, the PostgreSQL state still converges, and only the in-memory
/// provider runtime for the cleared slot threads is left behind.
///
/// Rounds 3 and 4 pinned this with a source guard that asserted on the text of
/// the call site. A single adjacent line defeated it — re-binding
/// `tick_health_registry` to `None` after the correct assignment left both the
/// positive and the negative string assertion satisfied while the thread
/// captured `None` (#5142 r5, mutation S2). This records the pointer identity of
/// what the thread really closed over, so the assertion is about the value that
/// reaches the tick rather than about the characters in this file.
///
/// The recorded value is a raw address used only for identity comparison; it is
/// never dereferenced, and the test holds its own `Arc` for the whole
/// comparison so the address cannot be reused.
#[cfg(test)]
fn policy_tick_captured_slot() -> &'static Mutex<Option<Option<usize>>> {
    static SLOT: OnceLock<Mutex<Option<Option<usize>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
fn reset_policy_tick_captured_registry() {
    *policy_tick_captured_slot()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = None;
}

#[cfg(test)]
fn record_policy_tick_captured_registry(registry: Option<usize>) {
    *policy_tick_captured_slot()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = Some(registry);
}

/// `None` until the spawned tick thread reaches its first statement; then
/// `Some(None)` for a tick started without a registry, or `Some(Some(addr))` for
/// the registry it captured.
#[cfg(test)]
fn policy_tick_captured_registry() -> Option<Option<usize>> {
    *policy_tick_captured_slot()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
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

impl Drop for SupervisedWorkerRegistry {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

// #2202 regression guard. Verifies that `supervise_leader_tokio_worker` re-spawns
// the underlying worker future after a lease takeover (leader=true → false → true),
// which is the contract the PR #2115 fix introduced. Without it, leader-only
// workers like `routine-runtime` go dormant on the new leader until dcserver
// restart.
#[cfg(test)]
mod leader_takeover_tests {
    use super::{
        LEADER_ONLY_WORKER_ACTIVE_COUNT, LEADER_ONLY_WORKER_LAST_SPAWN_UNIX_MS,
        LEADER_ONLY_WORKERS_STARTED, SupervisedWorkerRegistry, WORKER_SPECS, WorkerExecutionScope,
    };
    use crate::server::cluster::ClusterRuntime;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    /// **#5142 D-4 regression, verified on the spawned thread.**
    ///
    /// The policy tick must be started with the process `HealthRegistry`. With
    /// `None` the cleanup replay still converges everything PostgreSQL can see,
    /// so no test, log line or health check notices — only
    /// `clear_provider_channel_runtime` is skipped, leaving the in-memory
    /// provider runtime for the cleared slot threads alive.
    ///
    /// Rounds 3 and 4 pinned this by asserting on the *text* of the call site.
    /// That guard was defeated by one adjacent line: re-binding
    /// `tick_health_registry` to `None` immediately after the correct assignment
    /// satisfied both the positive and the negative string assertion while the
    /// spawned thread captured `None` (mutation S2). A guard that a neighbouring
    /// line disarms is worse than no guard, because it reads as coverage.
    ///
    /// This runs `start_worker` for real and compares the pointer identity of
    /// what the thread closed over against the registry the process was built
    /// with. The tick never reaches its loop body: `shutdown` is already set, so
    /// `wait_until_leader_or_shutdown` returns immediately and the thread exits
    /// without a PostgreSQL round trip or a `PolicyEngine::new_for_tick`.
    ///
    /// What it still does not prove: that `policy_tick_loop` is handed this same
    /// binding once leadership is granted. That line sits inside the leader-only
    /// body, which needs a live pool and a leadership grant;
    /// `drain_with_health_registry_tears_down_provider_runtime_pg` is what proves
    /// the registry does something once it arrives.
    /// `#[tokio::test]` only because `PgPool::connect_lazy` installs a pool
    /// reaper and therefore needs a runtime handle in scope. Nothing here awaits.
    #[tokio::test]
    async fn policy_tick_thread_captures_the_process_health_registry() {
        let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());

        let captured = spawn_policy_tick_and_capture(Some(registry.clone()));
        assert_eq!(
            captured,
            Some(Some(Arc::as_ptr(&registry) as usize)),
            "the policy-tick thread must close over the process health registry; \
             anything else silently disables the runtime half of every replayed \
             auto-queue cleanup"
        );

        // Standalone / no-Discord mode (`launch.rs` passes `None` to
        // `server::run`) legitimately has no registry, and must stay `None`
        // rather than fabricate one.
        assert_eq!(
            spawn_policy_tick_and_capture(None),
            Some(None),
            "a process with no registry must not invent one for the tick"
        );
    }

    /// Start the real `policy-tick` worker with `health_registry` and return what
    /// `policy_tick_captured_registry` saw the spawned thread capture.
    fn spawn_policy_tick_and_capture(
        health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
    ) -> Option<Option<usize>> {
        super::reset_policy_tick_captured_registry();

        let mut config = crate::config::Config::default();
        config.policies.hot_reload = false;
        let engine = crate::engine::PolicyEngine::new(&config).expect("build a policy engine");
        // `start_worker` only requires the pool to exist. Nothing here issues a
        // query, and the address is deliberately unroutable so a regression that
        // did issue one could never reach a real PostgreSQL server.
        let pg_pool = sqlx::Pool::<sqlx::Postgres>::connect_lazy(
            "postgres://agentdesk-test@127.0.0.1:1/agentdesk-policy-tick-probe",
        )
        .expect("build a lazy pool");
        let cluster_runtime =
            ClusterRuntime::for_test_with_leader(Arc::new(AtomicBool::new(false)));

        let mut worker_registry = SupervisedWorkerRegistry::new(
            config,
            engine,
            health_registry,
            Some(Arc::new(pg_pool)),
            cluster_runtime,
        );
        // Stop the thread at its first leader check, after it has recorded what
        // it captured but before it touches the pool.
        worker_registry.shutdown.store(true, Ordering::Release);
        worker_registry
            .start_worker(policy_tick_spec(), None)
            .expect("start the policy-tick worker");

        for _ in 0..200 {
            if let Some(captured) = super::policy_tick_captured_registry() {
                return Some(captured);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }

    fn policy_tick_spec() -> super::WorkerSpec {
        WORKER_SPECS
            .iter()
            .copied()
            .find(|spec| spec.name == "policy-tick")
            .expect("the policy-tick worker spec is registered")
    }

    fn leader_only_spec_for_test() -> super::WorkerSpec {
        WORKER_SPECS
            .iter()
            .copied()
            .find(|spec| spec.execution_scope == WorkerExecutionScope::LeaderOnly)
            .expect("at least one leader-only worker spec is registered")
    }

    #[tokio::test(start_paused = true)]
    async fn supervisor_respawns_worker_after_lease_takeover() {
        // Reset the globals the supervisor mutates so this test stays
        // deterministic regardless of other tests in the binary.
        LEADER_ONLY_WORKERS_STARTED.store(false, Ordering::Release);
        LEADER_ONLY_WORKER_ACTIVE_COUNT.store(0, Ordering::Release);
        LEADER_ONLY_WORKER_LAST_SPAWN_UNIX_MS.store(0, Ordering::Release);

        let leader_active = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(AtomicBool::new(false));
        let runtime = ClusterRuntime::for_test_with_leader(leader_active.clone());
        let spawn_count = Arc::new(AtomicUsize::new(0));
        let spec = leader_only_spec_for_test();

        let supervisor_count = spawn_count.clone();
        let supervisor = tokio::spawn(SupervisedWorkerRegistry::supervise_leader_tokio_worker(
            spec,
            runtime,
            shutdown.clone(),
            move || {
                let counter = supervisor_count.clone();
                async move {
                    counter.fetch_add(1, Ordering::Release);
                    // Park so the supervisor only re-spawns on a leader flip,
                    // not because the worker future returned on its own.
                    std::future::pending::<()>().await;
                }
            },
        ));

        // Not leader yet → supervisor blocks in wait_until_leader.
        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;
        assert_eq!(spawn_count.load(Ordering::Acquire), 0);

        // Acquire leadership → supervisor must spawn the worker.
        leader_active.store(true, Ordering::Release);
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            spawn_count.load(Ordering::Acquire),
            1,
            "worker future should run as soon as leadership is acquired"
        );
        assert!(LEADER_ONLY_WORKERS_STARTED.load(Ordering::Acquire));
        assert_eq!(LEADER_ONLY_WORKER_ACTIVE_COUNT.load(Ordering::Acquire), 1);

        // Lose leadership → supervisor self-fences the worker.
        leader_active.store(false, Ordering::Release);
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert_eq!(LEADER_ONLY_WORKER_ACTIVE_COUNT.load(Ordering::Acquire), 0);

        // Lease takeover: regain leadership while supervisor is in the
        // post-loss 5s cooldown. The supervisor must re-enter the spawn loop.
        leader_active.store(true, Ordering::Release);
        // 5s cooldown + 1s poll interval + jitter buffer.
        tokio::time::advance(Duration::from_secs(8)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            spawn_count.load(Ordering::Acquire),
            2,
            "worker must re-spawn after lease takeover (regression guard for #2202)"
        );
        assert_eq!(LEADER_ONLY_WORKER_ACTIVE_COUNT.load(Ordering::Acquire), 1);

        shutdown.store(true, Ordering::Release);
        // Let the supervisor observe shutdown on its next poll tick and exit.
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        let _ = tokio::time::timeout(Duration::from_secs(2), supervisor).await;
    }
}
