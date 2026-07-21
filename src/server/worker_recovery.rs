use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use super::worker_registry::{WorkerRestartBudget, WorkerRestartPolicy, WorkerSpec};

pub(super) const WORKER_LOCAL_SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// #4515 PR2: how long an informational recovery observation (flapping or an
/// unexpected LoopOwned termination) stays visible in `/api/health` before a
/// read prunes it. Budget-exhausted (fatal) states are exempt — they persist
/// until the process is restarted so readiness stays down for operators.
const RECOVERY_OBSERVATION_TTL: Duration = Duration::from_secs(600);

/// #4515 PR3: pause before `process::exit(1)` so the fatal log line and a final
/// health scrape have time to flush before launchd KeepAlive respawns us.
pub(super) const FATAL_EXIT_GRACE: Duration = Duration::from_secs(2);

/// #4515 PR3 (§9.2): cross-process crash-loop guard window. If the same worker
/// already drove this many fatal process exits inside the window, exiting again
/// is proven not to help — hold Unhealthy(503) for human intervention instead.
const FATAL_CROSS_PROCESS_WINDOW: Duration = Duration::from_secs(1800);
const FATAL_CROSS_PROCESS_MAX: usize = 2;
const FATAL_EXIT_LEDGER_FILE: &str = "worker_fatal_exits.json";

static WORKER_RESTART_BUDGET_EXHAUSTED_COUNT: AtomicUsize = AtomicUsize::new(0);
static WORKER_RECOVERY_STATES: LazyLock<Mutex<HashMap<&'static str, WorkerRecoveryState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static FATAL_LEDGER_LOCK: Mutex<()> = Mutex::new(());
#[cfg(test)]
static WORKER_RESTART_BUDGET_TEST_MUTEX: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WorkerLocalTerminalReason {
    Returned,
    Panicked,
    Cancelled,
}

impl WorkerLocalTerminalReason {
    pub(super) const fn as_doc_str(self) -> &'static str {
        match self {
            Self::Returned => "returned",
            Self::Panicked => "panicked",
            Self::Cancelled => "cancelled",
        }
    }
}

/// #4515 PR2: classification of a worker-local recovery observation, driving
/// how it maps onto `/api/health`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryClassification {
    /// Restartable worker was re-spawned at least once inside the window but
    /// still has budget left. Informational only — must NOT worsen HTTP status
    /// (§9.3: an in-band Degraded here would trip deploy gates).
    Flapping,
    /// Restartable worker exhausted its budget → readiness down (Unhealthy/503)
    /// and, in production, a process exit.
    Exhausted,
    /// Non-restartable LoopOwned worker terminated unexpectedly. Degraded so the
    /// silent-wedge (`session_discovery` / `watcher_supervisor`) surfaces.
    LoopOwnedTerminated,
}

impl RecoveryClassification {
    const fn as_doc_str(self) -> &'static str {
        match self {
            Self::Flapping => "flapping",
            Self::Exhausted => "exhausted",
            Self::LoopOwnedTerminated => "loop_owned_terminated",
        }
    }
}

#[derive(Debug, Clone)]
struct WorkerRecoveryState {
    recent_restart_count: usize,
    last_reason: &'static str,
    classification: RecoveryClassification,
    observed_at: Instant,
}

impl WorkerRecoveryState {
    /// #4515 PR2 (§9.5): informational states expire on read; the fatal
    /// exhausted state is retained until the process restarts.
    fn is_expired(&self, now: Instant) -> bool {
        self.classification != RecoveryClassification::Exhausted
            && now.duration_since(self.observed_at) >= RECOVERY_OBSERVATION_TTL
    }
}

/// #4515 PR2: severity a recovery reason contributes to `/api/health`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryReasonSeverity {
    Degraded,
    Unhealthy,
}

/// #4515 PR2: a status-worsening recovery reason for `/api/health`. Flapping is
/// deliberately excluded (surfaced via [`recovery_flapping_info`] instead).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecoveryHealthReason {
    pub(crate) reason: String,
    pub(crate) severity: RecoveryReasonSeverity,
}

/// #4515 PR3: context handed to the fatal hook when a restart budget is
/// exhausted.
#[derive(Debug, Clone)]
pub(crate) struct FatalExhaustionRecord {
    pub(crate) worker: &'static str,
    pub(crate) window_restart_count: usize,
    pub(crate) max_restarts: u32,
    pub(crate) window: Duration,
}

/// #4515 PR3: injected budget-exhaustion action. Production wires the process
/// exit circuit ([`production_fatal_hook`]); tests inject a flag closure so the
/// supervisor loop is exercised without killing the test process.
pub(crate) type FatalHook = Arc<dyn Fn(&FatalExhaustionRecord) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerRunOutcome {
    Unexpected(WorkerLocalTerminalReason),
    Shutdown(WorkerLocalTerminalReason),
}

