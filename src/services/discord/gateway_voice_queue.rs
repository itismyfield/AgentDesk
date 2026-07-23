use poise::serenity_prelude::UserId;

use super::Intervention;

pub(super) fn queued_intervention_request_owner(
    intervention: &Intervention,
    fallback_request_owner: UserId,
) -> UserId {
    if intervention.voice_announcement.is_some()
        || crate::voice::prompt::is_readable_voice_transcript_announcement(&intervention.text)
    {
        intervention.author_id
    } else {
        fallback_request_owner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::InterventionMode;
    use poise::serenity_prelude::MessageId;
    use std::time::Instant;

    fn queued_intervention(
        author_id: u64,
        text: &str,
        voice_announcement: Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
    ) -> Intervention {
        Intervention {
            author_id: UserId::new(author_id),
            author_is_bot: true,
            message_id: MessageId::new(50_001),
            queued_generation: crate::services::discord::runtime_store::process_generation(),
            source_message_ids: vec![MessageId::new(50_001)],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: text.to_string(),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement,
        }
    }

    fn voice_announcement() -> crate::voice::prompt::VoiceTranscriptAnnouncement {
        crate::voice::prompt::VoiceTranscriptAnnouncement {
            transcript: "상태 알려줘".to_string(),
            user_id: "42".to_string(),
            utterance_id: "utt-gateway-owner".to_string(),
            language: "ko-KR".to_string(),
            verbose_progress: true,
            started_at: Some("2026-06-28T22:00:00+09:00".to_string()),
            completed_at: Some("2026-06-28T22:00:01+09:00".to_string()),
            samples_written: Some(48_000),
            control_channel_id: Some(300),
            stt_mode: Some("file".to_string()),
            stt_latency_ms: Some(120),
        }
    }

    #[test]
    fn uses_author_for_embedded_or_readable_voice_metadata() {
        let fallback = UserId::new(7);
        let announce_bot = UserId::new(88);
        let embedded = queued_intervention(
            announce_bot.get(),
            "plain fallback",
            Some(voice_announcement()),
        );
        assert_eq!(
            queued_intervention_request_owner(&embedded, fallback),
            announce_bot
        );

        let readable = queued_intervention(
            announce_bot.get(),
            "🎙️ \"상태 알려줘\"\n||ADK_VOICE_ANNOUNCE_REF key=voice:1:300:utt-gateway-owner::announce:generation:1||",
            None,
        );
        assert_eq!(
            queued_intervention_request_owner(&readable, fallback),
            announce_bot
        );

        let normal = queued_intervention(announce_bot.get(), "not a voice announcement", None);
        assert_eq!(
            queued_intervention_request_owner(&normal, fallback),
            fallback
        );
    }
}
