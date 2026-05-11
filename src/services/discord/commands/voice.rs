use anyhow::{Context as AnyhowContext, anyhow};
use poise::serenity_prelude as serenity;
use serenity::{ChannelId, GuildId, UserId};
use songbird::{CoreEvent, Event, SerenityInit as _};

use super::super::{Context, Data, Error, check_auth};
use crate::voice::barge_in::BargeInSensitivity;

/// /vc_join — Join the caller's current voice channel and start WAV capture.
#[poise::command(slash_command, rename = "vc_join")]
pub(in crate::services::discord) async fn cmd_vc_join(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    if !ctx.data().voice_config.enabled {
        ctx.say("Voice capture is disabled in `agentdesk.yaml` (`voice.enabled: true`).")
            .await?;
        return Ok(());
    }

    let (guild_id, channel_id) =
        resolve_user_voice_channel(ctx.serenity_context(), ctx.guild_id(), user_id)?;
    join_voice_channel(
        ctx.serenity_context(),
        ctx.data().voice_receiver.clone(),
        guild_id,
        channel_id,
        ctx.channel_id(),
    )
    .await?;
    ctx.data()
        .shared
        .voice_barge_in
        .register_voice_context(ctx.channel_id(), guild_id);

    ctx.say(format!(
        "VC joined `{}`; WAV utterance capture is active.",
        channel_id.get()
    ))
    .await?;
    Ok(())
}

/// /vc_leave — Leave the current guild voice channel and flush active WAV capture.
#[poise::command(slash_command, rename = "vc_leave")]
pub(in crate::services::discord) async fn cmd_vc_leave(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let guild_id = ctx
        .guild_id()
        .ok_or_else(|| anyhow!("VC leave requires a guild"))?;
    let flushed = leave_voice_channel(ctx.serenity_context(), ctx.data(), guild_id).await?;
    ctx.say(format!(
        "VC left; flushed `{}` pending utterance(s).",
        flushed
    ))
    .await?;
    Ok(())
}

pub(in crate::services::discord) async fn handle_vc_text_command(
    ctx: &serenity::Context,
    msg: &serenity::Message,
    data: &Data,
    subcommand: &str,
) -> Result<(), Error> {
    if !check_auth(msg.author.id, &msg.author.name, &data.shared, &data.token).await {
        return Ok(());
    }

    match subcommand {
        "join" => {
            if !data.voice_config.enabled {
                let _ = msg
                    .reply(
                        &ctx.http,
                        "Voice capture is disabled in `agentdesk.yaml` (`voice.enabled: true`).",
                    )
                    .await;
                return Ok(());
            }
            let (guild_id, channel_id) =
                resolve_user_voice_channel(ctx, msg.guild_id, msg.author.id)?;
            join_voice_channel(
                ctx,
                data.voice_receiver.clone(),
                guild_id,
                channel_id,
                msg.channel_id,
            )
            .await?;
            data.shared
                .voice_barge_in
                .register_voice_context(msg.channel_id, guild_id);
            let _ = msg
                .reply(
                    &ctx.http,
                    format!(
                        "VC joined `{}`; WAV utterance capture is active.",
                        channel_id.get()
                    ),
                )
                .await;
        }
        "conservative" | "보수" | "보수모드" => {
            data.shared
                .voice_barge_in
                .set_sensitivity(BargeInSensitivity::Conservative)
                .await;
            let _ = msg
                .reply(&ctx.http, "Voice barge-in sensitivity: conservative.")
                .await;
        }
        "normal" | "기본" | "기본감도" | "일반" => {
            data.shared
                .voice_barge_in
                .set_sensitivity(BargeInSensitivity::Normal)
                .await;
            let _ = msg
                .reply(&ctx.http, "Voice barge-in sensitivity: normal.")
                .await;
        }
        "leave" => {
            let guild_id = msg
                .guild_id
                .ok_or_else(|| anyhow!("!vc leave requires a guild"))?;
            let flushed = leave_voice_channel(ctx, data, guild_id).await?;
            let _ = msg
                .reply(
                    &ctx.http,
                    format!("VC left; flushed `{}` pending utterance(s).", flushed),
                )
                .await;
        }
        _ => {
            let _ = msg
                .reply(
                    &ctx.http,
                    "Usage: `!vc join`, `!vc leave`, `!vc conservative`, or `!vc normal`.",
                )
                .await;
        }
    }

    Ok(())
}