pub(super) async fn supervise_worker_local<MakeFuture, Fut, RecordTerminal>(
    spec: WorkerSpec,
    shutdown: Arc<AtomicBool>,
    make_future: MakeFuture,
    mut record_terminal: RecordTerminal,
    fatal_hook: FatalHook,
) where
    MakeFuture: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
    RecordTerminal: FnMut(WorkerLocalTerminalReason, bool, bool, usize) + Send + 'static,
{
    match spec.restart_policy {
        WorkerRestartPolicy::RestartableWithBudget(budget) => {
            supervise_restartable(
                spec,
                shutdown,
                make_future,
                budget,
                &mut record_terminal,
                &fatal_hook,
            )
            .await;
        }
        _ => match run_worker_once(spec, shutdown, make_future()).await {
            WorkerRunOutcome::Unexpected(reason) => {
                // #4515 PR2: expose the silent wedge of an un-migrated LoopOwned
                // worker (session_discovery / watcher_supervisor) as a Degraded
                // health reason — previously this only bumped a counter.
                record_recovery_state(
                    spec.name,
                    0,
                    reason,
                    RecoveryClassification::LoopOwnedTerminated,
                );
                record_terminal(reason, false, false, 0);
            }
            WorkerRunOutcome::Shutdown(reason) => {
                record_terminal(reason, true, false, 0);
            }
        },
    }
}

async fn supervise_restartable<MakeFuture, Fut, RecordTerminal>(
    spec: WorkerSpec,
    shutdown: Arc<AtomicBool>,
    make_future: MakeFuture,
    budget: WorkerRestartBudget,
    record_terminal: &mut RecordTerminal,
    fatal_hook: &FatalHook,
) where
    MakeFuture: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
    RecordTerminal: FnMut(WorkerLocalTerminalReason, bool, bool, usize) + Send + 'static,
{
    let mut restart_times = VecDeque::with_capacity(budget.max_restarts as usize);
    let mut consecutive_failures = 0_u32;

    loop {
        if shutdown.load(Ordering::Acquire) {
            return;
        }

        let started_at = Instant::now();
        let reason = match run_worker_once(spec, shutdown.clone(), make_future()).await {
            WorkerRunOutcome::Unexpected(reason) if shutdown.load(Ordering::Acquire) => {
                record_terminal(reason, true, true, restart_times.len());
                return;
            }
            WorkerRunOutcome::Unexpected(reason) => reason,
            WorkerRunOutcome::Shutdown(reason) => {
                record_terminal(reason, true, true, restart_times.len());
                return;
            }
        };

        let now = Instant::now();
        while restart_times
            .front()
            .is_some_and(|restart_at| now.duration_since(*restart_at) >= budget.window)
        {
            restart_times.pop_front();
        }

        if now.duration_since(started_at) >= budget.max_backoff {
            consecutive_failures = 0;
        }

        if restart_times.len() >= budget.max_restarts as usize {
            record_terminal(reason, false, true, restart_times.len());
            record_recovery_state(
                spec.name,
                restart_times.len(),
                reason,
                RecoveryClassification::Exhausted,
            );
            WORKER_RESTART_BUDGET_EXHAUSTED_COUNT.fetch_add(1, Ordering::AcqRel);
            tracing::error!(
                worker = spec.name,
                restart = spec.restart_policy.as_doc_str(),
                reason = reason.as_doc_str(),
                restart_count = restart_times.len(),
                max_restarts = budget.max_restarts,
                window_secs = budget.window.as_secs(),
                "worker-local restart budget exhausted; promoting to fatal recovery circuit"
            );
            // #4515 PR3: hand off to the fatal circuit (readiness down already
            // wired via the Exhausted recovery state above; production also sets
            // shutdown + exits so launchd KeepAlive restarts a clean process).
            fatal_hook(&FatalExhaustionRecord {
                worker: spec.name,
                window_restart_count: restart_times.len(),
                max_restarts: budget.max_restarts,
                window: budget.window,
            });
            return;
        }

        restart_times.push_back(now);
        let restart_attempt = restart_times.len();
        record_terminal(reason, false, true, restart_attempt);
        record_recovery_state(
            spec.name,
            restart_attempt,
            reason,
            RecoveryClassification::Flapping,
        );

        let backoff = exponential_backoff(budget, consecutive_failures);
        consecutive_failures = consecutive_failures.saturating_add(1);
        tracing::warn!(
            worker = spec.name,
            restart = spec.restart_policy.as_doc_str(),
            reason = reason.as_doc_str(),
            restart_attempt,
            backoff_ms = backoff.as_millis(),
            "worker-local worker exited unexpectedly; scheduling restart"
        );

        if wait_for_backoff_or_shutdown(backoff, shutdown.clone()).await
            || shutdown.load(Ordering::Acquire)
        {
            return;
        }
    }
}

fn exponential_backoff(budget: WorkerRestartBudget, exponent: u32) -> Duration {
    let multiplier = 1_u32.checked_shl(exponent.min(31)).unwrap_or(u32::MAX);
    budget
        .initial_backoff
        .checked_mul(multiplier)
        .unwrap_or(budget.max_backoff)
        .min(budget.max_backoff)
}

async fn wait_for_backoff_or_shutdown(backoff: Duration, shutdown: Arc<AtomicBool>) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(backoff) => false,
        _ = wait_until_shutdown(shutdown) => true,
    }
}

