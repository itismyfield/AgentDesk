use std::sync::Arc;

use poise::CreateReply;
use poise::serenity_prelude as serenity;

use super::super::model_catalog::provider_supports_model_override;
use super::super::{Context, Error, SharedData, check_auth};
use super::config::{
    build_model_picker_components_from_snapshot, build_model_picker_embed_from_snapshot,
    current_working_dir, effective_model_snapshot, remember_model_picker_pending,
};
use crate::services::provider::ProviderKind;

struct ModelPickerView {
    override_model: Option<String>,
    embed: serenity::CreateEmbed,
    components: Vec<serenity::CreateActionRow>,
}

async fn build_model_picker_view(
    shared: &Arc<SharedData>,
    target_channel_id: serenity::ChannelId,
    provider: &ProviderKind,
) -> ModelPickerView {
    let snapshot = effective_model_snapshot(shared, target_channel_id).await;
    let working_dir = current_working_dir(shared, target_channel_id).await;
    let pending_model = snapshot.override_model.as_deref();
    let embed = build_model_picker_embed_from_snapshot(
        &snapshot,
        provider,
        pending_model,
        None,
        working_dir.as_deref(),
    );
    let components = build_model_picker_components_from_snapshot(
        &snapshot,
        target_channel_id,
        provider,
        pending_model,
        working_dir.as_deref(),
    );

    ModelPickerView {
        override_model: snapshot.override_model,
        embed,
        components,
    }
}

async fn send_model_picker_card(
    ctx: &serenity::Context,
    shared: &Arc<SharedData>,
    destination_channel_id: serenity::ChannelId,
    target_channel_id: serenity::ChannelId,
    provider: &ProviderKind,
    owner_user_id: serenity::UserId,
) -> serenity::Result<serenity::Message> {
    let view = build_model_picker_view(shared, target_channel_id, provider).await;
    let message = destination_channel_id
        .send_message(
            &ctx.http,
            serenity::CreateMessage::new()
                .embed(view.embed)
                .components(view.components),
        )
        .await?;
    remember_model_picker_pending(
        shared,
        message.id,
        owner_user_id,
        target_channel_id,
        view.override_model,
    );
    Ok(message)
}

async fn resolve_channel_kind(
    ctx: &serenity::Context,
    channel_id: serenity::ChannelId,
) -> Option<serenity::ChannelType> {
    let channel = channel_id.to_channel(&ctx.http).await.ok()?;
    match channel {
        serenity::Channel::Guild(guild_channel) => Some(guild_channel.kind),
        serenity::Channel::Private(_) => None,
        _ => None,
    }
}

async fn create_model_picker_forum_post(
    ctx: &serenity::Context,
    shared: &Arc<SharedData>,
    forum_channel_id: serenity::ChannelId,
    target_channel_id: serenity::ChannelId,
    provider: &ProviderKind,
    owner_user_id: serenity::UserId,
) -> serenity::Result<serenity::Message> {
    let post = forum_channel_id
        .create_forum_post(
            &ctx.http,
            serenity::CreateForumPost::new(
                format!("Model Picker • {}", provider.display_name()),
                serenity::CreateMessage::new().content(format!(
                    "Interactive model picker for <#{}>",
                    target_channel_id.get()
                )),
            ),
        )
        .await?;
    send_model_picker_card(
        ctx,
        shared,
        post.id,
        target_channel_id,
        provider,
        owner_user_id,
    )
    .await
}

async fn run_model_command(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    let channel_id = ctx.channel_id();

    if !provider_supports_model_override(&ctx.data().provider) {
        tracing::info!("  [{ts}] ◀ [{user_name}] /model (unsupported provider)");
        ctx.say(
            "Model override is only supported for Claude, Codex, Gemini, OpenCode, and Qwen channels.",
        )
            .await?;
        return Ok(());
    }

    tracing::info!("  [{ts}] ◀ [{user_name}] /model");
    match resolve_channel_kind(ctx.serenity_context(), channel_id).await {
        Some(serenity::ChannelType::Forum) => {
            let posted = create_model_picker_forum_post(
                ctx.serenity_context(),
                &ctx.data().shared,
                channel_id,
                channel_id,
                &ctx.data().provider,
                user_id,
            )
            .await?;
            ctx.send(CreateReply::default().ephemeral(true).content(format!(
                "This forum parent cannot host the picker directly. I sent it to <#{}>.",
                posted.channel_id.get()
            )))
            .await?;
        }
        _ => {
            let view =
                build_model_picker_view(&ctx.data().shared, channel_id, &ctx.data().provider).await;
            let posted = ctx
                .send(
                    CreateReply::default()
                        .embed(view.embed)
                        .components(view.components),
                )
                .await?
                .into_message()
                .await?;
            remember_model_picker_pending(
                &ctx.data().shared,
                posted.id,
                user_id,
                channel_id,
                view.override_model,
            );
        }
    }
    Ok(())
}

/// /model — Open the interactive model picker for this channel
#[poise::command(slash_command, rename = "model")]
pub(in crate::services::discord) async fn cmd_model(ctx: Context<'_>) -> Result<(), Error> {
    run_model_command(ctx).await
}
