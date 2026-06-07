use super::runtime_store::worktrees_root;
use super::*;
use crate::services::git::GitCommand;

/// Per-channel session state
#[derive(Clone)]
pub(super) struct DiscordSession {
    pub(super) session_id: Option<String>,
    pub(super) memento_context_loaded: bool,
    pub(super) memento_reflected: bool,
    pub(super) current_path: Option<String>,
    pub(super) history: Vec<HistoryItem>,
    pub(super) pending_uploads: Vec<String>,
    pub(super) cleared: bool,
    /// Remote profile name for SSH execution (None = local)
    pub(super) remote_profile_name: Option<String>,
    pub(super) channel_id: Option<u64>,
    pub(super) channel_name: Option<String>,
    pub(super) category_name: Option<String>,
    /// Last time this session was actively used (for TTL cleanup)
    pub(super) last_active: tokio::time::Instant,
    /// If this session runs in a git worktree, store the info here
    pub(super) worktree: Option<WorktreeInfo>,
    /// Restart generation at which this session was created/restored.
    #[allow(dead_code)]
    pub(super) born_generation: u64,
    pub(super) assistant_turns: usize,
}

pub(super) fn allows_nonlocal_session_path(remote_profile_name: Option<&str>) -> bool {
    remote_profile_name.is_some_and(|name| !name.trim().is_empty())
}

pub(super) fn session_path_is_usable(
    current_path: &str,
    remote_profile_name: Option<&str>,
) -> bool {
    allows_nonlocal_session_path(remote_profile_name) || std::path::Path::new(current_path).is_dir()
}

pub(super) fn select_restored_session_path(
    configured_path: Option<String>,
    db_cwd: Option<String>,
    yaml_path: Option<String>,
    remote_profile_name: Option<&str>,
) -> Option<String> {
    configured_path
        .filter(|path| session_path_is_usable(path, remote_profile_name))
        .or_else(|| db_cwd.filter(|path| session_path_is_usable(path, remote_profile_name)))
        .or_else(|| yaml_path.filter(|path| session_path_is_usable(path, remote_profile_name)))
}

/// #3216 GAP 2: decide whether a live tmux pane's cwd should override the
/// DB-resolved cwd during recovery. The live tmux pane is the authoritative
/// source of truth for where a session is actually running — if the DB cwd has
/// diverged (e.g. a phantom worktree rotation stamped a transcript-less path),
/// trusting the DB blindly relaunches `--resume` against the wrong cwd and the
/// conversation is lost.
///
/// Returns `Some(tmux_cwd)` only when ALL hold:
///   * a live tmux pane cwd is present (`tmux_cwd`);
///   * it is a real AgentDesk-managed, on-disk-usable worktree
///     (guarded by `tmux_cwd_is_managed` / `tmux_cwd_is_usable` predicates so we
///     never adopt a transient or garbage path);
///   * it actually DIFFERS from the DB cwd (nothing to reconcile otherwise).
///
/// Kept as a pure function (predicate results injected) so the reconcile policy
/// is unit-testable without a live tmux / filesystem.
fn reconcile_recovery_cwd(
    db_cwd: Option<&str>,
    tmux_cwd: Option<&str>,
    tmux_cwd_is_managed: bool,
    tmux_cwd_is_usable: bool,
) -> Option<String> {
    let tmux_cwd = tmux_cwd?.trim();
    if tmux_cwd.is_empty() || !tmux_cwd_is_managed || !tmux_cwd_is_usable {
        return None;
    }
    if db_cwd.map(str::trim) == Some(tmux_cwd) {
        // DB already agrees with the live pane — nothing to reconcile.
        return None;
    }
    Some(tmux_cwd.to_string())
}

impl DiscordSession {
    pub(super) fn clear_provider_session(&mut self) {
        self.session_id = None;
        self.memento_context_loaded = false;
        self.memento_reflected = false;
    }

    pub(super) fn restore_provider_session(&mut self, session_id: Option<String>) {
        self.memento_context_loaded = restored_memento_context_loaded(
            self.memento_context_loaded,
            self.session_id.as_deref(),
            session_id.as_deref(),
        );
        self.session_id = session_id;
        self.memento_reflected = false;
    }

    pub(super) fn note_memento_context_loaded(&mut self) {
        self.memento_context_loaded = true;
        self.memento_reflected = false;
    }

    pub(super) fn assistant_turn_count(&self) -> usize {
        self.history
            .iter()
            .filter(|item| item.item_type == HistoryType::Assistant)
            .count()
    }

    pub(super) fn recent_history_context(&self, max_messages: usize) -> Option<String> {
        if max_messages == 0 {
            return None;
        }

        let mut lines = self
            .history
            .iter()
            .rev()
            .filter_map(|item| {
                let speaker = match item.item_type {
                    HistoryType::User => "User",
                    HistoryType::Assistant => "Assistant",
                    _ => return None,
                };
                let content = item.content.trim();
                if content.is_empty() {
                    return None;
                }
                Some(format!(
                    "{speaker}: {}",
                    content.chars().take(300).collect::<String>()
                ))
            })
            .take(max_messages)
            .collect::<Vec<_>>();

        if lines.is_empty() {
            return None;
        }

        lines.reverse();
        Some(lines.join("\n"))
    }
    pub(super) fn clear_transcript_history(&mut self) {
        self.history.clear();
        self.assistant_turns = 0;
    }

    pub(super) fn will_reach_turn_cap(&self) -> bool {
        self.assistant_turns.saturating_add(1) >= SESSION_MAX_ASSISTANT_TURNS
    }

    pub(super) fn record_completed_turn(
        &mut self,
        user_text: String,
        assistant_text: String,
    ) -> bool {
        self.history.push(HistoryItem {
            item_type: HistoryType::User,
            content: user_text,
        });
        self.history.push(HistoryItem {
            item_type: HistoryType::Assistant,
            content: assistant_text,
        });
        self.assistant_turns = self.assistant_turns.saturating_add(1);
        self.assistant_turns >= SESSION_MAX_ASSISTANT_TURNS
    }
    /// Validate `current_path` and return it if it exists on disk.
    /// If the path is stale (deleted), clear `current_path` and `worktree`, log, and return `None`.
    pub(super) fn validated_path(&mut self, channel_id: impl std::fmt::Display) -> Option<String> {
        let current_path = self.current_path.as_ref()?;
        if session_path_is_usable(current_path, self.remote_profile_name.as_deref()) {
            return Some(current_path.clone());
        }
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ⚠ Ignoring stale local session path for channel {}: {}",
            channel_id,
            current_path
        );
        self.current_path = None;
        self.worktree = None;
        None
    }
}

