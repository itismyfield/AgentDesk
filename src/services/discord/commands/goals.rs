use super::super::{Context, Error, check_auth};
use super::config::{
    channel_codex_goals_setting, codex_goals_supported, effective_provider_for_channel,
    fallback_channel_name_for_feature_toggle, session_toggle_reset_line,
    update_channel_codex_goals,
};

fn build_goals_enabled_notice(reset_pending: bool) -> String {
    format!(
        "`/goals` 설정 저장: Codex goals를 켜도록 저장했습니다.\n다음 Codex 실행에 goals를 켜도록 요청합니다.\n{}",
        session_toggle_reset_line(reset_pending),
    )
}

fn build_goals_disabled_notice(reset_pending: bool) -> String {
    format!(
        "`/goals` 설정 저장: Codex goals를 끄도록 저장했습니다.\n다음 Codex 실행에 goals를 끄도록 요청합니다.\n{}",
        session_toggle_reset_line(reset_pending),
    )
}

/// /goals — Toggle Codex goals feature flag for this channel
#[poise::command(slash_command, rename = "goals")]
pub(in crate::services::discord) async fn cmd_goals(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    log_command_received!(ctx.channel_id().get(), user_name, "/goals");

    let channel_id = ctx.channel_id();
    let channel_name_hint = fallback_channel_name_for_feature_toggle(ctx, channel_id).await;
    let effective_provider = effective_provider_for_channel(
        &ctx.data().shared,
        channel_id,
        &ctx.data().provider,
        channel_name_hint.as_deref(),
    )
    .await;

    if !codex_goals_supported(&effective_provider) {
        ctx.say("/goals is only available in Codex channels.")
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
        let currently_enabled = channel_codex_goals_setting(&ctx.data().shared, channel_id)
            .await
            .unwrap_or(false);
        let next_enabled = !currently_enabled;
        update_channel_codex_goals(
            &ctx.data().shared,
            &ctx.data().token,
            channel_id,
            next_enabled,
        )
        .await?;

        let reset_pending = ctx
            .data()
            .shared
            .overrides
            .session_reset_pending
            .contains(&channel_id);
        let notice = if next_enabled {
            build_goals_enabled_notice(reset_pending)
        } else {
            build_goals_disabled_notice(reset_pending)
        };
        let scope = super::fast_mode::feature_scope_notice(runtime_channel_id != channel_id);
        Ok(format!("{notice}{scope}"))
    }
    .await;
    ctx.say(notice.unwrap_or_else(|error| format!("설정을 저장하지 못했습니다: {error}")))
        .await?;
    Ok(())
}
