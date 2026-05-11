use anyhow::{Context as AnyhowContext, anyhow};
use poise::serenity_prelude as serenity;
use serenity::{ChannelId, GuildId, UserId};
use songbird::{CoreEvent, Event, SerenityInit as _};

use super::super::{Context, Data, Error, check_auth};
use crate::voice::barge_in::BargeInSensitivity;
use crate::voice::commands::{VoiceCommand, parse_voice_command};

#[derive(Debug, Clone, Copy, poise::ChoiceParameter)]
enum VoiceSensitivityChoice {
    #[name = "normal"]
    Normal,
    #[name = "conservative"]
    Conservative,
}

impl VoiceSensitivityChoice {
    const fn sensitivity(self) -> BargeInSensitivity {
        match self {
            Self::Normal => BargeInSensitivity::Normal,
            Self::Conservative => BargeInSensitivity::Conservative,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Conservative => "conservative",
        }
    }
}

/// /voice — Voice capture and spoken-command namespace.
#[poise::command(
    slash_command,
    rename = "voice",
    subcommands(
        "cmd_voice_join",
        "cmd_voice_leave",
        "cmd_voice_attach",
        "cmd_voice_latency",
        "cmd_voice_sensitivity"
    )
)]
pub(in crate::services::discord) async fn cmd_voice(_ctx: Context<'_>) -> Result<(), Error> {
    Ok(())
}

/// /vc_join — Join the caller's current voice channel and start WAV capture.
#[poise::command(slash_command, rename = "vc_join")]
pub(in crate::services::discord) async fn cmd_vc_join(ctx: Context<'_>) -> Result<(), Error> {
    voice_join_impl(ctx).await
}

/// /voice join — Join the caller's current voice channel and start WAV capture.
#[poise::command(slash_command, rename = "join")]
async fn cmd_voice_join(ctx: Context<'_>) -> Result<(), Error> {
    voice_join_impl(ctx).await
}

async fn voice_join_impl(ctx: Context<'_>) -> Result<(), Error> {
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
    let control_channel_id = ctx
        .data()
        .shared
        .voice_pairings
        .target_channel(channel_id)
        .unwrap_or(ctx.channel_id());
    join_voice_channel(
        ctx.serenity_context(),
        ctx.data().voice_receiver.clone(),
        guild_id,
        channel_id,
        control_channel_id,
    )
    .await?;
    ctx.data()
        .shared
        .voice_barge_in
        .register_voice_context(control_channel_id, guild_id);

    ctx.say(format!(
        "VC joined `{}`; voice turns route to text channel `{}`.",
        channel_id.get(),
        control_channel_id.get()
    ))
    .await?;
    Ok(())
}

/// /vc_leave — Leave the current guild voice channel and flush active WAV capture.
#[poise::command(slash_command, rename = "vc_leave")]
pub(in crate::services::discord) async fn cmd_vc_leave(ctx: Context<'_>) -> Result<(), Error> {
    voice_leave_impl(ctx).await
}

/// /voice leave — Leave the current guild voice channel and flush active WAV capture.
#[poise::command(slash_command, rename = "leave")]
async fn cmd_voice_leave(ctx: Context<'_>) -> Result<(), Error> {
    voice_leave_impl(ctx).await
}

