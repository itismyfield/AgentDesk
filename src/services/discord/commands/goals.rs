use super::super::{Context, Error, check_auth};
use super::config::{
    channel_codex_goals_enabled, codex_goals_supported, effective_provider_for_channel,
    fallback_channel_name_for_feature_toggle, session_toggle_reset_line,
    update_channel_codex_goals,
};

fn build_goals_enabled_notice(reset_pending: bool) -> String {
    format!(
        "`/goals` 적용 완료: Codex goals feature flag를 켰습니다.\n다음 Codex 턴부터 `--enable goals`로 실행합니다.\n{}",
        session_toggle_reset_line(reset_pending),
    )
}

fn build_goals_disabled_notice(reset_pending: bool) -> String {
    format!(
        "`/goals` 해제 완료: Codex goals feature flag를 껐습니다.\n다음 Codex 턴부터 `--disable goals`로 실행합니다.\n{}",
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

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] ◀ [{user_name}] /goals");

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

    let currently_enabled = channel_codex_goals_enabled(&ctx.data().shared, channel_id);
    let next_enabled = !currently_enabled;
    update_channel_codex_goals(
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
        build_goals_enabled_notice(reset_pending)
    } else {
        build_goals_disabled_notice(reset_pending)
    };
    ctx.say(notice).await?;
    Ok(())
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::{build_goals_disabled_notice, build_goals_enabled_notice};
    use crate::services::discord::commands::config::codex_goals_supported;
    use crate::services::provider::ProviderKind;

    #[test]
    fn goals_supported_only_for_codex() {
        assert!(!codex_goals_supported(&ProviderKind::Claude));
        assert!(codex_goals_supported(&ProviderKind::Codex));
        assert!(!codex_goals_supported(&ProviderKind::Gemini));
        assert!(!codex_goals_supported(&ProviderKind::Qwen));
    }

    #[test]
    fn goals_notices_mention_codex_feature_flag() {
        let enabled = build_goals_enabled_notice(true);
        assert!(enabled.contains("Codex goals feature flag"));
        assert!(enabled.contains("--enable goals"));

        let disabled = build_goals_disabled_notice(true);
        assert!(disabled.contains("--disable goals"));
    }
}