pub(super) fn restored_memento_context_loaded(
    previous_loaded: bool,
    previous_session_id: Option<&str>,
    next_session_id: Option<&str>,
) -> bool {
    previous_loaded && previous_session_id == next_session_id && next_session_id.is_some()
}

/// Worktree info for sessions that were auto-redirected to avoid conflicts
#[derive(Clone, Debug)]
pub(super) struct WorktreeInfo {
    /// The original repo path that was conflicted
    pub original_path: String,
    /// The worktree directory path
    pub(super) worktree_path: String,
    /// The branch name created for this worktree
    pub(super) branch_name: String,
}

pub(super) fn synthetic_thread_channel_name(parent_name: &str, channel_id: ChannelId) -> String {
    format!("{parent_name}-t{}", channel_id.get())
}

pub(super) fn is_synthetic_thread_channel_name(channel_name: &str, channel_id: ChannelId) -> bool {
    channel_name.ends_with(&format!("-t{}", channel_id.get()))
}

pub(super) fn choose_restore_channel_name(
    existing_channel_name: Option<&str>,
    live_channel_name: Option<&str>,
    thread_parent: Option<(ChannelId, Option<String>)>,
    channel_id: ChannelId,
) -> Option<String> {
    if let Some(existing_name) = existing_channel_name
        && is_synthetic_thread_channel_name(existing_name, channel_id)
    {
        return Some(existing_name.to_string());
    }

    if let Some((parent_id, parent_name)) = thread_parent {
        let parent_name = parent_name.unwrap_or_else(|| parent_id.get().to_string());
        return Some(synthetic_thread_channel_name(&parent_name, channel_id));
    }

    live_channel_name
        .or(existing_channel_name)
        .map(ToOwned::to_owned)
}

pub(super) fn resolve_is_dm_channel(
    dm_hint: Option<bool>,
    live_channel_lookup_says_dm: bool,
) -> bool {
    // Prefer the gateway-provided DM hint when available so a transient
    // Discord channel lookup failure cannot disable DM default-agent fallback.
    dm_hint.unwrap_or(live_channel_lookup_says_dm)
}

