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

    let canonical_path = if path_str.is_empty() {
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
    } else {
        // Local + path specified: expand ~ and validate locally
        let expanded = crate::runtime_layout::expand_user_path(path_str)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| path_str.to_string());
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
    let serenity_ctx = ctx.serenity_context();
    let (ch_name, cat_name) = resolve_channel_category(
        &serenity_ctx.http,
        Some(&serenity_ctx.cache),
        ctx.channel_id(),
    )
    .await;

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

                born_generation: runtime_store::process_generation(),
            });
        session.channel_id = Some(channel_id.get());
        session.channel_name = ch_name;
        session.category_name = cat_name;
        session.last_active = tokio::time::Instant::now();
        session.current_path = Some(effective_path.clone());
        session.remote_profile_name = None;

        // Apply worktree info if created
        session.worktree = worktree_info.clone();

        let has_existing_session = session.session_id.is_some();

        if has_existing_session {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ▶ Session restored: {effective_path}");
            response_lines.push(super::session_restored_response(&effective_path));
        } else {
            session.history.clear();

            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ▶ Session started: {effective_path}");
            response_lines.push(super::session_started_response(&effective_path));
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
        drop(data);

        // Remote SSH is disabled by policy (#2193). `/start` is always local and
        // clears any legacy remote profile key left by older builds.
        save_last_session_runtime(
            ctx.data().shared.pg_pool.as_ref(),
            &ctx.data().shared.token_hash,
            ch_key.parse::<u64>().unwrap_or_default(),
            &canonical_path,
        );

        // Rescan skills with project path to pick up project-level commands
        let new_skills = scan_skills(&ctx.data().provider, Some(&effective_path));
        *ctx.data().shared.skills_cache.write().await = new_skills;
    }

    let response_text = response_lines.join("\n");
    send_long_message_ctx(ctx, &response_text).await?;

    Ok(())
}

/// Rebind this channel to a previous provider session (empty = auto-select).
#[poise::command(slash_command, rename = "resume")]
pub(in crate::services::discord) async fn cmd_resume(
    ctx: Context<'_>,
    #[description = "Provider session id to resume (empty = auto-select previous)"]
    session_id: Option<String>,
    #[description = "Worktree path for the resumed session (defaults to current)"] cwd: Option<
        String,
    >,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }
    // Runtime-control tier — owner-only, same as /clear and /stop.
    if !super::enforce_slash_command_policy(&ctx, "/resume").await? {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] ◀ [{user_name}] /resume session_id={session_id:?} cwd={cwd:?}");

    let shared = &ctx.data().shared;
    let provider = &ctx.data().provider;
    let channel_id = ctx.channel_id();

    let Some(session_key) =
        super::super::adk_session::build_adk_session_key(shared, channel_id, provider, None).await
    else {
        ctx.say("이 채널의 session_key를 확인할 수 없어요. `/start`로 세션을 먼저 붙여주세요.")
            .await?;
        return Ok(());
    };

    let Some(pool) = shared.pg_pool.clone() else {
        ctx.say("Error: postgres pool unavailable.").await?;
        return Ok(());
    };
    let registry = shared.health_registry();

    let opts = crate::services::session_resume::ResumePreviousOptions {
        session_id: session_id.and_then(|s| {
            let trimmed = s.trim().to_string();
            (!trimmed.is_empty()).then_some(trimmed)
        }),
        cwd: cwd.and_then(|c| {
            let trimmed = c.trim().to_string();
            (!trimmed.is_empty()).then_some(trimmed)
        }),
    };

    // S1: route through the same forwarding-aware dispatch the HTTP endpoint
    // uses so a gateway node whose runtime does not own this channel delegates
    // to the owner node (mirrors `/stop`'s remote-cancel forwarding) instead of
    // mutating a session it does not run.
    let forward_context =
        crate::services::session_forwarding::ForwardCallerContext::from_live_globals(Some(
            pool.clone(),
        ));
    let (_status, body) = crate::services::session_resume::dispatch_resume_previous(
        &pool,
        registry.as_deref(),
        &forward_context,
        false,
        &session_key,
        &opts,
    )
    .await;

    if body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        let auto = body
            .get("auto_selected")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mode = if auto {
            "직전 세션 자동 선택"
        } else {
            "지정 세션"
        };
        let target_session_id = body
            .get("target_session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let target_cwd = body
            .get("target_cwd")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let mut lines = vec![
            format!("↻ 세션 재바인딩 완료 ({mode})"),
            format!("• session_id: `{target_session_id}`"),
            format!("• cwd: `{target_cwd}`"),
        ];
        if let Some(prev) = body.get("previous_session_id").and_then(|v| v.as_str()) {
            lines.push(format!("• 이전 session_id: `{prev}`"));
        }
        lines.push("다음 메시지부터 이 세션의 맥락으로 이어집니다.".to_string());
        send_long_message_ctx(ctx, &lines.join("\n")).await?;
        tracing::info!(
            "  [{ts}] ▶ [{user_name}] /resume rebound → {target_session_id} @ {target_cwd}"
        );
    } else {
        let message = body
            .get("error")
            .and_then(|value| value.as_str())
            .unwrap_or("세션 재바인딩에 실패했어요.")
            .to_string();
        ctx.say(format!("⚠ /resume 실패: {message}")).await?;
    }

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
