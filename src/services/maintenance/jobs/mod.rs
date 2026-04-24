//! Maintenance job handlers (#1092 / 909-3 pattern; #1093 first concrete job).
//!
//! Individual job implementations live in submodules and are registered with
//! [`super::register_maintenance_job`]. Callers wire them in from
//! `server::boot` via [`spawn_storage_maintenance_jobs`], passing the live
//! `PgPool` so handlers can capture it.
//!
//! Each job is a small function (`Result<Report>`) with a companion
//! `register_*` helper that adapts it into the `() -> BoxFuture<Result<()>>`
//! signature expected by the registry.

use std::time::Duration;

use sqlx::PgPool;

pub mod db_retention;

/// Weekly cadence for storage maintenance jobs. Long enough that a single
/// missed tick is not a crisis, short enough that retention horizons (7/30/90d)
/// are never breached by more than a week.
pub const STORAGE_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Register every storage-retention job against the maintenance scheduler.
///
/// Called once from boot after the `PgPool` is established. If `pool` is
/// `None` (postgres disabled), nothing is registered — the jobs are postgres-
/// only.
pub fn spawn_storage_maintenance_jobs(pool: Option<PgPool>) {
    let Some(pool) = pool else {
        tracing::info!("[maintenance] storage jobs skipped (postgres pool unavailable)");
        return;
    };

    register_db_retention(pool);
}

fn register_db_retention(pool: PgPool) {
    super::register_maintenance_job(
        "storage.db_retention",
        STORAGE_MAINTENANCE_INTERVAL,
        move || {
            let pool = pool.clone();
            Box::pin(async move {
                let report = db_retention::db_retention_job(&pool, false).await?;
                tracing::info!(
                    tables = ?report.summary(),
                    "[maintenance] db_retention_job completed"
                );
                Ok(())
            })
        },
    );
}