/// Check if a path is a git repo and if another channel already uses it.
/// Returns the conflicting channel's name if found.
pub(super) fn detect_worktree_conflict(
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
pub(super) fn create_git_worktree(
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

fn disconnect_sessions_for_worktree_path(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    worktree_path: &str,
) {
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

    let Some(db) = db else {
        return;
    };

    let Ok(conn) = db.lock() else {
        tracing::warn!(
            "Failed to lock DB while disconnecting sessions for removed worktree {}",
            worktree_path
        );
        return;
    };

    match conn.execute(
        "UPDATE sessions
         SET cwd = NULL,
             status = 'disconnected',
             active_dispatch_id = NULL,
             claude_session_id = NULL
         WHERE cwd = ?1",
        [worktree_path],
    ) {
        Ok(updated) if updated > 0 => tracing::info!(
            "Disconnected {updated} SQLite session(s) referencing removed worktree {}",
            worktree_path
        ),
        Ok(_) => {}
        Err(err) => tracing::warn!(
            "Failed to disconnect SQLite sessions for removed worktree {}: {}",
            worktree_path,
            err
        ),
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
pub(super) fn cleanup_git_worktree(
    db: Option<&crate::db::Db>,
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
        disconnect_sessions_for_worktree_path(db, pg_pool, &wt_info.worktree_path);
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

/// Auto-restore session from bot_settings.json if not in memory
pub(super) async fn auto_restore_session(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    serenity_ctx: &serenity::prelude::Context,
) {
    auto_restore_session_with_dm_hint(shared, channel_id, serenity_ctx, None).await;
}

pub(super) async fn auto_restore_session_with_dm_hint(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    serenity_ctx: &serenity::prelude::Context,
    dm_hint: Option<bool>,
) {
    if matches!(
        resolve_runtime_channel_binding_status(&serenity_ctx.http, channel_id).await,
        RuntimeChannelBindingStatus::Unowned
    ) {
        return;
    }

    auto_restore_session_force(shared, channel_id, serenity_ctx, dm_hint).await;
}

/// Same as [`auto_restore_session_with_dm_hint`] but skips the
/// `RuntimeChannelBindingStatus::Unowned` early-return. Intended for callers
/// that have already decided an unbound channel deserves restoration —
/// e.g. the BINDING-GUARD's `can_route_unbound_direct_session` path which
/// only proceeds when persistent state already names a workspace for that
/// channel. Without this escape hatch the BINDING-GUARD's restoration step
/// silently no-ops on unowned channels and the channel stops responding
/// after a dcserver restart drops the in-memory session map (#1190 followup,
/// agentless direct sessions regression observed 2026-04-26).
pub(super) async fn auto_restore_session_force(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    serenity_ctx: &serenity::prelude::Context,
    dm_hint: Option<bool>,
) {
    // Resolve channel/category before taking the lock for mutation
    let (live_ch_name, cat_name) =
        resolve_channel_category(&serenity_ctx.http, Some(&serenity_ctx.cache), channel_id).await;
    let existing_channel_name = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.clone())
    };
    let restore_ch_name = choose_restore_channel_name(
        existing_channel_name.as_deref(),
        live_ch_name.as_deref(),
        resolve_thread_parent(&serenity_ctx.http, channel_id).await,
        channel_id,
    );
    let is_dm = matches!(
        channel_id.to_channel(&serenity_ctx.http).await.ok(),
        Some(serenity::Channel::Private(_))
    );
    let is_dm = resolve_is_dm_channel(dm_hint, is_dm);

    // Read settings first to get provider and runtime restore metadata.
    let (last_path, saved_remote, provider) = {
        let settings = shared.settings.read().await;
        let provider = settings.provider.clone();
        let configured_path = settings::resolve_workspace(channel_id, restore_ch_name.as_deref())
            .or_else(|| {
                if is_dm {
                    super::agentdesk_config::resolve_dm_default_agent(&provider)
                        .map(|resolved| resolved.workspace)
                } else {
                    None
                }
            });
        let saved_remote = load_last_remote_profile(
            shared.pg_pool.as_ref(),
            &shared.token_hash,
            channel_id.get(),
        );

        // Use the effective tmux channel name here so restart recovery keeps
        // looking up the same session key for thread sessions that intentionally
        // use a synthetic "{parent}-t{thread_id}" channel name.
        let mut db_cwd: Option<String> = restore_ch_name.as_ref().and_then(|ch| {
            // #3207 (part 2) P0-a: the DB cwd resolve is channel-scoped (see
            // `restore_session_cwd_from_db`); only a usable path is honored for
            // install into `session.current_path`.
            restore_session_cwd_from_db(
                shared.pg_pool.as_ref(),
                &shared.token_hash,
                &provider,
                ch,
                channel_id.get(),
            )
            .filter(|p| !p.is_empty() && session_path_is_usable(p, saved_remote.as_deref()))
        });

        // #3216 GAP 2: reconcile the DB cwd against the live tmux pane. The live
        // pane is the source of truth for where the session actually runs; if the
        // DB cwd diverged (e.g. a phantom worktree rotation), adopting it would
        // relaunch `--resume` against a transcript-less path. When a live tmux
        // session exists and its pane cwd is a real managed/usable worktree that
        // DIFFERS from the DB cwd, adopt the tmux cwd AND correct the DB row.
        if let Some(ch) = restore_ch_name.as_ref() {
            let tmux_name = provider.build_tmux_session_name(ch);
            let tmux_cwd = crate::services::platform::tmux::pane_current_path(&tmux_name);
            let tmux_cwd_is_managed = tmux_cwd
                .as_deref()
                .map(is_managed_worktree_path)
                .unwrap_or(false);
            let tmux_cwd_is_usable = tmux_cwd
                .as_deref()
                .map(|p| session_path_is_usable(p, saved_remote.as_deref()))
                .unwrap_or(false);
            if let Some(reconciled) = reconcile_recovery_cwd(
                db_cwd.as_deref(),
                tmux_cwd.as_deref(),
                tmux_cwd_is_managed,
                tmux_cwd_is_usable,
            ) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ↻ #3216 reconciling recovery cwd for channel {}: DB {:?} → live tmux {}",
                    channel_id,
                    db_cwd.as_deref(),
                    reconciled
                );
                correct_session_cwd_to_tmux(
                    shared.pg_pool.as_ref(),
                    &shared.token_hash,
                    &provider,
                    ch,
                    channel_id.get(),
                    &reconciled,
                );
                db_cwd = Some(reconciled);
            }
        }
        let persisted_path = load_last_session_path(
            shared.pg_pool.as_ref(),
            &shared.token_hash,
            channel_id.get(),
        );

        if let (Some(configured), Some(restored)) = (configured_path.as_ref(), db_cwd.as_ref())
            && configured != restored
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ Ignoring restored DB cwd for channel {}: {} (configured workspace: {})",
                channel_id,
                restored,
                configured
            );
        }

        let last_path = select_restored_session_path(
            configured_path,
            db_cwd,
            persisted_path,
            saved_remote.as_deref(),
        );

        (last_path, saved_remote, provider)
    };

    let mut data = shared.core.lock().await;
    if let Some(session) = data.sessions.get_mut(&channel_id) {
        session.channel_id = Some(channel_id.get());
        session.last_active = tokio::time::Instant::now();
        session.channel_name = restore_ch_name.clone();
        session.category_name = cat_name.clone();
        if session.remote_profile_name.is_none() {
            session.remote_profile_name = saved_remote.clone();
        }
        if session.current_path.is_some() || last_path.is_none() {
            // A pre-existing session (e.g. inserted by restart watcher
            // registration with `current_path` from `sessions.cwd` but
            // `worktree: None`) hits this early return before the insertion
            // block below. Reconstruct the managed-worktree metadata here too so
            // restarted thread sessions regain `WorktreeInfo` / inflight worktree
            // context and a correct cleanup root (#3011).
            if let Some(current_path) = session.current_path.clone() {
                reconstruct_managed_worktree_metadata(
                    session,
                    &provider,
                    channel_id,
                    &current_path,
                );
            }
            return;
        }
    }

    if let Some(last_path) = last_path
        && session_path_is_usable(&last_path, saved_remote.as_deref())
    {
        // Session ID is restored from DB (sessions.claude_session_id column)
        // which is already loaded into DiscordSession.session_id at startup.
        let session = data
            .sessions
            .entry(channel_id)
            .or_insert_with(|| DiscordSession {
                session_id: None,
                memento_context_loaded: false,
                memento_reflected: false,
                current_path: None,
                history: Vec::new(),
                pending_uploads: Vec::new(),
                cleared: false,
                channel_id: Some(channel_id.get()),
                channel_name: restore_ch_name.clone(),
                category_name: cat_name.clone(),
                remote_profile_name: saved_remote.clone(),
                last_active: tokio::time::Instant::now(),
                worktree: None,
                born_generation: runtime_store::load_generation(),
                assistant_turns: 0,
            });
        session.channel_id = Some(channel_id.get());
        session.last_active = tokio::time::Instant::now();
        session.channel_name = restore_ch_name.clone();
        session.category_name = cat_name.clone();
        if session.remote_profile_name.is_none() {
            session.remote_profile_name = saved_remote.clone();
        }
        session.current_path = Some(last_path.clone());
        reconstruct_managed_worktree_metadata(session, &provider, channel_id, &last_path);
        drop(data);

        // Rescan skills with project path
        let new_skills = scan_skills(&provider, Some(&last_path));
        *shared.skills_cache.write().await = new_skills;
        let ts = chrono::Local::now().format("%H:%M:%S");
        let remote_info = saved_remote
            .as_ref()
            .map(|n| format!(" (remote: {})", n))
            .unwrap_or_default();
        tracing::info!("  [{ts}] ↻ Auto-restored session: {last_path}{remote_info}");
    }
}

