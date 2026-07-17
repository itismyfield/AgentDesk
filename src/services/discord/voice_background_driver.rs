use std::{future::Future, pin::Pin, sync::Arc};

use poise::serenity_prelude::{ChannelId, GuildId, MessageId};

use super::SharedData;

const VOICE_ANNOUNCE_DEFAULT_GENERATION: u64 = 1;

/// Boundary between voice foreground interaction and long-running provider work.
///
/// Voice foreground owns STT transcript intake, short acknowledgements, TTS,
/// barge-in, cancel/resume commands, progress mirroring, and Discord chat logs.
/// A background driver owns the long-running turn trigger boundary. Voice
/// must not call provider/headless execution directly: the canonical trigger is
/// an announce-bot transcript message in the routed text channel, which then
/// flows through the normal Discord intake and turn bridge.
///
/// Production voice start currently has one supported background trigger:
/// `AnnounceBotTranscript`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum VoiceBackgroundDriverKind {
    AnnounceBotTranscript,
}

impl VoiceBackgroundDriverKind {
    pub(in crate::services::discord) const fn as_str(self) -> &'static str {
        match self {
            Self::AnnounceBotTranscript => "announce_bot_transcript",
        }
    }
}

// #3034: voice-driver capability introspection surface — the `capabilities()`
// trait method and this struct/const are exercised by the driver unit tests but
// not yet read by a live capability-gating callsite. Kept as the trait's
// capability contract.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) struct VoiceBackgroundDriverCapabilities {
    pub start: bool,
    pub follow_up: bool,
    pub cancel: bool,
    pub resume: bool,
    pub progress_observation: bool,
    pub terminal_result_delivery: bool,
}

impl VoiceBackgroundDriverCapabilities {
    #[allow(dead_code)] // #3034: capability preset for the announce-bot driver, see above.
    const ANNOUNCE_BOT_TRANSCRIPT: Self = Self {
        start: true,
        follow_up: true,
        cancel: true,
        resume: true,
        progress_observation: true,
        terminal_result_delivery: true,
    };
}

pub(in crate::services::discord) struct VoiceBackgroundStartRequest<'a> {
    pub guild_id: Option<GuildId>,
    pub voice_channel_id: ChannelId,
    pub channel_id: ChannelId,
    pub shared: &'a Arc<SharedData>,
    pub utterance_id: &'a str,
    pub generation: u64,
    pub message_content: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct VoiceAnnounceDeliveryId {
    pub correlation_id: String,
    pub semantic_event_id: String,
}

impl VoiceAnnounceDeliveryId {
    fn as_manual(&self) -> super::health::ManualOutboundDeliveryId<'_> {
        super::health::ManualOutboundDeliveryId {
            correlation_id: &self.correlation_id,
            semantic_event_id: &self.semantic_event_id,
        }
    }
}

pub(in crate::services::discord) fn default_voice_announce_generation() -> u64 {
    VOICE_ANNOUNCE_DEFAULT_GENERATION
}

