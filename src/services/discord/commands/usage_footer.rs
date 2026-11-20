use super::super::{Context, Error, check_auth, runtime_store::usage_footer};

#[derive(Debug, Clone, Copy, poise::ChoiceParameter)]
pub(in crate::services::discord) enum UsageFooterAction {
    #[name = "on"]
    On,
    #[name = "off"]
    Off,
}

/// /usage-footer [on|off] — 이 채널의 턴 푸터 쿼터 표시를 바꿉니다
#[poise::command(slash_command, rename = "usage-footer")]
pub(in crate::services::discord) async fn cmd_usage_footer(
    ctx: Context<'_>,
    #[description = "on / off (생략하면 토글)"] action: Option<UsageFooterAction>,
) -> Result<(), Error> {
    let user = ctx.author();
    if !check_auth(user.id, &user.name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let channel = ctx.channel_id().get();
    log_command_received!(channel, &user.name, "/usage-footer");
    ctx.defer().await?;
    let requested = action.map(|action| matches!(action, UsageFooterAction::On));
    let saved =
        tokio::task::spawn_blocking(move || usage_footer::update(channel, requested)).await?;
    let notice = match saved {
        Ok(true) => "이 채널의 턴 푸터 쿼터 표시를 켰어요.",
        Ok(false) => "이 채널의 턴 푸터 쿼터 표시를 껐어요.",
        Err(error) => {
            tracing::warn!(channel, %error, "usage footer preference write failed");
            "이 채널의 턴 푸터 쿼터 표시 설정을 저장하지 못했어요."
        }
    };
    ctx.say(notice).await?;
    Ok(())
}