/// Resolve a channel's persisted `sessions.cwd` for the given `session_key`,
/// scoped to the unique Discord `channel_id`, with a SAFE legacy fallback for
/// rows that predate the `channel_id` column.
///
/// #3216 GAP 1: migration `0071_sessions_channel_id.sql` only added the column;
/// existing rows kept `channel_id = NULL`. The strict `channel_id = $2` guard
/// (#3207) therefore never matches a legacy row, so the FIRST restore after
/// deploy still rotates a brand-new worktree and divorces the live session from
/// its transcript. We cannot backfill the numeric id in pure SQL (the id is not
/// derivable from `session_key`, which holds the channel NAME), so instead we
/// fall back to a name-only lookup — but ONLY when it is unambiguous:
///
///   * channel-scoped match (`session_key = $1 AND channel_id = $2`) wins first;
///   * otherwise, fall back ONLY when there is EXACTLY ONE row for the
///     `session_key` AND that row's `channel_id IS NULL` (a true legacy row).
///
/// `sessions.session_key` is globally UNIQUE (migration `001_initial.sql`), so
/// there is at most one row per key; the `rows.len() == 1` check is therefore a
/// defensive belt-and-braces guard. If the single row carries a DIFFERENT
/// non-null `channel_id`, the fallback is refused — that would reintroduce the
/// #3207 cross-channel hazard. New heartbeats stamp `channel_id`, so this
/// self-heals over time. Returns the resolved cwd (non-empty) for this
/// `session_key`.
async fn resolve_cwd_for_session_key(
    pool: &sqlx::PgPool,
    session_key: &str,
    channel_id: &str,
) -> Result<Option<String>, String> {
    // 1. Channel-scoped match (the #3207 cross-channel guard).
    let scoped = sqlx::query_scalar::<_, Option<String>>(
        "SELECT cwd FROM sessions \
         WHERE session_key = $1 AND channel_id = $2 LIMIT 1",
    )
    .bind(session_key)
    .bind(channel_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load session cwd {session_key}: {error}"))?
    .flatten();
    if let Some(path) = scoped.filter(|p| !p.is_empty()) {
        return Ok(Some(path));
    }

    // 2. #3216 GAP 1 safe legacy fallback: inspect ALL rows for this
    //    session_key. Honor only the unambiguous single-NULL-channel_id case.
    let rows = sqlx::query_as::<_, (Option<String>, Option<String>)>(
        "SELECT cwd, channel_id FROM sessions WHERE session_key = $1",
    )
    .bind(session_key)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load legacy session cwd {session_key}: {error}"))?;

    if rows.len() == 1 {
        let (cwd, row_channel_id) = &rows[0];
        if row_channel_id.is_none()
            && let Some(path) = cwd.clone().filter(|p| !p.is_empty())
        {
            tracing::info!(
                "  ↻ #3216 legacy NULL-channel_id fallback: reusing cwd {} for \
                 session_key={} (channel_id={}); row will self-heal on next heartbeat",
                path,
                session_key,
                channel_id
            );
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// Resolve a channel's persisted `sessions.cwd` for restart restore, scoped to
/// the unique Discord `channel_id` (with the #3216 safe legacy fallback).
///
/// Backs the `db_cwd` lookup in [`auto_restore_session_force`], which installs
/// the resolved path into `session.current_path`. See
/// [`resolve_cwd_for_session_key`] for the channel-scoping / legacy-fallback
/// semantics.
///
/// The on-disk usability filter (`session_path_is_usable`) is applied by the
/// caller so this helper stays a pure DB resolve.
fn restore_session_cwd_from_db(
    pg_pool: Option<&sqlx::PgPool>,
    token_hash: &str,
    provider: &ProviderKind,
    channel_name: &str,
    channel_id: u64,
) -> Option<String> {
    let tmux_name = provider.build_tmux_session_name(channel_name);
    let session_keys = build_session_key_candidates(token_hash, provider, &tmux_name);
    let channel_id = channel_id.to_string();
    let pg_pool = pg_pool?;
    crate::utils::async_bridge::block_on_pg_result(
        pg_pool,
        move |pool| async move {
            for session_key in session_keys {
                if let Some(path) =
                    resolve_cwd_for_session_key(&pool, &session_key, &channel_id).await?
                {
                    return Ok(Some(path));
                }
            }
            Ok(None)
        },
        |message| message,
    )
    .ok()
    .flatten()
}

/// #3216 GAP 2: correct the persisted `sessions.cwd` to the authoritative live
/// tmux pane cwd during recovery reconciliation. Scoped by `session_key` AND the
/// unique `channel_id` so a name collision can never write into another channel's
/// row (the #3207 cross-channel guard). A legacy NULL-channel_id row is matched
/// too — only the row whose id equals THIS channel OR is NULL is updated — so the
/// row both adopts the correct cwd and gets self-healed onto this channel.
fn correct_session_cwd_to_tmux(
    pg_pool: Option<&sqlx::PgPool>,
    token_hash: &str,
    provider: &ProviderKind,
    channel_name: &str,
    channel_id: u64,
    tmux_cwd: &str,
) {
    let Some(pool) = pg_pool else {
        return;
    };
    let tmux_name = provider.build_tmux_session_name(channel_name);
    let session_keys = build_session_key_candidates(token_hash, provider, &tmux_name);
    let channel_id_str = channel_id.to_string();
    let tmux_cwd = tmux_cwd.to_string();
    let result = crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let mut total = 0u64;
            for session_key in session_keys {
                let updated = sqlx::query(
                    "UPDATE sessions \
                     SET cwd = $1, channel_id = $2 \
                     WHERE session_key = $3 \
                       AND (channel_id = $2 OR channel_id IS NULL)",
                )
                .bind(&tmux_cwd)
                .bind(&channel_id_str)
                .bind(&session_key)
                .execute(&bridge_pool)
                .await
                .map_err(|err| format!("reconcile cwd for {session_key}: {err}"))?
                .rows_affected();
                total += updated;
            }
            Ok(total)
        },
        |error| error,
    );
    match result {
        Ok(updated) if updated > 0 => tracing::info!(
            "  ↻ #3216 reconciled DB cwd to live tmux pane for channel {} ({} row(s))",
            channel_id,
            updated
        ),
        Ok(_) => {}
        Err(err) => tracing::warn!(
            "  ⚠ #3216 failed to reconcile DB cwd to live tmux for channel {}: {}",
            channel_id,
            err
        ),
    }
}

/// Look up the persisted worktree path for a thread session from the `sessions`
/// DB table, mirroring the restore lookup in [`auto_restore_session_force`].
///
/// After a dcserver restart the in-memory `sessions` map is empty, so without
/// this lookup a new thread message would create a brand-new worktree and drop
/// the provider session fingerprint / recovery context tied to the previous
/// worktree path (#3011). The returned path is only honored when it still names
/// a usable git worktree on disk; otherwise we fall back to creating a fresh one.
///
/// #3207 (part 2) P0: the `session_key` is derived from the sanitized/truncated
/// channel NAME, so two distinct channels whose names collide produce the SAME
/// `session_key` and would resolve EACH OTHER's persisted cwd. The lookup is
/// therefore scoped by the unique `channel_id` — only a row stamped with THIS
/// channel's id is honored, so a name collision can never cross channels.
///
/// #3216 GAP 1: legacy rows written before the `channel_id` column existed carry
/// `channel_id = NULL` and so never match the strict scoped predicate, forcing a
/// worktree rotation on the first restore after deploy. [`resolve_cwd_for_session_key`]
/// adds a SAFE fallback that reuses such a row ONLY when it is unambiguous
/// (exactly one row for the `session_key` and its `channel_id IS NULL`), which
/// preserves the cross-channel guard while letting legacy sessions keep their
/// transcript-bearing worktree.
fn restore_thread_worktree_path_from_db(
    pg_pool: Option<&sqlx::PgPool>,
    token_hash: &str,
    provider: &ProviderKind,
    channel_name: &str,
    channel_id: u64,
) -> Option<String> {
    let tmux_name = provider.build_tmux_session_name(channel_name);
    let session_keys = build_session_key_candidates(token_hash, provider, &tmux_name);
    let channel_id = channel_id.to_string();
    let pg_pool = pg_pool?;
    crate::utils::async_bridge::block_on_pg_result(
        pg_pool,
        move |pool| async move {
            for session_key in session_keys {
                if let Some(path) =
                    resolve_cwd_for_session_key(&pool, &session_key, &channel_id).await?
                {
                    return Ok(Some(path));
                }
            }
            Ok(None)
        },
        |message| message,
    )
    .ok()
    .flatten()
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
fn restored_worktree_belongs_to_parent(parent_path: &str, worktree_path: &str) -> bool {
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
fn is_managed_worktree_path(path: &str) -> bool {
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
fn restored_worktree_info(parent_path: &str, worktree_path: &str) -> Option<WorktreeInfo> {
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
pub(super) fn resolve_reusable_worktree(
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
fn reconstruct_managed_worktree_metadata(
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

/// Create a lightweight session for a thread, bootstrapped from the parent channel's path.
/// The session's `channel_name` uses `{parent_channel}-t{thread_id}` so the derived
/// tmux session name stays short and unique instead of using the full thread title.
pub(super) async fn bootstrap_thread_session(
    shared: &Arc<SharedData>,
    thread_channel_id: ChannelId,
    parent_path: &str,
    http: &Arc<serenity::http::Http>,
    cache: Option<&Arc<serenity::cache::Cache>>,
) -> bool {
    let (thread_title, cat_name) = resolve_channel_category(http, cache, thread_channel_id).await;
    let provider_kind = shared.settings.read().await.provider.clone();
    // Build a short, stable channel_name: "{parent_channel}-t{thread_id}"
    let parent_info = resolve_thread_parent(http, thread_channel_id).await;
    let ch_name = if let Some((parent_id, parent_name)) = parent_info {
        let parent = parent_name.unwrap_or_else(|| format!("{parent_id}"));
        Some(synthetic_thread_channel_name(&parent, thread_channel_id))
    } else {
        // Not a thread (shouldn't happen here) — fall back to resolved name
        thread_title
    };
    let mut data = shared.core.lock().await;
    if data.sessions.contains_key(&thread_channel_id) {
        return false;
    }

    // Session ID comes from DB (sessions.claude_session_id), not from file.
    let session = data
        .sessions
        .entry(thread_channel_id)
        .or_insert_with(|| DiscordSession {
            session_id: None,
            memento_context_loaded: false,
            memento_reflected: false,
            current_path: None,
            history: Vec::new(),
            pending_uploads: Vec::new(),
            cleared: false,
            channel_id: Some(thread_channel_id.get()),
            channel_name: ch_name,
            category_name: cat_name,
            remote_profile_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: runtime_store::load_generation(),
            assistant_turns: 0,
        });
    // Prefer restoring the worktree persisted for this thread session across a
    // dcserver restart. The in-memory `sessions` map is cleared on restart, so
    // without this lookup a new thread message would create a brand-new worktree
    // and drop the provider session fingerprint / recovery context tied to the
    // previous worktree path (#3011). Mirror the DB cwd lookup used by
    // `auto_restore_session_force`, and only create a fresh worktree when the
    // stored path is absent or no longer a usable git worktree on disk.
    let ch = session
        .channel_name
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let restored_worktree = restore_thread_worktree_path_from_db(
        shared.pg_pool.as_ref(),
        &shared.token_hash,
        &provider_kind,
        &ch,
        thread_channel_id.get(),
    )
    .filter(|path| is_managed_worktree_path(path))
    .filter(|path| restored_worktree_belongs_to_parent(parent_path, path));
    // Only honor the restore when a branch is recoverable. A detached / unknown
    // branch would yield an empty `branch_name` that misleads idle cleanup, so
    // in that case fall through to create a fresh, well-formed worktree instead.
    if let Some(restored_path) = restored_worktree
        && let Some(wt_info) = restored_worktree_info(parent_path, &restored_path)
    {
        let base_commit = crate::services::platform::git_head_commit(&wt_info.original_path);
        let restored_path = wt_info.worktree_path.clone();
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ↻ Restored thread worktree: {} (branch: {})",
            wt_info.worktree_path,
            wt_info.branch_name
        );
        sync_inflight_worktree_context(
            &provider_kind,
            thread_channel_id.get(),
            Some(wt_info.worktree_path.clone()),
            Some(wt_info.branch_name.clone()),
            base_commit,
        );
        session.worktree = Some(wt_info);
        session.current_path = Some(restored_path.clone());
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!("  [{ts}] ↻ Bootstrapped thread session: {restored_path}");
        return true;
    }

    // Always create a worktree for thread sessions to isolate concurrent work.
    let effective_path = {
        let provider_str = shared.settings.read().await.provider.as_str().to_string();
        match create_git_worktree(parent_path, &ch, &provider_str) {
            Ok((wt_path, branch)) => {
                let base_commit = crate::services::platform::git_head_commit(parent_path);
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🌿 Thread worktree created: {} (branch: {})",
                    wt_path,
                    branch
                );
                session.worktree = Some(WorktreeInfo {
                    original_path: parent_path.to_string(),
                    worktree_path: wt_path.clone(),
                    branch_name: branch.clone(),
                });
                sync_inflight_worktree_context(
                    &provider_kind,
                    thread_channel_id.get(),
                    Some(wt_path.clone()),
                    Some(branch),
                    base_commit,
                );
                wt_path
            }
            Err(e) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ Thread worktree creation failed: {e}, falling back to parent path"
                );
                parent_path.to_string()
            }
        }
    };
    session.current_path = Some(effective_path.clone());
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] ↻ Bootstrapped thread session: {effective_path}");
    true
}

fn sync_inflight_worktree_context(
    provider: &crate::services::provider::ProviderKind,
    channel_id: u64,
    worktree_path: Option<String>,
    worktree_branch: Option<String>,
    base_commit: Option<String>,
) {
    if let Some(mut inflight) = super::inflight::load_inflight_state(provider, channel_id) {
        inflight.set_worktree_context(worktree_path, worktree_branch, base_commit);
        let _ = super::inflight::save_inflight_state(&inflight);
    }
}

/// Resolve the channel name and parent category name for a Discord channel.
///
/// `cache` is an optional optimization: when present (leader-side), category
/// names are looked up via the in-memory guild cache and avoid an extra REST
/// hop. Worker-side callers without a live shard pass `None` and pay the
/// REST fallback at line ~978 instead. Correctness is identical either way.
pub(super) async fn resolve_channel_category(
    http: &Arc<serenity::http::Http>,
    cache: Option<&Arc<serenity::cache::Cache>>,
    channel_id: serenity::model::id::ChannelId,
) -> (Option<String>, Option<String>) {
    let Ok(channel) = channel_id.to_channel(http).await else {
        return (None, None);
    };
    let serenity::model::channel::Channel::Guild(gc) = channel else {
        return (None, None);
    };
    let ch_name = Some(gc.name.clone());
    let cat_name = if let Some(parent_id) = gc.parent_id {
        let cached_cat_name = cache.and_then(|c| {
            c.guild(gc.guild_id).and_then(|guild| {
                guild
                    .channels
                    .get(&parent_id)
                    .map(|parent_ch| parent_ch.name.clone())
            })
        });

        if let Some(cat_name) = cached_cat_name {
            Some(cat_name)
        } else if let Ok(parent_ch) = parent_id.to_channel(http).await {
            match parent_ch {
                serenity::model::channel::Channel::Guild(cat) => Some(cat.name.clone()),
                _ => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] ⚠ Category channel {parent_id} is not a Guild channel for #{}",
                        gc.name
                    );
                    None
                }
            }
        } else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ Failed to resolve category {parent_id} for #{}",
                gc.name
            );
            None
        }
    } else {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!("  [{ts}] ⚠ No parent_id for #{}", gc.name);
        None
    };
    (ch_name, cat_name)
}

