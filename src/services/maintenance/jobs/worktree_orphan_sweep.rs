//! `storage.worktree_orphan_sweep` — hourly detection and cleanup of orphaned
//! git worktree directories under `~/.adk/release/worktrees/`.
//!
//! Two passes share one DB keep-set + one live-tmux owner set:
//!
//! **A — per-channel/thread worktrees (flat root)**: the runtime provisions
//! per-channel reuse worktrees directly under `~/.adk/release/worktrees/`
//! (`{provider}-{channel}-{ts}`, branch `wt/{provider}-…`). A flat-root dir is
//! KEPT when it is the cwd (or a parent of the cwd) of a `sessions` row carrying
//! a non-null resume GUID (`claude_session_id` OR `raw_provider_session_id`) —
//! the deterministic resumable signal (#3231) — OR it is a live `AgentDesk-*`
//! tmux pane path OR an active-dispatch cwd. A flat-root dir is DISCARDED only
//! when, on top of having no owner, its NAME matches the runtime naming
//! whitelist (`wt/<provider>-…` branch / `claude-adk-cc…` / `codex-adk-cdx…`
//! dir). Manual dev worktrees (`worker-*`, `integration-*`, `codex-*`,
//! `release-*`, `fix-*`, `e2e-*`, …) are NOT runtime-created and are NEVER
//! discard candidates (#3231 key safety fix).
//!
//! **B — managed dispatch/automation worktrees (managed root)**: dispatch and
//! automation worktrees live one level deeper under
//! `~/.adk/release/worktrees/<repo_name>/`. The flat-root scan is 1-depth and
//! misses these, so we recurse one level into each managed-root child. A managed
//! worktree is DISCARDED when it has no active-dispatch/live-tmux owner (i.e. its
//! dispatch is terminal) AND it is a managed worktree path AND the
//! [`crate::services::git::cleanup_managed_worktree`] guards pass (skips dirty
//! and mainline-unmerged trees). A far age backstop catches cancel-leaks (a
//! cancelled dispatch whose worktree was never terminal-cleaned).
//!
//! **Name-prefix infra protection (#3276)**: BOTH passes share a hard
//! name-prefix guard inside [`should_sweep_worktree`]: a directory whose NAME
//! starts with `release-` (e.g. the reusable `release-main-deploy-*` deploy
//! worktree that `scripts/deploy-release.sh` runs from) is deployment
//! infrastructure, not a runtime session worktree — it is never a dispatch
//! cwd, never a resumable session cwd, and never a live AgentDesk tmux pane,
//! so every ownership keep-set source misses it by construction. It is KEPT
//! unconditionally, before any owner check.
//!
//! For each orphan:
//!   1. Attempt `git worktree remove --force <path>` (from the parent repo).
//!      This is a no-op if the dir isn't actually a registered worktree.
//!   2. If the directory still exists, `std::fs::remove_dir_all` it.
//!
//! Fail-closed by design — it would rather leak an orphan than risk deleting a
//! live worktree:
//!   * when Postgres is not wired up it returns `Ok(())` (no DB keep-set, no
//!     deletes); and
//!   * when the tmux query FAILS (#3216 P0-1) it skips ALL deletions for the
//!     run, because a failed query cannot prove a worktree has no live owner.
//!     Only a SUCCESSFUL tmux query (even one with zero panes) lets the sweep
//!     proceed.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use sqlx::PgPool;

use crate::services::git::GitCommand;

#[derive(Debug, Clone)]
pub struct Config {
    /// Root directory that contains one sub-directory per active worktree.
    pub worktrees_root: PathBuf,
    /// If true, identify orphans and report counts but do not delete anything.
    pub dry_run: bool,
}

