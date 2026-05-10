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
        let sqlite_settings_db = if shared.pg_pool.is_some() {
            None
        } else {
            None::<&crate::db::Db>
        };
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
            sqlite_settings_db,
            shared.pg_pool.as_ref(),
            &shared.token_hash,
            channel_id.get(),
        );

        // Use the effective tmux channel name here so restart recovery keeps
        // looking up the same session key for thread sessions that intentionally
        // use a synthetic "{parent}-t{thread_id}" channel name.
        let db_cwd: Option<String> = restore_ch_name.as_ref().and_then(|ch| {
            let tmux_name = provider.build_tmux_session_name(ch);
            let session_keys =
                build_session_key_candidates(&shared.token_hash, &provider, &tmux_name);
            let saved_remote_for_pg = saved_remote.clone();
            if let Some(pg_pool) = shared.pg_pool.as_ref() {
                return crate::utils::async_bridge::block_on_pg_result(
                    pg_pool,
                    move |pool| async move {
                        for session_key in session_keys {
                            let path = sqlx::query_scalar::<_, String>(
                                "SELECT cwd FROM sessions WHERE session_key = $1 LIMIT 1",
                            )
                            .bind(&session_key)
                            .fetch_optional(&pool)
                            .await
                            .map_err(|error| format!("load session cwd {session_key}: {error}"))?;
                            if let Some(path) = path.filter(|p| {
                                !p.is_empty()
                                    && session_path_is_usable(p, saved_remote_for_pg.as_deref())
                            }) {
                                return Ok(Some(path));
                            }
                        }
                        Ok(None)
                    },
                    |message| message,
                )
                .ok()
                .flatten();
            }

            #[cfg(all(test, feature = "legacy-sqlite-tests"))]
            {
                None::<&crate::db::Db>.and_then(|db| {
                    db.lock().ok().and_then(|conn| {
                        session_keys.iter().find_map(|session_key| {
                            conn.query_row(
                                "SELECT cwd FROM sessions WHERE session_key = ?1",
                                [session_key],
                                |row| row.get::<_, String>(0),
                            )
                            .ok()
                            .filter(|p| {
                                !p.is_empty() && session_path_is_usable(p, saved_remote.as_deref())
                            })
                        })
                    })
                })
            }
            #[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
            {
                None
            }
        });
        let persisted_path = load_last_session_path(
            sqlite_settings_db,
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
    // Always create a worktree for thread sessions to isolate concurrent work.
    let effective_path = {
        let ch = session.channel_name.as_deref().unwrap_or("unknown");
        let provider_str = shared.settings.read().await.provider.as_str().to_string();
        match create_git_worktree(parent_path, ch, &provider_str) {
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use std::path::Path;

    fn run_git(repo_dir: &Path, args: &[&str]) -> String {
        let output = GitCommand::new()
            .args(args)
            .repo(repo_dir)
            .run_output()
            .unwrap_or_else(|e| panic!("git {:?} failed to spawn: {}", args, e));
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn branch_exists(repo_dir: &Path, branch: &str) -> bool {
        GitCommand::new()
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ])
            .repo(repo_dir)
            .run_output()
            .is_ok()
    }

    fn setup_git_repo_with_origin() -> (tempfile::TempDir, tempfile::TempDir) {
        let origin = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();

        run_git(origin.path(), &["init", "--bare"]);
        run_git(repo.path(), &["init", "-b", "main"]);
        run_git(repo.path(), &["config", "user.email", "test@test.com"]);
        run_git(repo.path(), &["config", "user.name", "Test"]);
        run_git(
            repo.path(),
            &["remote", "add", "origin", origin.path().to_str().unwrap()],
        );
        run_git(repo.path(), &["commit", "--allow-empty", "-m", "initial"]);
        run_git(repo.path(), &["push", "-u", "origin", "main"]);

        (repo, origin)
    }

    #[test]
    fn synthetic_thread_channel_name_round_trips() {
        let channel_id = ChannelId::new(12345);
        let synthetic = synthetic_thread_channel_name("agentdesk-codex", channel_id);

        assert_eq!(synthetic, "agentdesk-codex-t12345");
        assert!(is_synthetic_thread_channel_name(&synthetic, channel_id));
        assert!(!is_synthetic_thread_channel_name(
            "agentdesk-codex",
            channel_id
        ));
    }

    #[test]
    fn choose_restore_channel_name_prefers_existing_synthetic_thread_name() {
        let channel_id = ChannelId::new(12345);
        let chosen = choose_restore_channel_name(
            Some("agentdesk-codex-t12345"),
            Some("새 스레드 제목"),
            Some((ChannelId::new(777), Some("agentdesk-codex".to_string()))),
            channel_id,
        );

        assert_eq!(chosen.as_deref(), Some("agentdesk-codex-t12345"));
    }

    #[test]
    fn resolve_is_dm_channel_prefers_gateway_hint() {
        assert!(resolve_is_dm_channel(Some(true), false));
        assert!(!resolve_is_dm_channel(Some(false), true));
    }

    #[test]
    fn resolve_is_dm_channel_uses_lookup_when_hint_missing() {
        assert!(resolve_is_dm_channel(None, true));
        assert!(!resolve_is_dm_channel(None, false));
    }

    #[test]
    fn assistant_turn_count_only_counts_assistant_messages() {
        let session = DiscordSession {
            session_id: None,
            memento_context_loaded: false,
            memento_reflected: false,
            current_path: None,
            history: vec![
                HistoryItem {
                    item_type: HistoryType::User,
                    content: "user".to_string(),
                },
                HistoryItem {
                    item_type: HistoryType::Assistant,
                    content: "assistant-1".to_string(),
                },
                HistoryItem {
                    item_type: HistoryType::ToolUse,
                    content: "tool".to_string(),
                },
                HistoryItem {
                    item_type: HistoryType::Assistant,
                    content: "assistant-2".to_string(),
                },
            ],
            pending_uploads: Vec::new(),
            cleared: false,
            remote_profile_name: None,
            channel_id: None,
            channel_name: None,
            category_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: 0,
            assistant_turns: 0,
        };

        assert_eq!(session.assistant_turn_count(), 2);
    }

    #[test]
    fn recent_history_context_returns_latest_user_and_assistant_messages() {
        let session = DiscordSession {
            session_id: None,
            memento_context_loaded: false,
            memento_reflected: false,
            current_path: None,
            history: vec![
                HistoryItem {
                    item_type: HistoryType::User,
                    content: "첫 질문".to_string(),
                },
                HistoryItem {
                    item_type: HistoryType::Assistant,
                    content: "첫 답변".to_string(),
                },
                HistoryItem {
                    item_type: HistoryType::User,
                    content: "둘째 질문".to_string(),
                },
                HistoryItem {
                    item_type: HistoryType::Assistant,
                    content: "둘째 답변".to_string(),
                },
            ],
            pending_uploads: Vec::new(),
            cleared: false,
            remote_profile_name: None,
            channel_id: None,
            channel_name: None,
            category_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: 0,
            assistant_turns: 0,
        };

        assert_eq!(
            session.recent_history_context(3).as_deref(),
            Some("Assistant: 첫 답변\nUser: 둘째 질문\nAssistant: 둘째 답변")
        );
    }

    #[test]
    fn choose_restore_channel_name_builds_synthetic_name_for_threads() {
        let channel_id = ChannelId::new(12345);
        let chosen = choose_restore_channel_name(
            None,
            Some("새 스레드 제목"),
            Some((ChannelId::new(777), Some("agentdesk-codex".to_string()))),
            channel_id,
        );

        assert_eq!(chosen.as_deref(), Some("agentdesk-codex-t12345"));
    }

    #[test]
    fn choose_restore_channel_name_keeps_existing_name_when_live_metadata_missing() {
        let channel_id = ChannelId::new(12345);
        let chosen = choose_restore_channel_name(Some("agentdesk-codex"), None, None, channel_id);

        assert_eq!(chosen.as_deref(), Some("agentdesk-codex"));
    }

    #[test]
    fn allows_nonlocal_session_path_requires_remote_profile_name() {
        assert!(allows_nonlocal_session_path(Some("mac-mini")));
        assert!(!allows_nonlocal_session_path(Some("")));
        assert!(!allows_nonlocal_session_path(None));
    }

    #[test]
    fn session_path_is_usable_for_remote_nonlocal_path() {
        assert!(session_path_is_usable("~/repo", Some("mac-mini")));
    }

    #[test]
    fn select_restored_session_path_prefers_configured_workspace() {
        let selected = select_restored_session_path(
            Some("/new/workspace".to_string()),
            Some("/old/workspace".to_string()),
            Some("/yaml/workspace".to_string()),
            Some("remote"),
        );

        assert_eq!(selected.as_deref(), Some("/new/workspace"));
    }

    #[test]
    fn select_restored_session_path_falls_back_when_configured_missing() {
        let selected = select_restored_session_path(
            None,
            Some("/db/workspace".to_string()),
            Some("/yaml/workspace".to_string()),
            Some("remote"),
        );

        assert_eq!(selected.as_deref(), Some("/db/workspace"));
    }

    #[test]
    fn cleanup_git_worktree_preserves_branch_until_origin_main_contains_commit() {
        let (repo, _origin) = setup_git_repo_with_origin();
        let repo_dir = repo.path();
        let worktree_dir = repo_dir.join("wt-543");
        let branch = "wt/fix-543";

        run_git(
            repo_dir,
            &[
                "worktree",
                "add",
                "-b",
                branch,
                worktree_dir.to_str().unwrap(),
            ],
        );
        run_git(
            &worktree_dir,
            &[
                "commit",
                "--allow-empty",
                "-m",
                "fix: preserve worktree (#543)",
            ],
        );

        // Simulate local main advancing before auto-merge pushes to origin.
        run_git(repo_dir, &["merge", "--ff-only", branch]);

        cleanup_git_worktree(
            None,
            None,
            &WorktreeInfo {
                original_path: repo_dir.to_string_lossy().to_string(),
                worktree_path: worktree_dir.to_string_lossy().to_string(),
                branch_name: branch.to_string(),
            },
        );

        assert!(worktree_dir.exists(), "worktree should be preserved");
        assert!(
            branch_exists(repo_dir, branch),
            "branch should stay until origin/main contains it"
        );
    }

    #[test]
    fn create_git_worktree_starts_from_origin_main_even_when_local_main_is_ahead() {
        let (repo, _origin) = setup_git_repo_with_origin();
        let repo_dir = repo.path();

        let origin_head_before = run_git(repo_dir, &["rev-parse", "origin/main"]);
        run_git(
            repo_dir,
            &["commit", "--allow-empty", "-m", "local-only commit"],
        );
        let local_head_after = run_git(repo_dir, &["rev-parse", "HEAD"]);
        assert_ne!(
            origin_head_before, local_head_after,
            "test setup requires local main to be ahead of origin/main"
        );

        let (worktree_path, branch_name) =
            create_git_worktree(repo_dir.to_str().unwrap(), "slot-reset", "claude").unwrap();
        let worktree_head = run_git(Path::new(&worktree_path), &["rev-parse", "HEAD"]);
        let worktree_branch = run_git(Path::new(&worktree_path), &["branch", "--show-current"]);

        assert_eq!(
            worktree_head, origin_head_before,
            "fresh worktree must start from origin/main rather than local main HEAD"
        );
        assert_eq!(worktree_branch, branch_name);

        cleanup_git_worktree(
            None,
            None,
            &WorktreeInfo {
                original_path: repo_dir.to_string_lossy().to_string(),
                worktree_path,
                branch_name,
            },
        );
    }

    #[test]
    fn cleanup_git_worktree_removes_branch_once_origin_main_contains_commit() {
        let (repo, _origin) = setup_git_repo_with_origin();
        let repo_dir = repo.path();
        let worktree_dir = repo_dir.join("wt-merged");
        let branch = "wt/fix-merged";

        run_git(
            repo_dir,
            &[
                "worktree",
                "add",
                "-b",
                branch,
                worktree_dir.to_str().unwrap(),
            ],
        );
        run_git(
            &worktree_dir,
            &[
                "commit",
                "--allow-empty",
                "-m",
                "fix: merged worktree (#543)",
            ],
        );
        run_git(repo_dir, &["merge", "--ff-only", branch]);
        run_git(repo_dir, &["push", "origin", "main"]);

        cleanup_git_worktree(
            None,
            None,
            &WorktreeInfo {
                original_path: repo_dir.to_string_lossy().to_string(),
                worktree_path: worktree_dir.to_string_lossy().to_string(),
                branch_name: branch.to_string(),
            },
        );

        assert!(
            !worktree_dir.exists(),
            "merged worktree should be cleaned up"
        );
        assert!(
            !branch_exists(repo_dir, branch),
            "merged branch should be deleted after cleanup"
        );
    }

    #[test]
    fn cleanup_git_worktree_removes_squash_merged_branch() {
        let (repo, _origin) = setup_git_repo_with_origin();
        let repo_dir = repo.path();
        let worktree_dir = repo_dir.join("wt-squash-merged");
        let branch = "wt/fix-squash-merged";
        let notes = worktree_dir.join("notes.txt");

        run_git(
            repo_dir,
            &[
                "worktree",
                "add",
                "-b",
                branch,
                worktree_dir.to_str().unwrap(),
            ],
        );
        std::fs::write(&notes, "one\n").unwrap();
        run_git(&worktree_dir, &["add", "notes.txt"]);
        run_git(&worktree_dir, &["commit", "-m", "feat: add first note"]);
        std::fs::write(&notes, "one\ntwo\n").unwrap();
        run_git(&worktree_dir, &["add", "notes.txt"]);
        run_git(&worktree_dir, &["commit", "-m", "feat: add second note"]);

        run_git(repo_dir, &["merge", "--squash", branch]);
        run_git(
            repo_dir,
            &["commit", "-m", "feat: squash merged worktree (#543)"],
        );
        run_git(repo_dir, &["push", "origin", "main"]);

        cleanup_git_worktree(
            None,
            None,
            &WorktreeInfo {
                original_path: repo_dir.to_string_lossy().to_string(),
                worktree_path: worktree_dir.to_string_lossy().to_string(),
                branch_name: branch.to_string(),
            },
        );

        assert!(
            !worktree_dir.exists(),
            "squash-merged worktree should be cleaned up"
        );
        assert!(
            !branch_exists(repo_dir, branch),
            "squash-merged branch should be deleted after cleanup"
        );
    }

    #[test]
    fn cleanup_git_worktree_preserves_dirty_worktree() {
        let (repo, _origin) = setup_git_repo_with_origin();
        let repo_dir = repo.path();
        let worktree_dir = repo_dir.join("wt-dirty");
        let branch = "wt/fix-dirty";

        run_git(
            repo_dir,
            &[
                "worktree",
                "add",
                "-b",
                branch,
                worktree_dir.to_str().unwrap(),
            ],
        );
        std::fs::write(worktree_dir.join("dirty.txt"), "keep me\n").unwrap();

        cleanup_git_worktree(
            None,
            None,
            &WorktreeInfo {
                original_path: repo_dir.to_string_lossy().to_string(),
                worktree_path: worktree_dir.to_string_lossy().to_string(),
                branch_name: branch.to_string(),
            },
        );

        assert!(worktree_dir.exists(), "dirty worktree should be preserved");
        assert!(
            branch_exists(repo_dir, branch),
            "dirty branch should be preserved"
        );
    }

    #[test]
    fn cleanup_git_worktree_disconnects_sessions_referencing_removed_path() {
        let db = crate::db::test_db();
        let (repo, _origin) = setup_git_repo_with_origin();
        let repo_dir = repo.path();
        let worktree_dir = repo_dir.join("wt-session-cleanup");
        let branch = "wt/fix-session-cleanup";
        let worktree_path = worktree_dir.to_string_lossy().to_string();

        run_git(
            repo_dir,
            &[
                "worktree",
                "add",
                "-b",
                branch,
                worktree_dir.to_str().unwrap(),
            ],
        );
        run_git(
            &worktree_dir,
            &[
                "commit",
                "--allow-empty",
                "-m",
                "fix: merged worktree for session cleanup (#543)",
            ],
        );
        run_git(repo_dir, &["merge", "--ff-only", branch]);
        run_git(repo_dir, &["push", "origin", "main"]);

        db.lock()
            .unwrap()
            .execute(
                "INSERT INTO sessions
                 (session_key, status, cwd, active_dispatch_id, claude_session_id, created_at)
                 VALUES (?1, 'idle', ?2, 'dispatch-1', 'sid-1', datetime('now'))",
                ["host:worktree-cleanup-session", worktree_path.as_str()],
            )
            .unwrap();

        cleanup_git_worktree(
            Some(&db),
            None,
            &WorktreeInfo {
                original_path: repo_dir.to_string_lossy().to_string(),
                worktree_path: worktree_dir.to_string_lossy().to_string(),
                branch_name: branch.to_string(),
            },
        );

        let session_row: (Option<String>, String, Option<String>, Option<String>) = db
            .lock()
            .unwrap()
            .query_row(
                "SELECT cwd, status, active_dispatch_id, claude_session_id
                 FROM sessions
                 WHERE session_key = ?1",
                ["host:worktree-cleanup-session"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(session_row.0, None);
        assert_eq!(session_row.1, "disconnected");
        assert_eq!(session_row.2, None);
        assert_eq!(session_row.3, None);
    }

    #[test]
    fn cleanup_git_worktree_preserves_when_git_inspection_fails() {
        let (repo, _origin) = setup_git_repo_with_origin();
        let repo_dir = repo.path();
        let worktree_dir = repo_dir.join("wt-inspect-fail");
        let branch = "wt/fix-inspect-fail";

        run_git(
            repo_dir,
            &[
                "worktree",
                "add",
                "-b",
                branch,
                worktree_dir.to_str().unwrap(),
            ],
        );

        cleanup_git_worktree(
            None,
            None,
            &WorktreeInfo {
                original_path: repo_dir
                    .join("missing-parent")
                    .to_string_lossy()
                    .to_string(),
                worktree_path: worktree_dir.to_string_lossy().to_string(),
                branch_name: branch.to_string(),
            },
        );

        assert!(
            worktree_dir.exists(),
            "worktree should remain when cleanup cannot verify merge state"
        );
        assert!(
            branch_exists(repo_dir, branch),
            "branch should remain when cleanup cannot verify merge state"
        );
    }

    #[test]
    fn restored_memento_context_loaded_preserves_loaded_state_only_when_already_loaded() {
        assert!(!super::restored_memento_context_loaded(
            false,
            None,
            Some("session-1")
        ));
        assert!(super::restored_memento_context_loaded(
            true,
            Some("session-1"),
            Some("session-1")
        ));
        assert!(!super::restored_memento_context_loaded(
            true,
            Some("session-1"),
            Some("session-2")
        ));
        assert!(!super::restored_memento_context_loaded(
            true,
            Some("session-1"),
            None
        ));
    }

    #[test]
    fn restore_provider_session_keeps_unloaded_memento_state_until_context_reloads() {
        let mut session = DiscordSession {
            session_id: None,
            memento_context_loaded: false,
            memento_reflected: true,
            current_path: None,
            history: Vec::new(),
            pending_uploads: Vec::new(),
            cleared: false,
            remote_profile_name: None,
            channel_id: None,
            channel_name: None,
            category_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: 0,
            assistant_turns: 0,
        };

        session.restore_provider_session(Some("session-1".to_string()));

        assert_eq!(session.session_id.as_deref(), Some("session-1"));
        assert!(!session.memento_context_loaded);
        assert!(!session.memento_reflected);
    }

    #[test]
    fn restore_provider_session_clears_loaded_state_when_session_id_changes() {
        let mut session = DiscordSession {
            session_id: Some("session-1".to_string()),
            memento_context_loaded: true,
            memento_reflected: true,
            current_path: None,
            history: Vec::new(),
            pending_uploads: Vec::new(),
            cleared: false,
            remote_profile_name: None,
            channel_id: None,
            channel_name: None,
            category_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: 0,
            assistant_turns: 0,
        };

        session.restore_provider_session(Some("session-2".to_string()));

        assert_eq!(session.session_id.as_deref(), Some("session-2"));
        assert!(!session.memento_context_loaded);
        assert!(!session.memento_reflected);
    }

    #[test]
    fn sync_inflight_worktree_context_persists_bootstrap_worktree_metadata() {
        // Serialize against any other test that also mutates AGENTDESK_ROOT_DIR,
        // so the env var stays consistent across the save/load round-trip when
        // cargo test schedules tests on multiple threads in CI.
        let _guard = super::runtime_store::lock_test_env();

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("agentdesk-root");
        std::fs::create_dir_all(root.join("runtime").join("state").join("discord")).unwrap();

        struct EnvReset(Option<std::ffi::OsString>);
        impl Drop for EnvReset {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                    None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
                }
            }
        }

        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", &root) };
        let _reset = EnvReset(previous_root);

        let provider = crate::services::provider::ProviderKind::Codex;
        let inflight = crate::services::discord::inflight::InflightTurnState::new(
            provider.clone(),
            4242,
            Some("adk-cdx-t4242".to_string()),
            1,
            2,
            3,
            "resume".to_string(),
            Some("session-4242".to_string()),
            Some("AgentDesk-codex-adk-cdx-t4242".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );
        crate::services::discord::inflight::save_inflight_state(&inflight).unwrap();

        sync_inflight_worktree_context(
            &provider,
            4242,
            Some("/tmp/new-worktree".to_string()),
            Some("agentdesk/codex/adk-cdx-t4242".to_string()),
            Some("abc123".to_string()),
        );

        let saved = crate::services::discord::inflight::load_inflight_state(&provider, 4242)
            .expect("bootstrap should update existing inflight state");
        assert_eq!(saved.worktree_path.as_deref(), Some("/tmp/new-worktree"));
        assert_eq!(
            saved.worktree_branch.as_deref(),
            Some("agentdesk/codex/adk-cdx-t4242")
        );
        assert_eq!(saved.base_commit.as_deref(), Some("abc123"));
    }
}
