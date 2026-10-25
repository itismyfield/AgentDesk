use super::super::formatting::send_long_message_ctx;
use super::super::{Context, Error, check_auth};

mod reports;
mod session_details;

pub(in crate::services::discord) use reports::{
    build_health_report, build_inflight_report, build_queue_report, build_status_report,
};

/// /health — Show runtime health summary
#[poise::command(slash_command, rename = "health")]
pub(in crate::services::discord) async fn cmd_health(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    log_command_received!(ctx.channel_id().get(), user_name, "/health");

    let text =
        build_health_report(&ctx.data().shared, &ctx.data().provider, ctx.channel_id()).await;
    send_long_message_ctx(ctx, &text).await?;
    Ok(())
}

/// /status — Show concise per-channel runtime state
#[poise::command(slash_command, rename = "status")]
pub(in crate::services::discord) async fn cmd_status(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    log_command_received!(ctx.channel_id().get(), user_name, "/status");

    let text =
        build_status_report(&ctx.data().shared, &ctx.data().provider, ctx.channel_id()).await;
    send_long_message_ctx(ctx, &text).await?;
    Ok(())
}

/// /inflight — Show saved inflight turn state
#[poise::command(slash_command, rename = "inflight")]
pub(in crate::services::discord) async fn cmd_inflight(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    log_command_received!(ctx.channel_id().get(), user_name, "/inflight");

    let text =
        build_inflight_report(&ctx.data().shared, &ctx.data().provider, ctx.channel_id()).await;
    send_long_message_ctx(ctx, &text).await?;
    Ok(())
}

/// /queue — Show pending intervention queue state
#[poise::command(slash_command, rename = "queue")]
pub(in crate::services::discord) async fn cmd_queue(
    ctx: Context<'_>,
    #[description = "Show all channels (omit for current channel only)"] all: Option<bool>,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    log_command_received!(ctx.channel_id().get(), user_name, "/queue");

    let show_all = all.unwrap_or(false);
    let text = build_queue_report(
        &ctx.data().shared,
        &ctx.data().provider,
        ctx.channel_id(),
        show_all,
    )
    .await;
    send_long_message_ctx(ctx, &text).await?;
    Ok(())
}

#[cfg(test)]
mod recovery_command_tests;