async fn voice_leave_impl(ctx: Context<'_>) -> Result<(), Error> {
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

/// /voice attach — Persist the caller voice channel → text channel routing pair.
#[poise::command(slash_command, rename = "attach")]
async fn cmd_voice_attach(
    ctx: Context<'_>,
    #[description = "Text channel ID or mention; defaults to this channel"] text_channel: Option<
        String,
    >,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let (guild_id, voice_channel_id) =
        resolve_user_voice_channel(ctx.serenity_context(), ctx.guild_id(), user_id)?;
    let text_channel_id = match text_channel.as_deref() {
        Some(value) if !value.trim().is_empty() => parse_channel_id_arg(value)?,
        _ => ctx.channel_id(),
    };

    ctx.data()
        .shared
        .voice_pairings
        .attach(voice_channel_id, text_channel_id)
        .map_err(anyhow::Error::msg)?;
    ctx.data()
        .shared
        .voice_barge_in
        .register_voice_context(text_channel_id, guild_id);

    ctx.say(format!(
        "Voice channel `{}` is attached to text channel `{}`.",
        voice_channel_id.get(),
        text_channel_id.get()
    ))
    .await?;
    Ok(())
}

/// /voice latency — Report recent voice turn latency averages (Voice #10).
#[poise::command(slash_command, rename = "latency")]
async fn cmd_voice_latency(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let verbose = ctx.data().shared.voice_barge_in.verbose_progress_enabled();
    let summary = crate::voice::metrics::recent_summary(5);
    let body = if summary.sample_count == 0 {
        format!(
            "Voice path: enabled=`{}`, verbose_progress=`{}`. Capture idle: segment=`{}ms`, utterance=`{}ms`.\nNo `voice_latency_turn` events recorded yet.",
            ctx.data().voice_config.enabled,
            verbose,
            ctx.data().voice_config.idle.segment_idle_ms,
            ctx.data().voice_config.idle.utterance_idle_ms
        )
    } else {
        format!(
            "Voice path: enabled=`{}`, verbose_progress=`{}`. Capture idle: segment=`{}ms`, utterance=`{}ms`.\nLast {} turn(s) — avg stt=`{}ms` / agent=`{}ms` / tts_synth=`{}ms` / tts_play=`{}ms` / total=`{}ms`.",
            ctx.data().voice_config.enabled,
            verbose,
            ctx.data().voice_config.idle.segment_idle_ms,
            ctx.data().voice_config.idle.utterance_idle_ms,
            summary.sample_count,
            summary.avg_stt_ms,
            summary.avg_agent_ms,
            summary.avg_tts_synth_ms,
            summary.avg_tts_play_ms,
            summary.avg_total_ms,
        )
    };
    ctx.say(body).await?;
    Ok(())
}

/// /voice sensitivity <mode> — Set barge-in sensitivity.
#[poise::command(slash_command, rename = "sensitivity")]
async fn cmd_voice_sensitivity(
    ctx: Context<'_>,
    #[description = "Barge-in sensitivity mode"] mode: VoiceSensitivityChoice,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    ctx.data()
        .shared
        .voice_barge_in
        .set_sensitivity(mode.sensitivity())
        .await;
    ctx.say(format!("Voice barge-in sensitivity: {}.", mode.as_str()))
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
            let control_channel_id = data
                .shared
                .voice_pairings
                .target_channel(channel_id)
                .unwrap_or(msg.channel_id);
            join_voice_channel(
                ctx,
                data.voice_receiver.clone(),
                guild_id,
                channel_id,
                control_channel_id,
            )
            .await?;
            data.shared
                .voice_barge_in
                .register_voice_context(control_channel_id, guild_id);
            let _ = msg
                .reply(
                    &ctx.http,
                    format!(
                        "VC joined `{}`; voice turns route to text channel `{}`.",
                        channel_id.get(),
                        control_channel_id.get()
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
        "latency" => {
            let _ = msg
                .reply(
                    &ctx.http,
                    format!(
                        "Voice path: enabled=`{}`, verbose_progress=`{}`.",
                        data.voice_config.enabled,
                        data.shared.voice_barge_in.verbose_progress_enabled()
                    ),
                )
                .await;
        }
        _ => {
            if let Some(command) = parse_voice_command(subcommand) {
                match command {
                    VoiceCommand::Sensitivity(sensitivity) => {
                        data.shared
                            .voice_barge_in
                            .set_sensitivity(sensitivity)
                            .await;
                        let _ = msg
                            .reply(
                                &ctx.http,
                                format!("Voice barge-in sensitivity: {sensitivity:?}."),
                            )
                            .await;
                        return Ok(());
                    }
                    VoiceCommand::VerboseProgress(enabled) => {
                        data.shared
                            .voice_barge_in
                            .set_verbose_progress_enabled(enabled);
                        let _ = msg
                            .reply(&ctx.http, format!("Voice verbose progress: {enabled}."))
                            .await;
                        return Ok(());
                    }
                    _ => {}
                }
            }
            let _ = msg
                .reply(
                    &ctx.http,
                    "Usage: `!vc join`, `!vc leave`, `!vc latency`, `!vc conservative`, or `!vc normal`.",
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
    pairings: std::sync::Arc<super::super::voice_routing::VoiceChannelPairingStore>,
) {
    if !config.enabled {
        return;
    }

    for raw_channel_id in config.auto_join_channel_ids_with_lobby() {
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

        let control_channel_id = pairings.target_channel(channel_id).unwrap_or(channel_id);
        if let Some(manager) = songbird::get(&ctx).await
            && manager.get(guild_channel.guild_id).is_some()
        {
            tracing::info!(
                guild_id = guild_channel.guild_id.get(),
                channel_id = channel_id.get(),
                "voice auto-join skipped: songbird call already registered for guild (#2054 idempotency)"
            );
            barge_in.register_voice_context(control_channel_id, guild_channel.guild_id);
            continue;
        }

        match join_voice_channel(
            &ctx,
            receiver.clone(),
            guild_channel.guild_id,
            channel_id,
            control_channel_id,
        )
        .await
        {
            Ok(()) => {
                barge_in.register_voice_context(control_channel_id, guild_channel.guild_id);
            }
            Err(error) => {
                let mut chain: Vec<String> = vec![error.to_string()];
                let mut current = error.source();
                while let Some(src) = current {
                    chain.push(src.to_string());
                    current = src.source();
                }
                tracing::warn!(
                    error = %error,
                    error_chain = ?chain,
                    guild_id = guild_channel.guild_id.get(),
                    channel_id = channel_id.get(),
                    "failed to auto-join voice channel"
                );
            }
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
    let handler_lock = match manager.join(guild_id, channel_id).await {
        Ok(handle) => handle,
        Err(join_err) => {
            if let Some(existing) = manager.get(guild_id) {
                tracing::warn!(
                    join_error = %join_err,
                    guild_id = guild_id.get(),
                    channel_id = channel_id.get(),
                    "songbird manager.join() returned Err but call already registered; \
                     attaching receiver retroactively (#2054 fallback)"
                );
                existing
            } else {
                return Err(anyhow!(join_err)
                    .context(format!(
                        "songbird manager.join() failed for channel {} in guild {}",
                        channel_id.get(),
                        guild_id.get()
                    ))
                    .into());
            }
        }
    };

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

fn parse_channel_id_arg(value: &str) -> Result<ChannelId, Error> {
    let raw = value
        .trim()
        .trim_start_matches("<#")
        .trim_start_matches('#')
        .trim_end_matches('>');
    raw.parse::<u64>()
        .map(ChannelId::new)
        .map_err(|_| anyhow!("invalid text channel id `{}`", value).into())
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
