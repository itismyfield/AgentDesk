//! Runtime status, lifecycle counters, and leader-epoch observation.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use super::{
    ClusterRuntime, ServerWorkerId, WorkerExecutionScope, WorkerLocalTerminalReason, WorkerSpec,
};

pub(super) static LEADER_ONLY_WORKERS_STARTED: AtomicBool = AtomicBool::new(false);
pub(super) static LEADER_ONLY_WORKER_ACTIVE_COUNT: AtomicUsize = AtomicUsize::new(0);
pub(super) static LEADER_ONLY_WORKER_LAST_SPAWN_UNIX_MS: AtomicI64 = AtomicI64::new(0);
pub(super) static RATE_LIMIT_SYNC_ACTIVE: AtomicBool = AtomicBool::new(false);
pub(super) static WORKER_LOCAL_TERMINAL_SIGNAL_COUNT: AtomicUsize = AtomicUsize::new(0);
pub(super) static WORKER_LOCAL_UNEXPECTED_TERMINAL_SIGNAL_COUNT: AtomicUsize = AtomicUsize::new(0);
pub(super) static WORKER_LOCAL_LAST_TERMINAL_SIGNAL: LazyLock<
    Mutex<Option<WorkerLocalTerminalSignal>>,
> = LazyLock::new(|| Mutex::new(None));
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct WorkerLocalTerminalSignal {
    worker: &'static str,
    reason: &'static str,
    expected_shutdown: bool,
    observed_unix_ms: i64,
}

pub(crate) fn leader_only_worker_status_json() -> serde_json::Value {
    let last_spawn_unix_ms = LEADER_ONLY_WORKER_LAST_SPAWN_UNIX_MS.load(Ordering::Acquire);
    let last_worker_local_signal = worker_local_terminal_signal_snapshot().map(|signal| {
        serde_json::json!({
            "worker": signal.worker,
            "reason": signal.reason,
            "expected_shutdown": signal.expected_shutdown,
            "observed_at": chrono::DateTime::<chrono::Utc>::from_timestamp_millis(signal.observed_unix_ms),
        })
    });
    serde_json::json!({
        "leader_only_workers_started": LEADER_ONLY_WORKERS_STARTED.load(Ordering::Acquire),
        "leader_only_workers_active_count": LEADER_ONLY_WORKER_ACTIVE_COUNT.load(Ordering::Acquire),
        "last_leader_only_worker_spawn_at": if last_spawn_unix_ms > 0 {
            chrono::DateTime::<chrono::Utc>::from_timestamp_millis(last_spawn_unix_ms)
        } else {
            None
        },
        "worker_local_terminal_signal_count": WORKER_LOCAL_TERMINAL_SIGNAL_COUNT.load(Ordering::Acquire),
        "worker_local_unexpected_terminal_signal_count": WORKER_LOCAL_UNEXPECTED_TERMINAL_SIGNAL_COUNT.load(Ordering::Acquire),
        "last_worker_local_terminal_signal": last_worker_local_signal,
        // #4515 PR2: worker-local restart/exhaustion recovery counters.
        "worker_local_recovery": super::super::worker_recovery::recovery_runtime_json(),
        // Backward-compatible aliases for clients deployed before #4515.
        "worker_local_loop_owned_terminal_signal_count": WORKER_LOCAL_TERMINAL_SIGNAL_COUNT.load(Ordering::Acquire),
        "worker_local_loop_owned_unexpected_terminal_signal_count": WORKER_LOCAL_UNEXPECTED_TERMINAL_SIGNAL_COUNT.load(Ordering::Acquire),
        "last_worker_local_loop_owned_terminal_signal": last_worker_local_signal,
    })
}

pub(crate) fn rate_limit_sync_active() -> bool {
    RATE_LIMIT_SYNC_ACTIVE.load(Ordering::Acquire)
}

