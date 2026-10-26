mod autocomplete;
use autocomplete::autocomplete_session;
#[cfg(all(test, unix))]
mod resume_key_tests;

use super::super::formatting::send_long_message_ctx;
use super::super::{Context, Error, auto_restore_session, check_auth};

/// The key needs the channel's name: from its live session or its agentdesk.yaml binding.
const RESUME_SESSION_KEY_UNRESOLVED: &str = "이 채널의 session_key를 만들 수 없어요. agentdesk.yaml 채널 바인딩에 이 채널의 이름이나 별칭이 있는지 확인하거나, 일반 메시지를 한 번 보내 세션을 연 뒤 다시 시도해 주세요.";

/// Rebind this channel to a previous provider session (empty = auto-select).
#[poise::command(slash_command, rename = "resume")]
pub(in crate::services::discord) async fn cmd_resume(
    ctx: Context<'_>,
    #[description = "Provider session id to resume (empty = auto-select previous)"]
    #[autocomplete = "autocomplete_session"]
    session_id: Option<String>,
    #[description = "Worktree path (empty = locate the selected session transcript)"] cwd: Option<
        String,
    >,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }
    // Mutating tier like /clear and /stop: any authorized user passes this gate.
    if !super::enforce_slash_command_policy(&ctx, "/resume").await? {
        return Ok(());
    }

    log_command_received!(
        ctx.channel_id().get(),
        user_name,
        "/resume",
        provider_session_id = ?session_id,
        cwd = ?cwd
    );
    // A transition may legitimately wait for an intake mailbox claim. Acknowledge
    // the interaction before any database, forwarding, or bounded-lock work.
    ctx.defer().await?;

    let shared = &ctx.data().shared;
    let provider = &ctx.data().provider;
    let channel_id = ctx.channel_id();

    let Some(session_key) =
        super::super::adk_session::build_adk_session_key(shared, channel_id, provider, None).await
    else {
        ctx.say(RESUME_SESSION_KEY_UNRESOLVED).await?;
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
        None,
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
        log_info_event!(
            "discord_session_rebound",
            channel_id = channel_id.get(),
            provider = provider.as_str(),
            session_key = %session_key,
            provider_session_id = target_session_id,
            path = target_cwd,
            user_name = %user_name,
            status = "completed",
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

    log_command_received!(ctx.channel_id().get(), user_name, "/pwd");

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
            ctx.say("No active session. Set a workspace in agentdesk.yaml.")
                .await?
        }
    };
    Ok(())
}
