use std::fs;
use std::path::Path;

use poise::serenity_prelude as serenity;

use super::super::formatting::send_long_message_ctx;
use super::super::runtime_store::{self, workspace_root};
use super::super::{
    Context, DiscordSession, Error, WorktreeInfo, auto_restore_session, check_auth,
    create_git_worktree, detect_worktree_conflict, resolve_channel_category,
    save_bot_settings, scan_skills,
};

/// Autocomplete handler for remote profile names in /start
pub(in crate::services::discord) async fn autocomplete_remote_profile<'a>(
    _ctx: Context<'a>,
    partial: &'a str,
) -> Vec<serenity::AutocompleteChoice> {
    let settings = crate::config::Settings::load();
    let partial_lower = partial.to_lowercase();
    let mut choices = Vec::new();
    if partial.is_empty() || "off".contains(&partial_lower) {
        choices.push(serenity::AutocompleteChoice::new(
            "off (local execution)",
            "off",
        ));
    }
    for p in &settings.remote_profiles {
        if partial.is_empty() || p.name.to_lowercase().contains(&partial_lower) {
            choices.push(serenity::AutocompleteChoice::new(
                format!("{} — {}@{}:{}", p.name, p.user, p.host, p.port),
                p.name.clone(),
            ));
        }
    }
    choices.into_iter().take(25).collect()
}