async fn run_worker_once<F>(
    spec: WorkerSpec,
    shutdown: Arc<AtomicBool>,
    future: F,
) -> WorkerRunOutcome
where
    F: Future<Output = ()> + Send + 'static,
{
    let mut worker_handle = tokio::spawn(future);
    tokio::select! {
        result = &mut worker_handle => classify_join_result(result),
        _ = wait_until_shutdown(shutdown) => {
            tracing::info!(
                worker = spec.name,
                restart = spec.restart_policy.as_doc_str(),
                "worker-local Tokio supervisor waiting for worker shutdown cleanup"
            );
            let grace = tokio::time::sleep(WORKER_LOCAL_SHUTDOWN_GRACE);
            tokio::pin!(grace);
            let outcome = tokio::select! {
                result = &mut worker_handle => classify_join_result(result),
                _ = &mut grace => {
                    tracing::warn!(
                        worker = spec.name,
                        restart = spec.restart_policy.as_doc_str(),
                        "worker-local Tokio worker exceeded graceful shutdown timeout; aborting"
                    );
                    worker_handle.abort();
                    classify_join_result(worker_handle.await)
                }
            };
            let WorkerRunOutcome::Unexpected(reason) = outcome else {
                unreachable!("classify_join_result always returns an unexpected terminal reason")
            };
            WorkerRunOutcome::Shutdown(reason)
        }
    }
}

fn classify_join_result(result: Result<(), tokio::task::JoinError>) -> WorkerRunOutcome {
    match result {
        Ok(()) => WorkerRunOutcome::Unexpected(WorkerLocalTerminalReason::Returned),
        Err(error) if error.is_panic() => {
            WorkerRunOutcome::Unexpected(WorkerLocalTerminalReason::Panicked)
        }
        Err(error) if error.is_cancelled() => {
            WorkerRunOutcome::Unexpected(WorkerLocalTerminalReason::Cancelled)
        }
        Err(error) => {
            tracing::warn!(join_error = %error, "worker-local Tokio worker join failed");
            WorkerRunOutcome::Unexpected(WorkerLocalTerminalReason::Cancelled)
        }
    }
}

async fn wait_until_shutdown(shutdown: Arc<AtomicBool>) {
    while !shutdown.load(Ordering::Acquire) {
        tokio::time::sleep(SHUTDOWN_POLL_INTERVAL).await;
    }
}

fn record_recovery_state(
    worker: &'static str,
    recent_restart_count: usize,
    reason: WorkerLocalTerminalReason,
    classification: RecoveryClassification,
) {
    WORKER_RECOVERY_STATES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(
            worker,
            WorkerRecoveryState {
                recent_restart_count,
                last_reason: reason.as_doc_str(),
                classification,
                observed_at: Instant::now(),
            },
        );
}

/// #4515 PR2: prune expired informational observations (§9.5 read-time expiry)
/// and return a stable snapshot of what remains.
fn recovery_snapshot() -> Vec<(&'static str, WorkerRecoveryState)> {
    let now = Instant::now();
    let mut states = WORKER_RECOVERY_STATES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    states.retain(|_, state| !state.is_expired(now));
    states
        .iter()
        .map(|(worker, state)| (*worker, state.clone()))
        .collect()
}

/// #4515 PR2: status-worsening recovery reasons for `/api/health`.
///
/// - `worker_local_restart_budget_exhausted:<worker>` → Unhealthy (readiness
///   503). A necessary worker is permanently dead; this node must stop serving.
/// - `worker_local_loop_owned_terminated:<worker>` → Degraded. An un-migrated
///   LoopOwned worker died unexpectedly.
///
/// Flapping is intentionally NOT returned here — see [`recovery_flapping_info`].
pub(crate) fn recovery_health_reasons() -> Vec<RecoveryHealthReason> {
    recovery_snapshot()
        .into_iter()
        .filter_map(|(worker, state)| match state.classification {
            RecoveryClassification::Exhausted => Some(RecoveryHealthReason {
                reason: format!("worker_local_restart_budget_exhausted:{worker}"),
                severity: RecoveryReasonSeverity::Unhealthy,
            }),
            RecoveryClassification::LoopOwnedTerminated => Some(RecoveryHealthReason {
                reason: format!("worker_local_loop_owned_terminated:{worker}"),
                severity: RecoveryReasonSeverity::Degraded,
            }),
            RecoveryClassification::Flapping => None,
        })
        .collect()
}

/// #4515 PR2 (§9.3): flapping observations as an informational health-body
/// field. These must never enter `degraded_reasons` (deploy gates would trip on
/// intermittent, self-healing restarts before the budget is even exhausted).
pub(crate) fn recovery_flapping_info() -> Vec<serde_json::Value> {
    recovery_snapshot()
        .into_iter()
        .filter_map(|(worker, state)| match state.classification {
            RecoveryClassification::Flapping => Some(serde_json::json!(format!(
                "worker_local_restart_flapping:{worker}:{}",
                state.recent_restart_count
            ))),
            _ => None,
        })
        .collect()
}

/// #4515 PR2: worker-local recovery counters for `/api/cluster`
/// `local_worker_runtime`.
pub(crate) fn recovery_runtime_json() -> serde_json::Value {
    let workers: Vec<serde_json::Value> = recovery_snapshot()
        .into_iter()
        .map(|(worker, state)| {
            serde_json::json!({
                "worker": worker,
                "classification": state.classification.as_doc_str(),
                "recent_restart_count": state.recent_restart_count,
                "last_reason": state.last_reason,
            })
        })
        .collect();
    serde_json::json!({
        "restart_budget_exhausted_total": WORKER_RESTART_BUDGET_EXHAUSTED_COUNT.load(Ordering::Acquire),
        "workers": workers,
    })
}

