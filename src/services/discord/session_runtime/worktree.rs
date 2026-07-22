use super::super::runtime_store::worktrees_root;
use super::restore_cwd::restore_thread_worktree_path_from_db;
use super::*;
use crate::services::git::GitCommand;

/// Worktree info for sessions that were auto-redirected to avoid conflicts
#[derive(Clone, Debug)]
pub(in crate::services::discord) struct WorktreeInfo {
    /// The original repo path that was conflicted
    pub(in crate::services::discord) original_path: String,
    /// The worktree directory path
    pub(in crate::services::discord) worktree_path: String,
    /// The branch name created for this worktree
    pub(in crate::services::discord) branch_name: String,
}

/// Check if a path is a git repo and if another channel already uses it.
/// Returns the conflicting channel's name if found.
pub(in crate::services::discord) fn detect_worktree_conflict(
    sessions: &HashMap<ChannelId, DiscordSession>,
    path: &str,
    my_channel: ChannelId,
) -> Option<String> {
    let norm = path.trim_end_matches('/');
    for (cid, session) in sessions {
        if *cid == my_channel {
            continue;
        }
        let other_path = if let Some(ref wt) = session.worktree {
            &wt.original_path
        } else {
            match &session.current_path {
                Some(p) => p.as_str(),
                None => continue,
            }
        };
        if other_path.trim_end_matches('/') == norm {
            return session
                .channel_name
                .clone()
                .or_else(|| Some(cid.get().to_string()));
        }
    }
    None
}

/// Create a git worktree for the given repo path.
/// Returns (worktree_path, branch_name) on success.
pub(in crate::services::discord) fn create_git_worktree(
    repo_path: &str,
    channel_name: &str,
    provider: &str,
) -> Result<(String, String), String> {
    if GitCommand::new()
        .repo(repo_path)
        .args(["rev-parse", "--is-inside-work-tree"])
        .run_output()
        .is_err()
    {
        return Err(format!("{} is not a git repository", repo_path));
    }

    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let safe_name = channel_name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    let branch = format!("wt/{}-{}-{}", provider, safe_name, ts);

    let wt_base = worktrees_root().ok_or("Cannot determine worktree root")?;
    std::fs::create_dir_all(&wt_base)
        .map_err(|e| format!("Failed to create worktree base dir: {}", e))?;
    let wt_dir = wt_base.join(format!("{}-{}-{}", provider, safe_name, ts));
    let wt_path = wt_dir.display().to_string();
    let base_ref = git_upstream_base_ref(repo_path);

    GitCommand::new()
        .repo(repo_path)
        .args(["worktree", "add", "-b"])
        .arg(&branch)
        .arg(&wt_path)
        .arg(&base_ref)
        .run_output()
        .map_err(|e| format!("git worktree add failed: {}", e))?;

    let ts_log = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts_log}] 🌿 Created worktree: {} (branch: {})",
        wt_path,
        branch
    );
    Ok((wt_path, branch))
}

fn git_upstream_base_ref(repo_path: &str) -> String {
    if GitCommand::new()
        .repo(repo_path)
        .args(["rev-parse", "--verify", "origin/main"])
        .run_output()
        .is_ok()
    {
        return "origin/main".to_string();
    }

    // origin/main not available locally — attempt a shallow fetch before falling back
    if GitCommand::new()
        .repo(repo_path)
        .args(["fetch", "origin", "main", "--depth=1"])
        .run_output()
        .is_ok()
    {
        // Re-verify after fetch
        if GitCommand::new()
            .repo(repo_path)
            .args(["rev-parse", "--verify", "origin/main"])
            .run_output()
            .is_ok()
        {
            tracing::info!(
                "git fetch origin main --depth=1 succeeded for repo {repo_path}; using origin/main as base ref"
            );
            return "origin/main".to_string();
        }
    }

    tracing::warn!(
        "origin/main unavailable for repo {repo_path} even after fetch attempt; falling back to local 'main'"
    );
    "main".to_string()
}

fn worktree_has_local_changes(wt_info: &WorktreeInfo) -> Result<bool, String> {
    let status = GitCommand::new()
        .repo(&wt_info.worktree_path)
        .args(["status", "--porcelain"])
        .run_output()
        .map_err(|e| format!("git status failed: {e}"))?;
    Ok(!status.stdout.is_empty())
}