impl Config {
    pub fn default_runtime() -> Self {
        let worktrees_root = dirs::home_dir()
            .map(|home| home.join(".adk/release/worktrees"))
            .unwrap_or_else(|| PathBuf::from("worktrees"));
        Self {
            worktrees_root,
            dry_run: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub pg_available: bool,
    pub scanned_dirs: u64,
    pub active_cwd_count: u64,
    pub orphan_count: u64,
    pub removed_dirs: u64,
    pub errors: u64,
    /// #3231 (B): managed dispatch/automation worktrees discarded by the
    /// recursive managed-root pass (counted separately from flat-root orphans).
    pub managed_scanned: u64,
    pub managed_removed: u64,
    /// #3231 (A): flat-root dirs that had no owner but were SKIPPED because their
    /// name did not match the runtime naming whitelist (manual dev worktrees).
    pub protected_unmatched: u64,
    /// #3231 (codex re-review, TOCTOU): managed worktrees that had no owner but
    /// were SKIPPED because they were created too recently (within
    /// [`MANAGED_FRESH_PROVISION_MIN_AGE`]) — i.e. possibly still inside the
    /// dispatch-create create→insert race window before their owning row landed.
    pub protected_fresh: u64,
}

pub async fn run(config: Config, pg_pool: Option<PgPool>) -> Result<()> {
    let report = run_inner(&config, pg_pool).await?;
    tracing::info!(
        target: "maintenance",
        job = "storage.worktree_orphan_sweep",
        worktrees_root = %config.worktrees_root.display(),
        pg_available = report.pg_available,
        scanned = report.scanned_dirs,
        active_cwds = report.active_cwd_count,
        orphans = report.orphan_count,
        removed = report.removed_dirs,
        errors = report.errors,
        managed_scanned = report.managed_scanned,
        managed_removed = report.managed_removed,
        protected_unmatched = report.protected_unmatched,
        protected_fresh = report.protected_fresh,
        dry_run = config.dry_run,
        "worktree_orphan_sweep completed"
    );
    Ok(())
}

pub async fn run_inner(config: &Config, pg_pool: Option<PgPool>) -> Result<SweepReport> {
    let mut report = SweepReport::default();

    if !config.worktrees_root.exists() {
        return Ok(report);
    }

    let Some(pool) = pg_pool else {
        // No PG — deliberately do not delete anything; otherwise we'd orphan
        // legitimately active worktrees on a misconfigured host.
        return Ok(report);
    };
    report.pg_available = true;

    // #3231 (codex #2, fail-closed): EVERY DB keep-set query is load-bearing — a
    // worktree absent from the keep-set is a deletion candidate. If any keep-set
    // query FAILS (PG down, schema drift, query error) `unwrap_or_default()` would
    // silently substitute an EMPTY set and we would then delete live resumable /
    // active-dispatch worktrees. That is exactly the failure the tmux-probe path
    // already guards against by returning early. So we mirror that semantics here:
    // a keep-set query error means we cannot prove a worktree is unowned — warn and
    // skip ALL deletions for this run.
    let mut active_cwds = match fetch_active_cwds(&pool).await {
        Ok(set) => set,
        Err(error) => {
            tracing::warn!(
                target: "maintenance",
                job = "storage.worktree_orphan_sweep",
                error = %error,
                "active-dispatch keep-set query failed; cannot prove no live owner — skipping all deletions this run (fail-closed)"
            );
            return Ok(report);
        }
    };
    // #3207 (part 2): a reused worktree owned by a live/resumable channel session
    // must survive BETWEEN turns and across restarts so `--resume` can find the
    // sid's transcript. Between turns there is no `pending`/`dispatched` dispatch,
    // so the active-dispatch keep-set alone would let the hourly sweep delete the
    // very worktree the next message will resume into — re-creating the original
    // "worktree rotation → resume impossible" loss. Also protect cwds of recent
    // resumable sessions (a recorded provider session id + a fresh heartbeat).
    // The keep-set is bounded — only the LATEST fresh-heartbeat resumable session
    // PER CHANNEL is protected (see `fetch_resumable_cwds`) — so abandoned /
    // never-heartbeated sessions can no longer pin a worktree forever, while a
    // live channel still keeps its single in-flight reuse worktree.
    let resumable_cwds = match fetch_resumable_cwds(&pool).await {
        Ok(set) => set,
        Err(error) => {
            tracing::warn!(
                target: "maintenance",
                job = "storage.worktree_orphan_sweep",
                error = %error,
                "resumable-session keep-set query failed; cannot prove no live owner — skipping all deletions this run (fail-closed)"
            );
            return Ok(report);
        }
    };
    active_cwds.extend(resumable_cwds);
    // #3231 (codex #1): an active managed dispatch records its worktree under
    // `task_dispatches.context.worktree_path` (and `result.completed_worktree_path`)
    // at CREATE time — BEFORE the dispatched agent's `sessions.cwd` / live tmux pane
    // exist. So a freshly-provisioned managed worktree for a `pending`/`dispatched`
    // dispatch is owned by NO session cwd and NO tmux pane yet, and the managed
    // recursion below would delete it out from under the dispatch. Reuse the same
    // active-worktree-ref signal that terminal cleanup relies on (pending/dispatched
    // dispatch JSON + `pr_tracking.worktree_path`) as an additional keep-set source.
    let active_dispatch_worktrees = match fetch_active_dispatch_worktree_paths(&pool).await {
        Ok(set) => set,
        Err(error) => {
            tracing::warn!(
                target: "maintenance",
                job = "storage.worktree_orphan_sweep",
                error = %error,
                "active-dispatch worktree-path keep-set query failed; cannot prove no live owner — skipping all deletions this run (fail-closed)"
            );
            return Ok(report);
        }
    };
    active_cwds.extend(active_dispatch_worktrees);
    report.active_cwd_count = active_cwds.len() as u64;

    // #3216 (gap 3): a divorced/phantom per-channel worktree (provisioned by a
    // restart-time rotation, then abandoned when reconciliation pointed the DB
    // cwd back to the ORIGINAL worktree) has no kept session cwd and no live
    // owner — it must be swept. But the live tmux that actually owns the
    // original worktree is the SOURCE OF TRUTH: a managed worktree whose path is
    // the `#{pane_current_path}` of a live AgentDesk tmux pane must NEVER be
    // deleted, even if the DB keep-set transiently disagrees (e.g. before
    // reconciliation backfills `channel_id`). We add this live-tmux guard as an
    // independent safety net layered on top of the DB keep-set.
    //
    // #3216 P0-1 (fail-closed): a FAILED tmux query is indistinguishable, by an
    // empty path set alone, from "tmux is up with zero AgentDesk panes". If we
    // treated failure as "no live owners" we would sweep a live AgentDesk
    // worktree the moment tmux was momentarily unavailable AND its DB cwd was
    // transiently missing. So when the tmux query FAILS we cannot prove any
    // worktree is unowned — skip ALL deletions for this run. Only a SUCCESSFUL
    // query (even one returning zero panes) lets deletion proceed.
    let Some(live_tmux_paths) = collect_live_tmux_pane_paths() else {
        tracing::warn!(
            target: "maintenance",
            job = "storage.worktree_orphan_sweep",
            "tmux query failed; cannot prove no live worktree owner — skipping all deletions this run (fail-closed)"
        );
        return Ok(report);
    };

    let Ok(entries) = std::fs::read_dir(&config.worktrees_root) else {
        return Ok(report);
    };

    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_dir() {
            continue;
        }
        report.scanned_dirs = report.scanned_dirs.saturating_add(1);

        let dir_path = entry.path();

        // #3231 (B): a managed-root child (`worktrees/<repo_name>/`) is not a
        // per-channel worktree itself — its CHILDREN are the dispatch/automation
        // worktrees. Recurse one level into it (the flat 1-depth scan misses
        // them) and skip the directory itself from the flat-root A decision.
        if is_managed_root_child(&dir_path) {
            sweep_managed_root(
                &dir_path,
                &active_cwds,
                &live_tmux_paths,
                config,
                &mut report,
            )
            .await;
            continue;
        }

        if !should_sweep_worktree(&dir_path, &active_cwds, Some(&live_tmux_paths)) {
            continue;
        }

        // #3231 (A): naming whitelist — only runtime-named per-channel worktrees
        // (`wt/<provider>-…` branch / `claude-adk-cc…` / `codex-adk-cdx…` dir)
        // are ever discard candidates. Manual dev worktrees (worker-*,
        // integration-*, codex-*, release-*, fix-*, e2e-*, …) are NOT
        // runtime-created and must NEVER be swept, even with no owning row.
        if !is_runtime_named_worktree(&dir_path) {
            report.protected_unmatched = report.protected_unmatched.saturating_add(1);
            continue;
        }

        report.orphan_count = report.orphan_count.saturating_add(1);

        if config.dry_run {
            continue;
        }

        match remove_orphan_worktree(&dir_path).await {
            Ok(()) => {
                report.removed_dirs = report.removed_dirs.saturating_add(1);
            }
            Err(error) => {
                tracing::warn!(
                    target: "maintenance",
                    path = %dir_path.display(),
                    error = %error,
                    "worktree_orphan_sweep: failed to remove orphan"
                );
                report.errors = report.errors.saturating_add(1);
            }
        }
    }

    Ok(report)
}

/// #3231 (B): recurse one level into a managed-root child
/// (`worktrees/<repo_name>/`) and sweep terminal dispatch/automation worktrees.
///
/// A managed worktree is discarded when ALL of the following hold:
///   * it has no active-dispatch / live-tmux / kept-session owner (its dispatch
///     is terminal — an active dispatch pins its `sessions.cwd` into the
///     keep-set, so absence from the keep-set IS the terminal signal); AND
///   * it is recognized as a managed worktree path AND passes the
///     [`crate::services::git::cleanup_managed_worktree`] guards (dirty trees and
///     mainline-unmerged trees are skipped — never force-removed here).
///
/// We reuse `should_sweep_worktree` for the owner check so the fail-closed
/// tmux + keep-set semantics are identical to the flat-root pass. The actual
/// removal goes through `cleanup_managed_worktree` (NOT the flat-root
/// `--force` path) so dirty/unmerged work is preserved. A far age backstop
/// (`MANAGED_CANCEL_LEAK_BACKSTOP`) lets a long-abandoned managed worktree be
/// reconsidered even if it would otherwise be skipped — see [`is_old_enough`].
///
/// #3231 (codex #3): when the worktree's `.git` pointer cannot be resolved the
/// managed pass NEVER force-removes. A present-but-unreadable `.git` (a registered
/// worktree we merely failed to read) is SKIPPED; only a genuinely `.git`-less
/// leftover directory is eligible for a plain `remove_dir_all`, and only once
/// age-backstopped. The flat-root `--force` remover is never reachable from here.
async fn sweep_managed_root(
    repo_root_dir: &Path,
    active_cwds: &HashSet<String>,
    live_tmux_paths: &HashSet<String>,
    config: &Config,
    report: &mut SweepReport,
) {
    let Ok(children) = std::fs::read_dir(repo_root_dir) else {
        return;
    };
    for child in children.flatten() {
        let Ok(metadata) = child.metadata() else {
            continue;
        };
        if !metadata.is_dir() {
            continue;
        }
        report.managed_scanned = report.managed_scanned.saturating_add(1);

        let wt_path = child.path();
        if !should_sweep_worktree(&wt_path, active_cwds, Some(live_tmux_paths)) {
            continue;
        }

        // #3231 (codex re-review, TOCTOU): the keep-set snapshot was built once at
        // the start of `run_inner`, but dispatch creation provisions a managed
        // worktree BEFORE it commits the owning `task_dispatches` row (see
        // `crate::dispatch::dispatch_create`). If the snapshot was taken inside that
        // create→insert window, a just-provisioned clean/merged worktree is in no
        // keep-set source and has no live tmux owner yet — so the checks above
        // would (wrongly) select it for deletion. A creation-age floor closes the
        // window: a worktree younger than `MANAGED_FRESH_PROVISION_MIN_AGE` is
        // protected regardless of owner, because its row may simply not have landed
        // yet. The far cancel-leak backstop below still applies to genuinely old
        // leftovers. Fail-closed toward KEEP. See [`is_freshly_provisioned`].
        if is_freshly_provisioned(&wt_path, MANAGED_FRESH_PROVISION_MIN_AGE) {
            report.protected_fresh = report.protected_fresh.saturating_add(1);
            continue;
        }

        report.orphan_count = report.orphan_count.saturating_add(1);

        if config.dry_run {
            continue;
        }

        // Resolve the parent repo from the worktree's `.git` gitdir pointer so
        // `cleanup_managed_worktree` can run its dirty/unmerged guards + the
        // managed-path check against the real repo.
        let Some(repo_root) = infer_repo_root_from_worktree(&wt_path) else {
            // #3231 (codex #3): the `.git` pointer could not be resolved. We MUST
            // NOT fall back to the force/plain-delete path (`remove_orphan_worktree`)
            // here — that bypasses `cleanup_managed_worktree`'s dirty/unmerged
            // guards, so a registered worktree whose `.git` we merely failed to read
            // (transient FS error) — possibly holding uncommitted work — could be
            // blown away. Distinguish the two cases by the `.git` entry itself:
            //   * `.git` EXISTS but is unreadable/malformed → a registered worktree
            //     with an unresolvable pointer → SKIP (never delete; preserve any
            //     dirty/unmerged work for human review);
            //   * `.git` is genuinely ABSENT → not a git worktree, just a leftover
            //     directory with no tracked/uncommitted git state to lose → eligible
            //     for a plain `remove_dir_all` ONLY when age-backstopped (a
            //     cancel-leak), so a freshly-provisioned dir is never touched.
            match git_pointer_state(&wt_path) {
                GitPointerState::Missing => {
                    if is_old_enough(&wt_path, MANAGED_CANCEL_LEAK_BACKSTOP) {
                        match remove_dir_all_plain(&wt_path) {
                            Ok(()) => {
                                report.managed_removed = report.managed_removed.saturating_add(1)
                            }
                            Err(error) => {
                                tracing::warn!(
                                    target: "maintenance",
                                    path = %wt_path.display(),
                                    error = %error,
                                    "worktree_orphan_sweep: failed to remove age-backstopped managed leftover (no .git)"
                                );
                                report.errors = report.errors.saturating_add(1);
                            }
                        }
                    }
                }
                GitPointerState::PresentUnreadable => {
                    tracing::warn!(
                        target: "maintenance",
                        path = %wt_path.display(),
                        "worktree_orphan_sweep: managed worktree has an unreadable/unresolvable .git pointer — skipping (fail-closed; never force-removed)"
                    );
                }
            }
            continue;
        };

        let repo_dir = repo_root.to_string_lossy().to_string();
        let wt_str = wt_path.to_string_lossy().to_string();
        let cleanup = tokio::task::spawn_blocking(move || {
            crate::services::git::cleanup_managed_worktree(&repo_dir, &wt_str)
        })
        .await;

        match cleanup {
            Ok(result) if result.removed > 0 => {
                report.managed_removed = report.managed_removed.saturating_add(1);
            }
            Ok(_) => {
                // skipped_dirty / skipped_unmerged / skipped_unmanaged / failed —
                // preserved by design. Age backstop: a cancel-leak (dispatch
                // cancelled but worktree clean+merged yet not terminal-cleaned)
                // already removes above; a dirty/unmerged tree is intentionally
                // left for human review regardless of age.
            }
            Err(error) => {
                tracing::warn!(
                    target: "maintenance",
                    path = %wt_path.display(),
                    error = %error,
                    "worktree_orphan_sweep: cleanup_managed_worktree task panicked"
                );
                report.errors = report.errors.saturating_add(1);
            }
        }
    }
}

/// Returns the set of `sessions.cwd` values where the session is tied to an
/// active dispatch. `task_dispatches.status IN ('pending','dispatched')` is the
/// de-facto "active" set in this codebase (see `src/integration_tests.rs`
/// callers).
async fn fetch_active_cwds(pool: &PgPool) -> Result<HashSet<String>> {
    let rows: Vec<(Option<String>,)> = sqlx::query_as(
        "SELECT DISTINCT s.cwd
         FROM sessions s
         JOIN task_dispatches d
           ON d.id = s.active_dispatch_id
         WHERE d.status IN ('pending', 'dispatched')
           AND s.cwd IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|(cwd,)| cwd.filter(|s| !s.is_empty()))
        .collect())
}

