//! `storage.worktree_orphan_sweep` — hourly detection and cleanup of orphaned
//! git worktree directories under `~/.adk/release/worktrees/`.
//!
//! A directory is considered an orphan when NO row in `task_dispatches` with
//! `status IN ('pending', 'dispatched')` has an associated `sessions.cwd`
//! matching the directory path (either exactly or as a path prefix).
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
        if !should_sweep_worktree(&dir_path, &active_cwds, Some(&live_tmux_paths)) {
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

/// #3207 (part 2) P1: cwds of recent resumable sessions — those carrying a
/// recorded provider session id (`claude_session_id` / `raw_provider_session_id`)
/// whose worktree the next turn's `--resume` reuses, so they must not be swept
/// while idle between turns.
///
/// The keep-set is BOUNDED so abandoned sessions cannot permanently leak disk:
///   * only the LATEST resumable session PER CHANNEL is protected
///     (`DISTINCT ON (channel partition) ... ORDER BY last_heartbeat DESC`), so
///     a channel reuses ONE worktree rather than pinning every historical row;
///   * the heartbeat must be NON-NULL and within the freshness window. The
///     previous query kept `last_heartbeat IS NULL` rows forever, so a session
///     that recorded a provider id but never (or long-ago) heartbeated pinned
///     its worktree permanently. Excluding NULL/stale heartbeats lets genuinely
///     abandoned worktrees become collectable again.
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
           AND last_heartbeat IS NOT NULL
           AND last_heartbeat >= NOW() - INTERVAL '7 days'
         ORDER BY COALESCE(channel_id, thread_channel_id, session_key),
                  last_heartbeat DESC",
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
        path_equals_or_nested_under(pane, &dir_str)
            || path_equals_or_nested_under(pane, &canonical)
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
    let mut paths = HashSet::new();
    let sessions = crate::services::platform::tmux::list_session_names().ok()?;
    for session in sessions {
        // Only AgentDesk-managed panes are relevant; operator-created sessions
        // must not influence the sweep.
        if !session.starts_with("AgentDesk-") {
            continue;
        }
        if let Some(path) = crate::services::platform::tmux::pane_current_path(&session) {
            if path.is_empty() {
                continue;
            }
            if let Ok(canonical) = std::path::Path::new(&path).canonicalize() {
                paths.insert(canonical.to_string_lossy().to_string());
            }
            paths.insert(path);
        }
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
mod phantom_sweep_decision_tests {
    //! #3216 (gap 3): unit-test the pure `should_sweep_worktree` decision for the
    //! divorced/phantom per-channel worktree scenario. After gap1+gap2
    //! reconciliation points the DB cwd back to the ORIGINAL worktree, the
    //! phantom (a full checkout with its own branch, no transcript, no live
    //! owner) is a genuine orphan and must be swept — while the worktree that is
    //! a kept session's cwd OR the live tmux pane's cwd must survive.
    use super::{has_live_tmux_owner, should_sweep_worktree};
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
        assert!(should_sweep_worktree(Path::new(PHANTOM), &kept, Some(&live)));
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
        assert!(should_sweep_worktree(Path::new(ORIGINAL), &kept, Some(&live)));
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
}

#[cfg(test)]
mod resumable_keep_set_query_tests {
    //! #3207 (part 2) P1: exercise the REAL `fetch_resumable_cwds` query against
    //! Postgres so the bound (latest fresh-heartbeat resumable session PER
    //! CHANNEL; NULL/stale heartbeats excluded) is verified, not just the
    //! `is_dir_active` path-matching stub. The previous query kept every session
    //! with a provider id forever (`last_heartbeat IS NULL` matched), so an
    //! abandoned session permanently pinned its worktree — a disk leak. These
    //! assertions are RED against that query and GREEN against the bounded one.
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
    ) {
        let query = format!(
            "INSERT INTO sessions \
             (session_key, provider, status, cwd, channel_id, claude_session_id, last_heartbeat) \
             VALUES ($1, 'claude', 'idle', $2, $3, $4, {heartbeat_sql})"
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
    async fn keep_set_is_bounded_and_per_channel() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        // (1) fresh resumable session → KEPT.
        seed(
            &pool,
            "k-fresh",
            Some("1001"),
            "/wt/fresh",
            Some("sid-fresh"),
            "NOW()",
        )
        .await;
        // (2) NULL heartbeat resumable → EXCLUDED (the unbounded-leak case).
        seed(
            &pool,
            "k-null-hb",
            Some("1002"),
            "/wt/null-hb",
            Some("sid-null"),
            "NULL",
        )
        .await;
        // (3) stale heartbeat (older than the freshness window) → EXCLUDED.
        seed(
            &pool,
            "k-stale",
            Some("1003"),
            "/wt/stale",
            Some("sid-stale"),
            "NOW() - INTERVAL '30 days'",
        )
        .await;
        // (4) fresh heartbeat but NO provider session id → EXCLUDED (nothing to
        //     resume into).
        seed(&pool, "k-no-sid", Some("1004"), "/wt/no-sid", None, "NOW()").await;
        // (5) two resumable sessions for the SAME channel → only the LATEST
        //     heartbeat's cwd is kept (per-channel bound).
        seed(
            &pool,
            "k-chan5-old",
            Some("1005"),
            "/wt/chan5-old",
            Some("sid-5-old"),
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
        )
        .await;

        let kept = fetch_resumable_cwds(&pool).await.expect("query keep-set");

        assert!(
            kept.contains("/wt/fresh"),
            "fresh resumable cwd must be kept"
        );
        assert!(
            !kept.contains("/wt/null-hb"),
            "NULL-heartbeat session must NOT pin its worktree forever"
        );
        assert!(
            !kept.contains("/wt/stale"),
            "stale-heartbeat session must be collectable"
        );
        assert!(
            !kept.contains("/wt/no-sid"),
            "a session without a provider id has nothing to resume into"
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