pub(in crate::services::discord) fn voice_announce_delivery_id(
    guild_id: GuildId,
    voice_channel_id: ChannelId,
    utterance_id: &str,
    generation: u64,
) -> VoiceAnnounceDeliveryId {
    VoiceAnnounceDeliveryId {
        correlation_id: format!(
            "voice:{}:{}:{}",
            guild_id.get(),
            voice_channel_id.get(),
            utterance_id
        ),
        semantic_event_id: format!("announce:generation:{generation}"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct VoiceBackgroundStartOutcome {
    pub turn_id: String,
    pub driver_kind: VoiceBackgroundDriverKind,
    pub message_id: Option<MessageId>,
}

pub(in crate::services::discord) trait VoiceBackgroundTurnDriver {
    fn kind(&self) -> VoiceBackgroundDriverKind;
    #[allow(dead_code)] // #3034: capability introspection, test-only so far. See above.
    fn capabilities(&self) -> VoiceBackgroundDriverCapabilities;

    fn start<'a>(
        &'a self,
        request: VoiceBackgroundStartRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<VoiceBackgroundStartOutcome, String>> + Send + 'a>>;
}

#[derive(Debug, Clone, Copy)]
pub(in crate::services::discord) struct AnnounceBotTranscriptDriver;

impl VoiceBackgroundTurnDriver for AnnounceBotTranscriptDriver {
    fn kind(&self) -> VoiceBackgroundDriverKind {
        VoiceBackgroundDriverKind::AnnounceBotTranscript
    }

    fn capabilities(&self) -> VoiceBackgroundDriverCapabilities {
        VoiceBackgroundDriverCapabilities::ANNOUNCE_BOT_TRANSCRIPT
    }

    fn start<'a>(
        &'a self,
        request: VoiceBackgroundStartRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<VoiceBackgroundStartOutcome, String>> + Send + 'a>>
    {
        Box::pin(async move {
            let registry = request
                .shared
                .health_registry()
                .ok_or_else(|| "health registry unavailable".to_string())?;
            let target = format!("channel:{}", request.channel_id.get());
            let delivery_id = request.guild_id.map(|guild_id| {
                voice_announce_delivery_id(
                    guild_id,
                    request.voice_channel_id,
                    request.utterance_id,
                    request.generation,
                )
            });
            let (status, body) = super::health::send_message_with_backends_and_delivery_id(
                &registry,
                request.shared.pg_pool.as_ref(),
                &target,
                request.message_content,
                "voice",
                super::bot_role::UtilityBotRole::Announce.alias(),
                Some("voice transcript"),
                delivery_id.as_ref().map(VoiceAnnounceDeliveryId::as_manual),
            )
            .await;
            if !status.starts_with("200") {
                return Err(format!("announce send returned {status}: {body}"));
            }
            let value = serde_json::from_str::<serde_json::Value>(&body)
                .map_err(|error| format!("parse announce send response: {error}"))?;
            let message_id = value
                .get("message_id")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .and_then(|value| value.parse::<u64>().ok())
                .map(MessageId::new)
                .ok_or_else(|| "announce send response missing message_id".to_string())?;
            Ok(VoiceBackgroundStartOutcome {
                turn_id: format!("voice-announce:{}", message_id.get()),
                driver_kind: self.kind(),
                message_id: Some(message_id),
            })
        })
    }
}

pub(in crate::services::discord) fn select_voice_background_driver() -> AnnounceBotTranscriptDriver
{
    let driver = AnnounceBotTranscriptDriver;
    debug_assert!(candidate_voice_background_driver_kinds().contains(&driver.kind()));
    driver
}

pub(in crate::services::discord) fn candidate_voice_background_driver_kinds()
-> &'static [VoiceBackgroundDriverKind] {
    &[VoiceBackgroundDriverKind::AnnounceBotTranscript]
}

#[cfg(test)]
mod tests {
    use super::{
        VoiceBackgroundDriverKind, VoiceBackgroundTurnDriver,
        candidate_voice_background_driver_kinds, default_voice_announce_generation,
        select_voice_background_driver, voice_announce_delivery_id,
    };
    use poise::serenity_prelude::{ChannelId, GuildId};

    #[test]
    fn voice_announce_delivery_id_is_utterance_bound() {
        let delivery_id = voice_announce_delivery_id(
            GuildId::new(111),
            ChannelId::new(222),
            "20260516-test",
            default_voice_announce_generation(),
        );

        assert_eq!(delivery_id.correlation_id, "voice:111:222:20260516-test");
        assert_eq!(delivery_id.semantic_event_id, "announce:generation:1");
    }

    #[test]
    fn voice_background_candidates_match_selected_driver() {
        assert_eq!(
            candidate_voice_background_driver_kinds(),
            &[VoiceBackgroundDriverKind::AnnounceBotTranscript]
        );
        assert_eq!(
            select_voice_background_driver().kind(),
            VoiceBackgroundDriverKind::AnnounceBotTranscript
        );
        assert!(select_voice_background_driver().capabilities().start);
    }
}