fn git_command_output(repo_path: &str, args: &[&str]) -> Result<std::process::Output, String> {
    GitCommand::new()
        .repo(repo_path)
        .args(args.iter().copied())
        .run_output()
        .map_err(|e| format!("git {:?} failed: {e}", args))
}

fn git_command_stdout(repo_path: &str, args: &[&str]) -> Result<String, String> {
    let output = git_command_output(repo_path, args)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!("git {:?} failed: {stderr}", args));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn patch_id_from_diff(diff: &[u8]) -> Result<Option<String>, String> {
    if diff.is_empty() {
        return Ok(None);
    }

    let output = GitCommand::new()
        .args(["patch-id", "--stable"])
        .run_output_with_stdin(diff)
        .map_err(|e| format!("git patch-id failed: {e}"))?;

    Ok(String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .map(str::to_string))
}

fn branch_diff_patch_id(
    repo_path: &str,
    from_ref: &str,
    to_ref: &str,
) -> Result<Option<String>, String> {
    let range = format!("{from_ref}..{to_ref}");
    let diff = git_command_output(repo_path, &["diff", "--binary", "--no-ext-diff", &range])?;
    if !diff.status.success() {
        let stderr = String::from_utf8_lossy(&diff.stderr).trim().to_string();
        return Err(format!("git diff {range} failed: {stderr}"));
    }
    patch_id_from_diff(&diff.stdout)
}

fn commit_patch_id(repo_path: &str, commit_sha: &str) -> Result<Option<String>, String> {
    let show = git_command_output(
        repo_path,
        &[
            "show",
            "--format=",
            "--patch",
            "--binary",
            "--no-ext-diff",
            commit_sha,
        ],
    )?;
    if !show.status.success() {
        let stderr = String::from_utf8_lossy(&show.stderr).trim().to_string();
        return Err(format!("git show {commit_sha} failed: {stderr}"));
    }
    patch_id_from_diff(&show.stdout)
}

fn worktree_is_squash_merged(
    repo_path: &str,
    base_ref: &str,
    branch_name: &str,
) -> Result<bool, String> {
    let merge_base = git_command_stdout(repo_path, &["merge-base", base_ref, branch_name])?;
    if merge_base.is_empty() {
        return Ok(false);
    }

    let Some(branch_patch_id) = branch_diff_patch_id(repo_path, &merge_base, branch_name)? else {
        return Ok(false);
    };

    let commit_range = format!("{merge_base}..{base_ref}");
    let base_commits = git_command_stdout(repo_path, &["rev-list", "--no-merges", &commit_range])?;
    for commit_sha in base_commits
        .lines()
        .map(str::trim)
        .filter(|sha| !sha.is_empty())
    {
        if commit_patch_id(repo_path, commit_sha)?.as_deref() == Some(branch_patch_id.as_str()) {
            tracing::info!(
                "Detected squash-merged worktree branch {branch_name} via patch-id match on {commit_sha}"
            );
            return Ok(true);
        }
    }

    Ok(false)
}

fn disconnect_sessions_for_worktree_path(pg_pool: Option<&sqlx::PgPool>, worktree_path: &str) {
    if let Some(pool) = pg_pool {
        let worktree_path_owned = worktree_path.to_string();
        match crate::utils::async_bridge::block_on_pg_result(
            pool,
            move |bridge_pool| async move {
                let updated = sqlx::query(
                    "UPDATE sessions
                     SET cwd = NULL,
                         status = 'disconnected',
                         active_dispatch_id = NULL,
                         claude_session_id = NULL
                     WHERE cwd = $1",
                )
                .bind(&worktree_path_owned)
                .execute(&bridge_pool)
                .await
                .map_err(|err| {
                    format!(
                        "disconnect pg sessions for removed worktree {worktree_path_owned}: {err}"
                    )
                })?
                .rows_affected();
                Ok(updated)
            },
            |error| error,
        ) {
            Ok(updated) if updated > 0 => tracing::info!(
                "Disconnected {updated} PG session(s) referencing removed worktree {}",
                worktree_path
            ),
            Ok(_) => {}
            Err(err) => tracing::warn!(
                "Failed to disconnect PG sessions for removed worktree {}: {}",
                worktree_path,
                err
            ),
        }
    }
}