pub(super) fn worker_local_terminal_signal_snapshot() -> Option<WorkerLocalTerminalSignal> {
    *WORKER_LOCAL_LAST_TERMINAL_SIGNAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(super) fn record_worker_local_terminal_signal(
    spec: WorkerSpec,
    reason: WorkerLocalTerminalReason,
    expected_shutdown: bool,
    auto_restart: bool,
    restart_attempt: usize,
) {
    if spec.execution_scope != WorkerExecutionScope::WorkerLocal {
        return;
    }

    let reason = reason.as_doc_str();
    let signal = WorkerLocalTerminalSignal {
        worker: spec.name,
        reason,
        expected_shutdown,
        observed_unix_ms: chrono::Utc::now().timestamp_millis(),
    };
    WORKER_LOCAL_TERMINAL_SIGNAL_COUNT.fetch_add(1, Ordering::AcqRel);
    if !expected_shutdown {
        WORKER_LOCAL_UNEXPECTED_TERMINAL_SIGNAL_COUNT.fetch_add(1, Ordering::AcqRel);
    }
    *WORKER_LOCAL_LAST_TERMINAL_SIGNAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(signal);

    if expected_shutdown {
        tracing::info!(
            worker = spec.name,
            target = spec.target,
            observability_target = spec.target,
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
            auto_restart,
            restart_attempt,
            "worker-local loop-owned worker future exited after shutdown"
        );
    } else if reason == WorkerLocalTerminalReason::Panicked.as_doc_str() {
        tracing::error!(
            worker = spec.name,
            target = spec.target,
            observability_target = spec.target,
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
            auto_restart,
            restart_attempt,
            "worker-local loop-owned worker future panicked"
        );
    } else {
        tracing::warn!(
            worker = spec.name,
            target = spec.target,
            observability_target = spec.target,
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
            auto_restart,
            restart_attempt,
            "worker-local loop-owned worker future exited unexpectedly"
        );
    }
}

pub(super) fn record_leader_only_worker_started(spec: WorkerSpec) {
    LEADER_ONLY_WORKERS_STARTED.store(true, Ordering::Release);
    LEADER_ONLY_WORKER_ACTIVE_COUNT.fetch_add(1, Ordering::AcqRel);
    if spec.id == ServerWorkerId::RateLimitSync {
        RATE_LIMIT_SYNC_ACTIVE.store(true, Ordering::Release);
    }
    LEADER_ONLY_WORKER_LAST_SPAWN_UNIX_MS
        .store(chrono::Utc::now().timestamp_millis(), Ordering::Release);
    tracing::info!(
        worker = spec.name,
        target = spec.target,
        observability_target = spec.target,
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
        "leader-only worker epoch started"
    );
}

pub(super) fn record_leader_only_worker_stopped(spec: WorkerSpec, reason: &str) {
    let _ = LEADER_ONLY_WORKER_ACTIVE_COUNT.fetch_update(
        Ordering::AcqRel,
        Ordering::Acquire,
        |count| Some(count.saturating_sub(1)),
    );
    if spec.id == ServerWorkerId::RateLimitSync {
        RATE_LIMIT_SYNC_ACTIVE.store(false, Ordering::Release);
    }
    tracing::warn!(
        worker = spec.name,
        target = spec.target,
        observability_target = spec.target,
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
        "leader-only worker epoch stopped"
    );
}

pub(super) struct LeaderOnlyWorkerEpoch {
    spec: WorkerSpec,
}

impl LeaderOnlyWorkerEpoch {
    pub(super) fn start(spec: WorkerSpec) -> Self {
        record_leader_only_worker_started(spec);
        Self { spec }
    }
}

impl Drop for LeaderOnlyWorkerEpoch {
    fn drop(&mut self) {
        record_leader_only_worker_stopped(self.spec, "leader worker epoch ended");
    }
}

pub(super) async fn wait_until_shutdown(shutdown: Arc<AtomicBool>) {
    while !shutdown.load(Ordering::Acquire) {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

pub(super) async fn wait_until_leader_or_shutdown(
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