/// #3207 (part 2) P1 / #3231 (A): cwds of resumable sessions — those carrying a
/// recorded provider session GUID (`claude_session_id` /
/// `raw_provider_session_id`) whose worktree the next turn's `--resume` reuses,
/// so they must not be swept while idle between turns.
///
/// #3231: the RESUME GUID is the PRIMARY keep signal — it is deterministic
/// (session clear records the GUID as NULL in the DB, see
/// `clear_provider_session_id`), so a non-null GUID means a resumable transcript
/// genuinely exists for that worktree, whereas a heartbeat is only an
/// approximate liveness proxy. The previous query made a fresh `last_heartbeat`
/// (7d) the gate AND excluded NULL-heartbeat rows, so a session that recorded a
/// GUID but never heartbeated lost its worktree even though `--resume` could
/// still find the transcript. We now key on GUID-presence and keep TIME only as
/// a GENEROUS far backstop via `COALESCE(last_heartbeat, created_at)` so disk is
/// still bounded:
///   * non-null GUID is required (nothing to resume into otherwise — a cleared /
///     never-recorded GUID is collectable);
///   * `COALESCE(last_heartbeat, created_at) >= NOW() - 30d` — a row that never
///     heartbeated survives until 30d after creation, not forever, while a
///     genuinely abandoned (very old) worktree becomes collectable again;
///   * only the LATEST such row PER CHANNEL is protected
///     (`DISTINCT ON (channel partition) ... ORDER BY ... DESC`), so a channel
///     reuses ONE worktree rather than pinning every historical row.
///
/// The channel partition prefers the unique `channel_id` (#3207 P0), falling
/// back to `thread_channel_id`/`session_key` for legacy rows that predate the
/// `channel_id` column so each still collapses to a single protected worktree.
async fn fetch_resumable_cwds(pool: &PgPool) -> Result<HashSet<String>> {
    let rows: Vec<(Option<String>,)> = sqlx::query_as(
        "SELECT DISTINCT ON (COALESCE(channel_id, thread_channel_id, session_key)) cwd
         FROM sessions
         WHERE cwd IS NOT NULL
           AND cwd <> ''
           AND (claude_session_id IS NOT NULL OR raw_provider_session_id IS NOT NULL)
           AND COALESCE(last_heartbeat, created_at) >= NOW() - INTERVAL '30 days'
         ORDER BY COALESCE(channel_id, thread_channel_id, session_key),
                  COALESCE(last_heartbeat, created_at) DESC",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|(cwd,)| cwd.filter(|s| !s.is_empty()))
        .collect())
}

/// #3231 (codex #1): JSON keys under which a dispatch records the worktree it
/// owns — mirrors `WORKTREE_PATH_REFERENCE_KEYS` in
/// `crate::kanban::terminal_cleanup`, the active-ref signal terminal cleanup uses
/// to refuse removing a worktree still claimed by another live dispatch.
const DISPATCH_WORKTREE_PATH_KEYS: &[&str] = &["worktree_path", "completed_worktree_path"];

/// #3231 (codex #1): worktree paths claimed by an ACTIVE (`pending`/`dispatched`,
/// i.e. not-yet-terminal) managed dispatch, plus live `pr_tracking` worktrees.
///
/// A managed dispatch's `worktree_path` is injected into `task_dispatches.context`
/// at CREATE time, BEFORE the dispatched agent produces a `sessions.cwd` or a live
/// tmux pane. Between create and first turn the freshly-provisioned worktree is
/// therefore owned by NOTHING the other keep-set sources can see, so the managed
/// recursion would delete it. This reuses the exact active-reference signal that
/// `crate::kanban::terminal_cleanup::active_worktree_refs_pg` relies on (the same
/// status filter, the same JSON keys, the same `pr_tracking` source) so the sweep
/// never removes a worktree terminal cleanup itself would refuse to remove.
async fn fetch_active_dispatch_worktree_paths(pool: &PgPool) -> Result<HashSet<String>> {
    let mut paths = HashSet::new();

    // Cast JSON-ish TEXT columns to TEXT explicitly so the decode is uniform
    // regardless of whether the column is stored as TEXT or JSON/JSONB.
    let rows: Vec<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT context::TEXT, result::TEXT
         FROM task_dispatches
         WHERE status IN ('pending', 'dispatched')",
    )
    .fetch_all(pool)
    .await?;

    for (context_raw, result_raw) in rows {
        for raw in [context_raw, result_raw].into_iter().flatten() {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
                continue;
            };
            for key in DISPATCH_WORKTREE_PATH_KEYS {
                if let Some(path) = value
                    .get(*key)
                    .and_then(|field| field.as_str())
                    .map(str::trim)
                    .filter(|field| !field.is_empty())
                {
                    paths.insert(path.to_string());
                }
            }
        }
    }

    // `pr_tracking.worktree_path` pins a worktree that a PR still tracks — same
    // source terminal cleanup consults before removing a managed worktree.
    let pr_rows: Vec<(Option<String>,)> = sqlx::query_as(
        "SELECT worktree_path
         FROM pr_tracking
         WHERE NULLIF(BTRIM(worktree_path), '') IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;
    for (path,) in pr_rows {
        if let Some(path) = path.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
            paths.insert(path);
        }
    }

    Ok(paths)
}

/// True when `candidate` equals `dir` OR is nested directly/transitively under
/// it (a real path boundary — `dir` followed by `/`). Shared by both the
/// kept-cwd check ([`is_dir_active`]) and the live-tmux check
/// ([`has_live_tmux_owner`]) so a pane/cwd sitting in a SUBDIR of a worktree
/// (e.g. `/worktree/src`) protects the worktree root in both cases.
pub(crate) fn path_equals_or_nested_under(candidate: &str, dir: &str) -> bool {
    if candidate == dir {
        return true;
    }
    candidate.starts_with(dir)
        && candidate
            .as_bytes()
            .get(dir.len())
            .map(|b| *b == b'/')
            .unwrap_or(false)
}

/// #3231 (B): far age backstop for managed cancel-leaks. A managed worktree
/// whose dispatch was cancelled but whose terminal cleanup never ran can linger
/// even when its `.git` pointer is unreadable; only such directories OLDER than
/// this horizon are eligible for the plain-delete fallback in
/// [`sweep_managed_root`]. Generous on purpose — the keep-set + live-tmux gate
/// already prove no live owner; age is only a final guard against deleting a
/// freshly-provisioned dir whose `.git` we momentarily failed to read.
const MANAGED_CANCEL_LEAK_BACKSTOP: std::time::Duration =
    std::time::Duration::from_secs(60 * 60 * 24); // 24h

/// #3231 (codex re-review, TOCTOU): minimum age a managed worktree must reach
/// before the recursive managed-root pass may delete it. Closes the create→insert
/// race in dispatch creation: [`crate::dispatch::dispatch_create`] provisions the
/// worktree FIRST (`ensure_card_worktree`) and only commits the owning
/// `task_dispatches` row a moment LATER. If the hourly sweep's keep-set snapshot is
/// taken inside that window, the just-provisioned clean/merged worktree is in NO
/// keep-set source and has NO live tmux owner yet, so the managed pass would delete
/// it out from under the in-flight dispatch. The row lands within seconds of
/// creation, so a generous 30-minute floor guarantees the window has closed before
/// any managed worktree becomes a delete candidate — fail-closed toward KEEP.
///
/// This is a CREATION-age floor, deliberately NOT an idle gate: a fresh managed
/// worktree may be actively building (heavy `target/` churn) and then fall idle
/// the instant the build finishes — possibly still before the row lands — so an
/// idle/mtime-quiescence signal could mis-classify it as collectable. "Created
/// recently" is the only signal that reliably protects the whole window.
const MANAGED_FRESH_PROVISION_MIN_AGE: std::time::Duration =
    std::time::Duration::from_secs(60 * 30); // 30m

/// #3276: infrastructure worktree NAME prefixes protected from the sweep in
/// BOTH passes, regardless of any owner signal. The release deploy worktree
/// (`release-main-deploy-<ts>`, the cwd `scripts/deploy-release.sh` reuses
/// across deploys) is created once by an operator and is never a dispatch cwd,
/// never a resumable session cwd, and never a live AgentDesk tmux pane — so
/// every ownership keep-set source misses it by construction and an
/// owner-based decision would always (wrongly) select it. Deleting it breaks
/// the next deploy, hence the unconditional name guard.
///
/// `release-` (broader than the exact `release-main-deploy` form, per the
/// issue's recommendation) is safe to protect wholesale WITHOUT pinning real
/// orphans forever: the runtime NEVER creates `release-*` names — flat-root
/// worktrees are `{provider}-…` (`claude-…` / `codex-adk-cdx…` / `wt-…`, see
/// [`is_runtime_named_worktree`]) and managed worktrees are `issue-<n>-<ts>` /
/// `automation-<card>-iter-<n>` (`crate::services::git::worktree_resolver`) —
/// so no genuine runtime orphan can ever hide behind this prefix.
const PROTECTED_INFRA_NAME_PREFIXES: &[&str] = &["release-"];

/// #3276: true when `dir`'s NAME (the final path segment only — the scan root
/// itself lives under `~/.adk/release/`, so matching the full path would
/// protect everything) starts with a protected infrastructure prefix.
/// Case-insensitive, mirroring [`is_runtime_named_worktree`].
pub(crate) fn is_protected_infra_worktree(dir: &Path) -> bool {
    let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    PROTECTED_INFRA_NAME_PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix))
}

/// #3231 (A): true when the worktree dir name matches the runtime per-channel
/// naming the AgentDesk runtime actually creates — `{provider}-…` flat-root dirs
/// (`create_git_worktree`: `claude-…` / `codex-…`, branch `wt/<provider>-…`).
/// Returning `false` PROTECTS manual dev worktrees (`worker-*`, `integration-*`,
/// `codex-*`-without-`-adk-cdx`, `release-*`, `fix-*`, `e2e-*`, …) that a human
/// dropped into the flat root — they are never runtime-created and must NEVER be
/// discard candidates. This is the key #3231 safety fix.
///
/// Matched dir-name prefixes (case-insensitive on the leading provider token):
///   * `claude-`  — `create_git_worktree` provider `claude`
///   * `codex-adk-cdx` — the runtime codex per-channel worktree
///   * `wt-`/`wt/` — defensive: a branch-derived `wt/<provider>-…` name
///
/// Note `codex-` ALONE is intentionally NOT whitelisted: a manual `codex-*` dev
/// worktree must survive. Only the runtime's `codex-adk-cdx…` form is matched.
pub(crate) fn is_runtime_named_worktree(dir: &Path) -> bool {
    let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    lower.starts_with("claude-")
        || lower.starts_with("codex-adk-cdx")
        || lower.starts_with("wt-")
        || lower.starts_with("wt/")
}