fn worktree_has_unmerged_commits(wt_info: &WorktreeInfo) -> Result<bool, String> {
    let base_ref = git_upstream_base_ref(&wt_info.original_path);
    let range = format!("{base_ref}..{}", wt_info.branch_name);
    let diff = git_command_output(
        &wt_info.original_path,
        &["log", "--oneline", range.as_str()],
    )?;
    if !diff.status.success() {
        let stderr = String::from_utf8_lossy(&diff.stderr).trim().to_string();
        return Err(format!("git log failed: {stderr}"));
    }
    if diff.stdout.is_empty() {
        return Ok(false);
    }

    if worktree_is_squash_merged(&wt_info.original_path, &base_ref, &wt_info.branch_name)? {
        return Ok(false);
    }

    Ok(true)
}

/// Clean up a git worktree after session ends.
pub(in crate::services::discord) fn cleanup_git_worktree(
    pg_pool: Option<&sqlx::PgPool>,
    wt_info: &WorktreeInfo,
) {
    let ts = chrono::Local::now().format("%H:%M:%S");

    let has_changes = match worktree_has_local_changes(wt_info) {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!(
                "  [{ts}] ⚠ Could not inspect worktree {} for cleanup: {} — preserving",
                wt_info.worktree_path,
                err
            );
            return;
        }
    };

    let has_commits = match worktree_has_unmerged_commits(wt_info) {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!(
                "  [{ts}] ⚠ Could not inspect branch {} for cleanup: {} — preserving",
                wt_info.branch_name,
                err
            );
            return;
        }
    };

    if has_changes || has_commits {
        tracing::info!(
            "  [{ts}] 🌿 Worktree {} has changes/unmerged commits — preserving until merge cleanup",
            wt_info.worktree_path
        );
        tracing::info!(
            "  [{ts}] 🌿 Branch: {} | Original: {}",
            wt_info.branch_name,
            wt_info.original_path
        );
    } else {
        if let Err(err) = GitCommand::new()
            .repo(&wt_info.original_path)
            .args(["worktree", "remove"])
            .arg(&wt_info.worktree_path)
            .run_output()
        {
            tracing::warn!(
                "  [{ts}] ⚠ Failed to remove worktree {}: {} — preserving DB session path",
                wt_info.worktree_path,
                err
            );
            return;
        }

        let branch_delete = GitCommand::new()
            .repo(&wt_info.original_path)
            .args(["branch", "-D"])
            .arg(&wt_info.branch_name)
            .run_output();
        let _ = std::fs::remove_dir_all(&wt_info.worktree_path);
        disconnect_sessions_for_worktree_path(pg_pool, &wt_info.worktree_path);
        if let Err(err) = branch_delete {
            tracing::warn!(
                "  [{ts}] ⚠ Removed worktree {} but could not delete branch {}: {}",
                wt_info.worktree_path,
                wt_info.branch_name,
                err
            );
        }
        tracing::info!("  [{ts}] 🧹 Cleaned up worktree: {}", wt_info.worktree_path);
    }
}

/// True when `worktree_path` exists, is a git worktree, and shares the same
/// repository (git common dir) as `parent_path`. The shared-repo match is
/// essential: a thread can be reused by a later dispatch that targets a
/// *different* repo, in which case the stored cwd must NOT be restored — we must
/// fall through to create a worktree off the requested `parent_path` so the
/// dispatch runs against its real target (#3011 codex review: avoid treating a
/// restored cwd as the dispatch target).
///
/// Both paths are compared by their `--git-common-dir` so the check holds even
/// when `parent_path` is itself a linked worktree (e.g. a dispatch worktree),
/// where comparing against the main checkout would otherwise reject a valid
/// restored thread worktree.
pub(super) fn restored_worktree_belongs_to_parent(parent_path: &str, worktree_path: &str) -> bool {
    if !std::path::Path::new(worktree_path).is_dir() {
        return false;
    }
    // Only accept a *distinct linked* worktree. If a previous
    // `create_git_worktree` failure persisted the fallback `parent_path` (the
    // shared parent checkout) as this thread's cwd, restoring it would record
    // the main checkout as `session.worktree`, defeating isolation and exposing
    // it to worktree idle-cleanup. A linked worktree's per-worktree git dir
    // differs from its shared common dir; the main checkout's do not.
    if !is_linked_worktree(worktree_path) {
        return false;
    }
    let worktree_repo = match git_common_dir(worktree_path) {
        Some(dir) => dir,
        None => return false,
    };
    let parent_repo = match git_common_dir(parent_path) {
        Some(dir) => dir,
        None => return false,
    };
    paths_equal(&worktree_repo, &parent_repo)
}

