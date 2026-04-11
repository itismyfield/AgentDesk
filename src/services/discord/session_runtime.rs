use super::runtime_store::worktrees_root;
use super::*;

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
        self.session_id = session_id;
        self.memento_context_loaded = self.session_id.is_some();
        self.memento_reflected = false;
    }

    pub(super) fn note_memento_context_loaded(&mut self) {
        self.memento_context_loaded = true;
        self.memento_reflected = false;
    }

    /// Validate `current_path` and return it if it exists on disk.
    /// If the path is stale (deleted), clear `current_path` and `worktree`, log, and return `None`.
    pub(super) fn validated_path(&mut self, channel_id: impl std::fmt::Display) -> Option<String> {
        let current_path = self.current_path.as_ref()?;
        if session_path_is_usable(current_path, self.remote_profile_name.as_deref()) {
            return Some(current_path.clone());
        }
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!(
            "  [{ts}] ⚠ Ignoring stale local session path for channel {}: {}",
            channel_id, current_path
        );
        self.current_path = None;
        self.worktree = None;
        None
    }
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
    let git_check = std::process::Command::new("git")
        .args(["-C", repo_path, "rev-parse", "--is-inside-work-tree"])
        .output()
        .map_err(|e| format!("git check failed: {}", e))?;
    if !git_check.status.success() {
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

    let output = std::process::Command::new("git")
        .args(["-C", repo_path, "worktree", "add", &wt_path, "-b", &branch])
        .output()
        .map_err(|e| format!("git worktree add failed: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git worktree add failed: {}", stderr));
    }

    let ts_log = chrono::Local::now().format("%H:%M:%S");
    println!(
        "  [{ts_log}] 🌿 Created worktree: {} (branch: {})",
        wt_path, branch
    );
    Ok((wt_path, branch))
}

/// Clean up a git worktree after session ends.
pub(super) fn cleanup_git_worktree(wt_info: &WorktreeInfo) {
    let ts = chrono::Local::now().format("%H:%M:%S");

    let status = std::process::Command::new("git")
        .args(["-C", &wt_info.worktree_path, "status", "--porcelain"])
        .output();
    let has_changes = match &status {
        Ok(out) => !out.stdout.is_empty(),
        Err(_) => false,
    };

    // Check if branch has new commits
    let diff = std::process::Command::new("git")
        .args([
            "-C",
            &wt_info.original_path,
            "log",
            "--oneline",
            &format!("HEAD..{}", wt_info.branch_name),
        ])
        .output();
    let has_commits = match &diff {
        Ok(out) => !out.stdout.is_empty(),
        Err(_) => false,
    };

    if has_changes || has_commits {
        println!(
            "  [{ts}] 🌿 Worktree {} has changes/commits — keeping for manual merge",
            wt_info.worktree_path
        );
        println!(
            "  [{ts}] 🌿 Branch: {} | Original: {}",
            wt_info.branch_name, wt_info.original_path
        );
    } else {
        let _ = std::process::Command::new("git")
            .args([
                "-C",
                &wt_info.original_path,
                "worktree",
                "remove",
                &wt_info.worktree_path,
            ])
            .output();
        let _ = std::process::Command::new("git")
            .args([
                "-C",
                &wt_info.original_path,
                "branch",
                "-d",
                &wt_info.branch_name,
            ])
            .output();
        let _ = std::fs::remove_dir_all(&wt_info.worktree_path);
        println!("  [{ts}] 🧹 Cleaned up worktree: {}", wt_info.worktree_path);
    }
}

