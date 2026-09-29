//! `storage.worktree_orphan_sweep`: hourly cleanup of orphaned git worktrees under
//! `~/.adk/release/worktrees/`.
//!
//! - Flat root: a per-channel worktree is removed only when no kept session cwd,
//!   active dispatch or PR worktree path, or live `AgentDesk-*` tmux pane sits at or
//!   under it, and its name is runtime-created ([`is_runtime_named_worktree`]).
//! - Managed root (`worktrees/<repo>/`): unowned dispatch/automation worktrees older
//!   than [`MANAGED_FRESH_PROVISION_MIN_AGE`] go through
//!   [`crate::services::git::cleanup_managed_worktree`], which keeps dirty or unmerged trees.
//! - `release-*` names are never swept ([`PROTECTED_INFRA_NAME_PREFIXES`]).
//! - Fail-closed: with no Postgres pool, or when any keep-set or tmux query fails, the
//!   run deletes nothing, because it cannot prove a worktree is unowned.

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
    /// Worktrees scanned by the managed-root pass; its removals go to `managed_removed`.
    pub managed_scanned: u64,
    pub managed_removed: u64,
    /// Unowned flat-root dirs kept because their name is not runtime-created.
    pub protected_unmatched: u64,
    /// Unowned managed worktrees kept because they are younger than
    /// [`MANAGED_FRESH_PROVISION_MIN_AGE`].
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

async fn run_blocking_filesystem<T, F>(operation: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        #[cfg(test)]
        observe_blocking_call_site();
        operation()
    })
    .await
    .map_err(|error| anyhow::anyhow!("worktree filesystem task failed: {error}"))
}

fn collect_child_directories(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            entry
                .metadata()
                .ok()
                .filter(|metadata| metadata.is_dir())
                .map(|_| entry.path())
        })
        .collect()
}

#[cfg(test)]
static BLOCKING_CALL_SITE_OBSERVER: std::sync::Mutex<
    Option<std::sync::mpsc::Sender<std::thread::ThreadId>>,
> = std::sync::Mutex::new(None);

#[cfg(test)]
static BLOCKING_CALL_SITE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
fn observe_blocking_call_site() {
    if let Some(observer) = BLOCKING_CALL_SITE_OBSERVER
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
    {
        let _ = observer.send(std::thread::current().id());
    }
}

async fn collect_child_directories_off_runtime(root: PathBuf) -> Result<Vec<PathBuf>> {
    run_blocking_filesystem(move || collect_child_directories(&root)).await
}

async fn is_managed_root_child_off_runtime(dir: PathBuf) -> Result<bool> {
    run_blocking_filesystem(move || is_managed_root_child(&dir)).await
}