/// #3231 (B): true when `dir` is a managed-root child — i.e. the per-repo
/// container `worktrees/<repo_name>/` whose CHILDREN are managed dispatch /
/// automation worktrees, NOT a per-channel worktree itself. Heuristic: it is
/// NOT a git worktree (no `.git` gitdir pointer) AND is NOT runtime-named, yet
/// contains at least one child that IS a git worktree. This keeps a manual dev
/// worktree (which has a `.git` file) from being treated as a managed root.
pub(crate) fn is_managed_root_child(dir: &Path) -> bool {
    // A registered git worktree has a `.git` FILE (gitdir pointer); a managed
    // root is a plain container directory, so it must NOT have one.
    if dir.join(".git").exists() {
        return false;
    }
    let Ok(children) = std::fs::read_dir(dir) else {
        return false;
    };
    children.flatten().any(|child| {
        child.metadata().map(|m| m.is_dir()).unwrap_or(false) && child.path().join(".git").exists()
    })
}

/// True when `dir`'s modification time is older than `min_age`. Used as the
/// far cancel-leak backstop in [`sweep_managed_root`]. A missing/unreadable mtime
/// returns `false` (conservative — do not delete what we cannot age).
fn is_old_enough(dir: &Path, min_age: std::time::Duration) -> bool {
    let Ok(modified) = dir.metadata().and_then(|m| m.modified()) else {
        return false;
    };
    modified
        .elapsed()
        .map(|elapsed| elapsed >= min_age)
        .unwrap_or(false)
}

/// #3231 (codex re-review, TOCTOU): true when `dir` was created too recently to be
/// a safe delete candidate — i.e. it may still be inside the dispatch-create
/// create→insert window (see [`MANAGED_FRESH_PROVISION_MIN_AGE`]). Used by the
/// managed-root pass to PROTECT a just-provisioned worktree whose owning
/// `task_dispatches` row has not yet been committed to (or become visible in) the
/// keep-set snapshot.
///
/// Age is read from the directory's CREATION time (`created()`), falling back to
/// its modification time when the platform/FS does not expose a birth time. Unlike
/// [`is_old_enough`] (which fails toward "do not delete" by returning `false` on an
/// unreadable timestamp), this gate is biased toward PROTECTION: if the age cannot
/// be determined at all, the worktree is treated AS IF freshly provisioned
/// (`true`) so an indeterminate timestamp never licenses a delete inside the race
/// window. "Doubtful → KEEP."
fn is_freshly_provisioned(dir: &Path, min_age: std::time::Duration) -> bool {
    let Ok(metadata) = dir.metadata() else {
        // Cannot stat the dir at all → assume fresh and protect it.
        return true;
    };
    // Prefer the birth time; fall back to mtime where `created()` is unsupported.
    let created = metadata.created().or_else(|_| metadata.modified());
    let Ok(created) = created else {
        return true;
    };
    match created.elapsed() {
        // Younger than the floor (or a future-dated clock skew gave a tiny/zero
        // elapsed) → still inside the window → protect.
        Ok(elapsed) => elapsed < min_age,
        // `elapsed()` errors when the timestamp is in the FUTURE (clock skew) →
        // by definition not old enough → protect.
        Err(_) => true,
    }
}

/// A worktree dir is "active" if ANY session cwd equals it or is nested under
/// it (subshell cwds sometimes land inside `src/...` relative to the worktree
/// root).
pub(crate) fn is_dir_active(dir: &Path, active_cwds: &HashSet<String>) -> bool {
    let dir_str = dir.to_string_lossy();
    active_cwds
        .iter()
        .any(|cwd| path_equals_or_nested_under(cwd, dir_str.as_ref()))
}

/// #3216 (gap 3): the pure sweep decision for a single managed worktree dir.
///
/// A worktree is swept ONLY when ALL of the following hold:
///   * its NAME does not carry a protected infrastructure prefix (`release-`,
///     #3276) — deploy worktrees are owned by NO keep-set source, so they must
///     be excluded by name BEFORE any owner-based reasoning; AND
///   * the tmux query SUCCEEDED (`live_tmux_paths` is `Some`). When it FAILED
///     (`None`, #3216 P0-1) we cannot prove the worktree has no live owner, so
///     we fail-closed and KEEP everything; AND
///   * it is not the cwd (nor a parent of the cwd) of any kept session — the DB
///     keep-set built from active dispatches + recent resumable sessions; AND
///   * it is not the live `#{pane_current_path}` of any AgentDesk tmux pane
///     (equal OR a parent of one — #3216 P0-2) — the authoritative live owner.
///
/// Factored out as a pure fn (path + kept-cwd set + optional live-tmux-path set)
/// so the divorced-phantom sweep decision AND the fail-closed tmux-unavailable
/// behavior are unit-testable without touching Postgres or the real tmux server.
/// Returning `false` (KEEP) is the conservative default: if tmux is unavailable
/// OR either source claims the worktree, it survives.
pub(crate) fn should_sweep_worktree(
    dir: &Path,
    kept_cwds: &HashSet<String>,
    live_tmux_paths: Option<&HashSet<String>>,
) -> bool {
    // #3276: infrastructure worktrees (release deploy trees) are protected by
    // NAME, unconditionally — no keep-set source can ever own them, so any
    // owner-based decision below would always (wrongly) select them.
    if is_protected_infra_worktree(dir) {
        tracing::info!(
            target: "maintenance",
            job = "storage.worktree_orphan_sweep",
            path = %dir.display(),
            "protected infrastructure worktree (name prefix 'release-') — kept regardless of owner (#3276)"
        );
        return false;
    }
    // Fail-closed: a failed tmux query proves nothing about live ownership.
    let Some(live_tmux_paths) = live_tmux_paths else {
        return false;
    };
    if is_dir_active(dir, kept_cwds) {
        return false;
    }
    if has_live_tmux_owner(dir, live_tmux_paths) {
        return false;
    }
    true
}

/// True when `dir` is the live `pane_current_path` of some AgentDesk tmux
/// session — OR a parent of one (a live pane often sits in a SUBDIR of the
/// worktree, e.g. pane cwd `/worktree/src` while the scanned dir is
/// `/worktree`). Mirrors [`is_dir_active`]'s nested-path rule via the shared
/// [`path_equals_or_nested_under`] predicate so a worktree with a live pane in a
/// subdir is never swept. Compares both the raw `dir` string and its
/// canonicalized form so a symlinked / non-normalized `read_dir` path still
/// matches the canonical path tmux reports (and vice-versa).
pub(crate) fn has_live_tmux_owner(dir: &Path, live_tmux_paths: &HashSet<String>) -> bool {
    if live_tmux_paths.is_empty() {
        return false;
    }
    let dir_str = dir.to_string_lossy().to_string();
    let canonical = dir
        .canonicalize()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| dir_str.clone());
    live_tmux_paths.iter().any(|pane| {
        path_equals_or_nested_under(pane, &dir_str) || path_equals_or_nested_under(pane, &canonical)
    })
}

/// Gather the `#{pane_current_path}` of every AgentDesk-owned tmux session, both
/// raw and canonicalized, so [`has_live_tmux_owner`] can protect a worktree that
/// is the live cwd of a running pane.
///
/// Returns `None` when the tmux query FAILS — which is fundamentally different
/// from "tmux is up but has zero AgentDesk panes" (`Some(empty set)`). A FAILURE
/// means we cannot prove a worktree has no live owner, so the caller MUST
/// fail-closed and skip all deletions for this run (#3216 P0-1: a live pane
/// whose DB cwd is momentarily missing must not be swept merely because tmux was
/// temporarily unavailable). Only a SUCCESSFUL query (even an empty one) lets the
/// sweep proceed.
pub(crate) fn collect_live_tmux_pane_paths() -> Option<HashSet<String>> {
    let sessions = crate::services::platform::tmux::list_session_names().ok()?;
    fold_pane_paths(sessions, |session| {
        crate::services::platform::tmux::pane_current_path(session)
    })
}

/// Pure core of [`collect_live_tmux_pane_paths`], parameterised on the pane-path
/// query so it can be unit-tested without a live tmux server.
///
/// Fail-closed (returns `None`) the moment an AgentDesk-owned session's pane path
/// cannot be determined — either the query FAILS (`None`) or returns an empty
/// string. A per-session failure means the live-owner set would be INCOMPLETE,
/// and an incomplete set could let the caller sweep a worktree whose live pane we
/// simply failed to read (#3216 P0). Non-AgentDesk sessions are skipped BEFORE the
/// query, so an unrelated operator session failing has no effect.
fn fold_pane_paths(
    sessions: Vec<String>,
    query: impl Fn(&str) -> Option<String>,
) -> Option<HashSet<String>> {
    let mut paths = HashSet::new();
    for session in sessions {
        // Only AgentDesk-managed panes are relevant; operator-created sessions
        // must not influence the sweep.
        if !session.starts_with("AgentDesk-") {
            continue;
        }
        // A live AgentDesk session must report a non-empty pane cwd. If we cannot
        // read it, fail-closed for the whole run rather than proceed with a
        // partial set (which would risk deleting a live worktree).
        let path = query(&session)?;
        if path.is_empty() {
            return None;
        }
        if let Ok(canonical) = std::path::Path::new(&path).canonicalize() {
            paths.insert(canonical.to_string_lossy().to_string());
        }
        paths.insert(path);
    }
    Some(paths)
}

pub(crate) async fn remove_orphan_worktree(path: &Path) -> Result<()> {
    // Try `git worktree remove --force <path>` first. This requires running
    // from the parent repo, which we infer by reading the .git file inside
    // the worktree (format: `gitdir: /abs/path/.git/worktrees/<name>`).
    if let Some(repo_root) = infer_repo_root_from_worktree(path) {
        let worktree_path = path.to_path_buf();
        let _ = tokio::task::spawn_blocking(move || {
            GitCommand::new()
                .repo(&repo_root)
                .args(["worktree", "remove", "--force"])
                .arg(worktree_path)
                .run_output()
        })
        .await;
    }

    if path.exists() {
        std::fs::remove_dir_all(path)?;
    }
    Ok(())
}