/// #4515 PR3: production budget-exhaustion action. Completes the recovery
/// circuit — set the shutdown flag, log, grace-flush, then `process::exit(1)`
/// so launchd `KeepAlive` respawns a clean process — unless the cross-process
/// crash-loop guard (§9.2) proves a restart will not help.
pub(crate) fn production_fatal_hook(shutdown: Arc<AtomicBool>) -> FatalHook {
    Arc::new(move |record| commit_fatal_exit(record, &shutdown))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CrossProcessDecision {
    Exit,
    HoldWithoutExit { recent_fatal_exits: usize },
}

fn commit_fatal_exit(record: &FatalExhaustionRecord, shutdown: &Arc<AtomicBool>) {
    let ledger_path = fatal_exit_ledger_path();
    let decision =
        cross_process_fatal_decision_at(ledger_path.as_deref(), record.worker, now_unix_ms());

    if let CrossProcessDecision::HoldWithoutExit { recent_fatal_exits } = decision {
        tracing::error!(
            worker = record.worker,
            recent_fatal_exits,
            window_secs = FATAL_CROSS_PROCESS_WINDOW.as_secs(),
            "worker-local restart budget exhausted but the process already crash-looped on this worker within the window; holding Unhealthy(503) without exit for operator intervention"
        );
        return;
    }

    // #9.4: the fatal path cannot rely on registry-drop to set the shutdown
    // flag, so set it here before granting the grace window. This short grace
    // may truncate an in-flight dispatch outbox delivery. The row remains
    // claimed in PostgreSQL and is reclaimed after 300 seconds; delivery uses
    // its stable reservation/identity, so the crash window can cause a bounded
    // duplicate retry but cannot silently lose the durable outbox row.
    shutdown.store(true, Ordering::Release);
    tracing::error!(
        worker = record.worker,
        window_restart_count = record.window_restart_count,
        max_restarts = record.max_restarts,
        window_secs = record.window.as_secs(),
        grace_secs = FATAL_EXIT_GRACE.as_secs(),
        "worker-local restart budget exhausted; exiting process so launchd KeepAlive restarts a clean process"
    );
    std::thread::sleep(FATAL_EXIT_GRACE);
    std::process::exit(1);
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FatalExitLedgerEntry {
    worker: String,
    observed_unix_ms: i64,
}

fn fatal_exit_ledger_path() -> Option<PathBuf> {
    crate::config::runtime_root().map(|root| root.join(FATAL_EXIT_LEDGER_FILE))
}

fn now_unix_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn cross_process_fatal_decision_at(
    path: Option<&Path>,
    worker: &str,
    now_ms: i64,
) -> CrossProcessDecision {
    match path {
        Some(path) => record_and_check_cross_process_fatal_at(path, worker, now_ms),
        // Without a runtime root there is no durable cross-process evidence, so
        // hold for operator intervention rather than entering an exit loop.
        None => CrossProcessDecision::HoldWithoutExit {
            recent_fatal_exits: 0,
        },
    }
}

/// #4515 PR3 (§9.2): node-local (no PG / leader coordination) crash-loop guard.
/// Records this fatal exit and decides whether exiting is still worthwhile.
/// Returns [`CrossProcessDecision::HoldWithoutExit`] when the same worker has
/// already caused `FATAL_CROSS_PROCESS_MAX` fatal exits inside the window.
fn record_and_check_cross_process_fatal_at(
    path: &Path,
    worker: &str,
    now_ms: i64,
) -> CrossProcessDecision {
    let _guard = FATAL_LEDGER_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let window_ms = FATAL_CROSS_PROCESS_WINDOW.as_millis() as i64;
    let mut entries = load_fatal_ledger(path);
    // A future entry indicates backward wall-clock skew. Ignore it rather than
    // extending a stale crash-loop hold until the clock catches up.
    entries.retain(|entry| {
        entry.observed_unix_ms <= now_ms
            && now_ms.saturating_sub(entry.observed_unix_ms) < window_ms
    });

    let recent_fatal_exits = entries
        .iter()
        .filter(|entry| entry.worker == worker)
        .count();
    if recent_fatal_exits >= FATAL_CROSS_PROCESS_MAX {
        // Do not append: this exit is being suppressed, not performed. Failure
        // to compact the ledger is safe because the decision already holds.
        if let Err(error) = save_fatal_ledger(path, &entries) {
            log_fatal_ledger_save_error(path, &error);
        }
        return CrossProcessDecision::HoldWithoutExit { recent_fatal_exits };
    }

    entries.push(FatalExitLedgerEntry {
        worker: worker.to_string(),
        observed_unix_ms: now_ms,
    });
    if let Err(error) = save_fatal_ledger(path, &entries) {
        log_fatal_ledger_save_error(path, &error);
        // Exiting without a durable record lets every fresh process repeat the
        // same exit forever. Fail closed toward the existing Unhealthy(503)
        // state whenever the cross-process guard cannot persist its evidence.
        return CrossProcessDecision::HoldWithoutExit { recent_fatal_exits };
    }
    CrossProcessDecision::Exit
}

fn load_fatal_ledger(path: &Path) -> Vec<FatalExitLedgerEntry> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "failed to read worker fatal-exit ledger; treating as empty"
            );
            Vec::new()
        }
    }
}

fn fatal_ledger_temp_path(path: &Path) -> io::Result<PathBuf> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "ledger path has no file name")
        })?;
    Ok(path.with_file_name(format!(".{file_name}.tmp")))
}