pub async fn run_inner(config: &Config, pg_pool: Option<PgPool>) -> Result<SweepReport> {
    let mut report = SweepReport::default();

    let worktrees_root = config.worktrees_root.clone();
    if !run_blocking_filesystem(move || worktrees_root.exists()).await? {
        return Ok(report);
    }

    let Some(pool) = pg_pool else {
        // Without the DB keep-set no worktree can be proven unowned, so delete nothing.
        return Ok(report);
    };
    report.pg_available = true;

    // A failed keep-set query must skip all deletions: an empty fallback set would make
    // every live worktree look unowned.
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

    // A live AgentDesk pane owns its worktree even when the DB keep-set disagrees.
    // A failed tmux query cannot prove a worktree unowned, so skip all deletions.
    let Some(live_tmux_paths) = run_blocking_filesystem(collect_live_tmux_pane_paths).await? else {
        tracing::warn!(
            target: "maintenance",
            job = "storage.worktree_orphan_sweep",
            "tmux query failed; cannot prove no live worktree owner — skipping all deletions this run (fail-closed)"
        );
        return Ok(report);
    };

    let directories = collect_child_directories_off_runtime(config.worktrees_root.clone()).await?;

    for dir_path in directories {
        report.scanned_dirs = report.scanned_dirs.saturating_add(1);

        // A managed-root container is never a flat-root candidate; its children are the
        // managed worktrees the flat scan would miss.
        if is_managed_root_child_off_runtime(dir_path.clone()).await? {
            sweep_managed_root(
                &dir_path,
                &active_cwds,
                &live_tmux_paths,
                config,
                &mut report,
            )
            .await?;
            continue;
        }

        if !should_sweep_worktree(&dir_path, &active_cwds, Some(&live_tmux_paths)) {
            continue;
        }

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

/// Sweeps unowned worktrees one level inside a managed-root child (`worktrees/<repo>/`).
///
/// Removal uses [`crate::services::git::cleanup_managed_worktree`] (keeps dirty and
/// unmerged trees) or the [`GitPointerState`] fallback, never the flat-root `--force` path.
async fn sweep_managed_root(
    repo_root_dir: &Path,
    active_cwds: &HashSet<String>,
    live_tmux_paths: &HashSet<String>,
    config: &Config,
    report: &mut SweepReport,
) -> Result<()> {
    let children = collect_child_directories_off_runtime(repo_root_dir.to_path_buf()).await?;
    for wt_path in children {
        report.managed_scanned = report.managed_scanned.saturating_add(1);
        if !should_sweep_worktree(&wt_path, active_cwds, Some(live_tmux_paths)) {
            continue;
        }

        // A new worktree may look unowned only because its dispatch row has not landed.
        let age_path = wt_path.clone();
        if run_blocking_filesystem(move || {
            is_freshly_provisioned(&age_path, MANAGED_FRESH_PROVISION_MIN_AGE)
        })
        .await?
        {
            report.protected_fresh = report.protected_fresh.saturating_add(1);
            continue;
        }

        report.orphan_count = report.orphan_count.saturating_add(1);

        if config.dry_run {
            continue;
        }

        let cleanup_path = wt_path.clone();
        let cleanup = cleanup_managed_candidate_off_runtime(cleanup_path).await;

        record_cleanup_outcome(report, &wt_path, cleanup);
    }
    Ok(())
}

type CleanupOutcome = (usize, usize);

fn record_cleanup_outcome(
    report: &mut SweepReport,
    path: &Path,
    cleanup: Result<Option<CleanupOutcome>>,
) {
    match cleanup {
        Ok(Some((_, failed))) if failed > 0 => {
            tracing::warn!(target: "maintenance", path = %path.display(), failed,
                "worktree_orphan_sweep: managed cleanup failed");
            report.errors = report.errors.saturating_add(failed as u64);
        }
        Ok(Some((removed, _))) if removed > 0 => {
            report.managed_removed = report.managed_removed.saturating_add(removed as u64);
        }
        Ok(Some(_)) => {}
        Ok(None) => tracing::warn!(target: "maintenance", path = %path.display(),
            "worktree_orphan_sweep: managed worktree has an unreadable/unresolvable .git pointer — skipping (fail-closed; never force-removed)"),
        Err(error) => {
            tracing::warn!(target: "maintenance", path = %path.display(), error = %error,
                "worktree_orphan_sweep: managed cleanup failed");
            report.errors = report.errors.saturating_add(1);
        }
    }
}

async fn cleanup_managed_candidate_off_runtime(path: PathBuf) -> Result<Option<CleanupOutcome>> {
    run_blocking_filesystem(move || cleanup_managed_candidate(&path)).await?
}

fn cleanup_managed_candidate(path: &Path) -> Result<Option<CleanupOutcome>> {
    let Some(repo_root) = infer_repo_root_from_worktree(path) else {
        return match git_pointer_state(path) {
            GitPointerState::Missing if is_old_enough(path, MANAGED_CANCEL_LEAK_BACKSTOP) => {
                remove_dir_all_plain(path)
                    .map(|()| Some((1, 0)))
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "age-backstop plain removal of managed worktree {} failed: {error}",
                            path.display()
                        )
                    })
            }
            GitPointerState::Missing => Ok(Some((0, 0))),
            GitPointerState::PresentUnreadable => Ok(None),
        };
    };
    let result = crate::services::git::cleanup_managed_worktree(
        &repo_root.to_string_lossy(),
        &path.to_string_lossy(),
    );
    Ok(Some((result.removed, result.failed)))
}