/// #3231 (codex #3): classification of a managed worktree's `.git` entry for the
/// fallback in [`sweep_managed_root`] when [`infer_repo_root_from_worktree`]
/// could not resolve the parent repo. Only [`GitPointerState::Missing`] (truly no
/// `.git`) is eligible for the plain age-backstopped delete; a present-but-
/// unreadable `.git` belongs to a registered worktree whose pointer we merely
/// failed to read and must be left untouched.
enum GitPointerState {
    /// No `.git` entry at all — not a registered worktree, just a leftover dir.
    Missing,
    /// A `.git` entry EXISTS but could not be read / did not yield a gitdir — a
    /// registered worktree with an unresolvable pointer; never deleted here.
    PresentUnreadable,
}

/// #3231 (codex #3): inspect the worktree's `.git` entry WITHOUT force-deleting
/// anything. `try_exists` distinguishes "definitively absent" from "exists but
/// unreadable" (a permission/IO error is treated conservatively as present, since
/// we could not prove absence).
fn git_pointer_state(path: &Path) -> GitPointerState {
    match path.join(".git").try_exists() {
        Ok(false) => GitPointerState::Missing,
        // Exists, OR we could not even determine existence — conservatively treat
        // as a registered worktree we must not blow away.
        Ok(true) | Err(_) => GitPointerState::PresentUnreadable,
    }
}

/// #3231 (codex #3): plain recursive directory delete for an age-backstopped
/// managed leftover that has NO `.git` pointer (so there is no git-tracked or
/// uncommitted state to preserve). Deliberately does NOT invoke
/// `git worktree remove --force` — the force path is reserved for the flat-root
/// pass and must never run in the managed fallback.
fn remove_dir_all_plain(path: &Path) -> std::io::Result<()> {
    if path.exists() {
        std::fs::remove_dir_all(path)?;
    }
    Ok(())
}

fn infer_repo_root_from_worktree(path: &Path) -> Option<PathBuf> {
    let git_file = path.join(".git");
    let contents = std::fs::read_to_string(&git_file).ok()?;
    // `gitdir: /abs/path/.git/worktrees/<name>`
    let gitdir = contents
        .lines()
        .find_map(|line| line.strip_prefix("gitdir: "))
        .map(str::trim)?;
    let gitdir = PathBuf::from(gitdir);
    // Walk up from `.git/worktrees/<name>` to the repo root.
    let repo_dot_git = gitdir.parent()?.parent()?;
    repo_dot_git.parent().map(|p| p.to_path_buf())
}

#[cfg(test)]
mod resumable_keep_set_tests {
    use super::is_dir_active;
    use std::collections::HashSet;
    use std::path::Path;

    /// #3207 (part 2): a worktree whose path is in the keep-set (the union of
    /// active-dispatch cwds AND recent resumable-session cwds) must be treated as
    /// active and therefore NOT swept while idle between turns.
    #[test]
    fn resumable_cwd_protects_its_worktree_dir() {
        let dir = "/home/u/.adk/release/worktrees/claude-chan-20260101-000000";
        let mut keep: HashSet<String> = HashSet::new();
        keep.insert(dir.to_string());
        assert!(
            is_dir_active(Path::new(dir), &keep),
            "a resumable session's worktree must survive the sweep between turns"
        );
    }

    /// A nested subshell cwd inside the resumable worktree still protects the
    /// worktree root (mirrors the active-dispatch nesting rule).
    #[test]
    fn nested_resumable_cwd_protects_worktree_root() {
        let dir = "/home/u/.adk/release/worktrees/claude-chan-20260101-000000";
        let nested = format!("{dir}/src/services");
        let mut keep: HashSet<String> = HashSet::new();
        keep.insert(nested);
        assert!(is_dir_active(Path::new(dir), &keep));
    }

    /// A worktree NOT referenced by any keep-set cwd remains an orphan candidate.
    #[test]
    fn unreferenced_worktree_is_not_protected() {
        let dir = "/home/u/.adk/release/worktrees/claude-chan-stale";
        let mut keep: HashSet<String> = HashSet::new();
        keep.insert("/home/u/.adk/release/worktrees/other".to_string());
        assert!(!is_dir_active(Path::new(dir), &keep));
    }
}

#[cfg(test)]
mod naming_whitelist_tests {
    //! #3231 (A): the naming whitelist is the KEY safety fix — only worktrees the
    //! runtime actually creates (`claude-…`, `codex-adk-cdx…`, `wt-…`) are ever
    //! discard candidates. Manual dev worktrees dropped into the flat root must
    //! NEVER be swept, regardless of whether any session row owns them.
    use super::is_runtime_named_worktree;
    use std::path::Path;

    fn wt(name: &str) -> std::path::PathBuf {
        Path::new("/home/u/.adk/release/worktrees").join(name)
    }

    #[test]
    fn runtime_named_worktrees_are_discard_candidates() {
        // `create_git_worktree` flat-root forms: `{provider}-{channel}-{ts}`.
        assert!(is_runtime_named_worktree(&wt(
            "claude-adk-cc-20260607-113822"
        )));
        assert!(is_runtime_named_worktree(&wt(
            "codex-adk-cdx-20260607-113822"
        )));
        // Defensive branch-derived `wt-…` form.
        assert!(is_runtime_named_worktree(&wt("wt-claude-foo-20260607")));
    }

    #[test]
    fn manual_dev_worktrees_are_never_discard_candidates() {
        // None of these are runtime-created — they must be protected forever.
        for manual in [
            "worker-1",
            "integration-main",
            "codex-scratch", // plain `codex-*` (NOT the `codex-adk-cdx` runtime form)
            "release-2026",
            "fix-3231",
            "e2e-relay",
            "main",
        ] {
            assert!(
                !is_runtime_named_worktree(&wt(manual)),
                "manual dev worktree {manual:?} must never be a discard candidate"
            );
        }
    }
}

#[cfg(test)]
mod phantom_sweep_decision_tests {
    //! #3216 (gap 3): unit-test the pure `should_sweep_worktree` decision for the
    //! divorced/phantom per-channel worktree scenario. After gap1+gap2
    //! reconciliation points the DB cwd back to the ORIGINAL worktree, the
    //! phantom (a full checkout with its own branch, no transcript, no live
    //! owner) is a genuine orphan and must be swept — while the worktree that is
    //! a kept session's cwd OR the live tmux pane's cwd must survive.
    use super::{fold_pane_paths, has_live_tmux_owner, should_sweep_worktree};
    use std::collections::HashSet;
    use std::path::Path;

    const ORIGINAL: &str = "/home/u/.adk/release/worktrees/claude-adk-cc-20260607-113822";
    const PHANTOM: &str = "/home/u/.adk/release/worktrees/claude-adk-cc-20260607-212437";

    /// A divorced managed worktree with no live owner and not matching any kept
    /// session cwd IS selected for sweep (tmux query SUCCEEDED).
    #[test]
    fn divorced_phantom_with_no_owner_is_swept() {
        // The kept-set points at the ORIGINAL worktree (post-reconciliation),
        // and the live tmux pane is the ORIGINAL too — the phantom is divorced.
        let mut kept: HashSet<String> = HashSet::new();
        kept.insert(ORIGINAL.to_string());
        let mut live: HashSet<String> = HashSet::new();
        live.insert(ORIGINAL.to_string());

        assert!(
            should_sweep_worktree(Path::new(PHANTOM), &kept, Some(&live)),
            "a phantom worktree that is neither a kept cwd nor a live tmux pane must be swept"
        );
    }

    /// A worktree that IS a kept session's cwd is NOT swept.
    #[test]
    fn kept_session_cwd_is_not_swept() {
        let mut kept: HashSet<String> = HashSet::new();
        kept.insert(ORIGINAL.to_string());
        let live: HashSet<String> = HashSet::new(); // tmux up, zero panes

        assert!(
            !should_sweep_worktree(Path::new(ORIGINAL), &kept, Some(&live)),
            "a worktree recorded as a kept session's cwd must never be swept"
        );
    }

    /// A worktree with a live tmux owner is NOT swept — even if the DB keep-set
    /// transiently disagrees (e.g. before channel_id backfill), the live pane is
    /// the source of truth.
    #[test]
    fn live_tmux_owner_is_not_swept_even_if_not_in_keep_set() {
        let kept: HashSet<String> = HashSet::new(); // keep-set has NOTHING for it
        let mut live: HashSet<String> = HashSet::new();
        live.insert(ORIGINAL.to_string());

        assert!(
            !should_sweep_worktree(Path::new(ORIGINAL), &kept, Some(&live)),
            "a worktree that is a live tmux pane's cwd must survive regardless of the keep-set"
        );
    }

    /// (b) tmux AVAILABLE with ZERO panes (`Some(empty)`): a phantom with no
    /// kept cwd and no live pane IS swept — a successful query proved no owner.
    #[test]
    fn phantom_is_swept_when_tmux_available_with_zero_panes() {
        let kept: HashSet<String> = HashSet::new();
        let live: HashSet<String> = HashSet::new(); // tmux up, but no AgentDesk panes
        assert!(should_sweep_worktree(
            Path::new(PHANTOM),
            &kept,
            Some(&live)
        ));
    }

    /// (a) tmux UNAVAILABLE (`None`, the query FAILED): NOTHING is swept — not
    /// even a phantom that has no kept cwd and no live pane — because a failed
    /// query cannot prove the absence of a live owner (#3216 P0-1 fail-closed).
    #[test]
    fn nothing_is_swept_when_tmux_unavailable() {
        let kept: HashSet<String> = HashSet::new();
        // Even the most clearly-orphaned phantom must survive a failed tmux query.
        assert!(
            !should_sweep_worktree(Path::new(PHANTOM), &kept, None),
            "tmux-unavailable (failed query) must suppress ALL deletions, even of phantoms"
        );
    }

