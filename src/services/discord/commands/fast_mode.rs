use crate::services::provider::ProviderKind;

use super::super::{Context, Error, check_auth};
use super::config::{channel_fast_mode_enabled, update_channel_fast_mode};

fn fast_mode_supported(provider: &ProviderKind) -> bool {
    matches!(provider, ProviderKind::Claude | ProviderKind::Codex)
}

fn fast_mode_reset_line(reset_pending: bool) -> &'static str {
    if reset_pending {
        "다음 사용자 턴 시작 전에 기존 세션을 정리한 뒤 반영됩니다."
    } else {
        "현재 세션부터 반영됩니다."
    }
}

fn build_fast_enabled_notice(provider: &ProviderKind, reset_pending: bool) -> String {
    format!(
        "`/fast` 적용 완료: {} 채널의 native fast mode를 켰습니다.\n속도 우선 모드를 강제 적용합니다. provider 정책상 응답은 더 빨라질 수 있지만 사용량/비용은 더 크게 집계될 수 있습니다.\n{}",
        provider.display_name(),
        fast_mode_reset_line(reset_pending),
    )
}

fn build_fast_disabled_notice(provider: &ProviderKind, reset_pending: bool) -> String {
    format!(
        "`/fast` 해제 완료: {} 채널의 native fast mode를 껐습니다.\n다음 턴부터 기본 응답 모드로 되돌립니다.\n{}",
        provider.display_name(),
        fast_mode_reset_line(reset_pending),
    )
}

/// /fast — Toggle native fast mode for the current provider session
#[poise::command(slash_command, rename = "fast")]
pub(in crate::services::discord) async fn cmd_fast(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] ◀ [{user_name}] /fast");

    if !fast_mode_supported(&ctx.data().provider) {
        ctx.say("/fast is only available in Claude and Codex channels.")
            .await?;
        return Ok(());
    }

    let channel_id = ctx.channel_id();
    let currently_enabled = channel_fast_mode_enabled(&ctx.data().shared, channel_id);
    let next_enabled = !currently_enabled;
    update_channel_fast_mode(
        &ctx.data().shared,
        &ctx.data().token,
        channel_id,
        next_enabled,
    )
    .await;

    let reset_pending = ctx
        .data()
        .shared
        .session_reset_pending
        .contains(&channel_id);
    let notice = if next_enabled {
        build_fast_enabled_notice(&ctx.data().provider, reset_pending)
    } else {
        build_fast_disabled_notice(&ctx.data().provider, reset_pending)
    };
    ctx.say(notice).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        build_fast_disabled_notice, build_fast_enabled_notice, fast_mode_reset_line,
        fast_mode_supported,
    };
    use crate::services::provider::ProviderKind;

    #[test]
    fn fast_mode_supported_only_for_claude_and_codex() {
        assert!(fast_mode_supported(&ProviderKind::Claude));
        assert!(fast_mode_supported(&ProviderKind::Codex));
        assert!(!fast_mode_supported(&ProviderKind::Gemini));
        assert!(!fast_mode_supported(&ProviderKind::Qwen));
    }

    #[test]
    fn fast_mode_reset_line_mentions_next_turn_when_reset_pending() {
        assert!(fast_mode_reset_line(true).contains("다음 사용자 턴"));
        assert!(fast_mode_reset_line(false).contains("현재 세션부터"));
    }

    #[test]
    fn fast_enabled_notice_mentions_native_mode_and_cost() {
        let notice = build_fast_enabled_notice(&ProviderKind::Codex, true);
        assert!(notice.contains("native fast mode"));
        assert!(notice.contains("더 빨라질 수"));
        assert!(notice.contains("사용량/비용"));
    }

    #[test]
    fn fast_disabled_notice_mentions_default_mode() {
        let notice = build_fast_disabled_notice(&ProviderKind::Claude, true);
        assert!(notice.contains("native fast mode를 껐습니다"));
        assert!(notice.contains("기본 응답 모드"));
    }
}
