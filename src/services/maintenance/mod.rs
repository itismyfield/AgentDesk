//! ADK-internal maintenance job infrastructure (#1091 / 909-2).
//!
//! Provides a dynamic, in-process registry for lightweight periodic background
//! work. Unlike the static `server::maintenance` subsystem (which drives
//! postgres-pool-bound jobs keyed into `kv_meta`), this module is intentionally
//! self-contained:
//!
//!   * Callers register jobs via [`register_maintenance_job`] at startup —
//!     no DB schema, no trait impls, just `(name, interval, async fn)`.
//!   * The scheduler loop ticks every [`DEFAULT_TICK_INTERVAL`] and, for every
//!     job whose `last_run + interval <= now`, spawns the handler on a detached
//!     `tokio::spawn` so a long-running job never blocks siblings.
//!   * Results (success/failure + duration) are recorded in-memory for the
//!     `/api/cron-jobs` surface and emitted as structured observability events
//!     via [`crate::services::observability::events::record_simple`] (landed in
//!     #1070).
//!
//! Intended use: per-agent reconciliation tasks, cache warmers, soft-TTL
//! sweepers — anything that needs to run "every N seconds/minutes" without the
//! ceremony of adding a row to `server::worker_registry` or a kv_meta key.
//!
//! 909-3 will register the first real job against this surface.

use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use chrono::Utc;
use futures::future::BoxFuture;
use serde::Serialize;
use serde_json::json;

/// How often the scheduler wakes up to re-check job schedules. Individual jobs
/// fire on their own `interval`; this tick just bounds scheduling latency.
pub const DEFAULT_TICK_INTERVAL: Duration = Duration::from_secs(30);

/// Handler signature: takes no arguments (closures capture any state they
/// need) and returns a `BoxFuture<'static, Result<()>>`.
pub type MaintenanceHandler = Arc<dyn Fn() -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;

/// A registered maintenance job. Constructed via [`register_maintenance_job`].
#[derive(Clone)]
pub struct MaintenanceJob {
    pub name: String,
    pub interval: Duration,
    pub handler: MaintenanceHandler,
}

impl std::fmt::Debug for MaintenanceJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MaintenanceJob")
            .field("name", &self.name)
            .field("interval", &self.interval)
            .finish_non_exhaustive()
    }
}

/// Snapshot of a job's runtime state. Tracked in-memory alongside the
/// registry; exposed through [`list_maintenance_jobs`].
#[derive(Debug, Clone, Default)]
struct JobState {
    /// `Instant` of the last completed run (monotonic — used for scheduling).
    last_run_instant: Option<Instant>,
    /// Wall-clock ms of the last completed run (for API output).
    last_run_at_ms: Option<i64>,
    /// `"ok" | "error" | "running" | "never"`.
    last_status: String,
    last_error: Option<String>,
    last_duration_ms: Option<i64>,
    run_count: i64,
    failure_count: i64,
    /// Set when the job is currently running so the scheduler does not
    /// double-dispatch if a previous invocation is still in flight.
    in_flight: bool,
}

/// Public-facing snapshot for [`/api/cron-jobs`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceJobInfo {
    pub id: String,
    pub name: String,
    pub source: &'static str,
    pub enabled: bool,
    pub schedule: ScheduleInfo,
    pub state: StateInfo,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleInfo {
    pub kind: &'static str,
    pub every_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StateInfo {
    pub status: &'static str,
    pub last_status: String,
    pub last_run_at_ms: Option<i64>,
    pub next_run_at_ms: Option<i64>,
    pub last_duration_ms: Option<i64>,
    pub last_error: Option<String>,
    pub run_count: i64,
    pub failure_count: i64,
}

/// Combined `(job, state)` entry inside the registry.
struct RegistryEntry {
    job: MaintenanceJob,
    state: JobState,
}

type Registry = RwLock<Vec<RegistryEntry>>;

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(Vec::new()))
}

/// Register a maintenance job to be driven by [`spawn_maintenance_scheduler`].
///
/// Idempotent on `name`: re-registering the same name replaces the handler
/// and interval (the state counters are preserved). This keeps restart-loops
/// and test re-registrations safe.
pub fn register_maintenance_job<F>(name: impl Into<String>, interval: Duration, handler: F)
where
    F: Fn() -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync + 'static,
{
    let name = name.into();
    let job = MaintenanceJob {
        name: name.clone(),
        interval,
        handler: Arc::new(handler),
    };

    let Ok(mut guard) = registry().write() else {
        tracing::warn!(
            job = %name,
            "[maintenance] registry poisoned; dropping registration"
        );
        return;
    };

    if let Some(existing) = guard.iter_mut().find(|entry| entry.job.name == name) {
        existing.job = job;
        tracing::info!(job = %name, "[maintenance] job re-registered (handler replaced)");
    } else {
        guard.push(RegistryEntry {
            job,
            state: JobState {
                last_status: "never".to_string(),
                ..JobState::default()
            },
        });
        tracing::info!(job = %name, "[maintenance] job registered");
    }
}