    /// (c) A live pane sitting in a SUBDIR of a worktree (e.g. `/worktree/src`)
    /// protects the worktree root — it must be KEPT (#3216 P0-2 nested match).
    #[test]
    fn live_pane_in_subdir_keeps_worktree() {
        let kept: HashSet<String> = HashSet::new();
        let mut live: HashSet<String> = HashSet::new();
        live.insert(format!("{ORIGINAL}/src/services"));

        assert!(
            has_live_tmux_owner(Path::new(ORIGINAL), &live),
            "a live pane nested under the worktree must be recognized as an owner"
        );
        assert!(
            !should_sweep_worktree(Path::new(ORIGINAL), &kept, Some(&live)),
            "a worktree whose live pane sits in a subdir must never be swept"
        );
    }

    /// A pane path that merely shares a STRING PREFIX (no `/` boundary) with the
    /// worktree must NOT be treated as an owner — guards the nested-match logic.
    #[test]
    fn sibling_prefix_pane_does_not_keep_worktree() {
        let kept: HashSet<String> = HashSet::new();
        let mut live: HashSet<String> = HashSet::new();
        // `ORIGINAL` + suffix without a path separator — a different directory.
        live.insert(format!("{ORIGINAL}-sibling"));

        assert!(!has_live_tmux_owner(Path::new(ORIGINAL), &live));
        assert!(should_sweep_worktree(
            Path::new(ORIGINAL),
            &kept,
            Some(&live)
        ));
    }

    /// `has_live_tmux_owner` returns false against an empty live set and true on
    /// an exact path match.
    #[test]
    fn has_live_tmux_owner_basic() {
        let empty: HashSet<String> = HashSet::new();
        assert!(!has_live_tmux_owner(Path::new(ORIGINAL), &empty));

        let mut live: HashSet<String> = HashSet::new();
        live.insert(ORIGINAL.to_string());
        assert!(has_live_tmux_owner(Path::new(ORIGINAL), &live));
        assert!(!has_live_tmux_owner(Path::new(PHANTOM), &live));
    }

    /// #3216 P0: a successful, complete query yields `Some(set)` containing every
    /// AgentDesk pane path; non-AgentDesk sessions are excluded.
    #[test]
    fn fold_pane_paths_collects_agentdesk_panes_only() {
        let sessions = vec![
            "AgentDesk-claude-adk-cc".to_string(),
            "operator-shell".to_string(),
        ];
        let result = fold_pane_paths(sessions, |s| match s {
            "AgentDesk-claude-adk-cc" => Some(ORIGINAL.to_string()),
            _ => None, // non-AgentDesk failing must NOT abort the collection
        })
        .expect("complete agentdesk query yields Some");
        assert!(result.contains(ORIGINAL));
        assert_eq!(result.len(), 1);
    }

    /// #3216 P0 (fail-closed): if ANY AgentDesk session's pane path cannot be read
    /// (`None`), the whole collection fails-closed (`None`) so the caller skips
    /// deletion — a partial set must never drive a sweep.
    #[test]
    fn fold_pane_paths_fails_closed_on_agentdesk_query_failure() {
        let sessions = vec![
            "AgentDesk-claude-adk-cc".to_string(),
            "AgentDesk-flaky".to_string(),
        ];
        let result = fold_pane_paths(sessions, |s| match s {
            "AgentDesk-claude-adk-cc" => Some(ORIGINAL.to_string()),
            _ => None, // a live AgentDesk session whose pane path query failed
        });
        assert!(
            result.is_none(),
            "partial AgentDesk failure must fail-closed"
        );
    }

    /// #3216 P0 (fail-closed): an empty pane path is also indeterminate.
    #[test]
    fn fold_pane_paths_fails_closed_on_empty_pane_path() {
        let sessions = vec!["AgentDesk-claude-adk-cc".to_string()];
        let result = fold_pane_paths(sessions, |_| Some(String::new()));
        assert!(result.is_none(), "empty pane path must fail-closed");
    }
}

#[cfg(test)]
mod deploy_worktree_protection_tests {
    //! #3276: the release deploy worktree (`release-main-deploy-*`) under the
    //! flat scan root is deployment infrastructure — never a dispatch cwd,
    //! never a resumable session cwd, never a live AgentDesk tmux pane — so
    //! EVERY owner-based KEEP condition misses it by construction. The
    //! name-prefix guard in `should_sweep_worktree` must keep it
    //! unconditionally, in BOTH the flat-root and managed-root passes, while
    //! non-protected names keep the existing sweep behavior.
    use super::{is_protected_infra_worktree, should_sweep_worktree};
    use std::collections::HashSet;
    use std::path::Path;

    const DEPLOY: &str = "/home/u/.adk/release/worktrees/release-main-deploy-20260530";
    const PHANTOM: &str = "/home/u/.adk/release/worktrees/claude-adk-cc-20260607-212437";

    /// The exact #3276 failure mode: tmux query SUCCEEDED with zero panes and
    /// the DB keep-set is empty — every KEEP condition misses, yet the deploy
    /// worktree must never be swept.
    #[test]
    fn deploy_worktree_is_never_swept_when_all_keep_conditions_miss() {
        let kept: HashSet<String> = HashSet::new();
        let live: HashSet<String> = HashSet::new(); // tmux up, zero AgentDesk panes
        assert!(
            !should_sweep_worktree(Path::new(DEPLOY), &kept, Some(&live)),
            "the release deploy worktree must survive even with no owner in any keep-set"
        );
    }

    /// A populated keep-set / live-pane set that points elsewhere changes
    /// nothing — the protection is independent of every owner signal.
    #[test]
    fn deploy_worktree_is_kept_with_unrelated_keepset_and_panes() {
        let mut kept: HashSet<String> = HashSet::new();
        kept.insert(PHANTOM.to_string());
        let mut live: HashSet<String> = HashSet::new();
        live.insert(PHANTOM.to_string());
        assert!(!should_sweep_worktree(
            Path::new(DEPLOY),
            &kept,
            Some(&live)
        ));
    }

    /// The managed-root pass (`sweep_managed_root`) reuses the same
    /// `should_sweep_worktree` — a `release-*` name one level deeper
    /// (`worktrees/<repo>/release-…`) is protected there too.
    #[test]
    fn deploy_worktree_under_managed_root_is_protected_too() {
        let nested = "/home/u/.adk/release/worktrees/agentdesk/release-main-deploy-20260530";
        let kept: HashSet<String> = HashSet::new();
        let live: HashSet<String> = HashSet::new();
        assert!(!should_sweep_worktree(
            Path::new(nested),
            &kept,
            Some(&live)
        ));
    }

    /// Protection matches on the directory NAME only — the scan root itself
    /// lives under `~/.adk/release/`, so a full-path match would protect
    /// everything. A non-protected runtime-named orphan with no owner keeps
    /// the existing sweep behavior (it IS swept).
    #[test]
    fn non_protected_names_keep_existing_sweep_behavior() {
        let kept: HashSet<String> = HashSet::new();
        let live: HashSet<String> = HashSet::new();
        assert!(
            should_sweep_worktree(Path::new(PHANTOM), &kept, Some(&live)),
            "an unowned runtime-named worktree must remain a sweep candidate"
        );
    }

    /// Predicate basics: prefix match on the dir NAME, case-insensitive
    /// (mirroring `is_runtime_named_worktree`); names merely CONTAINING
    /// `release` (or with it mid-name) are not protected.
    #[test]
    fn protected_infra_name_predicate() {
        assert!(is_protected_infra_worktree(Path::new(DEPLOY)));
        assert!(is_protected_infra_worktree(Path::new(
            "/x/Release-Main-Deploy-20260530"
        )));
        assert!(is_protected_infra_worktree(Path::new("/x/release-2026")));
        assert!(!is_protected_infra_worktree(Path::new(PHANTOM)));
        assert!(!is_protected_infra_worktree(Path::new(
            "/x/pre-release-main-deploy"
        )));
        assert!(!is_protected_infra_worktree(Path::new("/x/main")));
    }
}

#[cfg(test)]
mod resumable_keep_set_query_tests {
    //! #3207 P1 / #3231 (A): exercise the REAL `fetch_resumable_cwds` query
    //! against Postgres so the GUID-primary keep rule is verified, not just the
    //! `is_dir_active` path-matching stub.
    //!
    //! #3231 makes the resume GUID the PRIMARY keep signal with TIME only as a
    //! generous 30d backstop over `COALESCE(last_heartbeat, created_at)`:
    //!   * a non-null GUID is REQUIRED (a cleared / never-recorded GUID is
    //!     collectable — nothing to `--resume` into);
    //!   * a GUID row that NEVER heartbeated is KEPT until 30d after creation
    //!     (the previous query excluded all NULL-heartbeat rows, losing a
    //!     resumable transcript between turns);
    //!   * a very old row (beyond the 30d backstop) is collectable again so disk
    //!     stays bounded;
    //!   * only the LATEST row PER CHANNEL is protected (per-channel bound).
    use super::fetch_resumable_cwds;
    use crate::db::auto_queue::test_support::TestPostgresDb;

