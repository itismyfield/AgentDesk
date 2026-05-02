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

    let active_cwds = fetch_active_cwds(&pool).await.unwrap_or_default();
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;

    #[test]
    fn is_dir_active_matches_exact_path() {
        let mut set = HashSet::new();
        set.insert("/tmp/worktrees/foo".to_string());
        assert!(is_dir_active(Path::new("/tmp/worktrees/foo"), &set));
        assert!(!is_dir_active(Path::new("/tmp/worktrees/bar"), &set));
    }

    #[test]
    fn is_dir_active_matches_nested_cwd() {
        let mut set = HashSet::new();
        set.insert("/tmp/worktrees/foo/src/lib".to_string());
        assert!(is_dir_active(Path::new("/tmp/worktrees/foo"), &set));
    }

    #[test]
    fn is_dir_active_does_not_match_partial_prefix() {
        let mut set = HashSet::new();
        set.insert("/tmp/worktrees/foobar/x".to_string());
        // `/tmp/worktrees/foo` should NOT match `/tmp/worktrees/foobar/x`
        assert!(!is_dir_active(Path::new("/tmp/worktrees/foo"), &set));
    }

    #[tokio::test]
    async fn empty_root_returns_empty_report() {
        let config = Config {
            worktrees_root: PathBuf::from("/nonexistent/worktrees/root/xyz"),
            dry_run: true,
        };
        let report = run_inner(&config, None).await.unwrap();
        assert_eq!(report.scanned_dirs, 0);
        assert_eq!(report.orphan_count, 0);
    }

    #[tokio::test]
    async fn dry_run_identifies_orphan_without_deleting() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();

        let active = root.join("active-worktree");
        let orphan = root.join("orphan-worktree");
        std::fs::create_dir_all(&active).unwrap();
        std::fs::create_dir_all(&orphan).unwrap();

        // Simulate PG result: only `active` dir has a session cwd tied to a
        // pending/dispatched dispatch.
        let mut active_cwds = HashSet::new();
        active_cwds.insert(active.to_string_lossy().to_string());

        // Exercise the classification helper directly (avoids spinning up PG).
        assert!(is_dir_active(&active, &active_cwds));
        assert!(!is_dir_active(&orphan, &active_cwds));

        // And exercise the full `run_inner` path with no PG pool — it should
        // return `pg_available = false` and scan nothing destructively.
        let config = Config {
            worktrees_root: root,
            dry_run: true,
        };
        let report = run_inner(&config, None).await.unwrap();
        assert!(!report.pg_available);
        assert_eq!(report.removed_dirs, 0);
        assert!(orphan.exists(), "orphan must not be removed without PG");
    }
}