fn save_fatal_ledger(path: &Path, entries: &[FatalExitLedgerEntry]) -> io::Result<()> {
    let serialized = serde_json::to_vec_pretty(entries).map_err(io::Error::other)?;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "ledger path has no parent"))?;
    std::fs::create_dir_all(parent)?;

    let temp_path = fatal_ledger_temp_path(path)?;
    let write_result = (|| -> io::Result<()> {
        let mut temp = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp_path)?;
        temp.write_all(&serialized)?;
        temp.sync_all()?;
        std::fs::rename(&temp_path, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    write_result
}

fn log_fatal_ledger_save_error(path: &Path, error: &io::Error) {
    tracing::warn!(
        path = %path.display(),
        %error,
        "failed to atomically persist worker fatal-exit ledger"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::worker_registry::{
        DEFAULT_WORKER_LOCAL_RESTART_BUDGET, WORKER_SPECS, WorkerExecutionScope,
    };
    use std::sync::atomic::AtomicUsize;

    fn restartable_spec() -> WorkerSpec {
        WORKER_SPECS
            .iter()
            .copied()
            .find(|spec| {
                spec.execution_scope == WorkerExecutionScope::WorkerLocal
                    && matches!(
                        spec.restart_policy,
                        WorkerRestartPolicy::RestartableWithBudget(_)
                    )
            })
            .expect("restartable worker-local spec")
    }

    fn loop_owned_spec() -> WorkerSpec {
        WORKER_SPECS
            .iter()
            .copied()
            .find(|spec| {
                spec.execution_scope == WorkerExecutionScope::WorkerLocal
                    && spec.restart_policy == WorkerRestartPolicy::LoopOwned
            })
            .expect("loop-owned worker-local spec")
    }

    fn test_budget(max_restarts: u32) -> WorkerRestartBudget {
        WorkerRestartBudget {
            max_restarts,
            window: Duration::from_secs(600),
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(4),
        }
    }

    fn noop_fatal_hook() -> FatalHook {
        Arc::new(|_record: &FatalExhaustionRecord| {})
    }

    fn counting_fatal_hook(counter: Arc<AtomicUsize>) -> FatalHook {
        Arc::new(move |_record: &FatalExhaustionRecord| {
            counter.fetch_add(1, Ordering::AcqRel);
        })
    }

    fn clear_recovery_state(worker: &str) {
        WORKER_RECOVERY_STATES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(worker);
    }

    async fn advance(duration: Duration) {
        tokio::time::advance(duration).await;
        tokio::task::yield_now().await;
    }

    #[tokio::test(start_paused = true)]
    async fn restarts_after_unexpected_return() {
        let spawns = Arc::new(AtomicUsize::new(0));
        let factory_spawns = spawns.clone();
        let supervisor = tokio::spawn(supervise_worker_local(
            restartable_spec(),
            Arc::new(AtomicBool::new(false)),
            move || {
                let spawns = factory_spawns.clone();
                async move {
                    spawns.fetch_add(1, Ordering::AcqRel);
                }
            },
            |_, _, _, _| {},
            noop_fatal_hook(),
        ));
        tokio::task::yield_now().await;
        assert_eq!(spawns.load(Ordering::Acquire), 1);
        advance(Duration::from_secs(1)).await;
        assert_eq!(spawns.load(Ordering::Acquire), 2);
        supervisor.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn restarts_after_panic() {
        let spawns = Arc::new(AtomicUsize::new(0));
        let panicked = Arc::new(AtomicBool::new(false));
        let factory_spawns = spawns.clone();
        let observed_panic = panicked.clone();
        let supervisor = tokio::spawn(supervise_worker_local(
            restartable_spec(),
            Arc::new(AtomicBool::new(false)),
            move || {
                let spawns = factory_spawns.clone();
                async move {
                    spawns.fetch_add(1, Ordering::AcqRel);
                    panic!("restartable worker panic");
                }
            },
            move |reason, _, _, _| {
                if reason == WorkerLocalTerminalReason::Panicked {
                    observed_panic.store(true, Ordering::Release);
                }
            },
            noop_fatal_hook(),
        ));
        tokio::task::yield_now().await;
        advance(Duration::from_secs(1)).await;
        assert_eq!(spawns.load(Ordering::Acquire), 2);
        assert!(panicked.load(Ordering::Acquire));
        supervisor.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_doubles_and_caps() {
        let spawns = Arc::new(AtomicUsize::new(0));
        let factory_spawns = spawns.clone();
        let mut spec = restartable_spec();
        spec.restart_policy = WorkerRestartPolicy::RestartableWithBudget(test_budget(10));
        let supervisor = tokio::spawn(supervise_worker_local(
            spec,
            Arc::new(AtomicBool::new(false)),
            move || {
                let spawns = factory_spawns.clone();
                async move {
                    spawns.fetch_add(1, Ordering::AcqRel);
                }
            },
            |_, _, _, _| {},
            noop_fatal_hook(),
        ));
        tokio::task::yield_now().await;
        for (delay, expected) in [(1, 2), (2, 3), (4, 4), (4, 5)] {
            advance(Duration::from_millis(delay * 1000 - 1)).await;
            assert_eq!(spawns.load(Ordering::Acquire), expected - 1);
            advance(Duration::from_millis(1)).await;
            assert_eq!(spawns.load(Ordering::Acquire), expected);
        }
        supervisor.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn budget_exhaustion_fires_fatal_hook_once() {
        let _counter_guard = WORKER_RESTART_BUDGET_TEST_MUTEX.lock().await;
        WORKER_RESTART_BUDGET_EXHAUSTED_COUNT.store(0, Ordering::Release);
        let spawns = Arc::new(AtomicUsize::new(0));
        let factory_spawns = spawns.clone();
        let fatal_calls = Arc::new(AtomicUsize::new(0));
        let mut spec = restartable_spec();
        spec.name = "test_budget_exhaustion_worker";
        spec.restart_policy = WorkerRestartPolicy::RestartableWithBudget(test_budget(2));
        supervise_worker_local(
            spec,
            Arc::new(AtomicBool::new(false)),
            move || {
                let spawns = factory_spawns.clone();
                async move {
                    spawns.fetch_add(1, Ordering::AcqRel);
                }
            },
            |_, _, _, _| {},
            counting_fatal_hook(fatal_calls.clone()),
        )
        .await;
        // max_restarts=2 → 1 initial + 2 restarts = 3 spawns, then no 4th.
        assert_eq!(spawns.load(Ordering::Acquire), 3);
        // Fatal hook fires exactly once on exhaustion (readiness/exit circuit).
        assert_eq!(fatal_calls.load(Ordering::Acquire), 1);
        assert_eq!(
            WORKER_RESTART_BUDGET_EXHAUSTED_COUNT.load(Ordering::Acquire),
            1
        );
        let state = WORKER_RECOVERY_STATES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(spec.name)
            .cloned()
            .expect("recovery state");
        assert_eq!(state.classification, RecoveryClassification::Exhausted);
        assert_eq!(state.recent_restart_count, 2);
        assert_eq!(state.last_reason, "returned");
        assert_eq!(state.observed_at, Instant::now());
        // Exhausted → Unhealthy health reason for readiness 503.
        let reasons = recovery_health_reasons();
        let exhausted = reasons
            .iter()
            .find(|reason| reason.reason.contains(spec.name))
            .expect("exhausted health reason");
        assert_eq!(exhausted.severity, RecoveryReasonSeverity::Unhealthy);
        assert_eq!(
            exhausted.reason,
            format!("worker_local_restart_budget_exhausted:{}", spec.name)
        );
        clear_recovery_state(spec.name);
    }

    #[tokio::test(start_paused = true)]
    async fn stable_run_resets_backoff() {
        let spawns = Arc::new(AtomicUsize::new(0));
        let factory_spawns = spawns.clone();
        let mut spec = restartable_spec();
        spec.restart_policy = WorkerRestartPolicy::RestartableWithBudget(test_budget(10));
        let supervisor = tokio::spawn(supervise_worker_local(
            spec,
            Arc::new(AtomicBool::new(false)),
            move || {
                let ordinal = factory_spawns.fetch_add(1, Ordering::AcqRel);
                async move {
                    if ordinal == 1 {
                        tokio::time::sleep(Duration::from_secs(4)).await;
                    }
                }
            },
            |_, _, _, _| {},
            noop_fatal_hook(),
        ));
        tokio::task::yield_now().await;
        advance(Duration::from_secs(1)).await;
        assert_eq!(spawns.load(Ordering::Acquire), 2);
        advance(Duration::from_secs(4)).await;
        advance(Duration::from_millis(999)).await;
        assert_eq!(spawns.load(Ordering::Acquire), 2);
        advance(Duration::from_millis(1)).await;
        assert_eq!(spawns.load(Ordering::Acquire), 3);
        supervisor.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn simultaneous_return_and_shutdown_is_expected_without_budget_use() {
        let _counter_guard = WORKER_RESTART_BUDGET_TEST_MUTEX.lock().await;
        WORKER_RESTART_BUDGET_EXHAUSTED_COUNT.store(0, Ordering::Release);
        let shutdown = Arc::new(AtomicBool::new(false));
        let factory_shutdown = shutdown.clone();
        let spawns = Arc::new(AtomicUsize::new(0));
        let factory_spawns = spawns.clone();
        let expected_terminal = Arc::new(AtomicBool::new(false));
        let observed_expected = expected_terminal.clone();
        let restart_attempt = Arc::new(AtomicUsize::new(usize::MAX));
        let observed_attempt = restart_attempt.clone();
        let mut spec = restartable_spec();
        spec.name = "test_simultaneous_shutdown_worker";

        supervise_worker_local(
            spec,
            shutdown,
            move || {
                let shutdown = factory_shutdown.clone();
                let spawns = factory_spawns.clone();
                async move {
                    spawns.fetch_add(1, Ordering::AcqRel);
                    shutdown.store(true, Ordering::Release);
                }
            },
            move |_, expected_shutdown, _, attempt| {
                observed_expected.store(expected_shutdown, Ordering::Release);
                observed_attempt.store(attempt, Ordering::Release);
            },
            noop_fatal_hook(),
        )
        .await;

        assert_eq!(spawns.load(Ordering::Acquire), 1);
        assert!(expected_terminal.load(Ordering::Acquire));
        assert_eq!(restart_attempt.load(Ordering::Acquire), 0);
        assert_eq!(
            WORKER_RESTART_BUDGET_EXHAUSTED_COUNT.load(Ordering::Acquire),
            0
        );
        assert!(
            WORKER_RECOVERY_STATES
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(spec.name)
                .is_none(),
            "normal shutdown must not consume or persist restart budget state"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn no_restart_during_shutdown_backoff() {
        let spawns = Arc::new(AtomicUsize::new(0));
        let factory_spawns = spawns.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let supervisor = tokio::spawn(supervise_worker_local(
            restartable_spec(),
            shutdown.clone(),
            move || {
                let spawns = factory_spawns.clone();
                async move {
                    spawns.fetch_add(1, Ordering::AcqRel);
                }
            },
            |_, _, _, _| {},
            noop_fatal_hook(),
        ));
        tokio::task::yield_now().await;
        shutdown.store(true, Ordering::Release);
        advance(Duration::from_secs(1)).await;
        supervisor.await.expect("supervisor exits");
        assert_eq!(spawns.load(Ordering::Acquire), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_waits_for_inner_cleanup() {
        for mut spec in [loop_owned_spec(), restartable_spec()] {
            let shutdown = Arc::new(AtomicBool::new(false));
            let cleanup_ran = Arc::new(AtomicBool::new(false));
            let worker_shutdown = shutdown.clone();
            let worker_cleanup = cleanup_ran.clone();
            let expected_terminal = Arc::new(AtomicBool::new(false));
            let observed_expected = expected_terminal.clone();
            if matches!(
                spec.restart_policy,
                WorkerRestartPolicy::RestartableWithBudget(_)
            ) {
                spec.restart_policy = WorkerRestartPolicy::RestartableWithBudget(test_budget(2));
            }

            let supervisor = tokio::spawn(supervise_worker_local(
                spec,
                shutdown.clone(),
                move || {
                    let worker_shutdown = worker_shutdown.clone();
                    let worker_cleanup = worker_cleanup.clone();
                    async move {
                        wait_until_shutdown(worker_shutdown).await;
                        worker_cleanup.store(true, Ordering::Release);
                    }
                },
                move |_, expected_shutdown, _, _| {
                    observed_expected.store(expected_shutdown, Ordering::Release);
                },
                noop_fatal_hook(),
            ));
            tokio::task::yield_now().await;
            shutdown.store(true, Ordering::Release);
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
            supervisor.await.expect("supervisor exits after cleanup");
            assert!(cleanup_ran.load(Ordering::Acquire));
            if spec.restart_policy == WorkerRestartPolicy::LoopOwned {
                assert!(expected_terminal.load(Ordering::Acquire));
            }
        }
    }

    #[tokio::test]
    async fn loop_owned_worker_remains_non_restartable() {
        let spawns = Arc::new(AtomicUsize::new(0));
        let terminal = Arc::new(AtomicUsize::new(0));
        let factory_spawns = spawns.clone();
        let observed_terminal = terminal.clone();
        supervise_worker_local(
            loop_owned_spec(),
            Arc::new(AtomicBool::new(false)),
            move || {
                let spawns = factory_spawns.clone();
                async move {
                    spawns.fetch_add(1, Ordering::AcqRel);
                }
            },
            move |_, _, auto_restart, _| {
                assert!(!auto_restart);
                observed_terminal.fetch_add(1, Ordering::AcqRel);
            },
            noop_fatal_hook(),
        )
        .await;
        clear_recovery_state(loop_owned_spec().name);
        assert_eq!(spawns.load(Ordering::Acquire), 1);
        assert_eq!(terminal.load(Ordering::Acquire), 1);
        assert_eq!(
            DEFAULT_WORKER_LOCAL_RESTART_BUDGET.max_restarts, 5,
            "production policy remains explicit"
        );
    }

    #[tokio::test]
    async fn loop_owned_unexpected_exit_exposes_terminated_reason() {
        let mut spec = loop_owned_spec();
        spec.name = "test_loop_owned_terminated_worker";
        supervise_worker_local(
            spec,
            Arc::new(AtomicBool::new(false)),
            move || async move {},
            |_, _, _, _| {},
            noop_fatal_hook(),
        )
        .await;
        let reasons = recovery_health_reasons();
        let terminated = reasons
            .iter()
            .find(|reason| reason.reason.contains(spec.name))
            .expect("loop-owned terminated health reason");
        assert_eq!(terminated.severity, RecoveryReasonSeverity::Degraded);
        assert_eq!(
            terminated.reason,
            format!("worker_local_loop_owned_terminated:{}", spec.name)
        );
        // A LoopOwned unexpected termination is Degraded, never a flapping entry.
        assert!(
            !recovery_flapping_info()
                .iter()
                .any(|value| value.as_str().is_some_and(|s| s.contains(spec.name)))
        );
        clear_recovery_state(spec.name);
    }

    #[tokio::test(start_paused = true)]
    async fn flapping_is_info_only_and_not_a_health_reason() {
        let spawns = Arc::new(AtomicUsize::new(0));
        let factory_spawns = spawns.clone();
        let mut spec = restartable_spec();
        spec.name = "test_flapping_worker";
        spec.restart_policy = WorkerRestartPolicy::RestartableWithBudget(test_budget(10));
        let supervisor = tokio::spawn(supervise_worker_local(
            spec,
            Arc::new(AtomicBool::new(false)),
            move || {
                let spawns = factory_spawns.clone();
                async move {
                    spawns.fetch_add(1, Ordering::AcqRel);
                }
            },
            |_, _, _, _| {},
            noop_fatal_hook(),
        ));
        tokio::task::yield_now().await;
        advance(Duration::from_secs(1)).await;
        assert!(spawns.load(Ordering::Acquire) >= 2);
        // Flapping must not worsen health (deploy-gate safety, §9.3) ...
        assert!(
            !recovery_health_reasons()
                .iter()
                .any(|reason| reason.reason.contains(spec.name))
        );
        // ... but is visible as an informational field.
        assert!(
            recovery_flapping_info()
                .iter()
                .any(|value| value.as_str().is_some_and(|s| s.contains(spec.name)))
        );
        supervisor.abort();
        clear_recovery_state(spec.name);
    }

    #[test]
    fn cross_process_guard_holds_without_ledger_path() {
        assert_eq!(
            cross_process_fatal_decision_at(None, "dispatch_outbox", 1_000),
            CrossProcessDecision::HoldWithoutExit {
                recent_fatal_exits: 0
            }
        );
    }

    #[test]
    fn cross_process_guard_holds_after_repeated_fatal_exits() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(FATAL_EXIT_LEDGER_FILE);
        let worker = "dispatch_outbox";
        let base = 1_000_000_000_000_i64;

        // First two fatal exits inside the window still exit (launchd may help).
        assert_eq!(
            record_and_check_cross_process_fatal_at(&path, worker, base),
            CrossProcessDecision::Exit
        );
        assert_eq!(
            record_and_check_cross_process_fatal_at(&path, worker, base + 1_000),
            CrossProcessDecision::Exit
        );
        // Third within 30min: the guard proves restarting will not help → hold.
        assert_eq!(
            record_and_check_cross_process_fatal_at(&path, worker, base + 2_000),
            CrossProcessDecision::HoldWithoutExit {
                recent_fatal_exits: 2
            }
        );
    }

    #[test]
    fn cross_process_guard_holds_when_ledger_persistence_fails() {
        let dir = tempfile::tempdir().expect("temp dir");
        let blocking_parent = dir.path().join("not-a-directory");
        std::fs::write(&blocking_parent, b"file").expect("blocking parent file");
        let path = blocking_parent.join(FATAL_EXIT_LEDGER_FILE);

        assert_eq!(
            record_and_check_cross_process_fatal_at(&path, "dispatch_outbox", 1_000),
            CrossProcessDecision::HoldWithoutExit {
                recent_fatal_exits: 0
            }
        );
    }

    #[test]
    fn fatal_ledger_atomic_write_preserves_target_when_temp_write_fails() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(FATAL_EXIT_LEDGER_FILE);
        let original = vec![FatalExitLedgerEntry {
            worker: "original".to_string(),
            observed_unix_ms: 1_000,
        }];
        std::fs::write(&path, serde_json::to_vec_pretty(&original).unwrap()).expect("seed ledger");
        let temp_path = fatal_ledger_temp_path(&path).expect("temp path");
        std::fs::create_dir(&temp_path).expect("block temp-file creation");

        let replacement = vec![FatalExitLedgerEntry {
            worker: "replacement".to_string(),
            observed_unix_ms: 2_000,
        }];
        assert!(save_fatal_ledger(&path, &replacement).is_err());
        let persisted: Vec<FatalExitLedgerEntry> =
            serde_json::from_slice(&std::fs::read(&path).expect("read ledger"))
                .expect("valid original ledger");
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].worker, "original");
    }

    #[test]
    fn cross_process_guard_ignores_future_entries_after_backward_clock_skew() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(FATAL_EXIT_LEDGER_FILE);
        let entries = vec![
            FatalExitLedgerEntry {
                worker: "session_discovery".to_string(),
                observed_unix_ms: 10_000,
            },
            FatalExitLedgerEntry {
                worker: "session_discovery".to_string(),
                observed_unix_ms: 11_000,
            },
        ];
        save_fatal_ledger(&path, &entries).expect("seed future ledger");

        assert_eq!(
            record_and_check_cross_process_fatal_at(&path, "session_discovery", 1_000),
            CrossProcessDecision::Exit
        );
    }

    #[test]
    fn cross_process_guard_resets_after_window() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(FATAL_EXIT_LEDGER_FILE);
        let worker = "session_discovery";
        let base = 2_000_000_000_000_i64;
        let past_window = FATAL_CROSS_PROCESS_WINDOW.as_millis() as i64 + 1;

        record_and_check_cross_process_fatal_at(&path, worker, base);
        record_and_check_cross_process_fatal_at(&path, worker, base + 1_000);
        // The two prior exits age out of the window, so exiting is worthwhile again.
        assert_eq!(
            record_and_check_cross_process_fatal_at(&path, worker, base + past_window),
            CrossProcessDecision::Exit
        );
    }

    #[test]
    fn cross_process_guard_is_per_worker() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(FATAL_EXIT_LEDGER_FILE);
        let base = 3_000_000_000_000_i64;

        record_and_check_cross_process_fatal_at(&path, "dispatch_outbox", base);
        record_and_check_cross_process_fatal_at(&path, "dispatch_outbox", base + 1);
        // A different worker is unaffected by dispatch_outbox's history.
        assert_eq!(
            record_and_check_cross_process_fatal_at(&path, "session_discovery", base + 2),
            CrossProcessDecision::Exit
        );
    }
}