/// True when `path` lives under the AgentDesk-managed worktrees root, i.e. it is
/// a worktree this process created via [`create_git_worktree`] and therefore owns
/// for cleanup. A user's *configured workspace* that happens to be a linked git
/// worktree lives elsewhere and must NOT be treated as a disposable
/// AgentDesk-owned worktree — otherwise idle session cleanup could remove the
/// user's checkout and delete its branch (#3011 codex review P1).
pub(super) fn is_managed_worktree_path(path: &str) -> bool {
    let Some(root) = worktrees_root() else {
        return false;
    };
    let root = root.canonicalize().unwrap_or(root);
    let candidate = std::path::PathBuf::from(path);
    let candidate = candidate.canonicalize().unwrap_or(candidate);
    candidate.starts_with(&root)
}

/// True when `path` is a *linked* git worktree rather than the repository's main
/// checkout. A linked worktree's per-worktree git dir
/// (`<repo>/.git/worktrees/<name>`) differs from the shared common dir
/// (`<repo>/.git`), whereas they are identical for the main checkout.
fn is_linked_worktree(path: &str) -> bool {
    let git_dir = git_command_stdout(path, &["rev-parse", "--path-format=absolute", "--git-dir"])
        .ok()
        .filter(|dir| !dir.is_empty());
    let common_dir = git_common_dir(path);
    match (git_dir, common_dir) {
        (Some(git_dir), Some(common_dir)) => {
            let git_dir = std::path::PathBuf::from(git_dir);
            let git_dir = git_dir.canonicalize().unwrap_or(git_dir);
            !paths_equal(&git_dir, &common_dir)
        }
        _ => false,
    }
}

