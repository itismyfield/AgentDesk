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
//! Degrades gracefully when Postgres is not wired up: returns `Ok(())` with a
//! `pg_unavailable = true` log line rather than risking false-positive deletes.

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
        if is_dir_active(&dir_path, &active_cwds) {
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

/// A worktree dir is "active" if ANY session cwd equals it or is nested under
/// it (subshell cwds sometimes land inside `src/...` relative to the worktree
/// root).
pub(crate) fn is_dir_active(dir: &Path, active_cwds: &HashSet<String>) -> bool {
    let dir_str = dir.to_string_lossy();
    for cwd in active_cwds {
        if cwd == dir_str.as_ref() {
            return true;
        }
        if cwd.starts_with(dir_str.as_ref())
            && cwd
                .as_bytes()
                .get(dir_str.len())
                .map(|b| *b == b'/')
                .unwrap_or(false)
        {
            return true;
        }
    }
    false
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