/// /start [path] [remote] — Start session at directory
#[poise::command(slash_command, rename = "start")]
pub(in crate::services::discord) async fn cmd_start(
    ctx: Context<'_>,
    #[description = "Directory path (empty for auto workspace)"] path: Option<String>,
    #[description = "Remote profile ('off' for local)"]
    #[autocomplete = "autocomplete_remote_profile"]
    remote: Option<String>,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!(
        "  [{ts}] ◀ [{user_name}] /start path={:?} remote={:?}",
        path, remote
    );

    let path_str = path.as_deref().unwrap_or("").trim();

    // remote_override: None=not specified, Some(None)="off", Some(Some(name))=profile
    let remote_override = match remote.as_deref() {
        None => None,
        Some("off") => Some(None),
        Some(name) => {
            let settings = crate::config::Settings::load();
            if settings.remote_profiles.iter().any(|p| p.name == name) {
                Some(Some(name.to_string()))
            } else {
                ctx.say(format!("Remote profile '{}' not found.", name))
                    .await?;
                return Ok(());
            }
        }
    };

    // Determine if session will be remote (for path validation logic)
    let will_be_remote = match &remote_override {
        Some(Some(_)) => true,
        Some(None) => false,
        None => {
            let data = ctx.data().shared.core.lock().await;
            data.sessions
                .get(&ctx.channel_id())
                .and_then(|s| s.remote_profile_name.as_ref())
                .is_some()
        }
    };

    let canonical_path = if path_str.is_empty() && will_be_remote {
        // Remote + no path: use profile's default_path or "~"
        if let Some(Some(ref name)) = remote_override {
            let settings = crate::config::Settings::load();
            settings
                .remote_profiles
                .iter()
                .find(|p| p.name == *name)
                .map(|p| {
                    if p.default_path.is_empty() {
                        "~".to_string()
                    } else {
                        p.default_path.clone()
                    }
                })
                .unwrap_or_else(|| "~".to_string())
        } else {
            "~".to_string()
        }
    } else if path_str.is_empty() {
        // Local + no path: create random workspace directory
        let Some(workspace_dir) = workspace_root() else {
            ctx.say("Error: cannot determine workspace root.").await?;
            return Ok(());
        };
        use rand::Rng;
        let random_name: String = rand::thread_rng()
            .sample_iter(&rand::distributions::Alphanumeric)
            .take(8)
            .map(|b| (b as char).to_ascii_lowercase())
            .collect();
        let new_dir = workspace_dir.join(&random_name);
        if let Err(e) = fs::create_dir_all(&new_dir) {
            ctx.say(format!("Error: failed to create workspace: {}", e))
                .await?;
            return Ok(());
        }
        new_dir.display().to_string()
    } else if will_be_remote {
        // Remote + path specified: expand tilde only, skip local validation
        if path_str.starts_with("~/") || path_str == "~" {
            // Keep tilde as-is for remote (remote shell will expand it)
            path_str.to_string()
        } else {
            path_str.to_string()
        }
    } else {
        // Local + path specified: expand ~ and validate locally
        let expanded = if path_str.starts_with("~/") || path_str == "~" {
            if let Some(home) = dirs::home_dir() {
                home.join(path_str.strip_prefix("~/").unwrap_or(""))
                    .display()
                    .to_string()
            } else {
                path_str.to_string()
            }
        } else {
            path_str.to_string()
        };
        let p = Path::new(&expanded);
        if !p.exists() || !p.is_dir() {
            ctx.say(format!("Error: '{}' is not a valid directory.", expanded))
                .await?;
            return Ok(());
        }
        p.canonicalize()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| expanded)
    };

    // Resolve channel/category names before taking the lock
    let (ch_name, cat_name) =
        resolve_channel_category(ctx.serenity_context(), ctx.channel_id()).await;

    // Check for worktree conflict (another channel using same git repo path)
    let worktree_info = {
        let data = ctx.data().shared.core.lock().await;
        let conflict = detect_worktree_conflict(&data.sessions, &canonical_path, ctx.channel_id());
        drop(data);
        if let Some(conflicting_channel) = conflict {
            let provider_str = {
                let settings = ctx.data().shared.settings.read().await;
                settings.provider.as_str().to_string()
            };
            let ch = ch_name.as_deref().unwrap_or("unknown");
            match create_git_worktree(&canonical_path, ch, &provider_str) {
                Ok((wt_path, branch)) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!(
                        "  [{ts}] 🌿 Worktree conflict: {} already uses {}. Created worktree.",
                        conflicting_channel, canonical_path
                    );
                    Some(WorktreeInfo {
                        original_path: canonical_path.clone(),
                        worktree_path: wt_path,
                        branch_name: branch,
                    })
                }
                Err(e) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!("  [{ts}] 🌿 Worktree creation skipped: {e}");
                    None
                }
            }
        } else {
            None
        }
    };

    // Use worktree path if created, otherwise original
    let effective_path = worktree_info
        .as_ref()
        .map(|wt| wt.worktree_path.clone())
        .unwrap_or_else(|| canonical_path.clone());

    // Session ID comes from DB (sessions.claude_session_id column),
    // not from ai_sessions JSON files.
    let mut response_lines = Vec::new();

    {
        let mut data = ctx.data().shared.core.lock().await;
        let channel_id = ctx.channel_id();

        let session = data
            .sessions
            .entry(channel_id)
            .or_insert_with(|| DiscordSession {
                session_id: None,
                current_path: None,
                history: Vec::new(),
                pending_uploads: Vec::new(),
                cleared: false,
                channel_name: None,
                category_name: None,
                remote_profile_name: None,
                channel_id: Some(channel_id.get()),

                last_active: tokio::time::Instant::now(),
                worktree: None,

                born_generation: runtime_store::load_generation(),
            });
        session.channel_id = Some(channel_id.get());
        session.channel_name = ch_name;
        session.category_name = cat_name;
        session.last_active = tokio::time::Instant::now();
        session.current_path = Some(effective_path.clone());

        // Apply remote override from /start parameter
        if let Some(ref new_remote) = remote_override {
            let old_remote = session.remote_profile_name.clone();
            session.remote_profile_name = new_remote.clone();
            if old_remote != *new_remote {
                session.session_id = None;
            }
        }

        // Apply worktree info if created
        session.worktree = worktree_info.clone();

        let has_existing_session = session.session_id.is_some();

        if has_existing_session {
            let ts = chrono::Local::now().format("%H:%M:%S");
            let remote_info = session
                .remote_profile_name
                .as_ref()
                .map(|n| format!(" (remote: {})", n))
                .unwrap_or_default();
            println!("  [{ts}] ▶ Session restored: {effective_path}{remote_info}");
            response_lines.push(format!(
                "Session restored at `{}`{}.",
                effective_path, remote_info
            ));
        } else {
            session.history.clear();

            let ts = chrono::Local::now().format("%H:%M:%S");
            let remote_info = session
                .remote_profile_name
                .as_ref()
                .map(|n| format!(" (remote: {})", n))
                .unwrap_or_default();
            println!("  [{ts}] ▶ Session started: {effective_path}{remote_info}");
            response_lines.push(format!(
                "Session started at `{}`{}.",
                effective_path, remote_info
            ));
        }

        // Notify about worktree if created
        if let Some(ref wt) = session.worktree {
            response_lines.push(format!(
                "🌿 Worktree: `{}` 가 이미 사용 중이라 분리된 worktree에서 작업합니다.",
                wt.original_path
            ));
            response_lines.push(format!("Branch: `{}`", wt.branch_name));
        }

        // Persist channel → path mapping for auto-restore
        let ch_key = channel_id.get().to_string();
        let current_remote_for_settings = match &remote_override {
            None => {
                // No explicit override — persist current session state
                data.sessions
                    .get(&channel_id)
                    .and_then(|s| s.remote_profile_name.clone())
            }
            _ => None,
        };
        drop(data);

        let mut settings = ctx.data().shared.settings.write().await;
        settings
            .last_sessions
            .insert(ch_key.clone(), canonical_path.clone());
        // Persist remote profile: store if active, remove if cleared
        match &remote_override {
            Some(Some(name)) => {
                settings.last_remotes.insert(ch_key, name.clone());
            }
            Some(None) => {
                settings.last_remotes.remove(&ch_key);
            }
            None => {
                if let Some(name) = current_remote_for_settings {
                    settings.last_remotes.insert(ch_key, name);
                }
            }
        }
        save_bot_settings(&ctx.data().token, &settings);
        drop(settings);

        // Rescan skills with project path to pick up project-level commands
        let new_skills = scan_skills(&ctx.data().provider, Some(&effective_path));
        *ctx.data().shared.skills_cache.write().await = new_skills;
    }

    let response_text = response_lines.join("\n");
    send_long_message_ctx(ctx, &response_text).await?;

    Ok(())
}

/// /pwd — Show current working directory
#[poise::command(slash_command, rename = "pwd")]
pub(in crate::services::discord) async fn cmd_pwd(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] ◀ [{user_name}] /pwd");

    // Auto-restore session
    auto_restore_session(&ctx.data().shared, ctx.channel_id(), ctx.serenity_context()).await;

    let (current_path, remote_name) = {
        let data = ctx.data().shared.core.lock().await;
        let session = data.sessions.get(&ctx.channel_id());
        (
            session.and_then(|s| s.current_path.clone()),
            session.and_then(|s| s.remote_profile_name.clone()),
        )
    };

    match current_path {
        Some(path) => {
            let remote_info = remote_name
                .map(|n| format!(" (remote: **{}**)", n))
                .unwrap_or_else(|| " (local)".to_string());
            ctx.say(format!("`{}`{}", path, remote_info)).await?
        }
        None => {
            ctx.say("No active session. Use `/start <path>` first.")
                .await?
        }
    };
    Ok(())
}