pub(in crate::services::discord) async fn auto_join_voice_channels(
    ctx: serenity::Context,
    receiver: crate::voice::VoiceReceiver,
    config: crate::voice::VoiceConfig,
    barge_in: std::sync::Arc<super::super::voice_barge_in::VoiceBargeInRuntime>,
) {
    if !config.enabled {
        return;
    }

    for raw_channel_id in config.auto_join_channel_ids {
        let Ok(channel_id) = raw_channel_id.trim().parse::<u64>().map(ChannelId::new) else {
            tracing::warn!(
                channel_id = raw_channel_id,
                "invalid voice auto-join channel id"
            );
            continue;
        };

        let Ok(channel) = channel_id.to_channel(&ctx).await else {
            tracing::warn!(
                channel_id = channel_id.get(),
                "failed to resolve voice auto-join channel"
            );
            continue;
        };
        let Some(guild_channel) = channel.guild() else {
            tracing::warn!(
                channel_id = channel_id.get(),
                "voice auto-join channel is not a guild channel"
            );
            continue;
        };

        if let Err(error) = join_voice_channel(
            &ctx,
            receiver.clone(),
            guild_channel.guild_id,
            channel_id,
            channel_id,
        )
        .await
        {
            tracing::warn!(
                error = %error,
                guild_id = guild_channel.guild_id.get(),
                channel_id = channel_id.get(),
                "failed to auto-join voice channel"
            );
        } else {
            barge_in.register_voice_context(channel_id, guild_channel.guild_id);
        }
    }
}

async fn join_voice_channel(
    ctx: &serenity::Context,
    receiver: crate::voice::VoiceReceiver,
    guild_id: GuildId,
    channel_id: ChannelId,
    control_channel_id: ChannelId,
) -> Result<(), Error> {
    let manager = songbird::get(ctx)
        .await
        .ok_or_else(|| anyhow!("Songbird voice manager is not registered"))?;
    let handler_lock = manager.join(guild_id, channel_id).await.with_context(|| {
        format!(
            "failed to join voice channel {} in guild {}",
            channel_id.get(),
            guild_id.get()
        )
    })?;

    let mut handler = handler_lock.lock().await;
    handler.remove_all_global_events();
    let receiver_handler = receiver.event_handler(control_channel_id.get());
    handler.add_global_event(
        Event::Core(CoreEvent::SpeakingStateUpdate),
        receiver_handler.clone(),
    );
    handler.add_global_event(Event::Core(CoreEvent::VoiceTick), receiver_handler);
    Ok(())
}

async fn leave_voice_channel(
    ctx: &serenity::Context,
    data: &Data,
    guild_id: GuildId,
) -> Result<usize, Error> {
    let manager = songbird::get(ctx)
        .await
        .ok_or_else(|| anyhow!("Songbird voice manager is not registered"))?;
    manager
        .leave(guild_id)
        .await
        .with_context(|| format!("failed to leave voice guild {}", guild_id.get()))?;
    data.shared.voice_barge_in.unregister_voice_guild(guild_id);
    Ok(data.voice_receiver.flush_all().await.len())
}

fn resolve_user_voice_channel(
    ctx: &serenity::Context,
    guild_id: Option<GuildId>,
    user_id: UserId,
) -> Result<(GuildId, ChannelId), Error> {
    let guild_id = guild_id.ok_or_else(|| anyhow!("VC join requires a guild"))?;
    let channel_id = guild_id
        .to_guild_cached(&ctx.cache)
        .and_then(|guild| {
            guild
                .voice_states
                .get(&user_id)
                .and_then(|voice_state| voice_state.channel_id)
        })
        .ok_or_else(|| anyhow!("caller is not connected to a voice channel"))?;
    Ok((guild_id, channel_id))
}

pub(in crate::services::discord) fn songbird_decode_config() -> songbird::Config {
    songbird::Config::default()
        .decode_mode(songbird::driver::DecodeMode::Decode)
        .decode_channels(songbird::driver::Channels::Stereo)
        .decode_sample_rate(songbird::driver::SampleRate::Hz48000)
}

pub(in crate::services::discord) fn register_songbird(
    builder: serenity::ClientBuilder,
) -> serenity::ClientBuilder {
    builder.register_songbird_from_config(songbird_decode_config())
}