    #[allow(clippy::too_many_arguments)]
    async fn seed(
        pool: &sqlx::PgPool,
        session_key: &str,
        channel_id: Option<&str>,
        cwd: &str,
        claude_session_id: Option<&str>,
        heartbeat_sql: &str,
        created_sql: &str,
    ) {
        let query = format!(
            "INSERT INTO sessions \
             (session_key, provider, status, cwd, channel_id, claude_session_id, \
              last_heartbeat, created_at) \
             VALUES ($1, 'claude', 'idle', $2, $3, $4, {heartbeat_sql}, {created_sql})"
        );
        sqlx::query(&query)
            .bind(session_key)
            .bind(cwd)
            .bind(channel_id)
            .bind(claude_session_id)
            .execute(pool)
            .await
            .expect("seed sessions row");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn keep_set_is_guid_primary_and_per_channel() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        // (1) fresh resumable session with a GUID → KEPT.
        seed(
            &pool,
            "k-fresh",
            Some("1001"),
            "/wt/fresh",
            Some("sid-fresh"),
            "NOW()",
            "NOW()",
        )
        .await;
        // (2) #3231: NULL heartbeat but a GUID + fresh created_at → KEPT now
        //     (GUID is the primary signal; the next turn can still --resume).
        seed(
            &pool,
            "k-null-hb",
            Some("1002"),
            "/wt/null-hb",
            Some("sid-null"),
            "NULL",
            "NOW()",
        )
        .await;
        // (3) GUID row beyond the 30d far backstop (both heartbeat AND created_at
        //     old) → EXCLUDED so disk stays bounded.
        seed(
            &pool,
            "k-stale",
            Some("1003"),
            "/wt/stale",
            Some("sid-stale"),
            "NOW() - INTERVAL '60 days'",
            "NOW() - INTERVAL '60 days'",
        )
        .await;
        // (4) #3231: GUID was CLEARED (NULL) → EXCLUDED (nothing to resume into),
        //     even with a fresh heartbeat.
        seed(
            &pool,
            "k-no-sid",
            Some("1004"),
            "/wt/no-sid",
            None,
            "NOW()",
            "NOW()",
        )
        .await;
        // (5) two resumable sessions for the SAME channel → only the LATEST
        //     cwd is kept (per-channel bound).
        seed(
            &pool,
            "k-chan5-old",
            Some("1005"),
            "/wt/chan5-old",
            Some("sid-5-old"),
            "NOW() - INTERVAL '3 hours'",
            "NOW() - INTERVAL '3 hours'",
        )
        .await;
        seed(
            &pool,
            "k-chan5-new",
            Some("1005"),
            "/wt/chan5-new",
            Some("sid-5-new"),
            "NOW() - INTERVAL '10 minutes'",
            "NOW() - INTERVAL '10 minutes'",
        )
        .await;

        let kept = fetch_resumable_cwds(&pool).await.expect("query keep-set");

        assert!(
            kept.contains("/wt/fresh"),
            "fresh resumable cwd must be kept"
        );
        assert!(
            kept.contains("/wt/null-hb"),
            "#3231: a GUID row that never heartbeated must survive until the 30d backstop"
        );
        assert!(
            !kept.contains("/wt/stale"),
            "a GUID row beyond the 30d far backstop must be collectable"
        );
        assert!(
            !kept.contains("/wt/no-sid"),
            "#3231: a cleared (NULL) GUID has nothing to resume into → not kept"
        );
        assert!(
            kept.contains("/wt/chan5-new"),
            "the latest session for a channel must keep its worktree"
        );
        assert!(
            !kept.contains("/wt/chan5-old"),
            "an older session for the same channel must NOT add a second worktree"
        );

        pool.close().await;
        pg_db.drop().await;
    }
}

#[cfg(test)]
mod active_dispatch_worktree_keep_set_tests {
    //! #3231 (codex #1): an active managed dispatch records its worktree under
    //! `task_dispatches.context.worktree_path` at CREATE time — before any
    //! `sessions.cwd` or live tmux pane exists. `fetch_active_dispatch_worktree_paths`
    //! must surface those paths (from context AND result, for `pending`/`dispatched`
    //! dispatches) plus `pr_tracking.worktree_path`, so a just-provisioned worktree
    //! is not deleted out from under the dispatch.
    use super::fetch_active_dispatch_worktree_paths;
    use crate::db::auto_queue::test_support::TestPostgresDb;

    async fn seed_dispatch(
        pool: &sqlx::PgPool,
        id: &str,
        status: &str,
        context: Option<&str>,
        result: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context, result) \
             VALUES ($1, 'agent-1', $2, $3, $4)",
        )
        .bind(id)
        .bind(status)
        .bind(context)
        .bind(result)
        .execute(pool)
        .await
        .expect("seed task_dispatches row");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn active_dispatch_worktree_paths_are_collected() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        // (1) pending dispatch with context.worktree_path → KEPT.
        seed_dispatch(
            &pool,
            "d-pending",
            "pending",
            Some(r#"{"worktree_path":"/wt/managed-pending"}"#),
            None,
        )
        .await;
        // (2) dispatched dispatch with result.completed_worktree_path → KEPT.
        seed_dispatch(
            &pool,
            "d-dispatched",
            "dispatched",
            None,
            Some(r#"{"completed_worktree_path":"/wt/managed-completed"}"#),
        )
        .await;
        // (3) terminal (completed) dispatch → its worktree is NOT kept by this set
        //     (terminal cleanup owns it; absence from the active set is correct).
        seed_dispatch(
            &pool,
            "d-completed",
            "completed",
            Some(r#"{"worktree_path":"/wt/managed-terminal"}"#),
            None,
        )
        .await;
        // (4) pending dispatch with no worktree_path → contributes nothing.
        seed_dispatch(
            &pool,
            "d-no-wt",
            "pending",
            Some(r#"{"auto_queue":true}"#),
            None,
        )
        .await;

        let kept = fetch_active_dispatch_worktree_paths(&pool)
            .await
            .expect("query active-dispatch worktree paths");

        assert!(
            kept.contains("/wt/managed-pending"),
            "a pending dispatch's context.worktree_path must be kept"
        );
        assert!(
            kept.contains("/wt/managed-completed"),
            "a dispatched dispatch's result.completed_worktree_path must be kept"
        );
        assert!(
            !kept.contains("/wt/managed-terminal"),
            "a terminal dispatch's worktree must NOT be in the active keep-set"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pr_tracking_worktree_path_is_collected() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        // pr_tracking.card_id FKs kanban_cards(id); seed the card first.
        sqlx::query("INSERT INTO kanban_cards (id, title) VALUES ('card-1', 't')")
            .execute(&pool)
            .await
            .expect("seed kanban card");
        sqlx::query(
            "INSERT INTO pr_tracking (card_id, worktree_path) VALUES ('card-1', '/wt/pr-tracked')",
        )
        .execute(&pool)
        .await
        .expect("seed pr_tracking row");

        let kept = fetch_active_dispatch_worktree_paths(&pool)
            .await
            .expect("query active-dispatch worktree paths");
        assert!(
            kept.contains("/wt/pr-tracked"),
            "a live pr_tracking.worktree_path must be kept"
        );

        pool.close().await;
        pg_db.drop().await;
    }
}

#[cfg(test)]
mod managed_root_recursion_tests {
    //! #3231 (B): the managed dispatch/automation worktrees live one level deeper
    //! under `worktrees/<repo_name>/` — the flat 1-depth scan never reached them.
    //! These tests build a real git repo + managed worktree on disk and verify
    //! the recursion classifier (`is_managed_root_child`) reaches managed-root
    //! children, that a terminal (unowned) managed worktree is swept while an
    //! owned one survives, and that dirty managed worktrees are preserved.
    use super::{Config, is_managed_root_child, is_runtime_named_worktree, run_inner};
    use crate::services::git::GitCommand;
    use std::collections::HashSet;
    use std::path::Path;
    // `cleanup_managed_worktree` resolves the managed root via the PROCESS-GLOBAL
    // `AGENTDESK_ROOT_DIR` env var (`managed_worktrees_root`), so the tests that
    // drive it must serialize against every env-mutating test in the crate.

    /// Run `git <args>` in `repo` via the centralised `GitCommand` helper (the
    /// audit gate forbids raw `Command::new("git")` outside `src/services/git`).
    fn git(repo: &Path, args: &[&str]) {
        let output = GitCommand::new()
            .repo(repo)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .run_output()
            .expect("run git");
        assert!(output.status.success(), "git {args:?} failed");
    }

    /// Build a repo with `main`, an `origin/main` ref (so the mainline-merged
    /// guard can resolve), and a managed worktree checked out at `main` HEAD.
    /// Returns (worktrees_root, managed_root, managed_worktree_path).
    fn setup_repo_with_managed_worktree(
        base: &Path,
    ) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let repo = base.join("agentdesk");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("README"), b"x").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "init"]);
        // A self-referencing `origin/main` so `merge-base --is-ancestor` works.
        let head = repo.join(".git/refs/heads/main");
        let origin = repo.join(".git/refs/remotes/origin");
        std::fs::create_dir_all(&origin).unwrap();
        std::fs::copy(&head, origin.join("main")).unwrap();

        // managed root = worktrees/<repo_name>/ ; the flat worktrees root is its
        // parent (mirrors `managed_worktrees_root`).
        let worktrees_root = base.join("worktrees");
        let managed_root = worktrees_root.join("agentdesk");
        std::fs::create_dir_all(&managed_root).unwrap();
        let wt = managed_root.join("issue-3231-20260607");
        // `--detach` at `main` HEAD: git refuses to check out `main` in a second
        // worktree while it is the primary checkout, so detach instead. HEAD is
        // still the mainline commit, so `merge-base --is-ancestor` reports merged.
        git(
            &repo,
            &["worktree", "add", "--detach", wt.to_str().unwrap(), "main"],
        );
        (worktrees_root, managed_root, wt)
    }

    #[test]
    fn managed_root_child_is_classified_and_worktree_is_not() {
        let tmp = tempfile::tempdir().unwrap();
        let (_root, managed_root, wt) = setup_repo_with_managed_worktree(tmp.path());
        // The managed root has no `.git` file but contains a child worktree.
        assert!(
            is_managed_root_child(&managed_root),
            "worktrees/<repo>/ must be recognized as a managed-root container"
        );
        // A registered worktree (has a `.git` FILE) is NOT a managed root.
        assert!(
            !is_managed_root_child(&wt),
            "a registered git worktree must not be treated as a managed root"
        );
    }

    /// Build the repo with the managed root resolving to `tmp` (so
    /// `is_managed_worktree_path` recognizes the worktree) under the env lock,
    /// then run `body` with the lock still held.
    fn with_managed_root_env<R>(body: impl FnOnce(&Path, &Path, &Path, &Path) -> R) -> R {
        let _guard = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        // managed_worktrees_root(repo) = $AGENTDESK_ROOT_DIR/worktrees/<repo_name>.
        // Point the runtime root at tmp so it equals our on-disk managed root.
        // SAFETY: serialized by ENV_LOCK; restored before the lock is released.
        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path());
        }
        let (worktrees_root, managed_root, wt) = setup_repo_with_managed_worktree(tmp.path());
        let repo = tmp.path().join("agentdesk");
        let result = body(&repo, &worktrees_root, &managed_root, &wt);
        unsafe {
            std::env::remove_var("AGENTDESK_ROOT_DIR");
        }
        result
    }

    #[test]
    fn terminal_managed_worktree_is_swept_via_recursion() {
        with_managed_root_env(|repo, worktrees_root, _managed_root, wt| {
            assert!(wt.exists());
            // Clean + mainline-merged managed worktree, no owner → cleanup removes
            // it. (The owner-gate + recursion dispatch is covered by the pure
            // `should_sweep_worktree` tests and `is_managed_root_child`; here we
            // assert the removal arm the recursion delegates to.)
            let cleanup = crate::services::git::cleanup_managed_worktree(
                repo.to_str().unwrap(),
                wt.to_str().unwrap(),
            );
            assert_eq!(
                cleanup.removed, 1,
                "clean+merged managed worktree is removed"
            );
            assert!(!wt.exists(), "managed worktree dir is gone after cleanup");
            assert!(worktrees_root.exists());
        });
    }

    #[test]
    fn dirty_managed_worktree_is_preserved() {
        with_managed_root_env(|repo, _worktrees_root, _managed_root, wt| {
            // Make the worktree dirty — the cleanup guard must skip it.
            std::fs::write(wt.join("DIRTY"), b"uncommitted").unwrap();
            let cleanup = crate::services::git::cleanup_managed_worktree(
                repo.to_str().unwrap(),
                wt.to_str().unwrap(),
            );
            assert_eq!(
                cleanup.removed, 0,
                "dirty managed worktree must NOT be removed"
            );
            assert_eq!(cleanup.skipped_dirty, 1);
            assert!(wt.exists(), "dirty managed worktree dir survives");
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_pg_is_noop_even_with_managed_orphans() {
        let tmp = tempfile::tempdir().unwrap();
        let (worktrees_root, _managed_root, wt) = setup_repo_with_managed_worktree(tmp.path());
        // No PG → the whole sweep is a no-op (fail-closed): nothing is deleted.
        let config = Config {
            worktrees_root,
            dry_run: false,
        };
        let report = run_inner(&config, None).await.unwrap();
        assert!(!report.pg_available);
        assert_eq!(report.removed_dirs, 0);
        assert_eq!(report.managed_removed, 0);
        assert!(wt.exists(), "no-PG sweep must never delete a worktree");
    }

    #[test]
    fn manual_worktree_in_flat_root_is_protected_by_naming() {
        // A `worker-*` style manual worktree dropped in the flat root: even with
        // no owning row it is NOT a discard candidate (naming whitelist).
        let _keep: HashSet<String> = HashSet::new();
        let manual = Path::new("/home/u/.adk/release/worktrees/worker-1");
        assert!(!is_runtime_named_worktree(manual));
    }
}

#[cfg(test)]
mod git_pointer_fallback_tests {
    //! #3231 (codex #3): the managed fallback (used when the `.git` gitdir pointer
    //! cannot be resolved) must NEVER force-remove. A present-but-unreadable `.git`
    //! belongs to a registered worktree (possibly dirty/unmerged) and must be
    //! SKIPPED; only a genuinely `.git`-less leftover dir is eligible for a plain
    //! delete, and only when age-backstopped.
    use super::{GitPointerState, git_pointer_state, remove_dir_all_plain};

    #[test]
    fn missing_git_is_classified_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("leftover");
        std::fs::create_dir_all(&dir).unwrap();
        // No `.git` entry at all → eligible for the plain age-backstopped delete.
        assert!(matches!(git_pointer_state(&dir), GitPointerState::Missing));
    }

    #[test]
    fn present_git_file_is_classified_present_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("registered");
        std::fs::create_dir_all(&dir).unwrap();
        // A `.git` FILE that does NOT yield a resolvable gitdir pointer — this is a
        // registered worktree whose pointer we could not resolve. It must be
        // classified PresentUnreadable so the fallback SKIPS it (never deletes).
        std::fs::write(dir.join(".git"), b"garbage-not-a-gitdir-pointer").unwrap();
        assert!(matches!(
            git_pointer_state(&dir),
            GitPointerState::PresentUnreadable
        ));
        // The dir must still exist — classification never deletes.
        assert!(dir.exists());
    }

    #[test]
    fn present_git_dir_is_classified_present_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("with-git-dir");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        // A `.git` DIRECTORY (not a worktree pointer file) also counts as present —
        // we conservatively never force-delete it via the managed fallback.
        assert!(matches!(
            git_pointer_state(&dir),
            GitPointerState::PresentUnreadable
        ));
    }

    #[test]
    fn remove_dir_all_plain_removes_only_existing_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("leftover");
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("file"), b"x").unwrap();
        remove_dir_all_plain(&dir).expect("plain remove succeeds");
        assert!(!dir.exists());
        // Idempotent: removing a non-existent path is a no-op (no error).
        remove_dir_all_plain(&dir).expect("plain remove of missing dir is a no-op");
    }
}

