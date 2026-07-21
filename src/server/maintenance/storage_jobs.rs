use std::time::Duration;

use sqlx::PgPool;

use super::{MaintenanceFuture, MaintenanceJob, MaintenanceSchedule};

/// `voice.progress_tts_cache_sweep`: 35s keeps it clear of the other voice GC
/// jobs (25/60/75s); the first run is the startup bound (#3909).
const PROGRESS_TTS_CACHE_SWEEP_STARTUP_STAGGER: Duration = Duration::from_secs(35);

/// `storage.target_sweep`: 30s keeps the monthly `target/` sweep off the
/// boot-time pile-up (the job also self-triggers on the disk threshold).
const STORAGE_TARGET_SWEEP_STARTUP_STAGGER: Duration = Duration::from_secs(30);

/// `storage.worktree_orphan_sweep`: boot offset so the hourly orphan sweep does
/// not pile on top of the other storage jobs at startup.
const STORAGE_WORKTREE_ORPHAN_SWEEP_STARTUP_STAGGER: Duration = Duration::from_secs(50);

/// `storage.tmp_pipeline_sweep`: boot offset so the daily tmp pipeline sweep
/// does not pile on top of the other storage jobs at startup.
const STORAGE_TMP_PIPELINE_SWEEP_STARTUP_STAGGER: Duration = Duration::from_secs(65);

/// `storage.hang_dump_cleanup`: boot offset so the weekly hang-dump cleanup does
/// not pile on top of the other storage jobs at startup.
const STORAGE_HANG_DUMP_CLEANUP_STARTUP_STAGGER: Duration = Duration::from_secs(70);

/// `storage.db_retention`: later boot offset so the weekly PG retention sweep
/// trails the lighter storage GCs at startup.
const STORAGE_DB_RETENTION_STARTUP_STAGGER: Duration = Duration::from_secs(90);

/// `memory.memento_consolidation`: last base offset (registry tail) so the
/// weekly memento consolidation trails every other job at startup.
const MEMORY_MEMENTO_CONSOLIDATION_STARTUP_STAGGER: Duration = Duration::from_secs(110);

/// #3909 — leader-only voice TTS cache/temp sweep (pool-less thin wrapper; the
/// sweep logic + full rationale live in
/// `services::maintenance::jobs::voice_cache_sweep`). The `config` is resolved
/// from the loaded runtime `VoiceConfig` (same dirs the TTS write path uses, so
/// operator overrides are swept). 30-minute cadence; the 35s startup stagger
/// makes the first run the startup bound.
pub(super) struct ProgressTtsCacheSweepJob {
    pub(super) config: crate::services::maintenance::jobs::voice_cache_sweep::Config,
}

impl MaintenanceJob for ProgressTtsCacheSweepJob {
    fn name(&self) -> &'static str {
        "voice.progress_tts_cache_sweep"
    }

    fn schedule(&self) -> MaintenanceSchedule {
        MaintenanceSchedule::every(
            Duration::from_secs(30 * 60),
            PROGRESS_TTS_CACHE_SWEEP_STARTUP_STAGGER,
        )
    }

    fn run<'a>(&'a self, pool: &'a PgPool) -> MaintenanceFuture<'a> {
        Box::pin(async move {
            let _ = pool;
            crate::services::maintenance::jobs::voice_cache_sweep::run(self.config.clone()).await
        })
    }
}

/// #3231 — monthly `target/` sweep. Reuses the verbatim sweep logic in
/// `services::maintenance::jobs::target_sweep` (runs `cargo sweep --time 30`
/// in the main workspace `target/` when disk usage exceeds the threshold or
/// the 30d cadence has elapsed). Pool-less. Implemented since #1092 but never
/// wired into the live scheduler — registered here so the disk-full prevention
/// from #3231 actually runs.
pub(super) struct StorageTargetSweepJob;