/// Auto-restore session from bot_settings.json if not in memory
pub(super) async fn auto_restore_session(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    serenity_ctx: &serenity::prelude::Context,
) {
    if matches!(
        resolve_runtime_channel_binding_status(&serenity_ctx.http, channel_id).await,
        RuntimeChannelBindingStatus::Unowned
    ) {
        return;
    }

    // Resolve channel/category before taking the lock for mutation
    let (live_ch_name, cat_name) = resolve_channel_category(serenity_ctx, channel_id).await;
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

    // Read settings first to get provider and runtime restore metadata.
    let (last_path, saved_remote, provider) = {
        let settings = shared.settings.read().await;
        let provider = settings.provider.clone();
        let configured_path = settings::resolve_workspace(channel_id, restore_ch_name.as_deref());
        let saved_remote =
            load_last_remote_profile(shared.db.as_ref(), &shared.token_hash, channel_id.get());

        // Use the effective tmux channel name here so restart recovery keeps
        // looking up the same session key for thread sessions that intentionally
        // use a synthetic "{parent}-t{thread_id}" channel name.
        let db_cwd: Option<String> = restore_ch_name.as_ref().and_then(|ch| {
            let tmux_name = provider.build_tmux_session_name(ch);
            let session_keys =
                build_session_key_candidates(&shared.token_hash, &provider, &tmux_name);
            shared.db.as_ref().and_then(|db| {
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
        });
        let persisted_path =
            load_last_session_path(shared.db.as_ref(), &shared.token_hash, channel_id.get());

        if let (Some(configured), Some(restored)) = (configured_path.as_ref(), db_cwd.as_ref())
            && configured != restored
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ⚠ Ignoring restored DB cwd for channel {}: {} (configured workspace: {})",
                channel_id, restored, configured
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
        println!("  [{ts}] ↻ Auto-restored session: {last_path}{remote_info}");
    }
}

/// Create a lightweight session for a thread, bootstrapped from the parent channel's path.
/// The session's `channel_name` uses `{parent_channel}-t{thread_id}` so the derived
/// tmux session name stays short and unique instead of using the full thread title.
pub(super) async fn bootstrap_thread_session(
    shared: &Arc<SharedData>,
    thread_channel_id: ChannelId,
    parent_path: &str,
    serenity_ctx: &serenity::prelude::Context,
) {
    let (thread_title, cat_name) = resolve_channel_category(serenity_ctx, thread_channel_id).await;
    // Build a short, stable channel_name: "{parent_channel}-t{thread_id}"
    let parent_info = resolve_thread_parent(&serenity_ctx.http, thread_channel_id).await;
    let ch_name = if let Some((parent_id, parent_name)) = parent_info {
        let parent = parent_name.unwrap_or_else(|| format!("{parent_id}"));
        Some(synthetic_thread_channel_name(&parent, thread_channel_id))
    } else {
        // Not a thread (shouldn't happen here) — fall back to resolved name
        thread_title
    };
    let mut data = shared.core.lock().await;
    if data.sessions.contains_key(&thread_channel_id) {
        return;
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
        });
    // Always create a worktree for thread sessions to isolate concurrent work.
    let effective_path = {
        let ch = session.channel_name.as_deref().unwrap_or("unknown");
        let provider_str = shared.settings.read().await.provider.as_str().to_string();
        match create_git_worktree(parent_path, ch, &provider_str) {
            Ok((wt_path, branch)) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!(
                    "  [{ts}] 🌿 Thread worktree created: {} (branch: {})",
                    wt_path, branch
                );
                session.worktree = Some(WorktreeInfo {
                    original_path: parent_path.to_string(),
                    worktree_path: wt_path.clone(),
                    branch_name: branch,
                });
                wt_path
            }
            Err(e) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                eprintln!(
                    "  [{ts}] ⚠ Thread worktree creation failed: {e}, falling back to parent path"
                );
                parent_path.to_string()
            }
        }
    };
    session.current_path = Some(effective_path.clone());
    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] ↻ Bootstrapped thread session: {effective_path}");
}

/// Resolve the channel name and parent category name for a Discord channel.
pub(super) async fn resolve_channel_category(
    ctx: &serenity::prelude::Context,
    channel_id: serenity::model::id::ChannelId,
) -> (Option<String>, Option<String>) {
    let Ok(channel) = channel_id.to_channel(&ctx.http).await else {
        return (None, None);
    };
    let serenity::model::channel::Channel::Guild(gc) = channel else {
        return (None, None);
    };
    let ch_name = Some(gc.name.clone());
    let cat_name = if let Some(parent_id) = gc.parent_id {
        let cached_cat_name = ctx.cache.guild(gc.guild_id).and_then(|guild| {
            guild
                .channels
                .get(&parent_id)
                .map(|parent_ch| parent_ch.name.clone())
        });

        if let Some(cat_name) = cached_cat_name {
            Some(cat_name)
        } else if let Ok(parent_ch) = parent_id.to_channel(&ctx.http).await {
            match parent_ch {
                serenity::model::channel::Channel::Guild(cat) => Some(cat.name.clone()),
                _ => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!(
                        "  [{ts}] ⚠ Category channel {parent_id} is not a Guild channel for #{}",
                        gc.name
                    );
                    None
                }
            }
        } else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ⚠ Failed to resolve category {parent_id} for #{}",
                gc.name
            );
            None
        }
    } else {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!("  [{ts}] ⚠ No parent_id for #{}", gc.name);
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
    let (channel_name, _) = resolve_channel_category(ctx, channel_id).await;
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
mod tests {
    use super::*;

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
}