/// `sessions.cwd` of sessions bound to an active (`pending`/`dispatched`) dispatch.
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

/// Cwds of resumable sessions, so the next turn's `--resume` still finds its worktree.
///
/// Keyed on a non-null provider session GUID (clearing a session nulls it); the 30-day
/// `COALESCE(last_heartbeat, created_at)` window only bounds disk use. Only the latest
/// row per channel is kept, so each channel pins at most one worktree.
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

/// Dispatch JSON keys naming an owned worktree; mirrors `WORKTREE_PATH_REFERENCE_KEYS`
/// in `crate::kanban::terminal_cleanup`.
const DISPATCH_WORKTREE_PATH_KEYS: &[&str] = &["worktree_path", "completed_worktree_path"];

/// Worktree paths claimed by active (`pending`/`dispatched`) dispatches or by
/// `pr_tracking`: the refs `terminal_cleanup::active_worktree_refs_pg` also honors.
///
/// A dispatch records its worktree at create time, before any session cwd or tmux pane
/// exists, so without this set a new managed worktree would look unowned.
async fn fetch_active_dispatch_worktree_paths(pool: &PgPool) -> Result<HashSet<String>> {
    let mut paths = HashSet::new();

    // `::TEXT` decodes the same whether the column is TEXT or JSON/JSONB.
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

/// True when `candidate` is `dir` or lies under it at a `/` boundary, so a cwd or pane
/// in a subdirectory still protects the worktree root.
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

/// Minimum mtime age before a managed dir with no `.git` (a cancel leak that terminal
/// cleanup never removed) may be plain-deleted.
const MANAGED_CANCEL_LEAK_BACKSTOP: std::time::Duration =
    std::time::Duration::from_secs(60 * 60 * 24); // 24h

/// Creation-age floor before a managed worktree may be deleted. Dispatch creation
/// provisions the worktree before committing its `task_dispatches` row, so a keep-set
/// snapshot taken in between sees it unowned. Creation age, not idle time, because a
/// new worktree can go idle right after a build, before its row lands.
const MANAGED_FRESH_PROVISION_MIN_AGE: std::time::Duration =
    std::time::Duration::from_secs(60 * 30); // 30m

/// Name prefixes never swept by either pass, whatever the owner signals say. Deploy
/// worktrees (`release-main-deploy-*`) have no dispatch, session or tmux owner, so an
/// owner check would always pick them; the runtime never creates `release-*` names,
/// so this cannot hide a real orphan.
const PROTECTED_INFRA_NAME_PREFIXES: &[&str] = &["release-"];

/// True when `dir`'s final segment starts with a protected prefix, case-insensitive.
/// Only the name is checked because the scan root itself lives under `~/.adk/release/`.
pub(crate) fn is_protected_infra_worktree(dir: &Path) -> bool {
    let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    PROTECTED_INFRA_NAME_PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix))
}

/// True when the dir name is one the runtime creates for per-channel worktrees:
/// `claude-`, `codex-adk-cdx`, `wt-` or `wt/` prefixes, case-insensitive. Anything else
/// (`worker-*`, `integration-*`, plain `codex-*`, `fix-*`, …) is a manual dev worktree
/// and is never a discard candidate.
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