/// Snapshot of every registered job, for `/api/cron-jobs`.
pub fn list_maintenance_jobs() -> Vec<MaintenanceJobInfo> {
    let Ok(guard) = registry().read() else {
        return Vec::new();
    };
    guard
        .iter()
        .map(|entry| {
            let every_ms = duration_to_i64_ms(entry.job.interval);
            let next_run_at_ms = match (entry.state.last_run_at_ms, every_ms) {
                (Some(last), every) if every > 0 => Some(last.saturating_add(every)),
                _ => None,
            };
            MaintenanceJobInfo {
                id: format!("maintenance:{}", entry.job.name),
                name: entry.job.name.clone(),
                source: "maintenance",
                enabled: true,
                schedule: ScheduleInfo {
                    kind: "every",
                    every_ms,
                },
                state: StateInfo {
                    status: "active",
                    last_status: entry.state.last_status.clone(),
                    last_run_at_ms: entry.state.last_run_at_ms,
                    next_run_at_ms,
                    last_duration_ms: entry.state.last_duration_ms,
                    last_error: entry.state.last_error.clone(),
                    run_count: entry.state.run_count,
                    failure_count: entry.state.failure_count,
                },
            }
        })
        .collect()
}

/// Spawn the maintenance scheduler task. The task runs until the tokio
/// runtime shuts down. Call once during server boot (under `#[cfg(not(test))]`).
///
/// Scheduling model: every [`DEFAULT_TICK_INTERVAL`] the scheduler iterates
/// the registry; for each job whose `last_run + interval <= now` (or which
/// has never run), it spawns `tokio::spawn(handler())`. The scheduler itself
/// never `.await`s the handler — long-running jobs do not block the tick.
pub async fn spawn_maintenance_scheduler() {
    run_scheduler_loop(DEFAULT_TICK_INTERVAL, None).await;
}

/// Core loop. Extracted so tests can control tick cadence and bounded runs.
async fn run_scheduler_loop(tick: Duration, max_iterations: Option<usize>) {
    let mut iteration = 0usize;
    tracing::info!(
        tick_ms = duration_to_i64_ms(tick),
        "[maintenance] scheduler loop started"
    );
    loop {
        // Snapshot the due jobs under the write lock so we can mark
        // `in_flight = true` atomically with the "should we dispatch" check.
        let due: Vec<(String, MaintenanceHandler)> = {
            let Ok(mut guard) = registry().write() else {
                tracing::warn!("[maintenance] registry poisoned; scheduler idling");
                tokio::time::sleep(tick).await;
                continue;
            };
            let now = Instant::now();
            let mut collected = Vec::new();
            for entry in guard.iter_mut() {
                if entry.state.in_flight {
                    continue;
                }
                let due = match entry.state.last_run_instant {
                    None => true,
                    Some(last) => now.saturating_duration_since(last) >= entry.job.interval,
                };
                if due {
                    entry.state.in_flight = true;
                    collected.push((entry.job.name.clone(), entry.job.handler.clone()));
                }
            }
            collected
        };

        for (name, handler) in due {
            tokio::spawn(run_job_and_record(name, handler));
        }

        iteration = iteration.saturating_add(1);
        if max_iterations.is_some_and(|limit| iteration >= limit) {
            return;
        }
        tokio::time::sleep(tick).await;
    }
}

async fn run_job_and_record(name: String, handler: MaintenanceHandler) {
    let started_ms = Utc::now().timestamp_millis();
    let started = Instant::now();
    tracing::info!(job = %name, "[maintenance] job started");

    // Handler may panic — isolate behind an inner `tokio::spawn` so a rogue
    // job surfaces as `Err(JoinError)` here instead of bubbling up through
    // the outer detached task.
    let future = handler();
    let outcome = tokio::task::spawn(future).await;

    let elapsed = started.elapsed();
    let elapsed_ms = duration_to_i64_ms(elapsed);
    let finished_ms = Utc::now().timestamp_millis();

    let (status_text, error_text): (&'static str, Option<String>) = match outcome {
        Ok(Ok(())) => ("ok", None),
        Ok(Err(error)) => ("error", Some(error.to_string())),
        Err(join_error) => ("error", Some(format!("handler panicked: {join_error}"))),
    };

    // Update in-memory state.
    if let Ok(mut guard) = registry().write() {
        if let Some(entry) = guard.iter_mut().find(|entry| entry.job.name == name) {
            entry.state.in_flight = false;
            entry.state.last_run_instant = Some(started);
            entry.state.last_run_at_ms = Some(finished_ms);
            entry.state.last_duration_ms = Some(elapsed_ms);
            entry.state.run_count = entry.state.run_count.saturating_add(1);
            entry.state.last_status = status_text.to_string();
            if status_text == "error" {
                entry.state.failure_count = entry.state.failure_count.saturating_add(1);
                entry.state.last_error = error_text.clone();
            } else {
                entry.state.last_error = None;
            }
        }
    }

    // Structured event (#1070 observability integration).
    let payload = json!({
        "job": name,
        "status": status_text,
        "duration_ms": elapsed_ms,
        "started_ms": started_ms,
        "finished_ms": finished_ms,
        "error": error_text,
    });
    crate::services::observability::events::record_simple(
        "maintenance_job_completed",
        None,
        None,
        payload,
    );

    match (status_text, &error_text) {
        ("ok", _) => tracing::info!(
            job = %name,
            duration_ms = elapsed_ms,
            outcome = "ok",
            "[maintenance] job completed"
        ),
        ("error", Some(message)) => tracing::warn!(
            job = %name,
            duration_ms = elapsed_ms,
            outcome = "error",
            error = %message,
            "[maintenance] job completed"
        ),
        _ => {}
    }
}