#[cfg(test)]
mod keep_set_query_failure_fail_closed_tests {
    //! #3231 (codex #2): a FAILED keep-set query must suppress ALL deletions for
    //! the run (fail-closed), exactly like a failed tmux probe. A closed pool makes
    //! every keep-set query error, so the sweep must delete nothing even though a
    //! runtime-named orphan sits in the flat root with no owner.
    use super::{Config, run_inner};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closed_pool_keep_set_query_error_skips_all_deletions() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        // Closing the pool makes the keep-set queries return Err — simulating a
        // PG/schema/query failure mid-run.
        pool.close().await;

        let tmp = tempfile::tempdir().unwrap();
        let worktrees_root = tmp.path().join("worktrees");
        // A runtime-named flat-root dir with NO owner — normally a prime orphan.
        let orphan = worktrees_root.join("claude-adk-cc-20260607-000000");
        std::fs::create_dir_all(&orphan).unwrap();

        let config = Config {
            worktrees_root,
            dry_run: false,
        };
        let report = run_inner(&config, Some(pool)).await.unwrap();

        assert!(report.pg_available, "pool was present (just failing)");
        assert_eq!(
            report.removed_dirs, 0,
            "a keep-set query failure must suppress ALL flat-root deletions"
        );
        assert_eq!(report.managed_removed, 0);
        assert!(
            orphan.exists(),
            "the orphan must survive a keep-set query failure (fail-closed)"
        );

        pg_db.drop().await;
    }
}

#[cfg(test)]
mod fresh_provision_toctou_tests {
    //! #3231 (codex re-review, TOCTOU): the keep-set snapshot is built once at the
    //! start of the run, but dispatch creation provisions a managed worktree BEFORE
    //! it commits the owning `task_dispatches` row. A snapshot taken inside that
    //! create→insert window sees a just-provisioned clean/merged worktree with NO
    //! keep-set owner and NO live tmux pane — the managed pass would delete it out
    //! from under the in-flight dispatch.
    //!
    //! The fix is a creation-age floor (`MANAGED_FRESH_PROVISION_MIN_AGE`): a
    //! managed worktree younger than the floor is PROTECTED regardless of owner.
    //! These tests prove (a) a too-young managed worktree is SKIPPED even with no
    //! owner and a successful (zero-pane) tmux query, and exercise the pure age
    //! predicate at the boundaries. The "sufficiently old terminal managed worktree
    //! is still removed" guarantee is covered by
    //! `managed_root_recursion_tests::terminal_managed_worktree_is_swept_via_recursion`
    //! plus the `min_age == ZERO` boundary case below (a real dir clears a zero
    //! floor, so the gate does not block deletion of old-enough worktrees).
    use super::{
        Config, MANAGED_FRESH_PROVISION_MIN_AGE, SweepReport, is_freshly_provisioned,
        sweep_managed_root,
    };
    use std::collections::HashSet;

    #[test]
    fn just_created_dir_is_protected() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("fresh");
        std::fs::create_dir_all(&dir).unwrap();
        // Created microseconds ago → well under the 30m floor → protected.
        assert!(
            is_freshly_provisioned(&dir, MANAGED_FRESH_PROVISION_MIN_AGE),
            "a just-created worktree must be treated as freshly provisioned"
        );
    }

    #[test]
    fn zero_floor_never_protects_an_existing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("any");
        std::fs::create_dir_all(&dir).unwrap();
        // With a ZERO floor a real (non-future) dir is always "old enough" → NOT
        // protected → the gate does not block deletion of sufficiently-aged trees.
        assert!(
            !is_freshly_provisioned(&dir, std::time::Duration::ZERO),
            "a zero min-age floor must never protect an existing dir"
        );
    }

    #[test]
    fn unstatable_path_is_protected() {
        // A path we cannot stat at all (does not exist) must fail-closed toward
        // protection — an indeterminate age must never license a delete.
        let missing = std::path::Path::new("/nonexistent/worktree/path/xyz");
        assert!(
            is_freshly_provisioned(missing, MANAGED_FRESH_PROVISION_MIN_AGE),
            "an unstatable path must be treated as freshly provisioned (KEEP)"
        );
    }

    /// (a) End-to-end: a freshly-created managed worktree with NO owner and a
    /// successful (zero-pane) tmux query is SKIPPED by the managed recursion —
    /// `protected_fresh` is incremented and nothing is removed. This is the exact
    /// TOCTOU scenario: the worktree exists on disk but its owning dispatch row has
    /// not yet landed in the keep-set snapshot (here: empty keep-sets).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fresh_managed_worktree_with_no_owner_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        // managed root = worktrees/<repo>/ ; create a child dir that looks like a
        // just-provisioned managed worktree (created now → inside the floor).
        let managed_root = tmp.path().join("worktrees").join("agentdesk");
        let wt = managed_root.join("issue-9999-fresh");
        std::fs::create_dir_all(&wt).unwrap();

        // Empty keep-sets (the row has not landed) + a SUCCESSFUL tmux query with
        // zero panes — so the owner gate alone would select the worktree for sweep.
        let kept: HashSet<String> = HashSet::new();
        let live: HashSet<String> = HashSet::new();
        let config = Config {
            worktrees_root: tmp.path().join("worktrees"),
            dry_run: false,
        };
        let mut report = SweepReport::default();

        sweep_managed_root(&managed_root, &kept, &live, &config, &mut report).await;

        assert_eq!(
            report.protected_fresh, 1,
            "a too-young managed worktree must be protected by the age floor"
        );
        assert_eq!(
            report.managed_removed, 0,
            "a freshly-provisioned managed worktree must never be removed"
        );
        assert_eq!(
            report.orphan_count, 0,
            "a protected-fresh worktree must not even be counted as an orphan"
        );
        assert!(
            wt.exists(),
            "the freshly-provisioned worktree dir must survive the sweep (TOCTOU)"
        );
    }
}
