use crate::services::provider::ProviderKind;

use super::super::{Context, Error, check_auth};
use super::config::{effective_provider_for_channel, fallback_channel_name_for_feature_toggle};

#[derive(Debug, Clone, Copy, poise::ChoiceParameter)]
enum EffortLevel {
    #[name = "low"]
    Low,
    #[name = "medium"]
    Medium,
    #[name = "high"]
    High,
    #[name = "xhigh"]
    Xhigh,
    #[name = "max"]
    Max,
    #[name = "ultracode"]
    Ultracode,
}

impl EffortLevel {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
            Self::Ultracode => "ultracode",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ClaudeSlashPassthrough {
    Effort(EffortLevel),
    Compact,
    Cost,
    Context,
}

impl ClaudeSlashPassthrough {
    const fn slash_name(self) -> &'static str {
        match self {
            Self::Effort(_) => "/effort",
            Self::Compact => "/compact",
            Self::Cost => "/cost",
            Self::Context => "/context",
        }
    }

    fn prompt(self) -> String {
        match self {
            Self::Effort(level) => format!("/effort {}", level.as_str()),
            Self::Compact => "/compact".to_string(),
            Self::Cost => "/cost".to_string(),
            Self::Context => "/context".to_string(),
        }
    }
}

fn ultracode_notice() -> &'static str {
    "`/effort ultracode`는 live Claude 세션에 안전하게 passthrough하지 않습니다. \
현재 범위에서는 세션 재시작/별도 설정 연동 없이 보장되는 경로만 열었고, 안정 경로는 \
`/effort max`까지입니다."
}

fn codex_effort_notice() -> &'static str {
    "`/effort`는 Claude live TUI passthrough로만 연결됩니다. Codex의 reasoning effort는 \
wrapper/env 시작 옵션 경로는 있지만 AgentDesk에 채널 단위 설정면이 아직 없어 여기서는 \
즉시 토글하지 않습니다."
}

fn unsupported_notice(provider: &ProviderKind, command: ClaudeSlashPassthrough) -> String {
    if matches!(command, ClaudeSlashPassthrough::Effort(_))
        && matches!(provider, ProviderKind::Codex)
    {
        return codex_effort_notice().to_string();
    }
    format!(
        "{} is only available for live Claude TUI channels. Current provider: {}.",
        command.slash_name(),
        provider.display_name(),
    )
}

fn live_session_required_notice(command: ClaudeSlashPassthrough) -> String {
    format!(
        "{} needs a live Claude tmux session for this channel. Start or resume the Claude session first.",
        command.slash_name(),
    )
}

fn provider_preflight_notice(
    provider: &ProviderKind,
    command: ClaudeSlashPassthrough,
) -> Option<String> {
    if !matches!(provider, ProviderKind::Claude) {
        return Some(unsupported_notice(provider, command));
    }
    if let ClaudeSlashPassthrough::Effort(EffortLevel::Ultracode) = command {
        return Some(ultracode_notice().to_string());
    }
    None
}

async fn resolve_effective_provider_and_tmux_name(
    ctx: Context<'_>,
) -> (ProviderKind, Option<String>) {
    let channel_id = ctx.channel_id();
    let channel_name_hint = fallback_channel_name_for_feature_toggle(ctx, channel_id).await;
    let effective_provider = effective_provider_for_channel(
        &ctx.data().shared,
        channel_id,
        &ctx.data().provider,
        channel_name_hint.as_deref(),
    )
    .await;
    let session_channel_name = {
        let data = ctx.data().shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.clone())
    };
    let tmux_name = session_channel_name
        .as_deref()
        .or(channel_name_hint.as_deref())
        .map(|channel_name| effective_provider.build_tmux_session_name(channel_name));
    (effective_provider, tmux_name)
}

async fn run_claude_passthrough(
    ctx: Context<'_>,
    command: ClaudeSlashPassthrough,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] ◀ [{user_name}] {}", command.prompt());

    let (effective_provider, tmux_name) = resolve_effective_provider_and_tmux_name(ctx).await;
    if let Some(notice) = provider_preflight_notice(&effective_provider, command) {
        ctx.say(notice).await?;
        return Ok(());
    }

    let Some(tmux_name) = tmux_name else {
        ctx.say(live_session_required_notice(command)).await?;
        return Ok(());
    };
    if !crate::services::tmux_diagnostics::tmux_session_has_live_pane(&tmux_name) {
        ctx.say(live_session_required_notice(command)).await?;
        return Ok(());
    }

    ctx.defer().await?;

    let prompt = command.prompt();
    let tmux_name_for_send = tmux_name.clone();
    let send_result = tokio::task::spawn_blocking(move || {
        crate::services::claude_tui::input::send_followup_prompt(&tmux_name_for_send, &prompt, None)
    })
    .await
    .unwrap_or_else(|error| Err(format!("join error: {error}")));

    match send_result {
        Ok(()) => {
            ctx.say(format!(
                "{}를 live Claude session `{}`에 전달했습니다. Claude 응답은 채널에 이어서 올라옵니다.",
                command.slash_name(),
                tmux_name,
            ))
            .await?;
        }
        Err(error) if crate::services::claude_tui::input::is_prompt_ready_timeout_error(&error) => {
            ctx.say(format!(
                "{} 전달 대기 중 timeout이 났습니다. Claude turn이 아직 바쁘거나 prompt ready 상태가 아닙니다.",
                command.slash_name(),
            ))
            .await?;
        }
        Err(error)
            if crate::services::claude_tui::input::is_prompt_ready_cancelled_error(&error) =>
        {
            ctx.say(format!(
                "{} 전달이 취소됐습니다. 다른 stop/restart/reset이 먼저 들어온 상태입니다.",
                command.slash_name(),
            ))
            .await?;
        }
        Err(error) => {
            ctx.say(format!(
                "{} passthrough failed for `{}`: {}",
                command.slash_name(),
                tmux_name,
                error,
            ))
            .await?;
        }
    }

    Ok(())
}

