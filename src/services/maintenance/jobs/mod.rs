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
//!     postgres tables (7/30/90d horizons). Requires a live `PgPool`; if
//!     postgres is disabled, this job is skipped (remaining jobs still
//!     register).
//!   * `memory.memento_consolidation` — weekly (#1089 / 908-7). Calls the
//!     memento MCP `memory_consolidate` tool to merge low-importance /
//!     duplicate fragments. No-ops when memento is not configured.
//!
//! The `voice.turn_link_gc` job (#2362 / #2164 Voice A) is intentionally
//! NOT registered here — it lives on the production
//! `server::maintenance::MaintenanceJobRegistry` so it runs through the
//! leader-only worker_registry::MaintenanceScheduler path that owns
//! persistent state in PG (same path as `storage.cancel_tombstone_prune`).
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