pub(in crate::services::discord) async fn validate_live_channel_routing_with_dm_hint(
    ctx: &serenity::prelude::Context,
    provider: &ProviderKind,
    settings: &DiscordBotSettings,
    channel_id: serenity::model::id::ChannelId,
    is_dm_hint: Option<bool>,
) -> Result<(), settings::BotChannelRoutingGuardFailure> {
    let is_dm = match is_dm_hint {
        Some(is_dm) => is_dm,
        None => matches!(
            channel_id.to_channel(&ctx.http).await,
            Ok(serenity::model::channel::Channel::Private(_))
        ),
    };
    let (channel_name, _) = resolve_channel_category(&ctx.http, Some(&ctx.cache), channel_id).await;
    let (allowlist_channel_id, provider_channel_name) = if let Some((parent_id, parent_name)) =
        resolve_thread_parent(&ctx.http, channel_id).await
    {
        (parent_id, parent_name.or(channel_name.clone()))
    } else {
        (channel_id, channel_name.clone())
    };
    validate_bot_channel_routing_with_provider_channel(
        settings,
        provider,
        allowlist_channel_id,
        channel_name.as_deref(),
        provider_channel_name.as_deref(),
        is_dm,
    )
}

pub(in crate::services::discord) async fn validate_live_channel_routing(
    ctx: &serenity::prelude::Context,
    provider: &ProviderKind,
    settings: &DiscordBotSettings,
    channel_id: serenity::model::id::ChannelId,
) -> Result<(), settings::BotChannelRoutingGuardFailure> {
    validate_live_channel_routing_with_dm_hint(ctx, provider, settings, channel_id, None).await
}