impl MaintenanceJob for StorageTargetSweepJob {
    fn name(&self) -> &'static str {
        "storage.target_sweep"
    }

    fn schedule(&self) -> MaintenanceSchedule {
        // ~30d cadence; the underlying job also self-triggers on the disk
        // threshold. 30s stagger keeps it off the boot-time pile-up.
        MaintenanceSchedule::every(
            Duration::from_secs(30 * 24 * 60 * 60),
            STORAGE_TARGET_SWEEP_STARTUP_STAGGER,
        )
    }

    fn run<'a>(&'a self, pool: &'a PgPool) -> MaintenanceFuture<'a> {
        Box::pin(async move {
            let _ = pool;
            crate::services::maintenance::jobs::target_sweep::run(
                crate::services::maintenance::jobs::target_sweep::Config::default_runtime(),
            )
            .await
        })
    }
}

/// #3231 — hourly orphan-worktree sweep. Reuses the verbatim DESTRUCTIVE
/// sweep logic in `services::maintenance::jobs::worktree_orphan_sweep`,
/// which retains every existing safety guard: fail-closed PG keep-set,
/// runtime naming whitelist (manual dev worktrees are never swept), GUID
/// keep-set, and the 30-min freshness gate (#3231). This wrapper only
/// schedules it; it does not weaken any guard. Needs the PG pool for the
/// active-dispatch / resumable-session keep-set.
pub(super) struct StorageWorktreeOrphanSweepJob;

impl MaintenanceJob for StorageWorktreeOrphanSweepJob {
    fn name(&self) -> &'static str {
        "storage.worktree_orphan_sweep"
    }

    fn schedule(&self) -> MaintenanceSchedule {
        MaintenanceSchedule::every(
            Duration::from_secs(60 * 60),
            STORAGE_WORKTREE_ORPHAN_SWEEP_STARTUP_STAGGER,
        )
    }

    fn run<'a>(&'a self, pool: &'a PgPool) -> MaintenanceFuture<'a> {
        Box::pin(async move {
            crate::services::maintenance::jobs::worktree_orphan_sweep::run(
                crate::services::maintenance::jobs::worktree_orphan_sweep::Config::default_runtime(
                ),
                Some(pool.clone()),
            )
            .await
        })
    }
}

/// #4532 — daily tmp pipeline sweep. The implementation validates the
/// canonical root against `/private/tmp`, applies the basename whitelist and
/// activity-age gate, and fails closed when a live tmux owner cannot be ruled
/// out. This wrapper only schedules that guarded implementation.
pub(super) struct StorageTmpPipelineSweepJob;

impl MaintenanceJob for StorageTmpPipelineSweepJob {
    fn name(&self) -> &'static str {
        "storage.tmp_pipeline_sweep"
    }

    fn schedule(&self) -> MaintenanceSchedule {
        MaintenanceSchedule::every(
            Duration::from_secs(24 * 60 * 60),
            STORAGE_TMP_PIPELINE_SWEEP_STARTUP_STAGGER,
        )
    }

    fn run<'a>(&'a self, pool: &'a PgPool) -> MaintenanceFuture<'a> {
        Box::pin(async move {
            let _ = pool;
            crate::services::maintenance::jobs::tmp_pipeline_sweep::run(
                crate::services::maintenance::jobs::tmp_pipeline_sweep::Config::default_runtime(),
            )
            .await
        })
    }
}

/// #3231 — weekly hang-dump cleanup. Reuses the verbatim logic in
/// `services::maintenance::jobs::hang_dump_cleanup` (deletes `adk-hang-*.txt`
/// older than 14 days from `logs/`). Pool-less.
pub(super) struct StorageHangDumpCleanupJob;

impl MaintenanceJob for StorageHangDumpCleanupJob {
    fn name(&self) -> &'static str {
        "storage.hang_dump_cleanup"
    }

    fn schedule(&self) -> MaintenanceSchedule {
        MaintenanceSchedule::every(
            Duration::from_secs(7 * 24 * 60 * 60),
            STORAGE_HANG_DUMP_CLEANUP_STARTUP_STAGGER,
        )
    }

    fn run<'a>(&'a self, pool: &'a PgPool) -> MaintenanceFuture<'a> {
        Box::pin(async move {
            let _ = pool;
            crate::services::maintenance::jobs::hang_dump_cleanup::run(
                crate::services::maintenance::jobs::hang_dump_cleanup::Config::default_runtime(),
            )
            .await
        })
    }
}

