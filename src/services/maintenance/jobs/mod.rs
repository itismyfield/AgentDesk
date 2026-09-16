//! Storage maintenance job implementations (#1092/#1093). Registration lives in
//! `crate::server::maintenance::storage_jobs`; driven by the leader-only
//! `MaintenanceScheduler` worker (skipped entirely without a `PgPool`, so every
//! job here is leader-only and postgres-gated). Cadence is set per wrapper's
//! `schedule()`.
//!
//! Jobs: `storage.target_sweep` (monthly/50GB `target/`);
//! `storage.worktree_orphan_sweep` (hourly; only sweeps the runtime naming
//! whitelist under `~/.adk/release/worktrees/`, #3231, so manual dev worktrees
//! are never touched); `storage.tmp_pipeline_sweep` (daily, `/private/tmp`
//! `adk-`/`agentdesk-` dirs); `storage.hang_dump_cleanup` (weekly);
//! `storage.db_retention` (weekly, 7/30/90d); `memory.memento_consolidation`
//! (weekly, #1089); `voice.progress_tts_cache_sweep` (#3909).
//! `register_maintenance_job` (#1091) has no live caller; `voice.turn_link_gc`
//! and `storage.cancel_tombstone_prune` live in `server::maintenance` instead.

use std::time::Duration;

pub mod db_retention;
pub mod hang_dump_cleanup;
pub mod memento_consolidation;
pub mod target_sweep;
pub mod tmp_pipeline_sweep;
pub mod voice_cache_sweep;
pub mod worktree_orphan_sweep;

/// Weekly cadence for postgres-backed retention jobs (7/30/90d horizons).
pub const STORAGE_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