/// True when `dir` is a managed-root container (`worktrees/<repo>/`): no `.git` of its
/// own, but at least one child directory that has one.
pub(crate) fn is_managed_root_child(dir: &Path) -> bool {
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

/// True when `dir`'s mtime is at least `min_age` old; an unreadable mtime is `false`,
/// so an unknown age never licenses a delete.
fn is_old_enough(dir: &Path, min_age: std::time::Duration) -> bool {
    let Ok(modified) = dir.metadata().and_then(|m| m.modified()) else {
        return false;
    };
    modified
        .elapsed()
        .map(|elapsed| elapsed >= min_age)
        .unwrap_or(false)
}

/// True when `dir` is younger than `min_age` by birth time (mtime where unsupported).
/// An unreadable or future timestamp counts as fresh: doubt means keep.
fn is_freshly_provisioned(dir: &Path, min_age: std::time::Duration) -> bool {
    let Ok(metadata) = dir.metadata() else {
        return true;
    };
    let created = metadata.created().or_else(|_| metadata.modified());
    let Ok(created) = created else {
        return true;
    };
    match created.elapsed() {
        Ok(elapsed) => elapsed < min_age,
        // `elapsed()` fails for a future timestamp (clock skew).
        Err(_) => true,
    }
}

/// True when any kept cwd is `dir` or nested under it (subshells may sit in `src/`).
pub(crate) fn is_dir_active(dir: &Path, active_cwds: &HashSet<String>) -> bool {
    let dir_str = dir.to_string_lossy();
    active_cwds
        .iter()
        .any(|cwd| path_equals_or_nested_under(cwd, dir_str.as_ref()))
}

/// Owner check shared by both passes: true only when `dir` has no protected name, the
/// tmux query succeeded, and no kept cwd or live pane sits at or under it.
pub(crate) fn should_sweep_worktree(
    dir: &Path,
    kept_cwds: &HashSet<String>,
    live_tmux_paths: Option<&HashSet<String>>,
) -> bool {
    if is_protected_infra_worktree(dir) {
        tracing::info!(
            target: "maintenance",
            job = "storage.worktree_orphan_sweep",
            path = %dir.display(),
            "protected infrastructure worktree (name prefix 'release-') — kept regardless of owner (#3276)"
        );
        return false;
    }
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

/// True when a live pane path is `dir` or nested under it. Compares both the raw and
/// the canonical `dir`, so a symlinked scan path still matches tmux's reported path.
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

/// `#{pane_current_path}` of every AgentDesk tmux session, raw and canonicalized.
///
/// `None` means the query failed, unlike `Some(empty)` (no panes): a failed query
/// cannot prove a worktree unowned, so the caller must skip all deletions.
pub(crate) fn collect_live_tmux_pane_paths() -> Option<HashSet<String>> {
    let sessions = crate::services::platform::tmux::list_session_names().ok()?;
    fold_pane_paths(sessions, |session| {
        crate::services::platform::tmux::pane_current_path(session)
    })
}

/// Core of [`collect_live_tmux_pane_paths`] with the pane query injected for tests.
/// `None` if any AgentDesk session's pane path is unreadable or empty, since a partial
/// set could miss a live owner. Other sessions are never queried.
fn fold_pane_paths(
    sessions: Vec<String>,
    query: impl Fn(&str) -> Option<String>,
) -> Option<HashSet<String>> {
    let mut paths = HashSet::new();
    for session in sessions {
        if !session.starts_with("AgentDesk-") {
            continue;
        }
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

#[allow(clippy::result_large_err)]
pub(crate) async fn remove_orphan_worktree(path: &Path) -> Result<()> {
    let path = path.to_path_buf();
    run_blocking_filesystem(move || remove_orphan_worktree_blocking(&path)).await?
}

fn remove_orphan_worktree_blocking(path: &Path) -> Result<()> {
    // Best effort: if `git worktree remove` fails, `remove_dir_all` below still runs.
    if let Some(repo_root) = infer_repo_root_from_worktree(path) {
        let _ = GitCommand::new()
            .repo(&repo_root)
            .args(["worktree", "remove", "--force"])
            .arg(path)
            .run_output();
    }

    if path.exists() {
        std::fs::remove_dir_all(path)?;
    }
    Ok(())
}

/// `.git` state of a managed worktree whose parent repo could not be resolved. Only
/// [`GitPointerState::Missing`] may be plain-deleted, after [`MANAGED_CANCEL_LEAK_BACKSTOP`];
/// an unreadable pointer may belong to a registered, possibly dirty worktree.
enum GitPointerState {
    /// No `.git` entry: a leftover dir, not a registered worktree.
    Missing,
    /// `.git` exists, or its existence is unknown; never deleted here.
    PresentUnreadable,
}

/// Classifies `path/.git` by existence only; an I/O error counts as present.
fn git_pointer_state(path: &Path) -> GitPointerState {
    match path.join(".git").try_exists() {
        Ok(false) => GitPointerState::Missing,
        Ok(true) | Err(_) => GitPointerState::PresentUnreadable,
    }
}

/// Deletes a `.git`-less managed leftover; there is no git state to preserve.
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

    #[test]
    fn nested_resumable_cwd_protects_worktree_root() {
        let dir = "/home/u/.adk/release/worktrees/claude-chan-20260101-000000";
        let nested = format!("{dir}/src/services");
        let mut keep: HashSet<String> = HashSet::new();
        keep.insert(nested);
        assert!(is_dir_active(Path::new(dir), &keep));
    }

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
    //! Owner-check cases, including a phantom worktree left behind by a rotation.
    use super::{fold_pane_paths, has_live_tmux_owner, should_sweep_worktree};
    use std::collections::HashSet;
    use std::path::Path;

    const ORIGINAL: &str = "/home/u/.adk/release/worktrees/claude-adk-cc-20260607-113822";
    const PHANTOM: &str = "/home/u/.adk/release/worktrees/claude-adk-cc-20260607-212437";

    #[test]
    fn divorced_phantom_with_no_owner_is_swept() {
        let mut kept: HashSet<String> = HashSet::new();
        kept.insert(ORIGINAL.to_string());
        let mut live: HashSet<String> = HashSet::new();
        live.insert(ORIGINAL.to_string());

        assert!(
            should_sweep_worktree(Path::new(PHANTOM), &kept, Some(&live)),
            "a phantom worktree that is neither a kept cwd nor a live tmux pane must be swept"
        );
    }

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

    #[test]
    fn nothing_is_swept_when_tmux_unavailable() {
        let kept: HashSet<String> = HashSet::new();
        assert!(
            !should_sweep_worktree(Path::new(PHANTOM), &kept, None),
            "tmux-unavailable (failed query) must suppress ALL deletions, even of phantoms"
        );
    }

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

    #[test]
    fn has_live_tmux_owner_basic() {
        let empty: HashSet<String> = HashSet::new();
        assert!(!has_live_tmux_owner(Path::new(ORIGINAL), &empty));

        let mut live: HashSet<String> = HashSet::new();
        live.insert(ORIGINAL.to_string());
        assert!(has_live_tmux_owner(Path::new(ORIGINAL), &live));
        assert!(!has_live_tmux_owner(Path::new(PHANTOM), &live));
    }

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

    #[test]
    fn fold_pane_paths_fails_closed_on_empty_pane_path() {
        let sessions = vec!["AgentDesk-claude-adk-cc".to_string()];
        let result = fold_pane_paths(sessions, |_| Some(String::new()));
        assert!(result.is_none(), "empty pane path must fail-closed");
    }
}

#[cfg(test)]
mod deploy_worktree_protection_tests {
    use super::{is_protected_infra_worktree, should_sweep_worktree};
    use std::collections::HashSet;
    use std::path::Path;

    const DEPLOY: &str = "/home/u/.adk/release/worktrees/release-main-deploy-20260530";
    const PHANTOM: &str = "/home/u/.adk/release/worktrees/claude-adk-cc-20260607-212437";

    #[test]
    fn deploy_worktree_is_never_swept_when_all_keep_conditions_miss() {
        let kept: HashSet<String> = HashSet::new();
        let live: HashSet<String> = HashSet::new(); // tmux up, zero AgentDesk panes
        assert!(
            !should_sweep_worktree(Path::new(DEPLOY), &kept, Some(&live)),
            "the release deploy worktree must survive even with no owner in any keep-set"
        );
    }

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

    #[test]
    fn non_protected_names_keep_existing_sweep_behavior() {
        let kept: HashSet<String> = HashSet::new();
        let live: HashSet<String> = HashSet::new();
        assert!(
            should_sweep_worktree(Path::new(PHANTOM), &kept, Some(&live)),
            "an unowned runtime-named worktree must remain a sweep candidate"
        );
    }

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
mod resumable_keep_set_query_pg_tests {
    //! Runs the real `fetch_resumable_cwds` query against Postgres.
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
        // (2) GUID but never heartbeated → kept.
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
        // (3) GUID row older than the 30-day backstop → excluded.
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
        // (4) cleared GUID → excluded, even with a fresh heartbeat.
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
        // (5) two sessions on one channel → only the latest cwd is kept.
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
mod active_dispatch_worktree_keep_set_pg_tests {
    //! Runs the real `fetch_active_dispatch_worktree_paths` queries against Postgres.
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
        // (3) terminal dispatch → not in the active set (terminal cleanup owns it).
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
mod blocking_directory_walk_tests {
    use super::{
        BLOCKING_CALL_SITE_OBSERVER, BLOCKING_CALL_SITE_TEST_LOCK, SweepReport,
        cleanup_managed_candidate_off_runtime, collect_child_directories_off_runtime,
        is_managed_root_child_off_runtime, record_cleanup_outcome, remove_orphan_worktree,
    };
    use std::sync::mpsc;
    use std::time::Duration;

    fn install_observer() -> mpsc::Receiver<std::thread::ThreadId> {
        let (sender, receiver) = mpsc::channel();
        *BLOCKING_CALL_SITE_OBSERVER
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(sender);
        receiver
    }

    fn observed_thread(receiver: mpsc::Receiver<std::thread::ThreadId>) -> std::thread::ThreadId {
        let observed = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("call-site observer must run");
        *BLOCKING_CALL_SITE_OBSERVER
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        observed
    }

    #[tokio::test(flavor = "current_thread")]
    async fn root_enumerator_call_site_runs_off_the_runtime_thread() {
        let _test_guard = BLOCKING_CALL_SITE_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let runtime_thread = std::thread::current().id();
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("child")).unwrap();
        let observer = install_observer();

        let directories = collect_child_directories_off_runtime(temp.path().to_path_buf())
            .await
            .unwrap();

        assert_eq!(directories, vec![temp.path().join("child")]);
        assert_ne!(observed_thread(observer), runtime_thread);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn managed_root_classifier_call_site_runs_off_the_runtime_thread() {
        let _test_guard = BLOCKING_CALL_SITE_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let runtime_thread = std::thread::current().id();
        let temp = tempfile::tempdir().unwrap();
        let managed_root = temp.path().join("repo");
        let child = managed_root.join("worktree");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(child.join(".git"), b"gitdir: /repo/.git/worktrees/wt").unwrap();
        let observer = install_observer();

        assert!(
            is_managed_root_child_off_runtime(managed_root)
                .await
                .unwrap()
        );
        assert_ne!(observed_thread(observer), runtime_thread);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn managed_cleanup_call_site_runs_off_the_runtime_thread() {
        let _test_guard = BLOCKING_CALL_SITE_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let runtime_thread = std::thread::current().id();
        let temp = tempfile::tempdir().unwrap();
        let candidate = temp.path().join("candidate");
        std::fs::create_dir(&candidate).unwrap();
        let observer = install_observer();

        assert_eq!(
            cleanup_managed_candidate_off_runtime(candidate)
                .await
                .unwrap(),
            Some((0, 0))
        );
        assert_ne!(observed_thread(observer), runtime_thread);
    }

    #[test]
    fn managed_cleanup_failure_increments_error_count() {
        let mut report = SweepReport::default();
        record_cleanup_outcome(
            &mut report,
            std::path::Path::new("/managed/worktree"),
            Ok(Some((0, 1))),
        );

        assert_eq!(report.managed_removed, 0);
        assert_eq!(
            report.errors, 1,
            "managed cleanup failures must be reported"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn orphan_removal_call_site_runs_off_the_runtime_thread() {
        let _test_guard = BLOCKING_CALL_SITE_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let runtime_thread = std::thread::current().id();
        let temp = tempfile::tempdir().unwrap();
        let observer = install_observer();

        remove_orphan_worktree(&temp.path().join("missing"))
            .await
            .unwrap();
        assert_ne!(observed_thread(observer), runtime_thread);
    }
}

#[cfg(test)]
mod managed_root_recursion_tests {
    //! Managed-root recursion against a real repo and managed worktree on disk.
    use super::{Config, is_managed_root_child, is_runtime_named_worktree, run_inner};
    use crate::services::git::GitCommand;
    use std::collections::HashSet;
    use std::path::Path;
    // Git-spawning tests hold the env lock across their git calls, not just env writes:
    // other tests mutate `PATH` (bare `git` lookup) and `AGENTDESK_ROOT_DIR`.

    fn env_lock() -> crate::config::test_env_lock::SharedTestEnvLockGuard {
        crate::config::test_env_lock::acquire_shared_test_env_lock()
    }

    /// Runs git via `GitCommand` (an audit gate bans raw `Command::new("git")` here).
    /// A spawn failure panics separately: it means git never ran, not that it failed.
    fn git(repo: &Path, args: &[&str]) {
        let output = GitCommand::new()
            .repo(repo)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .run_output()
            .unwrap_or_else(|error| panic!("git {args:?} could not be spawned: {error}"));
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    /// Builds a repo whose `origin/main` equals `main`, plus a managed worktree at that
    /// commit. Returns `(worktrees_root, managed_root, managed_worktree_path)`.
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

        // Mirrors `managed_worktrees_root`: `worktrees/<repo_name>/`.
        let worktrees_root = base.join("worktrees");
        let managed_root = worktrees_root.join("agentdesk");
        std::fs::create_dir_all(&managed_root).unwrap();
        let wt = managed_root.join("issue-3231-20260607");
        // Detached because `main` is already checked out in the primary worktree.
        git(
            &repo,
            &["worktree", "add", "--detach", wt.to_str().unwrap(), "main"],
        );
        (worktrees_root, managed_root, wt)
    }

    #[test]
    fn managed_root_child_is_classified_and_worktree_is_not() {
        let tmp = tempfile::tempdir().unwrap();
        // Only setup spawns git; the classifier touches no process-global state.
        let (_root, managed_root, wt) = {
            let _lock = env_lock();
            setup_repo_with_managed_worktree(tmp.path())
        };
        assert!(
            is_managed_root_child(&managed_root),
            "worktrees/<repo>/ must be recognized as a managed-root container"
        );
        assert!(
            !is_managed_root_child(&wt),
            "a registered git worktree must not be treated as a managed root"
        );
    }

    /// Runs `body` with the env lock held and `AGENTDESK_ROOT_DIR` at a temp root that
    /// holds the repo, so `is_managed_worktree_path` accepts the worktree.
    fn with_managed_root_env<R>(body: impl FnOnce(&Path, &Path, &Path, &Path) -> R) -> R {
        let _guard = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let _root_env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            tmp.path(),
        );
        let (worktrees_root, managed_root, wt) = setup_repo_with_managed_worktree(tmp.path());
        let repo = tmp.path().join("agentdesk");
        body(&repo, &worktrees_root, &managed_root, &wt)
    }

    use crate::test_env_panic_probe::{assert_root_restored, checkpoint};

    fn exercise_managed_worktree_root() {
        with_managed_root_env(|repo, _, _, _| {
            checkpoint(&[("AGENTDESK_ROOT_DIR", repo.parent().unwrap().as_os_str())])
        })
    }

    #[test]
    fn managed_worktree_root_restores_env_after_panic_present() {
        assert_root_restored(true, exercise_managed_worktree_root);
    }

    #[test]
    fn managed_worktree_root_restores_env_after_panic_absent() {
        assert_root_restored(false, exercise_managed_worktree_root);
    }

    #[test]
    fn terminal_managed_worktree_is_swept_via_recursion() {
        with_managed_root_env(|repo, worktrees_root, _managed_root, wt| {
            assert!(wt.exists());
            // Tests the removal step the recursion delegates to, not the owner check.
            let cleanup = crate::services::git::cleanup_managed_worktree(
                repo.to_str().unwrap(),
                wt.to_str().unwrap(),
            );
            // Every guard fails closed, so print the skip counters: an impossible skip
            // (dirty or unmerged here) means git failed to spawn.
            assert_eq!(
                cleanup.removed,
                1,
                "clean+merged managed worktree is removed \
                 (skipped_dirty={} skipped_unmerged={} skipped_unmanaged={} failed={} \
                 AGENTDESK_ROOT_DIR={:?} wt={})",
                cleanup.skipped_dirty,
                cleanup.skipped_unmerged,
                cleanup.skipped_unmanaged,
                cleanup.failed,
                std::env::var("AGENTDESK_ROOT_DIR").ok(),
                wt.display(),
            );
            assert!(!wt.exists(), "managed worktree dir is gone after cleanup");
            assert!(worktrees_root.exists());
        });
    }

    #[test]
    fn dirty_managed_worktree_is_preserved() {
        with_managed_root_env(|repo, _worktrees_root, _managed_root, wt| {
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
        // Drop the lock before `.await` (`await_holding_lock`); the sweep spawns no git.
        let (worktrees_root, _managed_root, wt) = {
            let _lock = env_lock();
            setup_repo_with_managed_worktree(tmp.path())
        };
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
        let _keep: HashSet<String> = HashSet::new();
        let manual = Path::new("/home/u/.adk/release/worktrees/worker-1");
        assert!(!is_runtime_named_worktree(manual));
    }
}

#[cfg(test)]
mod git_pointer_fallback_tests {
    use super::{GitPointerState, git_pointer_state, remove_dir_all_plain};

    #[test]
    fn missing_git_is_classified_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("leftover");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(matches!(git_pointer_state(&dir), GitPointerState::Missing));
    }

    #[test]
    fn present_git_file_is_classified_present_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("registered");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".git"), b"garbage-not-a-gitdir-pointer").unwrap();
        assert!(matches!(
            git_pointer_state(&dir),
            GitPointerState::PresentUnreadable
        ));
        assert!(dir.exists());
    }

    #[test]
    fn present_git_dir_is_classified_present_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("with-git-dir");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
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
        remove_dir_all_plain(&dir).expect("plain remove of missing dir is a no-op");
    }
}

#[cfg(test)]
mod keep_set_query_failure_fail_closed_pg_tests {
    //! A closed pool makes every keep-set query fail; the sweep must delete nothing.
    use super::{Config, run_inner};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closed_pool_keep_set_query_error_skips_all_deletions() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
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
    //! The creation-age floor that protects just-provisioned managed worktrees.
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
        // Stands in for an old worktree: any existing dir clears a zero floor.
        assert!(
            !is_freshly_provisioned(&dir, std::time::Duration::ZERO),
            "a zero min-age floor must never protect an existing dir"
        );
    }

    #[test]
    fn unstatable_path_is_protected() {
        let missing = std::path::Path::new("/nonexistent/worktree/path/xyz");
        assert!(
            is_freshly_provisioned(missing, MANAGED_FRESH_PROVISION_MIN_AGE),
            "an unstatable path must be treated as freshly provisioned (KEEP)"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fresh_managed_worktree_with_no_owner_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let managed_root = tmp.path().join("worktrees").join("agentdesk");
        let wt = managed_root.join("issue-9999-fresh");
        std::fs::create_dir_all(&wt).unwrap();

        // No owner and zero panes: only the age floor keeps this worktree.
        let kept: HashSet<String> = HashSet::new();
        let live: HashSet<String> = HashSet::new();
        let config = Config {
            worktrees_root: tmp.path().join("worktrees"),
            dry_run: false,
        };
        let mut report = SweepReport::default();

        sweep_managed_root(&managed_root, &kept, &live, &config, &mut report)
            .await
            .unwrap();

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
