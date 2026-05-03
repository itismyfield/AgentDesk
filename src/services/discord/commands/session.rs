use std::fs;
use std::path::Path;

use super::super::formatting::send_long_message_ctx;
use super::super::runtime_store::{self, workspace_root};
use super::super::settings::save_last_session_runtime;
use super::super::{
    Context, DiscordSession, Error, WorktreeInfo, auto_restore_session, check_auth,
    create_git_worktree, detect_worktree_conflict, resolve_channel_category, scan_skills,
};

/// /start [path] — Start session at directory
#[poise::command(slash_command, rename = "start")]
pub(in crate::services::discord) async fn cmd_start(
    ctx: Context<'_>,
    #[description = "Directory path (empty for auto workspace)"] path: Option<String>,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] ◀ [{user_name}] /start path={:?}", path);

    let path_str = path.as_deref().unwrap_or("").trim();

    // Existing remote session state still influences path validation; /start no longer exposes
    // a user-facing remote selector.
    let will_be_remote = {
        let data = ctx.data().shared.core.lock().await;
        data.sessions
            .get(&ctx.channel_id())
            .and_then(|s| s.remote_profile_name.as_ref())
            .is_some()
    };

    let canonical_path = if path_str.is_empty() && will_be_remote {
        // Remote + no path: keep remote-shell default expansion behavior.
        "~".to_string()
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
                    tracing::info!(
                        "  [{ts}] 🌿 Worktree conflict: {} already uses {}. Created worktree.",
                        conflicting_channel,
                        canonical_path
                    );
                    Some(WorktreeInfo {
                        original_path: canonical_path.clone(),
                        worktree_path: wt_path,
                        branch_name: branch,
                    })
                }
                Err(e) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!("  [{ts}] 🌿 Worktree creation skipped: {e}");
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
                memento_context_loaded: false,
                memento_reflected: false,
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
                assistant_turns: 0,
            });
        session.channel_id = Some(channel_id.get());
        session.channel_name = ch_name;
        session.category_name = cat_name;
        session.last_active = tokio::time::Instant::now();
        session.current_path = Some(effective_path.clone());

        // Apply worktree info if created
        session.worktree = worktree_info.clone();

        let has_existing_session = session.session_id.is_some();

        if has_existing_session {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ▶ Session restored: {effective_path}");
            response_lines.push(format!("Session restored at `{}`.", effective_path));
        } else {
            session.history.clear();

            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ▶ Session started: {effective_path}");
            response_lines.push(format!("Session started at `{}`.", effective_path));
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
        let current_remote_for_settings = data
            .sessions
            .get(&channel_id)
            .and_then(|s| s.remote_profile_name.clone());
        drop(data);

        save_last_session_runtime(
            None::<&crate::db::Db>,
            ctx.data().shared.pg_pool.as_ref(),
            &ctx.data().shared.token_hash,
            ch_key.parse::<u64>().unwrap_or_default(),
            &canonical_path,
            current_remote_for_settings.as_deref(),
        );

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
    tracing::info!("  [{ts}] ◀ [{user_name}] /pwd");

    // Auto-restore session
    auto_restore_session(&ctx.data().shared, ctx.channel_id(), ctx.serenity_context()).await;

    let current_path = {
        let data = ctx.data().shared.core.lock().await;
        let session = data.sessions.get(&ctx.channel_id());
        session.and_then(|s| s.current_path.clone())
    };

    match current_path {
        Some(path) => ctx.say(format!("`{}`", path)).await?,
        None => {
            ctx.say("No active session. Use `/start <path>` first.")
                .await?
        }
    };
    Ok(())
}
