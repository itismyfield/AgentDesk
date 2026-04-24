use crate::services::provider::ProviderKind;

use super::super::turn_bridge::cancel_active_token;
use super::super::{
    Context, Error, SharedData, check_auth, mailbox_cancel_active_turn, mailbox_has_active_turn,
};

const RESTART_SEED_PROMPT: &str = "안녕하세요. 세션 재시작 완료.";
const MCP_RELOAD_DEPRECATION_NOTICE: &str =
    "`/mcp-reload`는 deprecated alias입니다. `/restart`로 이름이 변경됐습니다.";

/// Outcome of resolving the reload action against the current session state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RestartAction {
    /// No active session — the seed turn will create one when a workspace is resolvable.
    NoActiveSession,
    /// Active session, no captured provider session_id (fresh session, hasn't completed
    /// its first turn yet). Killing tmux is safe but resume isn't possible.
    ActiveSessionWithoutSessionId,
    /// Active session with a captured provider session_id. Providers with resume support
    /// receive it on the immediate seed turn.
    ActiveSessionResumable { session_id: String },
}

/// Inspect the channel's current session state and decide what kind of reload action
/// applies. Pure with respect to the captured snapshot, so it can be unit-tested by
/// passing the (`session_present`, `session_id`) tuple directly via the helper below.
pub(super) async fn resolve_restart_action(
    shared: &SharedData,
    channel_id: poise::serenity_prelude::ChannelId,
) -> RestartAction {
    let snapshot = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .map(|session| session.session_id.clone())
    };
    classify_restart_action(snapshot)
}

/// Pure classifier — exposed for tests.
pub(super) fn classify_restart_action(session_snapshot: Option<Option<String>>) -> RestartAction {
    match session_snapshot {
        None => RestartAction::NoActiveSession,
        Some(None) => RestartAction::ActiveSessionWithoutSessionId,
        Some(Some(sid)) if sid.trim().is_empty() => RestartAction::ActiveSessionWithoutSessionId,
        Some(Some(sid)) => RestartAction::ActiveSessionResumable { session_id: sid },
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RestartSeedStatus {
    Started,
    Busy,
    Failed(String),
}

fn provider_supports_resume(provider: &ProviderKind) -> bool {
    provider
        .capabilities()
        .is_some_and(|capabilities| capabilities.supports_resume)
}

fn restart_resume_note(provider: &ProviderKind, action: &RestartAction) -> String {
    match action {
        RestartAction::ActiveSessionResumable { session_id }
            if provider_supports_resume(provider) =>
        {
            format!("대화 컨텍스트는 session_id `{session_id}`에서 `--resume`으로 이어집니다.")
        }
        RestartAction::ActiveSessionResumable { .. } => {
            "이 provider는 resume을 지원하지 않아 신규 세션으로 시작합니다.".to_string()
        }
        RestartAction::ActiveSessionWithoutSessionId => {
            "기존 session_id가 없어 신규 세션으로 시작합니다.".to_string()
        }
        RestartAction::NoActiveSession => {
            "활성 세션이 없어 가능한 경우 새 세션으로 시작합니다.".to_string()
        }
    }
}

fn restart_tmux_note(tmux_name: Option<&str>) -> String {
    tmux_name
        .map(|name| format!("tmux reset 요청됨 (`{name}`)"))
        .unwrap_or_else(|| "관리 중인 tmux 세션 없음 — 새 세션 시작 시 자동 생성".to_string())
}

fn restart_seed_note(status: &RestartSeedStatus) -> String {
    match status {
        RestartSeedStatus::Started => {
            "새 provider 세션을 즉시 기동했고, 짧은 인사말을 전달합니다.".to_string()
        }
        RestartSeedStatus::Busy => {
            "기존 턴 정리가 아직 끝나지 않아 인사말 turn은 시작하지 못했습니다.".to_string()
        }
        RestartSeedStatus::Failed(error) => {
            format!("인사말 turn 시작 실패: {error}")
        }
    }
}

fn build_restart_response(
    provider: &ProviderKind,
    action: &RestartAction,
    tmux_name: Option<&str>,
    seed_status: &RestartSeedStatus,
) -> String {
    format!(
        "♻ 세션 재시작됨 (provider={}). 현재 인증된 MCP/설정이 부착됐습니다.\n{}\n{}\n{}",
        provider.as_str(),
        restart_tmux_note(tmux_name),
        restart_resume_note(provider, action),
        restart_seed_note(seed_status)
    )
}

async fn start_restart_seed_turn(ctx: &Context<'_>) -> RestartSeedStatus {
    const MAX_ATTEMPTS: usize = 30;
    const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

    let channel_id = ctx.channel_id();
    let channel_name_hint = {
        let data = ctx.data().shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.clone())
    };
    let metadata = serde_json::json!({
        "command": "/restart",
        "provider": ctx.data().provider.as_str(),
    });

    for attempt in 0..MAX_ATTEMPTS {
        match super::super::router::start_headless_turn(
            ctx.serenity_context(),
            channel_id,
            RESTART_SEED_PROMPT,
            "/restart",
            &ctx.data().shared,
            &ctx.data().token,
            Some("/restart"),
            Some(metadata.clone()),
            channel_name_hint.clone(),
        )
        .await
        {
            Ok(_) => return RestartSeedStatus::Started,
            Err(super::super::router::HeadlessTurnStartError::Conflict(_))
                if attempt + 1 < MAX_ATTEMPTS =>
            {
                tokio::time::sleep(RETRY_DELAY).await;
            }
            Err(super::super::router::HeadlessTurnStartError::Conflict(_)) => {
                return RestartSeedStatus::Busy;
            }
            Err(super::super::router::HeadlessTurnStartError::Internal(error)) => {
                return RestartSeedStatus::Failed(error);
            }
        }
    }

    RestartSeedStatus::Busy
}

async fn run_restart(ctx: Context<'_>, command_name: &'static str) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    if command_name == "/mcp-reload" {
        ctx.say(MCP_RELOAD_DEPRECATION_NOTICE).await?;
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] ◀ [{user_name}] {command_name}");
    if command_name == "/restart" {
        ctx.say("♻ 세션 재시작 중...").await?;
    }

    let channel_id = ctx.channel_id();
    let action = resolve_restart_action(&ctx.data().shared, channel_id).await;
    let preserve_provider_session = provider_supports_resume(&ctx.data().provider);

    // Warn if a turn is in flight, then cancel it via the same path /stop uses.
    let in_flight = mailbox_has_active_turn(&ctx.data().shared, channel_id).await;
    if in_flight {
        ctx.say("⚠ 진행 중 턴 1회 손실 가능 — 안전하게 중단합니다.")
            .await?;

        let cancel = mailbox_cancel_active_turn(&ctx.data().shared, channel_id).await;
        if let Some(token) = cancel.token {
            cancel_active_token(
                &token,
                super::super::turn_bridge::TmuxCleanupPolicy::PreserveSession,
                command_name,
            );
        }
    }

    // Kill the managed tmux/process session without clearing session_id when the
    // provider can resume. The seed turn below immediately respawns the provider.
    let http = ctx.serenity_context().http.clone();
    let tmux_name = super::control::reset_channel_provider_state(
        &http,
        &ctx.data().shared,
        &ctx.data().provider,
        channel_id,
        command_name,
        !preserve_provider_session,
        false, // do NOT clear history
        true,  // recreate (kill) the tmux session so the seed turn fully respawns provider
    )
    .await;

    let seed_status = start_restart_seed_turn(&ctx).await;
    ctx.say(build_restart_response(
        &ctx.data().provider,
        &action,
        tmux_name.as_deref(),
        &seed_status,
    ))
    .await?;
    tracing::info!(
        "  [{ts}] ▶ [{user_name}] {command_name} triggered (provider={}, seed_status={seed_status:?})",
        ctx.data().provider.as_str()
    );
    Ok(())
}

