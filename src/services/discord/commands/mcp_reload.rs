use crate::services::provider::ProviderKind;

use super::super::turn_bridge::cancel_active_token;
use super::super::{
    Context, Error, SharedData, check_auth, mailbox_cancel_active_turn, mailbox_has_active_turn,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum McpReloadGate {
    /// Provider is Claude — proceed.
    Allow,
    /// Provider is not Claude — reject with a friendly message.
    UnsupportedProvider,
}

/// Decide whether the `/mcp-reload` slash command should proceed for the given provider.
/// Only Claude is supported because Codex/Qwen/Gemini have different MCP attachment
/// semantics (or don't support hot-swap via session restart at all).
pub(super) fn mcp_reload_gate_for_provider(provider: &ProviderKind) -> McpReloadGate {
    match provider {
        ProviderKind::Claude => McpReloadGate::Allow,
        _ => McpReloadGate::UnsupportedProvider,
    }
}

/// Outcome of resolving the reload action against the current session state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum McpReloadAction {
    /// No active session — next message will naturally use the freshly-attached MCP set.
    NoActiveSession,
    /// Active session, no captured Claude session_id (fresh session, hasn't completed
    /// its first turn yet). Killing tmux is safe but resume isn't possible.
    ActiveSessionWithoutSessionId,
    /// Active session with a captured Claude session_id. Next message will respawn
    /// Claude with `--resume <session_id>` and the new MCP set.
    ActiveSessionResumable { session_id: String },
}

/// Inspect the channel's current session state and decide what kind of reload action
/// applies. Pure with respect to the captured snapshot, so it can be unit-tested by
/// passing the (`session_present`, `session_id`) tuple directly via the helper below.
pub(super) async fn resolve_mcp_reload_action(
    shared: &SharedData,
    channel_id: poise::serenity_prelude::ChannelId,
) -> McpReloadAction {
    let snapshot = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .map(|session| session.session_id.clone())
    };
    classify_mcp_reload_action(snapshot)
}

/// Pure classifier — exposed for tests.
pub(super) fn classify_mcp_reload_action(
    session_snapshot: Option<Option<String>>,
) -> McpReloadAction {
    match session_snapshot {
        None => McpReloadAction::NoActiveSession,
        Some(None) => McpReloadAction::ActiveSessionWithoutSessionId,
        Some(Some(sid)) if sid.trim().is_empty() => McpReloadAction::ActiveSessionWithoutSessionId,
        Some(Some(sid)) => McpReloadAction::ActiveSessionResumable { session_id: sid },
    }
}

/// Reload Claude MCP servers (kills tmux, next msg respawns with --resume).
#[poise::command(slash_command, rename = "mcp-reload")]
pub(in crate::services::discord) async fn cmd_mcp_reload(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] ◀ [{user_name}] /mcp-reload");

    // Provider gate — only Claude is supported.
    if matches!(
        mcp_reload_gate_for_provider(&ctx.data().provider),
        McpReloadGate::UnsupportedProvider
    ) {
        ctx.say(
            "지원되지 않는 provider입니다 (Claude only). \
             Codex/Qwen/Gemini는 추후 별도 지원 예정.",
        )
        .await?;
        return Ok(());
    }

    let channel_id = ctx.channel_id();
    let action = resolve_mcp_reload_action(&ctx.data().shared, channel_id).await;

    match action {
        McpReloadAction::NoActiveSession => {
            ctx.say("활성 세션 없음. 다음 메시지부터 새 MCP가 자동 적용됩니다.")
                .await?;
            return Ok(());
        }
        McpReloadAction::ActiveSessionWithoutSessionId
        | McpReloadAction::ActiveSessionResumable { .. } => {}
    }

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
                "/mcp-reload",
            );
        }
    }

    // Kill the tmux session WITHOUT clearing session_id. The next inbound message
    // will spawn a fresh Claude process via `claude --resume <sid>`, attaching the
    // currently-authenticated MCP servers.
    let http = ctx.serenity_context().http.clone();
    let tmux_recreated = super::control::reset_channel_provider_state(
        &http,
        &ctx.data().shared,
        &ctx.data().provider,
        channel_id,
        "/mcp-reload",
        false, // do NOT reset provider state — we want to keep session_id for --resume
        false, // do NOT clear history
        true,  // recreate (kill) the tmux session so next message fully respawns Claude
    )
    .await;

    let session_id_disp = match action {
        McpReloadAction::ActiveSessionResumable { ref session_id } => session_id.clone(),
        _ => String::from("(없음 — 신규 세션으로 시작)"),
    };

    let tmux_note = if tmux_recreated.is_some() {
        "tmux 세션 종료됨"
    } else {
        "tmux 세션 없음 — 다음 메시지부터 새 세션 생성"
    };

    ctx.say(format!(
        "🔄 MCP reload 트리거됨 ({tmux_note}).\n\
         다음 메시지에서 현재 인증된 모든 MCP 서버가 부착된 \
         새 Claude 프로세스가 자동 spawn됩니다.\n\
         대화 컨텍스트는 session_id `{session_id_disp}`에서 이어집니다."
    ))
    .await?;
    tracing::info!("  [{ts}] ▶ [{user_name}] /mcp-reload triggered (sid={session_id_disp})");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        McpReloadAction, McpReloadGate, classify_mcp_reload_action, mcp_reload_gate_for_provider,
    };
    use crate::services::provider::ProviderKind;

    #[test]
    fn provider_gate_allows_claude_only() {
        assert_eq!(
            mcp_reload_gate_for_provider(&ProviderKind::Claude),
            McpReloadGate::Allow
        );
        assert_eq!(
            mcp_reload_gate_for_provider(&ProviderKind::Codex),
            McpReloadGate::UnsupportedProvider
        );
        assert_eq!(
            mcp_reload_gate_for_provider(&ProviderKind::Qwen),
            McpReloadGate::UnsupportedProvider
        );
        assert_eq!(
            mcp_reload_gate_for_provider(&ProviderKind::Gemini),
            McpReloadGate::UnsupportedProvider
        );
        assert_eq!(
            mcp_reload_gate_for_provider(&ProviderKind::Unsupported("foo".to_string())),
            McpReloadGate::UnsupportedProvider
        );
    }

    #[test]
    fn classify_returns_no_active_session_when_session_missing() {
        assert_eq!(
            classify_mcp_reload_action(None),
            McpReloadAction::NoActiveSession
        );
    }

    #[test]
    fn classify_returns_without_session_id_when_session_present_but_id_missing() {
        assert_eq!(
            classify_mcp_reload_action(Some(None)),
            McpReloadAction::ActiveSessionWithoutSessionId
        );
        assert_eq!(
            classify_mcp_reload_action(Some(Some("   ".to_string()))),
            McpReloadAction::ActiveSessionWithoutSessionId
        );
    }

    #[test]
    fn classify_returns_resumable_when_session_id_present() {
        let sid = "abc-123-uuid".to_string();
        assert_eq!(
            classify_mcp_reload_action(Some(Some(sid.clone()))),
            McpReloadAction::ActiveSessionResumable { session_id: sid }
        );
    }
}
