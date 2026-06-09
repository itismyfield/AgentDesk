//! ADK-internal maintenance job registry surface (#1091 / 909-2).
//!
//! Exposes the read-only [`list_maintenance_jobs`] snapshot consumed by the
//! `/api/cron-jobs` route (`server::routes::cron_api`). The in-process job
//! registry it reads is currently never populated — the dynamic scheduler that
//! used to register jobs here was never wired into server boot — so the
//! snapshot is presently always empty. The surface is retained because the
//! cron API contract depends on it.

use std::sync::{OnceLock, RwLock};
use std::time::Duration;

use serde::Serialize;

/// A registered maintenance job. The registry is not currently populated.
#[derive(Clone, Debug)]
struct MaintenanceJob {
    name: String,
    interval: Duration,
}

/// Snapshot of a job's runtime state. Tracked in-memory alongside the
/// registry; exposed through [`list_maintenance_jobs`].
#[derive(Debug, Clone, Default)]
struct JobState {
    /// Wall-clock ms of the last completed run (for API output).
    last_run_at_ms: Option<i64>,
    /// `"ok" | "error" | "running" | "never"`.
    last_status: String,
    last_error: Option<String>,
    last_duration_ms: Option<i64>,
    run_count: i64,
    failure_count: i64,
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

fn duration_to_i64_ms(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}
