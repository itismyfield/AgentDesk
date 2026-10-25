use crate::services::provider::ProviderKind;

use super::super::{Context, Error, check_auth};
use super::config::{
    channel_fast_mode_setting, effective_provider_for_channel,
    fallback_channel_name_for_feature_toggle, native_fast_mode_supported,
    session_toggle_reset_line, update_channel_fast_mode,
};

fn fast_mode_reset_line(reset_pending: bool) -> &'static str {
    session_toggle_reset_line(reset_pending)
}

fn build_fast_enabled_notice(provider: &ProviderKind, reset_pending: bool) -> String {
    format!(
        "`/fast` 설정 저장: {} 채널의 native fast mode를 켜도록 저장했습니다.\n속도 우선 모드는 공급자 정책에 따라 동작합니다. provider 정책상 응답은 더 빨라질 수 있지만 사용량/비용은 더 크게 집계될 수 있습니다.\n{}",
        provider.display_name(),
        fast_mode_reset_line(reset_pending),
    )
}

fn build_fast_disabled_notice(provider: &ProviderKind, reset_pending: bool) -> String {
    format!(
        "`/fast` 설정 저장: {} 채널의 native fast mode를 끄도록 저장했습니다.\n리셋 후 기본 응답 모드로 실행합니다.\n{}",
        provider.display_name(),
        fast_mode_reset_line(reset_pending),
    )
}

pub(super) fn feature_scope_notice(shared_with_parent: bool) -> &'static str {
    if shared_with_parent {
        "\n이 스레드는 부모 채널과 설정을 공유합니다. 부모 채널 또는 이 설정을 공유하는 스레드 중 먼저 턴을 시작하는 세션이 리셋됩니다(맥락 소실)."
    } else {
        ""
    }
}

/// /fast — Toggle native fast mode for the current provider session
#[poise::command(slash_command, rename = "fast")]
pub(in crate::services::discord) async fn cmd_fast(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    log_command_received!(ctx.channel_id().get(), user_name, "/fast");

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

    let notice: std::io::Result<String> = async {
        ctx.data()
            .shared
            .check_settings_owner(&effective_provider, channel_id)
            .await?;
        let runtime_channel_id = channel_id;
        let channel_id = ctx
            .data()
            .shared
            .feature_command_channel_id(
                &ctx.serenity_context().http,
                &effective_provider,
                channel_id,
            )
            .await;
        let currently_enabled = channel_fast_mode_setting(&ctx.data().shared, channel_id)
            .await
            .unwrap_or(false);
        let next_enabled = !currently_enabled;
        update_channel_fast_mode(
            &ctx.data().shared,
            &ctx.data().token,
            channel_id,
            &effective_provider,
            next_enabled,
            runtime_channel_id,
        )
        .await?;

        let reset_pending = ctx
            .data()
            .shared
            .overrides
            .session_reset_pending
            .contains(&channel_id);
        let notice = if next_enabled {
            build_fast_enabled_notice(&effective_provider, reset_pending)
        } else {
            build_fast_disabled_notice(&effective_provider, reset_pending)
        };
        let scope = feature_scope_notice(runtime_channel_id != channel_id);
        Ok(format!("{notice}{scope}"))
    }
    .await;
    ctx.say(notice.unwrap_or_else(|error| format!("설정을 저장하지 못했습니다: {error}")))
        .await?;
    Ok(())
}
