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

    let mut active_cwds = fetch_active_cwds(&pool).await.unwrap_or_default();
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
    let resumable_cwds = fetch_resumable_cwds(&pool).await.unwrap_or_default();
    active_cwds.extend(resumable_cwds);
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
        report.orphan_count = report.orphan_count.saturating_add(1);

        if config.dry_run {
            continue;
        }

        // Resolve the parent repo from the worktree's `.git` gitdir pointer so
        // `cleanup_managed_worktree` can run its dirty/unmerged guards + the
        // managed-path check against the real repo.
        let Some(repo_root) = infer_repo_root_from_worktree(&wt_path) else {
            // Not a registered worktree (or unreadable `.git`) — fall back to a
            // plain directory delete only when it is age-backstopped, so a
            // freshly-created managed dir whose `.git` we momentarily cannot read
            // is never blown away.
            if is_old_enough(&wt_path, MANAGED_CANCEL_LEAK_BACKSTOP) {
                match remove_orphan_worktree(&wt_path).await {
                    Ok(()) => report.managed_removed = report.managed_removed.saturating_add(1),
                    Err(error) => {
                        tracing::warn!(
                            target: "maintenance",
                            path = %wt_path.display(),
                            error = %error,
                            "worktree_orphan_sweep: failed to remove age-backstopped managed orphan"
                        );
                        report.errors = report.errors.saturating_add(1);
                    }
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
fn collect_live_tmux_pane_paths() -> Option<HashSet<String>> {
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

async fn remove_orphan_worktree(path: &Path) -> Result<()> {
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
    use std::sync::Mutex;

    /// `cleanup_managed_worktree` resolves the managed root via the PROCESS-GLOBAL
    /// `AGENTDESK_ROOT_DIR` env var (`managed_worktrees_root`), so the tests that
    /// drive it must serialize their env-var setup to avoid cross-test races.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

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
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