/// Resolve the absolute git common dir (the shared repository `.git` directory)
/// for `path`, canonicalizing so the same repo compares equal regardless of how
/// it was reached (main checkout vs. linked worktree, symlinks, relative input).
fn git_common_dir(path: &str) -> Option<std::path::PathBuf> {
    let common_dir = git_command_stdout(
        path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .ok()
    .filter(|dir| !dir.is_empty())?;
    let common_dir = std::path::PathBuf::from(common_dir);
    Some(common_dir.canonicalize().unwrap_or(common_dir))
}

/// Compare two filesystem paths, tolerating symlinked/relative differences by
/// canonicalizing when possible and falling back to a lexical comparison.
fn paths_equal(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

/// Reconstruct the [`WorktreeInfo`] for a restored thread worktree path. The
/// branch is read back from the worktree's HEAD so unmerged-commit / local-change
/// detection keeps working across restarts.
///
/// Returns `None` when the worktree's branch cannot be recovered (detached HEAD
/// or a failed lookup). Attaching `WorktreeInfo` with an empty branch is unsafe:
/// idle cleanup builds an `origin/main..<branch>` range from it and, with an
/// empty branch, inspects the wrong checkout's HEAD — so a clean detached
/// worktree carrying unmerged work could be wrongly removed (#3011 codex P1).
pub(super) fn restored_worktree_info(
    parent_path: &str,
    worktree_path: &str,
) -> Option<WorktreeInfo> {
    let branch_name = git_command_stdout(worktree_path, &["rev-parse", "--abbrev-ref", "HEAD"])
        .ok()
        .filter(|b| !b.is_empty() && b != "HEAD")?;
    Some(WorktreeInfo {
        // `original_path` is the cleanup root: idle cleanup runs
        // `git -C original_path worktree remove <worktree_path>` then
        // `git -C original_path branch -D <branch>`. It must therefore be a
        // stable checkout that survives removing this worktree — never the
        // restored worktree itself (which `parent_path` can equal when the
        // dispatch parent is itself a linked worktree). Resolve the main
        // checkout from the worktree's common dir so branch deletion still
        // works post-removal.
        original_path: main_checkout_for_worktree(worktree_path)
            .unwrap_or_else(|| parent_path.to_string()),
        worktree_path: worktree_path.to_string(),
        branch_name,
    })
}

/// #3207 (part 2): resolve the channel's EXISTING managed worktree for reuse on
/// the cold-start / resume path, instead of rotating a brand-new
/// `%Y%m%d-%H%M%S` worktree every turn. claude sessions are scoped to the cwd's
/// project dir (`~/.claude/projects/<cwd-mangled>/<sid>.jsonl`), so a rotated
/// worktree makes `--resume` structurally impossible and forces a fresh session
/// even when the DB still holds a provider session id — the conversation is lost
/// while the status panel reports "기존 세션 복원". Reusing the prior worktree
/// keeps the sid's transcript discoverable so the launch genuinely resumes.
///
/// This is the SAME persisted mapping the #3011 thread-bootstrap reuse relies on
/// (`sessions.cwd` keyed by the channel's session-key candidates), with the same
/// safety filters: the path must be an AgentDesk-managed linked worktree on disk
/// that belongs to the requested parent repo, with a recoverable branch.
/// Returns `None` when there is no reusable worktree (genuine fresh start).
pub(in crate::services::discord) fn resolve_reusable_worktree(
    pg_pool: Option<&sqlx::PgPool>,
    token_hash: &str,
    provider: &ProviderKind,
    channel_name: &str,
    channel_id: u64,
    parent_path: &str,
) -> Option<WorktreeInfo> {
    let restored = restore_thread_worktree_path_from_db(
        pg_pool,
        token_hash,
        provider,
        channel_name,
        channel_id,
    )
    .filter(|path| is_managed_worktree_path(path))
    .filter(|path| restored_worktree_belongs_to_parent(parent_path, path))?;
    restored_worktree_info(parent_path, &restored)
}

/// Reconstruct and attach [`WorktreeInfo`] to a restored session when its
/// `path` is an AgentDesk-managed linked git worktree and the session does not
/// already carry worktree metadata. No-op otherwise (already populated, a
/// user-configured workspace outside the managed worktrees root, or the main
/// checkout). Used by the auto-restore paths so a thread session that resumes
/// after a dcserver restart regains its worktree metadata, inflight worktree
/// context, and a stable cleanup root instead of silently dropping them (#3011).
pub(in crate::services::discord) fn reconstruct_managed_worktree_metadata(
    session: &mut DiscordSession,
    provider: &ProviderKind,
    channel_id: ChannelId,
    path: &str,
) {
    if session.worktree.is_some() || !is_managed_worktree_path(path) || !is_linked_worktree(path) {
        return;
    }
    // Skip reconstruction when no branch can be recovered (detached HEAD); see
    // `restored_worktree_info` — attaching an empty branch would mislead cleanup.
    let Some(wt_info) = restored_worktree_info(path, path) else {
        return;
    };
    let base_commit = crate::services::platform::git_head_commit(&wt_info.original_path);
    sync_inflight_worktree_context(
        provider,
        channel_id.get(),
        Some(wt_info.worktree_path.clone()),
        Some(wt_info.branch_name.clone()),
        base_commit,
    );
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] ↻ Restored worktree metadata: {} (branch: {})",
        wt_info.worktree_path,
        wt_info.branch_name
    );
    session.worktree = Some(wt_info);
}

/// Resolve the repository's main checkout directory for a linked `worktree_path`.
/// The common dir resolves to `<main_checkout>/.git`, so its parent is the main
/// checkout. Returns `None` when the path is not under a resolvable git repo.
fn main_checkout_for_worktree(worktree_path: &str) -> Option<String> {
    let common_dir = git_common_dir(worktree_path)?;
    let main_checkout = common_dir.parent()?;
    Some(main_checkout.to_string_lossy().to_string())
}

pub(super) fn sync_inflight_worktree_context(
    provider: &crate::services::provider::ProviderKind,
    channel_id: u64,
    worktree_path: Option<String>,
    worktree_branch: Option<String>,
    base_commit: Option<String>,
) {
    if let Some(mut inflight) = super::super::inflight::load_inflight_state(provider, channel_id) {
        inflight.set_worktree_context(worktree_path, worktree_branch, base_commit);
        let _ = super::super::inflight::save_inflight_state_if_identity_unchanged(
            &inflight,
            "sync_inflight_worktree_context",
        );
    }
}
