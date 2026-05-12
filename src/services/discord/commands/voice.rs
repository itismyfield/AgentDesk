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
    voice_occupancy().insert(
        (ctx.data().provider.as_str().to_string(), guild_id.get()),
        channel_id.get(),
    );

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
            voice_occupancy().insert(
                (data.provider.as_str().to_string(), guild_id.get()),
                channel_id.get(),
            );
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
                    // F8 (#2046): 텍스트 디스패처에서도 Language/TtsVoice/VoiceClone/
                    // WakeWords 명령을 모두 처리한다. 기존엔 무성공으로 끝나 사용자가
                    // 변경 적용 여부를 알 수 없었다.
                    VoiceCommand::Language(language) => {
                        data.shared
                            .voice_barge_in
                            .set_runtime_language_external(language.clone())
                            .await;
                        let _ = msg
                            .reply(&ctx.http, format!("Voice STT language: `{language}`."))
                            .await;
                        return Ok(());
                    }
                    VoiceCommand::TtsVoice(voice) => {
                        data.shared
                            .voice_barge_in
                            .set_runtime_tts_voice_external(voice.clone())
                            .await;
                        let _ = msg
                            .reply(&ctx.http, format!("Voice TTS voice: `{voice}`."))
                            .await;
                        return Ok(());
                    }
                    VoiceCommand::VoiceClone { reference } => {
                        let detail = reference.as_deref().unwrap_or("<none>");
                        tracing::info!(
                            reference = %detail,
                            "voice clone command accepted via text dispatcher"
                        );
                        let _ = msg
                            .reply(
                                &ctx.http,
                                format!("Voice clone request acknowledged (reference=`{detail}`)."),
                            )
                            .await;
                        return Ok(());
                    }
                    VoiceCommand::WakeWords(wake_command) => {
                        let words = data
                            .shared
                            .voice_barge_in
                            .apply_wake_word_command_external(wake_command)
                            .await;
                        let summary = if words.is_empty() {
                            "<disabled>".to_string()
                        } else {
                            words.join(", ")
                        };
                        let _ = msg
                            .reply(&ctx.http, format!("Voice wake words: {summary}"))
                            .await;
                        return Ok(());
                    }
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
    provider: crate::services::provider::ProviderKind,
    channel_provider_map: std::collections::HashMap<String, String>,
) {
    if !config.enabled {
        tracing::info!("voice auto-join skipped: voice.enabled=false");
        return;
    }

    let raw_ids: Vec<String> = config.auto_join_channel_ids_with_lobby();
    tracing::info!(
        provider = provider.as_str(),
        target_count = raw_ids.len(),
        targets = ?raw_ids,
        "voice auto-join starting"
    );

    let self_provider = provider.as_str().to_string();

    // Pass 1: self-mapped channels. Each provider bot enters only the channels
    // explicitly mapped to it in agentdesk.yaml.
    for raw_channel_id in raw_ids.iter() {
        let Some(mapped) = channel_provider_map.get(raw_channel_id.trim()) else {
            // Unmapped channels never receive a bot. Surface the gap once via
            // the notify bot so the operator can fix the agent yaml.
            let Ok(notify_channel_id) = raw_channel_id.trim().parse::<u64>().map(ChannelId::new)
            else {
                tracing::warn!(
                    channel_id = raw_channel_id,
                    "voice auto-join: invalid channel id in unmapped-skip path"
                );
                continue;
            };
            let alert_target = pairings
                .target_channel(notify_channel_id)
                .unwrap_or(notify_channel_id);
            tracing::warn!(
                provider = self_provider.as_str(),
                channel_id = raw_channel_id,
                "voice auto-join skipped: channel has no provider mapping in agentdesk.yaml"
            );
            notify_voice_alert(
                alert_target,
                format!(
                    "⚠️ 보이스 자동 진입 실패: <#{}>이 agentdesk.yaml provider 매핑에 없습니다. agent 의 channels 슬롯에 등록해 주세요.",
                    notify_channel_id.get()
                ),
                "mapping-missing",
            )
            .await;
            continue;
        };
        if mapped.as_str() != self_provider.as_str() {
            // Pass 2 candidate.
            continue;
        }
        try_join_for_provider(
            &ctx,
            &receiver,
            &barge_in,
            &pairings,
            &self_provider,
            raw_channel_id,
            JoinMode::Mapped,
        )
        .await;
    }

    // Wait briefly so the other provider's Pass 1 has a chance to settle before
    // we decide whether to take over an unfilled mapped channel.
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    // Pass 2: cross-provider takeover. Only steps in when the mapped provider
    // is already occupying the same guild on a different channel (Discord's
    // one-voice-connection-per-guild-per-bot constraint blocks the mapped bot
    // from also entering this channel). The fallback bot joins solely to make
    // voice available; text/TTS routing in `channel_provider_map` is untouched.
    for raw_channel_id in raw_ids.iter() {
        let Some(mapped) = channel_provider_map.get(raw_channel_id.trim()) else {
            continue; // Pass 1 already notified.
        };
        if mapped.as_str() == self_provider.as_str() {
            continue; // Already handled in Pass 1.
        }
        try_join_for_provider(
            &ctx,
            &receiver,
            &barge_in,
            &pairings,
            &self_provider,
            raw_channel_id,
            JoinMode::Takeover {
                mapped_provider: mapped.clone(),
            },
        )
        .await;
    }
}

