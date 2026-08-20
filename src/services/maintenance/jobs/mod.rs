//! Storage maintenance jobs (#1092 / 909-3; extended by #1093 / 909-4).
//!
//! This module holds the *implementations* of the long-running housekeeping
//! jobs. It registers nothing. The production registration lives in
//! `crate::server::maintenance` (`storage_jobs.rs`): one `MaintenanceJob`
//! wrapper struct per job, all listed in
//! `MaintenanceJobRegistry::static_registry()` and driven by the leader-only
//! `ServerWorkerId::MaintenanceScheduler` worker (`register_leader_tokio`).
//! That worker is skipped wholesale when no `PgPool` is available, so every job
//! below is leader-only and postgres-gated, and each job's cadence is fixed by
//! its wrapper's `schedule()` (which for `storage.db_retention` and
//! `memory.memento_consolidation` reads interval constants from this module
//! tree: `STORAGE_MAINTENANCE_INTERVAL` and
//! `memento_consolidation::DEFAULT_INTERVAL`).
//!
//! The dynamic scheduler of #1091 (909-2)
//! ([`crate::services::maintenance::register_maintenance_job`] /
//! `spawn_maintenance_scheduler`) has no registered jobs and no live caller;
//! its last registrar was removed in #5463.
//!
//! The jobs implemented here, with the cadence their production wrapper sets:
//!
//!   * `storage.target_sweep` — monthly (~30d). Runs `cargo sweep --time 30` in
//!     the main workspace `target/` dir if disk usage exceeds 50 GB OR the 30d
//!     cadence has elapsed. Reports removed-file counts via `tracing::info!`.
//!   * `storage.worktree_orphan_sweep` — hourly. Scans
//!     `~/.adk/release/worktrees/` and cross-checks each dir against the PG
//!     keep-set (active dispatches `status IN ('pending','dispatched')` +
//!     resumable sessions carrying a non-null resume GUID, #3231) and the live
//!     `AgentDesk-*` tmux panes. Pass A (flat-root per-channel worktrees) only
//!     discards dirs matching the runtime naming whitelist (`wt/<provider>-…` /
//!     `claude-adk-cc…` / `codex-adk-cdx…`) so manual dev worktrees are never
//!     swept (#3231); pass B recurses one level into the managed root
//!     (`worktrees/<repo_name>/`) and removes terminal dispatch/automation
//!     worktrees via `cleanup_managed_worktree` (dirty/unmerged skip).
//!   * `storage.tmp_pipeline_sweep` — daily. Scans only direct `/private/tmp`
//!     children with the `adk-` or `agentdesk-` basename prefix; a 3-day activity
//!     age gate and live-tmux owner guard fail closed before removal.
//!   * `storage.hang_dump_cleanup` — weekly. Deletes `adk-hang-*.txt` files
//!     older than 14 days from the `logs/` directory.
//!   * `storage.db_retention` — weekly. Applies retention policies to
//!     postgres tables (7/30/90d horizons). Takes the scheduler's live `PgPool`
//!     directly; there is no per-job postgres skip branch, because the whole
//!     maintenance worker is skipped when postgres is unavailable (see above).
//!   * `memory.memento_consolidation` — weekly (#1089 / 908-7). Calls the
//!     memento MCP `memory_consolidate` tool to merge low-importance /
//!     duplicate fragments. No-ops when memento is not configured.
//!
//! `voice.progress_tts_cache_sweep` (#3909) is also implemented here — see
//! `voice_cache_sweep` — and registered by the same static registry.
//!
//! The `voice.turn_link_gc` job (#2362 / #2164 Voice A) is intentionally NOT
//! implemented here — it lives directly in `server::maintenance` alongside the
//! other jobs that own persistent state in PG (`storage.cancel_tombstone_prune`).
//!
//! `dcserver.stdout.log` rotation is handled directly by `logging.rs` because
//! launchd/systemd open stdout before the process starts and cannot safely
//! rotate the active descriptor themselves.

use std::time::Duration;

pub mod db_retention;
pub mod hang_dump_cleanup;
pub mod memento_consolidation;
pub mod target_sweep;
pub mod tmp_pipeline_sweep;
pub mod voice_cache_sweep;
pub mod worktree_orphan_sweep;

/// Weekly cadence for postgres-backed retention jobs. Long enough that a single
/// missed tick is not a crisis, short enough that retention horizons (7/30/90d)
/// are never breached by more than a week.
pub const STORAGE_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
