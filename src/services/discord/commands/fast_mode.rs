use crate::services::provider::ProviderKind;

use super::super::{Context, Error, check_auth};
use super::config::{
    channel_fast_mode_enabled, effective_provider_for_channel,
    fallback_channel_name_for_feature_toggle, native_fast_mode_supported,
    session_toggle_reset_line, update_channel_fast_mode,
};

fn fast_mode_reset_line(reset_pending: bool) -> &'static str {
    session_toggle_reset_line(reset_pending)
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

    let channel_id = ctx.channel_id();
    let channel_name_hint = fallback_channel_name_for_feature_toggle(ctx, channel_id).await;
    let effective_provider = effective_provider_for_channel(
        &ctx.data().shared,
        channel_id,
        &ctx.data().provider,
        channel_name_hint.as_deref(),
    )
    .await;

    if !native_fast_mode_supported(&effective_provider) {
        ctx.say("/fast is only available in Claude and Codex channels.")
            .await?;
        return Ok(());
    }

    let currently_enabled = channel_fast_mode_enabled(&ctx.data().shared, channel_id);
    let next_enabled = !currently_enabled;
    update_channel_fast_mode(
        &ctx.data().shared,
        &ctx.data().token,
        channel_id,
        &effective_provider,
        next_enabled,
    )
    .await;

    let reset_pending = ctx
        .data()
        .shared
        .session_reset_pending
        .contains(&channel_id);
    let notice = if next_enabled {
        build_fast_enabled_notice(&effective_provider, reset_pending)
    } else {
        build_fast_disabled_notice(&effective_provider, reset_pending)
    };
    ctx.say(notice).await?;
    Ok(())
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::{build_fast_disabled_notice, build_fast_enabled_notice, fast_mode_reset_line};
    use crate::services::discord::commands::config::native_fast_mode_supported;
    use crate::services::provider::ProviderKind;

    #[test]
    fn fast_mode_supported_only_for_claude_and_codex() {
        assert!(native_fast_mode_supported(&ProviderKind::Claude));
        assert!(native_fast_mode_supported(&ProviderKind::Codex));
        assert!(!native_fast_mode_supported(&ProviderKind::Gemini));
        assert!(!native_fast_mode_supported(&ProviderKind::Qwen));
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