/// #1093 / #3231 — weekly postgres retention sweep. Reuses
/// `services::maintenance::jobs::db_retention::db_retention_job` verbatim
/// (the same fn the dead `register_db_retention` wrapped). PG-only; the
/// live scheduler always has a pool so no skip branch is needed here.
pub(super) struct StorageDbRetentionJob;

impl MaintenanceJob for StorageDbRetentionJob {
    fn name(&self) -> &'static str {
        "storage.db_retention"
    }

    fn schedule(&self) -> MaintenanceSchedule {
        MaintenanceSchedule::every(
            crate::services::maintenance::jobs::STORAGE_MAINTENANCE_INTERVAL,
            STORAGE_DB_RETENTION_STARTUP_STAGGER,
        )
    }

    fn run<'a>(&'a self, pool: &'a PgPool) -> MaintenanceFuture<'a> {
        Box::pin(async move {
            let report =
                crate::services::maintenance::jobs::db_retention::db_retention_job(pool, false)
                    .await?;
            tracing::info!(
                job = self.name(),
                tables = ?report.summary(),
                "[maintenance] db_retention_job completed"
            );
            Ok(())
        })
    }
}

/// #1089 / #3231 — weekly memento consolidation. Reuses
/// `services::maintenance::jobs::memento_consolidation::run` verbatim;
/// self-skips when memento is not configured, so registration is
/// unconditional. Pool-less.
pub(super) struct MemoryMementoConsolidationJob;

impl MaintenanceJob for MemoryMementoConsolidationJob {
    fn name(&self) -> &'static str {
        "memory.memento_consolidation"
    }

    fn schedule(&self) -> MaintenanceSchedule {
        MaintenanceSchedule::every(
            crate::services::maintenance::jobs::memento_consolidation::DEFAULT_INTERVAL,
            MEMORY_MEMENTO_CONSOLIDATION_STARTUP_STAGGER,
        )
    }

    fn run<'a>(&'a self, pool: &'a PgPool) -> MaintenanceFuture<'a> {
        Box::pin(async move {
            let _ = pool;
            crate::services::maintenance::jobs::memento_consolidation::run(
                crate::services::maintenance::jobs::memento_consolidation::Config::default_runtime(
                ),
            )
            .await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_tts_cache_sweep_schedule_matches_documented_interval() {
        // #3909 — 30-minute cadence, 35s startup stagger (clear of the other
        // voice GC jobs at 25/60/75s).
        let job = ProgressTtsCacheSweepJob {
            config: crate::services::maintenance::jobs::voice_cache_sweep::Config::default_runtime(
            ),
        };
        assert_eq!(job.schedule().every_ms(), 30 * 60 * 1_000);
        assert_eq!(job.schedule().startup_stagger_ms(), 35 * 1_000);
    }

    #[test]
    fn disk_gc_job_schedules_match_documented_intervals() {
        assert_eq!(
            StorageTargetSweepJob.schedule().every_ms(),
            30 * 24 * 60 * 60 * 1_000
        );
        assert_eq!(
            StorageWorktreeOrphanSweepJob.schedule().every_ms(),
            60 * 60 * 1_000
        );
        assert_eq!(
            StorageTmpPipelineSweepJob.schedule().every_ms(),
            24 * 60 * 60 * 1_000
        );
        assert_eq!(
            StorageHangDumpCleanupJob.schedule().every_ms(),
            7 * 24 * 60 * 60 * 1_000
        );
        assert_eq!(
            StorageDbRetentionJob.schedule().every_ms(),
            7 * 24 * 60 * 60 * 1_000
        );
        assert_eq!(
            MemoryMementoConsolidationJob.schedule().every_ms(),
            7 * 24 * 60 * 60 * 1_000
        );
    }
}