/// /effort <level> — pass through Claude native effort control to the live TUI.
#[poise::command(slash_command, rename = "effort")]
pub(in crate::services::discord) async fn cmd_effort(
    ctx: Context<'_>,
    #[description = "Level: low / medium / high / xhigh / max / ultracode"] level: EffortLevel,
) -> Result<(), Error> {
    run_claude_passthrough(ctx, ClaudeSlashPassthrough::Effort(level)).await
}

/// /compact — pass through Claude native /compact to the live TUI.
#[poise::command(slash_command, rename = "compact")]
pub(in crate::services::discord) async fn cmd_compact(ctx: Context<'_>) -> Result<(), Error> {
    run_claude_passthrough(ctx, ClaudeSlashPassthrough::Compact).await
}

/// /cost — pass through Claude native /cost to the live TUI.
#[poise::command(slash_command, rename = "cost")]
pub(in crate::services::discord) async fn cmd_cost(ctx: Context<'_>) -> Result<(), Error> {
    run_claude_passthrough(ctx, ClaudeSlashPassthrough::Cost).await
}

/// /context — pass through Claude native /context to the live TUI.
#[poise::command(slash_command, rename = "context")]
pub(in crate::services::discord) async fn cmd_context(ctx: Context<'_>) -> Result<(), Error> {
    run_claude_passthrough(ctx, ClaudeSlashPassthrough::Context).await
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::{
        ClaudeSlashPassthrough, EffortLevel, codex_effort_notice, live_session_required_notice,
        provider_preflight_notice, ultracode_notice, unsupported_notice,
    };
    use crate::services::provider::ProviderKind;

    #[test]
    fn effort_levels_render_expected_prompt() {
        assert_eq!(
            ClaudeSlashPassthrough::Effort(EffortLevel::Low).prompt(),
            "/effort low"
        );
        assert_eq!(
            ClaudeSlashPassthrough::Effort(EffortLevel::Xhigh).prompt(),
            "/effort xhigh"
        );
        assert_eq!(
            ClaudeSlashPassthrough::Effort(EffortLevel::Ultracode).prompt(),
            "/effort ultracode"
        );
    }

    #[test]
    fn unsupported_effort_is_codex_specific() {
        let notice = unsupported_notice(
            &ProviderKind::Codex,
            ClaudeSlashPassthrough::Effort(EffortLevel::High),
        );
        assert!(notice.contains("Codex"));
        assert!(notice.contains("채널 단위 설정면"));
        assert_eq!(notice, codex_effort_notice());
    }

    #[test]
    fn codex_ultracode_is_rejected_before_claude_ultracode_notice() {
        let notice = provider_preflight_notice(
            &ProviderKind::Codex,
            ClaudeSlashPassthrough::Effort(EffortLevel::Ultracode),
        )
        .expect("Codex /effort ultracode must be rejected as unsupported");
        assert_eq!(notice, codex_effort_notice());
        assert!(!notice.contains("안전하게 passthrough하지 않습니다"));
    }

    #[test]
    fn claude_ultracode_uses_claude_specific_notice() {
        let notice = provider_preflight_notice(
            &ProviderKind::Claude,
            ClaudeSlashPassthrough::Effort(EffortLevel::Ultracode),
        )
        .expect("Claude /effort ultracode must use the Claude-specific guardrail");
        assert_eq!(notice, ultracode_notice());
    }

    #[test]
    fn generic_unsupported_notice_mentions_provider_and_command() {
        let notice = unsupported_notice(&ProviderKind::Gemini, ClaudeSlashPassthrough::Compact);
        assert!(notice.contains("/compact"));
        assert!(notice.contains("Gemini"));
    }

    #[test]
    fn ultracode_notice_mentions_safe_limit() {
        let notice = ultracode_notice();
        assert!(notice.contains("ultracode"));
        assert!(notice.contains("/effort max"));
    }

    #[test]
    fn live_session_notice_mentions_live_tmux_requirement() {
        let notice = live_session_required_notice(ClaudeSlashPassthrough::Context);
        assert!(notice.contains("/context"));
        assert!(notice.contains("live Claude tmux session"));
    }
}