fn duration_to_i64_ms(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
fn reset_registry_for_tests() {
    if let Ok(mut guard) = registry().write() {
        guard.clear();
    }
}

/// Process-global test serialization lock for any test that touches the
/// maintenance registry (both this module's unit tests and integration tests
/// in `server::routes::routes_tests`). `cargo test` runs tests on multiple
/// threads and the registry is process-global — without this, concurrent
/// `reset_registry_for_tests()` calls race with registrations from other
/// tests. Acquire this at the top of any such test.
#[cfg(test)]
pub(crate) fn test_serialization_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::Mutex;
    static GUARD: Mutex<()> = Mutex::new(());
    GUARD.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn lock_tests() -> std::sync::MutexGuard<'static, ()> {
        // Share the process-global serialization lock with cross-module
        // tests so `reset_registry_for_tests()` can't race with other
        // tests' registrations.
        test_serialization_lock()
    }

    #[tokio::test]
    async fn register_adds_job_to_registry() {
        let _guard = lock_tests();
        reset_registry_for_tests();

        register_maintenance_job("test.noop", Duration::from_secs(60), || {
            Box::pin(async { Ok(()) })
        });

        let listed = list_maintenance_jobs();
        assert_eq!(listed.len(), 1, "one job should be registered");
        let info = &listed[0];
        assert_eq!(info.id, "maintenance:test.noop");
        assert_eq!(info.name, "test.noop");
        assert_eq!(info.source, "maintenance");
        assert!(info.enabled);
        assert_eq!(info.schedule.every_ms, 60_000);
        assert_eq!(info.state.last_status, "never");
        assert_eq!(info.state.run_count, 0);
        assert_eq!(info.state.failure_count, 0);
        assert!(info.state.last_run_at_ms.is_none());
    }

    #[tokio::test]
    async fn scheduler_spawns_due_job_and_records_result() {
        let _guard = lock_tests();
        reset_registry_for_tests();

        let counter = Arc::new(AtomicUsize::new(0));
        let counter_handle = counter.clone();
        register_maintenance_job("test.counter", Duration::from_millis(1), move || {
            let c = counter_handle.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        });

        // One scheduler iteration dispatches every due job; yield so the
        // detached `tokio::spawn` tasks can finish before we assert.
        run_scheduler_loop(Duration::from_millis(1), Some(1)).await;
        for _ in 0..20 {
            if counter.load(Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        assert!(
            counter.load(Ordering::SeqCst) >= 1,
            "handler should have fired at least once"
        );

        // Allow bookkeeping write to settle.
        for _ in 0..20 {
            let listed = list_maintenance_jobs();
            if listed.first().map(|info| info.state.run_count).unwrap_or(0) >= 1 {
                assert_eq!(listed[0].state.last_status, "ok");
                assert_eq!(listed[0].state.failure_count, 0);
                assert!(listed[0].state.last_run_at_ms.is_some());
                assert!(listed[0].state.last_duration_ms.is_some());
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("run_count did not reach 1");
    }

    #[tokio::test]
    async fn scheduler_handles_job_error_without_panicking() {
        let _guard = lock_tests();
        reset_registry_for_tests();

        register_maintenance_job("test.always_err", Duration::from_millis(1), || {
            Box::pin(async { Err(anyhow::anyhow!("synthetic failure")) })
        });

        run_scheduler_loop(Duration::from_millis(1), Some(1)).await;

        // Wait for detached task to finish and update state.
        for _ in 0..40 {
            let listed = list_maintenance_jobs();
            if let Some(info) = listed.first() {
                if info.state.run_count >= 1 {
                    assert_eq!(info.state.last_status, "error");
                    assert_eq!(info.state.failure_count, 1);
                    assert!(
                        info.state
                            .last_error
                            .as_deref()
                            .unwrap_or("")
                            .contains("synthetic failure")
                    );
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("error outcome not recorded");
    }
}
