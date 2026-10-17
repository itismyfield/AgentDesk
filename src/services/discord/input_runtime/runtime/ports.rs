//! Current input policy facts shared by the live boundary and actor.

use std::sync::Arc;

use poise::serenity_prelude::{Channel, ChannelId, ChannelType, Http, MessageId};

use crate::services::discord::input_runtime::fence;
use crate::services::discord::input_runtime::intake::{
    InputDisposition, PolicyFacts, PreparedSource, Protection, TargetFacts, VoiceTarget,
};
use crate::services::discord::{SharedData, discord_io, dispatch_policy, settings};

fn target(channel_id: u64) -> TargetFacts {
    TargetFacts {
        channel_id,
        protection: fence::channel_gate(channel_id).map_or(Protection::Unprotected, |gate| {
            Protection::Protected(gate.provider().clone())
        }),
    }
}

fn source_disposition(shared: &SharedData, source: &PreparedSource) -> Option<InputDisposition> {
    let input = source.input();
    let raw = input["original_text"].as_str()?;
    let kind = input["source_kind"].as_str()?;
    if matches!(kind, "live_message" | "raw_attachment") {
        if !matches!(input["message_kind"].as_str()?, "regular" | "inline_reply") {
            return Some(InputDisposition::Terminal);
        }
        if crate::services::discord::catch_up::handled_command::is_text_command(raw) {
            return Some(InputDisposition::Terminal);
        }
        if let Some(slash) = raw.strip_prefix('/') {
            let name = slash.split_whitespace().next().unwrap_or("");
            return shared.known_slash_commands.get().map(|commands| {
                if commands.contains(name) {
                    InputDisposition::Terminal
                } else {
                    InputDisposition::Prompt
                }
            });
        }
        return Some(InputDisposition::Prompt);
    }
    if !matches!(kind, "text_skill" | "slash_skill") {
        return None;
    }
    let raw = raw.trim_start();
    let raw = if crate::services::discord::catch_up::handled_command::is_text_command(raw)
        && raw.starts_with("<@")
    {
        raw.split_once('>')?.1.trim_start()
    } else {
        raw
    };
    let mut parts = raw.split_whitespace();
    let command = parts.next()?;
    let name = parts.next().unwrap_or("");
    if !matches!(command, "!skill" | "!cc" | "/skill" | "/cc") {
        return Some(InputDisposition::Unsupported);
    }
    if name.is_empty() {
        return Some(InputDisposition::Unsupported);
    }
    Some(
        if matches!(
            name,
            "clear" | "stop" | "pwd" | "health" | "status" | "inflight" | "help"
        ) {
            InputDisposition::Terminal
        } else {
            InputDisposition::Prompt
        },
    )
}

async fn current_route(http: &Arc<Http>, channel: u64) -> Result<(u64, String, bool), ()> {
    let fetched = ChannelId::new(channel)
        .to_channel(http)
        .await
        .map_err(|_| ())?;
    let Channel::Guild(channel) = fetched else {
        return Ok((channel, String::new(), false));
    };
    if matches!(
        channel.kind,
        ChannelType::PublicThread | ChannelType::PrivateThread | ChannelType::NewsThread
    ) {
        let parent = channel.parent_id.ok_or(())?;
        let Channel::Guild(parent_channel) = parent.to_channel(http).await.map_err(|_| ())? else {
            return Err(());
        };
        return Ok((
            parent.get(),
            parent_channel.name,
            matches!(parent_channel.kind, ChannelType::Text | ChannelType::News),
        ));
    }
    Ok((
        channel.id.get(),
        channel.name,
        matches!(channel.kind, ChannelType::Text | ChannelType::News),
    ))
}

