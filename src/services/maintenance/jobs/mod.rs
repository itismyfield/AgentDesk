//! Storage maintenance jobs (#1092 / 909-3; extended by #1093 / 909-4).
//!
//! This module registers long-running housekeeping jobs against the dynamic
//! maintenance scheduler introduced in #1091 (909-2). Each job is a thin wrapper
//! that produces a `BoxFuture` and is registered via
//! [`crate::services::maintenance::register_maintenance_job`].
//!
//! The jobs registered here:
//!
//!   * `storage.target_sweep` — monthly (~30d). Runs `cargo sweep --time 30` in
//!     the main workspace `target/` dir if disk usage exceeds 50 GB OR the 30d
//!     cadence has elapsed. Reports removed-file counts via `tracing::info!`.
//!   * `storage.worktree_orphan_sweep` — hourly. Scans
//!     `~/.adk/release/worktrees/` and cross-checks each dir against active
//!     PG dispatches (`status IN ('pending','dispatched')`). Orphaned dirs
//!     (no matching active dispatch) are removed via `git worktree remove`
//!     + directory delete.
//!   * `storage.hang_dump_cleanup` — weekly. Deletes `adk-hang-*.txt` files
//!     older than 14 days from the `logs/` directory.
//!   * `storage.db_retention` — weekly. Applies retention policies to
//!     postgres tables (7/30/90d horizons). Requires a live `PgPool`; if
//!     postgres is disabled, this job is skipped (remaining jobs still
//!     register).
//!   * `memory.memento_consolidation` — weekly (#1089 / 908-7). Calls the
//!     memento MCP `memory_consolidate` tool to merge low-importance /
//!     duplicate fragments. No-ops when memento is not configured.
//!   * `reconcile.zombie_resources` — hourly (#1076 / 905-7). Sweeps stale
//!     inflight state files, unrelocated `discord_uploads/*`, and any other
//!     zombie resources (see `crate::reconcile::reconcile_zombie_resources`).
//!
//! Log rotation for `dcserver.stdout.log` / `dcserver.stderr.log` is intentionally
//! deferred to a follow-up — it requires wiring `tracing-appender::rolling` into
//! the existing `logging.rs` subscriber init, which is out of scope for this PR.

use std::time::Duration;

use sqlx::PgPool;

use crate::services::maintenance::register_maintenance_job;

pub mod db_retention;
pub mod hang_dump_cleanup;
pub mod memento_consolidation;
pub mod target_sweep;
pub mod worktree_orphan_sweep;

/// Weekly cadence for postgres-backed retention jobs. Long enough that a single
/// missed tick is not a crisis, short enough that retention horizons (7/30/90d)
/// are never breached by more than a week.
pub const STORAGE_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Register all storage maintenance jobs. Call from server boot under
/// `#[cfg(not(test))]`.
///
/// The PG pool is optional — worktree orphan sweep degrades to a no-op when
/// Postgres is not configured, and the db_retention job is skipped entirely
/// when no pool is available (since it is postgres-only).
pub fn spawn_storage_maintenance_jobs(pg_pool: Option<PgPool>) {
    let pool_for_worktree = pg_pool.clone();

    // Monthly target/ sweep. 30d interval; handler also triggers on 50GB threshold.
    register_maintenance_job(
        "storage.target_sweep",
        Duration::from_secs(30 * 24 * 60 * 60),
        || Box::pin(target_sweep::run(target_sweep::Config::default_runtime())),
    );

    // Hourly worktree orphan sweep.
    register_maintenance_job(
        "storage.worktree_orphan_sweep",
        Duration::from_secs(60 * 60),
        move || {
            let pool = pool_for_worktree.clone();
            Box::pin(async move {
                let config = worktree_orphan_sweep::Config::default_runtime();
                worktree_orphan_sweep::run(config, pool).await
            })
        },
    );

    // Weekly hang dump cleanup.
    register_maintenance_job(
        "storage.hang_dump_cleanup",
        Duration::from_secs(7 * 24 * 60 * 60),
        || {
            Box::pin(async {
                hang_dump_cleanup::run(hang_dump_cleanup::Config::default_runtime()).await
            })
        },
    );

    // Weekly postgres retention sweep (#1093). Postgres-only; skipped if no pool.
    match pg_pool {
        Some(pool) => register_db_retention(pool),
        None => {
            tracing::info!(
                "[maintenance] storage.db_retention skipped (postgres pool unavailable)"
            );
        }
    }

    // Weekly memento consolidation (#1089 / 908-7). Self-skips if memento is
    // not configured, so registration is unconditional.
    register_maintenance_job(
        "memory.memento_consolidation",
        memento_consolidation::DEFAULT_INTERVAL,
        || {
            Box::pin(async {
                memento_consolidation::run(memento_consolidation::Config::default_runtime()).await
            })
        },
    );

    // Hourly zombie resource reconcile (#1076 / 905-7). No PG pool needed —
    // the file-system sweeps degrade gracefully when AGENTDESK_ROOT_DIR is
    // unset. Discord-runtime-facing zombie checks (tmux orphans, DashMap
    // trimming) are driven separately by the Discord bot's own tick loop
    // and merely *report* their counts here via tracing.
    register_maintenance_job(
        "reconcile.zombie_resources",
        Duration::from_secs(60 * 60),
        || {
            Box::pin(async {
                let _stats = crate::reconcile::reconcile_zombie_resources().await;
                Ok(())
            })
        },
    );
}

fn register_db_retention(pool: PgPool) {
    register_maintenance_job(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::maintenance::{
        list_maintenance_jobs, reset_registry_for_tests, test_serialization_lock,
    };

    #[test]
    fn spawn_storage_jobs_registers_memento_consolidation() {
        let _guard = test_serialization_lock();
        reset_registry_for_tests();

        spawn_storage_maintenance_jobs(None);

        let jobs = list_maintenance_jobs();
        let memento = jobs
            .iter()
            .find(|info| info.name == "memory.memento_consolidation")
            .expect("memory.memento_consolidation should be registered");

        // Weekly cadence: 7 * 24 * 60 * 60 * 1000 ms.
        assert_eq!(memento.schedule.every_ms, 604_800_000);
        assert!(memento.enabled);
        assert_eq!(memento.state.last_status, "never");
    }

    /// #1076 (905-7): the zombie reconcile job must be registered on hourly
    /// cadence so orphan tmux / stale inflight / unrelocated uploads get
    /// swept between boots.
    #[test]
    fn spawn_storage_jobs_registers_zombie_reconcile_hourly() {
        let _guard = test_serialization_lock();
        reset_registry_for_tests();

        spawn_storage_maintenance_jobs(None);

        let jobs = list_maintenance_jobs();
        let zombie = jobs
            .iter()
            .find(|info| info.name == "reconcile.zombie_resources")
            .expect("reconcile.zombie_resources should be registered");

        // Hourly cadence: 60 * 60 * 1000 ms.
        assert_eq!(zombie.schedule.every_ms, 3_600_000);
        assert!(zombie.enabled);
        assert_eq!(zombie.state.last_status, "never");
    }
}