#[derive(Clone)]
enum JoinMode {
    Mapped,
    Takeover { mapped_provider: String },
}

async fn try_join_for_provider(
    ctx: &serenity::Context,
    receiver: &crate::voice::VoiceReceiver,
    barge_in: &std::sync::Arc<super::super::voice_barge_in::VoiceBargeInRuntime>,
    pairings: &std::sync::Arc<super::super::voice_routing::VoiceChannelPairingStore>,
    self_provider: &str,
    raw_channel_id: &str,
    mode: JoinMode,
) {
    let Ok(channel_id) = raw_channel_id.trim().parse::<u64>().map(ChannelId::new) else {
        tracing::warn!(
            channel_id = raw_channel_id,
            "invalid voice auto-join channel id"
        );
        return;
    };

    let Ok(channel) = channel_id.to_channel(ctx).await else {
        tracing::warn!(
            channel_id = channel_id.get(),
            "failed to resolve voice auto-join channel"
        );
        return;
    };
    let Some(guild_channel) = channel.guild() else {
        tracing::warn!(
            channel_id = channel_id.get(),
            "voice auto-join channel is not a guild channel"
        );
        return;
    };
    let guild_id = guild_channel.guild_id;
    let control_channel_id = pairings.target_channel(channel_id).unwrap_or(channel_id);

    // Takeover-specific guards before touching songbird.
    if let JoinMode::Takeover { mapped_provider } = &mode {
        let occupancy = voice_occupancy();
        let mapped_channel = occupancy
            .get(&(mapped_provider.clone(), guild_id.get()))
            .map(|v| *v);
        let Some(mapped_ch) = mapped_channel else {
            // Mapped bot hasn't claimed this guild yet — defer to it.
            return;
        };
        if mapped_ch == channel_id.get() {
            // Mapped bot is already in this exact channel; takeover would
            // double-join and re-introduce the #2054 v6 duplicate voice/TTS bug.
            tracing::info!(
                provider = self_provider,
                channel_id = channel_id.get(),
                mapped_provider = mapped_provider.as_str(),
                "voice auto-join takeover skipped: mapped bot already on this channel"
            );
            return;
        }
        if occupancy.contains_key(&(self_provider.to_string(), guild_id.get())) {
            // Self is already occupying this guild on another channel; no spare
            // bot available, so emit a single alert and bow out.
            tracing::info!(
                provider = self_provider,
                channel_id = channel_id.get(),
                mapped_provider = mapped_provider.as_str(),
                "voice auto-join takeover unavailable: self also occupies guild"
            );
            notify_voice_alert(
                control_channel_id,
                format!(
                    "⚠️ 보이스 자동 진입 실패: <#{}>의 매핑 봇({mapped})은 다른 채널 점유 중이고 대체 봇({self_p})도 점유 중입니다.",
                    channel_id.get(),
                    mapped = mapped_provider,
                    self_p = self_provider
                ),
                "no-fallback",
            )
            .await;
            return;
        }
        tracing::info!(
            provider = self_provider,
            channel_id = channel_id.get(),
            mapped_provider = mapped_provider.as_str(),
            "voice auto-join takeover: mapped bot busy on another channel, falling back"
        );
    }

    if let Some(manager) = songbird::get(ctx).await
        && let Some(call_lock) = manager.get(guild_id)
    {
        let call = call_lock.lock().await;
        // current_connection() is Some only when ConnectionProgress::Complete,
        // i.e. voice gateway WS + UDP handshake actually finished. A stale
        // `Some(call)` from a previous failed get_or_insert is NOT enough —
        // it returns a zombie handle that never produces VoiceTick events.
        if call.current_connection().is_some() {
            // Record the channel songbird actually holds, not the channel we
            // intended to join. Discord's one-connection-per-guild constraint
            // means an existing call may be on a different voice channel in
            // the same guild; using `channel_id` blindly would poison takeover
            // judgments later.
            let actual_channel = call.current_channel().map(|c| c.0.get());
            drop(call);
            let recorded_channel = actual_channel.unwrap_or_else(|| channel_id.get());
            tracing::info!(
                guild_id = guild_id.get(),
                channel_id = channel_id.get(),
                actual_channel = ?actual_channel,
                "voice auto-join skipped: songbird call already connected for guild (#2054 idempotency)"
            );
            barge_in.register_voice_context(control_channel_id, guild_id);
            voice_occupancy().insert(
                (self_provider.to_string(), guild_id.get()),
                recorded_channel,
            );
            return;
        }
        // Zombie call detected — drop the lock then remove so manager.join() below
        // starts fresh instead of inheriting the dead ConnectionProgress.
        drop(call);
        let _ = manager.remove(guild_id).await;
        tracing::warn!(
            guild_id = guild_id.get(),
            channel_id = channel_id.get(),
            "removed zombie songbird call before retrying auto-join (#2054 zombie-cleanup)"
        );
    }

    match join_voice_channel(
        ctx,
        receiver.clone(),
        guild_id,
        channel_id,
        control_channel_id,
    )
    .await
    {
        Ok(()) => {
            tracing::info!(
                guild_id = guild_id.get(),
                channel_id = channel_id.get(),
                control_channel_id = control_channel_id.get(),
                fallback = matches!(mode, JoinMode::Takeover { .. }),
                "voice auto-join Ok: songbird connected, receiver registered"
            );
            barge_in.register_voice_context(control_channel_id, guild_id);
            voice_occupancy().insert(
                (self_provider.to_string(), guild_id.get()),
                channel_id.get(),
            );
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
                guild_id = guild_id.get(),
                channel_id = channel_id.get(),
                "failed to auto-join voice channel"
            );
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
            // Surface the full underlying songbird/serenity error chain so we
            // can diagnose `ConnectionError::*` variants (gateway timeout,
            // driver setup, encryption, etc.) which Display alone hides
            // behind "establishing connection failed".
            let mut chain: Vec<String> = vec![join_err.to_string()];
            let mut current = std::error::Error::source(&join_err);
            while let Some(src) = current {
                chain.push(src.to_string());
                current = src.source();
            }
            // Earlier #2054 fallback re-used `manager.get(guild_id)` as
            // proof-of-connection — that was wrong. `get()` returns Some after
            // any `get_or_insert`, including the empty Call created at the
            // start of every join(). The receiver attached to such a zombie
            // never fires SpeakingStateUpdate/VoiceTick because no UDP socket
            // is bound. Detect real connection via current_connection() and
            // only re-attach if Complete.
            if let Some(existing) = manager.get(guild_id) {
                let call = existing.lock().await;
                let connected = call.current_connection().is_some();
                drop(call);
                if connected {
                    tracing::warn!(
                        join_error = %join_err,
                        error_chain = ?chain,
                        guild_id = guild_id.get(),
                        channel_id = channel_id.get(),
                        "songbird manager.join() Err but call is actually connected; \
                         attaching receiver retroactively (#2054 connected-zombie fallback)"
                    );
                    existing
                } else {
                    // Zombie call — clean it up so the next attempt starts fresh.
                    let _ = manager.remove(guild_id).await;
                    return Err(anyhow!(join_err)
                        .context(format!(
                            "songbird manager.join() failed (zombie call cleaned) for channel {} in guild {}; error_chain={:?}",
                            channel_id.get(),
                            guild_id.get(),
                            chain
                        ))
                        .into());
                }
            } else {
                return Err(anyhow!(join_err)
                    .context(format!(
                        "songbird manager.join() failed for channel {} in guild {}; error_chain={:?}",
                        channel_id.get(),
                        guild_id.get(),
                        chain
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
    voice_occupancy().remove(&(data.provider.as_str().to_string(), guild_id.get()));
    // F2 (#2046): voice_guilds DashMap에서 guild_id에 매핑된 control_channel_id들을
    // 먼저 모은 다음 unregister한다. 이후 receiver flush는 해당 channel scope으로만 한다 —
    // 멀티-길드 환경에서 다른 길드의 진행 중인 utterance·SSRC 매핑을 보존한다.
    let control_channel_ids = data
        .shared
        .voice_barge_in
        .control_channel_ids_for_guild(guild_id);
    data.shared.voice_barge_in.unregister_voice_guild(guild_id);
    let mut flushed = 0usize;
    for cc_id in control_channel_ids {
        flushed += data
            .voice_receiver
            .flush_for_control_channel(cc_id)
            .await
            .len();
    }
    Ok(flushed)
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
    // songbird 0.6: DecodeMode::Decode is now a tuple variant holding a
    // DecodeConfig (channels + sample_rate). 0.4 had separate `.decode_channels()`
    // and `.decode_sample_rate()` builder methods — those were removed.
    songbird::Config::default().decode_mode(songbird::driver::DecodeMode::Decode(
        songbird::driver::DecodeConfig::new(
            songbird::driver::Channels::Stereo,
            songbird::driver::SampleRate::Hz48000,
        ),
    ))
}

pub(in crate::services::discord) fn register_songbird(
    builder: serenity::ClientBuilder,
) -> serenity::ClientBuilder {
    builder.register_songbird_from_config(songbird_decode_config())
}

/// Process-level voice occupancy registry, shared across every provider's
/// `run_bot()` instance in the same ADK process. Keyed by `(provider, guild_id)`:
/// Discord allows one bot-token to hold one voice connection per guild, so the
/// key has provider in it (different bots can coexist in the same guild on
/// different channels).
fn voice_occupancy() -> &'static dashmap::DashMap<(String, u64), u64> {
    static REGISTRY: std::sync::OnceLock<dashmap::DashMap<(String, u64), u64>> =
        std::sync::OnceLock::new();
    REGISTRY.get_or_init(dashmap::DashMap::new)
}

/// Track which (voice_channel, kind) auto-join notifications were already sent
/// so a single process lifetime emits each at most once.
fn voice_notify_dedup() -> &'static dashmap::DashSet<(u64, &'static str)> {
    static DEDUP: std::sync::OnceLock<dashmap::DashSet<(u64, &'static str)>> =
        std::sync::OnceLock::new();
    DEDUP.get_or_init(dashmap::DashSet::new)
}

fn voice_notify_should_send(channel_id: ChannelId, kind: &'static str) -> bool {
    voice_notify_dedup().insert((channel_id.get(), kind))
}

async fn notify_voice_alert(channel_id: ChannelId, content: String, kind: &'static str) {
    if !voice_notify_should_send(channel_id, kind) {
        return;
    }
    let Some(token) = crate::credential::read_bot_token("notify") else {
        tracing::warn!(
            channel_id = channel_id.get(),
            kind,
            "voice auto-join alert suppressed: notify bot token not configured"
        );
        return;
    };
    let client = reqwest::Client::new();
    let base = crate::server::routes::dispatches::discord_delivery::discord_api_base_url();
    let target = channel_id.get().to_string();
    if let Err(error) = crate::server::routes::dispatches::discord_delivery::post_raw_message_once(
        &client, &token, &base, &target, &content,
    )
    .await
    {
        tracing::warn!(
            channel_id = channel_id.get(),
            kind,
            error = %error,
            "voice auto-join alert delivery failed via notify bot"
        );
    }
}

#[cfg(test)]
mod auto_join_tests {
    use super::*;

    #[test]
    fn occupancy_registry_isolates_providers_per_guild() {
        let registry = voice_occupancy();
        // Use guild ids that are unlikely to collide with other tests in the
        // same process (registry is a process-level singleton).
        let guild_id: u64 = 0xC0FFEE_0000_0001;
        registry.insert(("claude".to_string(), guild_id), 1001);
        registry.insert(("codex".to_string(), guild_id), 1002);

        assert_eq!(
            registry.get(&("claude".to_string(), guild_id)).map(|v| *v),
            Some(1001)
        );
        assert_eq!(
            registry.get(&("codex".to_string(), guild_id)).map(|v| *v),
            Some(1002)
        );

        registry.remove(&("claude".to_string(), guild_id));
        assert!(registry.get(&("claude".to_string(), guild_id)).is_none());
        assert_eq!(
            registry.get(&("codex".to_string(), guild_id)).map(|v| *v),
            Some(1002)
        );

        registry.remove(&("codex".to_string(), guild_id));
    }

    #[test]
    fn notify_dedup_emits_once_per_channel_kind_pair() {
        // Use a channel id well outside the production range so other tests
        // and live runtime calls do not race against the same dedup slot.
        let channel = ChannelId::new(0xDEAD_BEEF_AAAA_0001);
        assert!(voice_notify_should_send(channel, "mapping-missing"));
        assert!(!voice_notify_should_send(channel, "mapping-missing"));
        // Different kind on the same channel is independently tracked.
        assert!(voice_notify_should_send(channel, "no-fallback"));
        assert!(!voice_notify_should_send(channel, "no-fallback"));
    }
}