/// The live boundary and actor read the same current facts without borrowing the WAL.
pub(in crate::services::discord) async fn source_policy_facts(
    shared: &Arc<SharedData>,
    http: &Arc<Http>,
    source: &PreparedSource,
) -> PolicyFacts {
    use crate::services::discord::health::UtilityBotUserIdResolution;
    let identity = source.identity();
    let current = shared.settings.read().await.clone();
    let announce = if let Some(registry) = shared.health_registry() {
        registry
            .utility_bot_user_id_resolution(
                crate::services::discord::bot_role::UtilityBotRole::Announce,
            )
            .await
    } else {
        UtilityBotUserIdResolution::Unconfigured
    };
    let allowed_bot = current.allowed_bot_ids.contains(&identity.author_id)
        || announce.user_id() == Some(identity.author_id);
    let bot = source.input()["author_is_bot"].as_bool();
    let raw = source.input()["original_text"].as_str();
    let authorized = bot.and_then(|is_bot| {
        if announce == UtilityBotUserIdResolution::Unavailable && !allowed_bot {
            return None;
        }
        Some(
            allowed_bot
                || (!is_bot && discord_io::user_is_authorized(&current, identity.author_id)),
        )
    });
    let mut disposition = source_disposition(shared, source);
    if let Some((is_bot, text)) = bot.zip(raw)
        && (!dispatch_policy::is_allowed_turn_sender(
            &current.allowed_bot_ids,
            announce.user_id(),
            identity.author_id,
            is_bot,
            text,
        ) || dispatch_policy::has_non_turn_provenance(text))
    {
        disposition = Some(InputDisposition::Unsupported);
    }
    let route = current_route(http, identity.original_channel_id).await;
    let name = {
        let core = shared.core.lock().await;
        core.sessions
            .get(&ChannelId::new(identity.execution_channel_id))
            .filter(|session| session.channel_id == Some(identity.execution_channel_id))
            .and_then(|session| session.channel_name.clone())
    };
    let supported = route
        .as_ref()
        .ok()
        .and_then(|(channel, rest_name, supported_kind)| {
            name.as_ref().map(|name| {
                *supported_kind
                    && *channel == identity.execution_channel_id
                    && current.provider == shared.provider
                    && settings::validate_bot_channel_routing(
                        &current,
                        &shared.provider,
                        ChannelId::new(*channel),
                        Some(rest_name),
                        false,
                    )
                    .is_ok()
                    && settings::validate_bot_channel_routing(
                        &current,
                        &shared.provider,
                        ChannelId::new(*channel),
                        Some(name),
                        false,
                    )
                    .is_ok()
            })
        });
    let execution = match route {
        Ok((channel, _, _)) => target(channel),
        Err(()) => TargetFacts {
            channel_id: identity.execution_channel_id,
            protection: Protection::Unknown,
        },
    };
    let stored = if source.input()["source_kind"] == "slash_skill" {
        None
    } else {
        crate::voice::announce_meta::global_store().peek_clone(MessageId::new(source.key()))
    };
    let voice_shape = raw.is_some_and(|raw| {
        crate::voice::prompt::is_voice_transcript_announcement_candidate(raw)
            || crate::voice::prompt::is_readable_voice_transcript_announcement(raw)
    }) || source
        .input()
        .get("voice_announcement")
        .is_some_and(|value| !value.is_null())
        || stored.is_some();
    let voice_target = if source.input()["source_kind"] == "slash_skill" {
        VoiceTarget::NotApplicable
    } else if voice_shape {
        let announcement = if announce.user_id() != Some(identity.author_id) {
            None
        } else if let Some(pool) = &shared.pg_pool {
            crate::voice::announce_meta::load_voice_announcement_durable(
                pool,
                MessageId::new(source.key()),
            )
            .await
            .ok()
            .flatten()
        } else {
            stored
        };
        if let Some(announcement) = announcement {
            if announcement.transcript.trim().is_empty()
                || crate::voice::commands::parse_voice_command(&announcement.transcript).is_some()
            {
                disposition = Some(InputDisposition::Unsupported);
            }
            let same = source
                .input()
                .get("voice_announcement")
                .filter(|value| !value.is_null())
                .is_none_or(|value| {
                    serde_json::to_value(&announcement).is_ok_and(|fresh| &fresh == value)
                });
            if !same || announcement.control_channel_id == Some(0) {
                VoiceTarget::Unknown
            } else {
                match shared
                    .voice_barge_in
                    .announcement_target(
                        ChannelId::new(identity.original_channel_id),
                        &current.provider,
                    )
                    .await
                {
                    Ok(Some(channel)) => VoiceTarget::Resolved(target(channel.get())),
                    Ok(None) => VoiceTarget::NotApplicable,
                    Err(_) => VoiceTarget::Unknown,
                }
            }
        } else {
            VoiceTarget::Unknown
        }
    } else {
        match shared
            .voice_barge_in
            .text_reply_target(
                ChannelId::new(identity.original_channel_id),
                &current.provider,
            )
            .await
        {
            Ok(Some(channel)) => VoiceTarget::Resolved(target(channel.get())),
            Ok(None) => VoiceTarget::NotApplicable,
            Err(_) => VoiceTarget::Unknown,
        }
    };
    let final_announce = if let Some(registry) = shared.health_registry() {
        registry
            .utility_bot_user_id_resolution(
                crate::services::discord::bot_role::UtilityBotRole::Announce,
            )
            .await
    } else {
        UtilityBotUserIdResolution::Unconfigured
    };
    let latest = shared.settings.read().await.clone();
    let latest_name = {
        let core = shared.core.lock().await;
        core.sessions
            .get(&ChannelId::new(identity.execution_channel_id))
            .filter(|session| session.channel_id == Some(identity.execution_channel_id))
            .and_then(|session| session.channel_name.clone())
    };
    let changed = announce != final_announce
        || current.provider != latest.provider
        || current.agent != latest.agent
        || current.owner_user_id != latest.owner_user_id
        || current.allowed_user_ids != latest.allowed_user_ids
        || current.allow_all_users != latest.allow_all_users
        || current.allowed_bot_ids != latest.allowed_bot_ids
        || current.allowed_channel_ids != latest.allowed_channel_ids
        || current.require_mention_channel_ids != latest.require_mention_channel_ids
        || name != latest_name;
    #[cfg(test)]
    let changed = changed
        && !crate::services::discord::input_runtime::supervisor::mutant(
            "c2b_policy_change_after_lookup_approved",
        );
    PolicyFacts {
        provider: Some(current.provider),
        original: target(identity.original_channel_id),
        execution,
        voice_target: if changed {
            VoiceTarget::Unknown
        } else {
            voice_target
        },
        authorized: if changed { None } else { authorized },
        supported: if changed { None } else { supported },
        disposition,
        client_ready: true,
        admission_open: true,
    }
}