/// Restart the current provider session immediately.
#[poise::command(slash_command, rename = "restart")]
pub(in crate::services::discord) async fn cmd_restart(ctx: Context<'_>) -> Result<(), Error> {
    run_restart(ctx, "/restart").await
}

/// Deprecated alias for `/restart`.
#[poise::command(slash_command, rename = "mcp-reload")]
pub(in crate::services::discord) async fn cmd_mcp_reload(ctx: Context<'_>) -> Result<(), Error> {
    run_restart(ctx, "/mcp-reload").await
}

#[cfg(test)]
mod tests {
    use super::{
        RestartAction, RestartSeedStatus, build_restart_response, classify_restart_action,
        provider_supports_resume,
    };
    use crate::services::provider::ProviderKind;

    #[test]
    fn restart_preserves_resume_for_all_supported_providers() {
        for provider in [
            ProviderKind::Claude,
            ProviderKind::Codex,
            ProviderKind::Qwen,
            ProviderKind::Gemini,
        ] {
            assert!(provider_supports_resume(&provider));
        }
        assert!(!provider_supports_resume(&ProviderKind::Unsupported(
            "foo".to_string()
        )));
    }

    #[test]
    fn classify_returns_no_active_session_when_session_missing() {
        assert_eq!(
            classify_restart_action(None),
            RestartAction::NoActiveSession
        );
    }

    #[test]
    fn classify_returns_without_session_id_when_session_present_but_id_missing() {
        assert_eq!(
            classify_restart_action(Some(None)),
            RestartAction::ActiveSessionWithoutSessionId
        );
        assert_eq!(
            classify_restart_action(Some(Some("   ".to_string()))),
            RestartAction::ActiveSessionWithoutSessionId
        );
    }

    #[test]
    fn classify_returns_resumable_when_session_id_present() {
        let sid = "abc-123-uuid".to_string();
        assert_eq!(
            classify_restart_action(Some(Some(sid.clone()))),
            RestartAction::ActiveSessionResumable { session_id: sid }
        );
    }

    #[test]
    fn restart_response_covers_each_provider_with_seed_started() {
        for provider in [
            ProviderKind::Claude,
            ProviderKind::Codex,
            ProviderKind::Qwen,
            ProviderKind::Gemini,
        ] {
            let response = build_restart_response(
                &provider,
                &RestartAction::ActiveSessionResumable {
                    session_id: "sid-1".to_string(),
                },
                Some("AgentDesk-test"),
                &RestartSeedStatus::Started,
            );
            assert!(response.contains(provider.as_str()));
            assert!(response.contains("`--resume`"));
            assert!(response.contains("즉시 기동"));
        }
    }

    #[test]
    fn restart_response_marks_new_session_when_resume_is_unavailable() {
        let response = build_restart_response(
            &ProviderKind::Unsupported("foo".to_string()),
            &RestartAction::ActiveSessionResumable {
                session_id: "sid-1".to_string(),
            },
            None,
            &RestartSeedStatus::Busy,
        );
        assert!(response.contains("신규 세션"));
        assert!(response.contains("관리 중인 tmux 세션 없음"));
        assert!(response.contains("시작하지 못했습니다"));
    }
}