pub(super) async fn provider_handles_channel(
    ctx: &serenity::prelude::Context,
    provider: &ProviderKind,
    settings: &DiscordBotSettings,
    channel_id: serenity::model::id::ChannelId,
) -> bool {
    validate_live_channel_routing(ctx, provider, settings, channel_id)
        .await
        .is_ok()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RuntimeChannelBindingStatus {
    Owned,
    Unowned,
    Unknown,
}

pub(super) async fn resolve_runtime_channel_binding_status(
    http: &Arc<serenity::Http>,
    channel_id: serenity::model::id::ChannelId,
) -> RuntimeChannelBindingStatus {
    if settings::has_configured_channel_binding(channel_id, None) {
        return RuntimeChannelBindingStatus::Owned;
    }

    let Ok(channel) = channel_id.to_channel(http).await else {
        return RuntimeChannelBindingStatus::Unknown;
    };

    match channel {
        serenity::model::channel::Channel::Private(_) => RuntimeChannelBindingStatus::Owned,
        serenity::model::channel::Channel::Guild(gc) => {
            use poise::serenity_prelude::ChannelType;
            match gc.kind {
                ChannelType::PublicThread | ChannelType::PrivateThread => {
                    let Some(parent_id) = gc.parent_id else {
                        return RuntimeChannelBindingStatus::Unowned;
                    };
                    let parent_name = match parent_id.to_channel(http).await {
                        Ok(serenity::model::channel::Channel::Guild(parent)) => {
                            Some(parent.name.clone())
                        }
                        Ok(_) => None,
                        Err(_) => None,
                    };
                    if settings::has_configured_channel_binding(parent_id, parent_name.as_deref()) {
                        RuntimeChannelBindingStatus::Owned
                    } else {
                        RuntimeChannelBindingStatus::Unowned
                    }
                }
                _ => {
                    if settings::has_configured_channel_binding(channel_id, Some(&gc.name)) {
                        RuntimeChannelBindingStatus::Owned
                    } else {
                        RuntimeChannelBindingStatus::Unowned
                    }
                }
            }
        }
        _ => RuntimeChannelBindingStatus::Unowned,
    }
}

/// If `channel_id` is a Discord thread, return the parent channel ID and name.
/// For non-thread channels, returns `None`.
pub(super) async fn resolve_thread_parent(
    http: &Arc<serenity::Http>,
    channel_id: serenity::model::id::ChannelId,
) -> Option<(serenity::model::id::ChannelId, Option<String>)> {
    let channel = channel_id.to_channel(http).await.ok()?;
    let serenity::model::channel::Channel::Guild(gc) = channel else {
        return None;
    };
    use poise::serenity_prelude::ChannelType;
    match gc.kind {
        ChannelType::PublicThread | ChannelType::PrivateThread => {
            let parent_id = gc.parent_id?;
            let parent_name = if let Ok(parent_ch) = parent_id.to_channel(http).await {
                match parent_ch {
                    serenity::model::channel::Channel::Guild(pg) => Some(pg.name.clone()),
                    _ => None,
                }
            } else {
                None
            };
            Some((parent_id, parent_name))
        }
        _ => None,
    }
}

#[cfg(test)]
mod worktree_reuse_channel_isolation_tests {
    //! #3207 (part 2) P0: the worktree-reuse DB lookup
    //! (`restore_thread_worktree_path_from_db`) must be scoped by the unique
    //! channel id. Two channels whose sanitized/truncated names collide produce
    //! the SAME `session_key`; without the channel-id predicate the second
    //! channel would resolve the first channel's persisted cwd and resume into
    //! its working tree (silent corruption). These tests are RED before the
    //! `channel_id = $2` predicate was added and GREEN after.
    //!
    //! #3216 extends this with the safe legacy NULL-channel_id fallback (GAP 1)
    //! and the live-tmux recovery-cwd reconcile policy (GAP 2).
    use super::{restore_session_cwd_from_db, restore_thread_worktree_path_from_db};
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::discord::adk_session::build_namespaced_session_key;
    use crate::services::provider::ProviderKind;

    async fn seed_session(
        pool: &sqlx::PgPool,
        session_key: &str,
        channel_id: Option<&str>,
        cwd: &str,
    ) {
        sqlx::query(
            "INSERT INTO sessions (session_key, provider, status, cwd, channel_id, last_heartbeat)
             VALUES ($1, 'claude', 'idle', $2, $3, NOW())",
        )
        .bind(session_key)
        .bind(cwd)
        .bind(channel_id)
        .execute(pool)
        .await
        .expect("seed sessions row");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn name_collision_does_not_cross_channels() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let provider = ProviderKind::Claude;
        let token_hash = "tok-3207";
        // Two distinct channels that sanitize/truncate to the SAME channel name
        // therefore share one session_key. Channel A is the persisted owner.
        let collide_name = "shared-name";
        let tmux_name = provider.build_tmux_session_name(collide_name);
        let session_key = build_namespaced_session_key(token_hash, &provider, &tmux_name);
        let channel_a: u64 = 111_111_111_111_111_111;
        let channel_b: u64 = 222_222_222_222_222_222;
        let owner_cwd = "/home/u/.adk/release/worktrees/claude-shared-name-20260101-000000";

        seed_session(&pool, &session_key, Some(&channel_a.to_string()), owner_cwd).await;

        // Owner channel resolves its own persisted worktree.
        let owner = restore_thread_worktree_path_from_db(
            Some(&pool),
            token_hash,
            &provider,
            collide_name,
            channel_a,
        );
        assert_eq!(
            owner.as_deref(),
            Some(owner_cwd),
            "the owning channel must resolve its own persisted worktree"
        );

        // The colliding (different-id) channel must NOT resolve channel A's cwd.
        // This is the cross-channel corruption guard (RED before the P0 fix).
        let cross = restore_thread_worktree_path_from_db(
            Some(&pool),
            token_hash,
            &provider,
            collide_name,
            channel_b,
        );
        assert_eq!(
            cross, None,
            "a different channel sharing the same session_key must NOT resolve \
             another channel's persisted worktree"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    // #3216 GAP 1: a SINGLE legacy NULL-channel_id row IS now reused via the
    // safe fallback (it cannot be cross-channel because it is the only row for
    // that session_key). Previously (#3207) this returned None and forced a
    // worktree rotation that divorced the live session from its transcript.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn legacy_null_channel_id_single_row_is_reused() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let provider = ProviderKind::Claude;
        let token_hash = "tok-3216-legacy";
        let channel_name = "legacy-chan";
        let tmux_name = provider.build_tmux_session_name(channel_name);
        let session_key = build_namespaced_session_key(token_hash, &provider, &tmux_name);
        let channel_id: u64 = 333_333_333_333_333_333;
        let cwd = "/home/u/.adk/release/worktrees/claude-legacy-chan-20260101-000000";

        // A row written before the channel_id column existed has NULL channel_id.
        seed_session(&pool, &session_key, None, cwd).await;

        let resolved = restore_thread_worktree_path_from_db(
            Some(&pool),
            token_hash,
            &provider,
            channel_name,
            channel_id,
        );
        assert_eq!(
            resolved.as_deref(),
            Some(cwd),
            "a single legacy NULL-channel_id row must be reused via the #3216 \
             safe fallback so the legacy session keeps its transcript worktree"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    // #3216 GAP 1 guard: a SINGLE row that carries a DIFFERENT non-null
    // channel_id must NOT be reused by another channel (cross-channel hazard).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fallback_refused_when_single_row_has_different_channel() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let provider = ProviderKind::Claude;
        let token_hash = "tok-3216-wrongchan";
        let channel_name = "wrong-chan";
        let tmux_name = provider.build_tmux_session_name(channel_name);
        let session_key = build_namespaced_session_key(token_hash, &provider, &tmux_name);
        let owner_channel: u64 = 121_212_121_212_121_212;
        let requester_channel: u64 = 343_434_343_434_343_434;

        // Single row, but stamped with a different (non-null) channel.
        seed_session(
            &pool,
            &session_key,
            Some(&owner_channel.to_string()),
            "/home/u/.adk/release/worktrees/claude-wrong-chan-owner",
        )
        .await;

        let resolved = restore_thread_worktree_path_from_db(
            Some(&pool),
            token_hash,
            &provider,
            channel_name,
            requester_channel,
        );
        assert_eq!(
            resolved, None,
            "a single row with a different non-null channel_id must not be \
             reused by another channel"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    // ---- P0-a: `auto_restore_session_force` restart restore (db_cwd) ----
    //
    // `restore_session_cwd_from_db` backs the restart-restore `db_cwd` lookup
    // that installs into `session.current_path`. It must be channel-scoped for
    // the same reason as the thread worktree reuse: a colliding session_key
    // must not let one channel install another channel's persisted cwd.

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restart_restore_cwd_does_not_cross_channels() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let provider = ProviderKind::Claude;
        let token_hash = "tok-3207-restore";
        let collide_name = "shared-restore-name";
        let tmux_name = provider.build_tmux_session_name(collide_name);
        let session_key = build_namespaced_session_key(token_hash, &provider, &tmux_name);
        let channel_a: u64 = 444_444_444_444_444_444;
        let channel_b: u64 = 555_555_555_555_555_555;
        let owner_cwd = "/home/u/.adk/release/worktrees/claude-shared-restore-20260101-000000";

        seed_session(&pool, &session_key, Some(&channel_a.to_string()), owner_cwd).await;

        // Owner channel resolves its own persisted cwd for restart restore.
        let owner = restore_session_cwd_from_db(
            Some(&pool),
            token_hash,
            &provider,
            collide_name,
            channel_a,
        );
        assert_eq!(
            owner.as_deref(),
            Some(owner_cwd),
            "the owning channel must resolve its own restart-restore cwd"
        );

        // The colliding (different-id) channel must NOT install channel A's cwd
        // into its restored runtime (RED before the P0-a `channel_id = $2` fix).
        let cross = restore_session_cwd_from_db(
            Some(&pool),
            token_hash,
            &provider,
            collide_name,
            channel_b,
        );
        assert_eq!(
            cross, None,
            "a different channel sharing the same session_key must NOT resolve \
             another channel's restart-restore cwd"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    // #3216 GAP 1: restart-restore resolves a single legacy NULL row via the
    // safe fallback too (mirrors the worktree-reuse path).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restart_restore_cwd_legacy_null_single_row_is_reused() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let provider = ProviderKind::Claude;
        let token_hash = "tok-3216-restore-legacy";
        let channel_name = "legacy-restore-chan";
        let tmux_name = provider.build_tmux_session_name(channel_name);
        let session_key = build_namespaced_session_key(token_hash, &provider, &tmux_name);
        let channel_id: u64 = 666_666_666_666_666_666;
        let cwd = "/home/u/.adk/release/worktrees/claude-legacy-restore-20260101-000000";

        seed_session(&pool, &session_key, None, cwd).await;

        let resolved = restore_session_cwd_from_db(
            Some(&pool),
            token_hash,
            &provider,
            channel_name,
            channel_id,
        );
        assert_eq!(
            resolved.as_deref(),
            Some(cwd),
            "a single legacy NULL-channel_id row must be reused for restart \
             restore via the #3216 safe fallback"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    // #3216 GAP 2: reconcile policy is a pure decision function; verify it picks
    // the live tmux cwd over a divergent DB cwd only when the tmux cwd is a real
    // managed/usable worktree, and is a no-op otherwise.
    #[test]
    fn reconcile_prefers_live_tmux_over_divergent_db_cwd() {
        let db = "/home/u/.adk/release/worktrees/claude-chan-PHANTOM-212437";
        let live = "/home/u/.adk/release/worktrees/claude-chan-LIVE-113822";
        // Divergent DB cwd, live tmux is managed + usable → adopt live.
        assert_eq!(
            super::reconcile_recovery_cwd(Some(db), Some(live), true, true).as_deref(),
            Some(live),
            "a divergent live tmux managed/usable cwd must be adopted as authoritative"
        );
    }

    #[test]
    fn reconcile_noop_when_db_matches_tmux() {
        let same = "/home/u/.adk/release/worktrees/claude-chan-SAME";
        assert_eq!(
            super::reconcile_recovery_cwd(Some(same), Some(same), true, true),
            None,
            "no reconcile when DB cwd already equals the live tmux cwd"
        );
    }

    #[test]
    fn reconcile_refused_when_tmux_cwd_not_managed_or_unusable() {
        let db = "/home/u/.adk/release/worktrees/claude-chan-DB";
        let transient = "/tmp/some-transient-path";
        // Not managed → refuse even though it differs.
        assert_eq!(
            super::reconcile_recovery_cwd(Some(db), Some(transient), false, true),
            None,
            "an unmanaged tmux cwd must never override the DB cwd"
        );
        // Managed but not usable on disk → refuse.
        assert_eq!(
            super::reconcile_recovery_cwd(Some(db), Some(transient), true, false),
            None,
            "an unusable tmux cwd must never override the DB cwd"
        );
        // No live tmux pane → refuse.
        assert_eq!(
            super::reconcile_recovery_cwd(Some(db), None, false, false),
            None,
            "absent live tmux must keep existing behavior"
        );
    }

    // #3216 GAP 2: end-to-end DB correction — after reconcile, the DB row's cwd
    // is rewritten to the live tmux path and the channel_id is stamped (so a
    // legacy NULL row both adopts the right cwd and self-heals onto this channel).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconcile_corrects_db_cwd_and_stamps_channel() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let provider = ProviderKind::Claude;
        let token_hash = "tok-3216-reconcile";
        let channel_name = "reconcile-chan";
        let tmux_name = provider.build_tmux_session_name(channel_name);
        let session_key = build_namespaced_session_key(token_hash, &provider, &tmux_name);
        let channel_id: u64 = 999_999_999_999_999_999;
        let phantom = "/home/u/.adk/release/worktrees/claude-reconcile-PHANTOM";
        let live = "/home/u/.adk/release/worktrees/claude-reconcile-LIVE";

        // Legacy NULL-channel_id row pointing at the phantom (transcript-less) cwd.
        seed_session(&pool, &session_key, None, phantom).await;

        super::correct_session_cwd_to_tmux(
            Some(&pool),
            token_hash,
            &provider,
            channel_name,
            channel_id,
            live,
        );

        let (cwd, stamped): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT cwd, channel_id FROM sessions WHERE session_key = $1")
                .bind(&session_key)
                .fetch_one(&pool)
                .await
                .expect("fetch reconciled row");
        assert_eq!(
            cwd.as_deref(),
            Some(live),
            "DB cwd must be corrected to live tmux cwd"
        );
        assert_eq!(
            stamped.as_deref(),
            Some(channel_id.to_string().as_str()),
            "the reconciled row must be stamped with this channel_id (self-heal)"
        );

        pool.close().await;
        pg_db.drop().await;
    }
}
