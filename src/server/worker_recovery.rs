use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use tokio::time::Instant;

use super::worker_registry::{WorkerRestartBudget, WorkerRestartPolicy, WorkerSpec};

pub(super) const WORKER_LOCAL_SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_secs(1);

static WORKER_RESTART_BUDGET_EXHAUSTED_COUNT: AtomicUsize = AtomicUsize::new(0);
static WORKER_RECOVERY_STATES: LazyLock<Mutex<HashMap<&'static str, WorkerRecoveryState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

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

#[derive(Debug, Clone)]
struct WorkerRecoveryState {
    recent_restart_count: usize,
    last_reason: &'static str,
    exhausted: bool,
    observed_at: Instant,
}

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
) where
    MakeFuture: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
    RecordTerminal: FnMut(WorkerLocalTerminalReason, bool, bool, usize) + Send + 'static,
{
    match spec.restart_policy {
        WorkerRestartPolicy::RestartableWithBudget(budget) => {
            supervise_restartable(spec, shutdown, make_future, budget, &mut record_terminal).await;
        }
        _ => match run_worker_once(spec, shutdown, make_future()).await {
            WorkerRunOutcome::Unexpected(reason) => {
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
            record_recovery_state(spec.name, restart_times.len(), reason, true);
            WORKER_RESTART_BUDGET_EXHAUSTED_COUNT.fetch_add(1, Ordering::AcqRel);
            tracing::error!(
                worker = spec.name,
                restart = spec.restart_policy.as_doc_str(),
                reason = reason.as_doc_str(),
                restart_count = restart_times.len(),
                max_restarts = budget.max_restarts,
                window_secs = budget.window.as_secs(),
                "worker-local restart budget exhausted; leaving worker stopped"
            );
            return;
        }

        restart_times.push_back(now);
        let restart_attempt = restart_times.len();
        record_terminal(reason, false, true, restart_attempt);
        record_recovery_state(spec.name, restart_attempt, reason, false);

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
    exhausted: bool,
) {
    WORKER_RECOVERY_STATES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(
            worker,
            WorkerRecoveryState {
                recent_restart_count,
                last_reason: reason.as_doc_str(),
                exhausted,
                observed_at: Instant::now(),
            },
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
    async fn budget_exhaustion_stops_after_allowed_restarts() {
        WORKER_RESTART_BUDGET_EXHAUSTED_COUNT.store(0, Ordering::Release);
        let spawns = Arc::new(AtomicUsize::new(0));
        let factory_spawns = spawns.clone();
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
        )
        .await;
        assert_eq!(spawns.load(Ordering::Acquire), 3);
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
        assert!(state.exhausted);
        assert_eq!(state.recent_restart_count, 2);
        assert_eq!(state.last_reason, "returned");
        assert_eq!(state.observed_at, Instant::now());
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
        )
        .await;
        assert_eq!(spawns.load(Ordering::Acquire), 1);
        assert_eq!(terminal.load(Ordering::Acquire), 1);
        assert_eq!(
            DEFAULT_WORKER_LOCAL_RESTART_BUDGET.max_restarts, 5,
            "production policy remains explicit"
        );
    }
}
