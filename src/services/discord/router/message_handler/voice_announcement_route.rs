//! EPIC #3479 — voice-transcript announcement routing extracted from the
//! LoC-frozen `intake_turn.rs`. Carries the durable claim guard, the route
//! outcome enum, and the one-shot foreground/fallback router so the
//! voice-announcement decision is unit-testable in isolation and the frozen
//! intake file only keeps a single call site.

use super::*;

pub(super) async fn claim_voice_transcript_announcement_processing(
    pg_pool: Option<&sqlx::PgPool>,
    channel_id: ChannelId,
    message_id: MessageId,
    already_accepted: bool,
    context: &'static str,
) -> bool {
    if already_accepted {
        return true;
    }
    let Some(pool) = pg_pool else {
        return true;
    };
    match crate::voice::announce_meta::mark_voice_announcement_durable_consumed(pool, message_id)
        .await
    {
        Ok(true) => true,
        Ok(false) => {
            tracing::info!(
                channel_id = channel_id.get(),
                message_id = message_id.get(),
                context,
                "voice transcript announcement durable metadata already claimed; skipping duplicate processing"
            );
            false
        }
        Err(error) => {
            tracing::warn!(
                error = %error,
                channel_id = channel_id.get(),
                message_id = message_id.get(),
                context,
                "voice transcript announcement durable metadata claim failed; skipping processing"
            );
            false
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VoiceTranscriptAnnouncementRouteOutcome {
    NotVoiceAnnouncement,
    DuplicateSuppressed,
    ForegroundHandled,
    FallbackNormalTurn,
}

impl VoiceTranscriptAnnouncementRouteOutcome {
    pub(super) fn bypasses_normal_turn(self) -> bool {
        matches!(self, Self::DuplicateSuppressed | Self::ForegroundHandled)
    }
}

pub(super) async fn route_voice_transcript_announcement_once<F, Fut>(
    pg_pool: Option<&sqlx::PgPool>,
    channel_id: ChannelId,
    message_id: MessageId,
    voice_announcement_already_accepted: bool,
    voice_announcement: Option<&crate::voice::prompt::VoiceTranscriptAnnouncement>,
    mut foreground_handler: F,
) -> VoiceTranscriptAnnouncementRouteOutcome
where
    F: FnMut(crate::voice::prompt::VoiceTranscriptAnnouncement) -> Fut,
    Fut: Future<Output = bool>,
{
    let Some(announcement) = voice_announcement else {
        return VoiceTranscriptAnnouncementRouteOutcome::NotVoiceAnnouncement;
    };
    if !claim_voice_transcript_announcement_processing(
        pg_pool,
        channel_id,
        message_id,
        voice_announcement_already_accepted,
        "handle_text_message_pre_accept",
    )
    .await
    {
        return VoiceTranscriptAnnouncementRouteOutcome::DuplicateSuppressed;
    }
    if foreground_handler(announcement.clone()).await {
        return VoiceTranscriptAnnouncementRouteOutcome::ForegroundHandled;
    }

    let mut event = crate::voice::flight::VoiceFlightEvent::new(
        crate::voice::flight::VoiceFlightRoute::FallbackNormalTurn,
    );
    event.voice_channel_id = Some(channel_id.get());
    event.control_channel_id = Some(announcement.control_channel_id.unwrap_or(channel_id.get()));
    event.user_id = Some(announcement.user_id.clone());
    event.utterance_id = Some(announcement.utterance_id.clone());
    event.stt_mode = announcement.stt_mode.clone();
    event.stt_latency_ms = announcement.stt_latency_ms;
    event.transcript_chars = Some(announcement.transcript.chars().count());
    event.reason = Some("voice_foreground_not_handled".to_string());
    crate::voice::flight::record_voice_flight_event(event);
    VoiceTranscriptAnnouncementRouteOutcome::FallbackNormalTurn
}

#[cfg(test)]
mod voice_route_tests {
    use super::*;
    use poise::serenity_prelude::{ChannelId, MessageId};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn voice_announcement_fixture(
        transcript: &str,
        utterance_id: &str,
    ) -> crate::voice::prompt::VoiceTranscriptAnnouncement {
        crate::voice::prompt::VoiceTranscriptAnnouncement {
            transcript: transcript.to_string(),
            user_id: "42".to_string(),
            utterance_id: utterance_id.to_string(),
            language: "ko-KR".to_string(),
            verbose_progress: true,
            started_at: Some("2026-05-24T21:00:00+09:00".to_string()),
            completed_at: Some("2026-05-24T21:00:01+09:00".to_string()),
            samples_written: Some(48_000),
            control_channel_id: Some(44_001),
            stt_mode: Some("file".to_string()),
            stt_latency_ms: Some(120),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn voice_announcement_foreground_response_bypasses_normal_turn() {
        let channel_id = ChannelId::new(44_001);
        let message_id = MessageId::new(55_001);
        let announcement = voice_announcement_fixture("지금 상태 알려줘", "utt-foreground");
        let calls = Arc::new(AtomicUsize::new(0));

        let outcome = route_voice_transcript_announcement_once(
            None,
            channel_id,
            message_id,
            false,
            Some(&announcement),
            {
                let calls = Arc::clone(&calls);
                move |seen| {
                    let calls = Arc::clone(&calls);
                    async move {
                        assert_eq!(seen.utterance_id, "utt-foreground");
                        calls.fetch_add(1, Ordering::SeqCst);
                        true
                    }
                }
            },
        )
        .await;

        assert_eq!(
            outcome,
            VoiceTranscriptAnnouncementRouteOutcome::ForegroundHandled
        );
        assert!(
            outcome.bypasses_normal_turn(),
            "foreground voice responses must return before normal text turn handling"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "foreground handler must be invoked exactly once"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn voice_announcement_foreground_miss_falls_back_to_normal_turn() {
        let channel_id = ChannelId::new(44_002);
        let message_id = MessageId::new(55_002);
        let announcement = voice_announcement_fixture("긴 작업 처리해줘", "utt-fallback");
        let calls = Arc::new(AtomicUsize::new(0));

        let outcome = route_voice_transcript_announcement_once(
            None,
            channel_id,
            message_id,
            false,
            Some(&announcement),
            {
                let calls = Arc::clone(&calls);
                move |_seen| {
                    let calls = Arc::clone(&calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        false
                    }
                }
            },
        )
        .await;

        assert_eq!(
            outcome,
            VoiceTranscriptAnnouncementRouteOutcome::FallbackNormalTurn
        );
        assert!(
            !outcome.bypasses_normal_turn(),
            "only an actual foreground response or duplicate claim may bypass the normal turn"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn durable_duplicate_voice_announcement_invokes_foreground_once() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let channel_id = ChannelId::new(44_003);
        let message_id = MessageId::new(55_003);
        let announcement = voice_announcement_fixture("상태 알려줘", "utt-durable-once");
        let pending_key = crate::voice::announce_meta::durable_voice_announcement_pending_key(
            "voice:1:44003:utt-durable-once",
            "announce:generation:1",
        );

        crate::voice::announce_meta::persist_voice_announcement_reservation_durable(
            &pool,
            &pending_key,
            channel_id,
            "🎙️ \"상태 알려줘\"",
            &announcement,
        )
        .await
        .expect("persist durable voice announcement");
        assert!(
            crate::voice::announce_meta::bind_voice_announcement_durable_message_id(
                &pool,
                &pending_key,
                message_id,
            )
            .await
            .expect("bind durable announcement message id")
        );

        let calls = Arc::new(AtomicUsize::new(0));
        let first = route_voice_transcript_announcement_once(
            Some(&pool),
            channel_id,
            message_id,
            false,
            Some(&announcement),
            {
                let calls = Arc::clone(&calls);
                move |seen| {
                    let calls = Arc::clone(&calls);
                    async move {
                        assert_eq!(seen.utterance_id, "utt-durable-once");
                        calls.fetch_add(1, Ordering::SeqCst);
                        true
                    }
                }
            },
        )
        .await;
        let duplicate = route_voice_transcript_announcement_once(
            Some(&pool),
            channel_id,
            message_id,
            false,
            Some(&announcement),
            {
                let calls = Arc::clone(&calls);
                move |_seen| {
                    let calls = Arc::clone(&calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        true
                    }
                }
            },
        )
        .await;

        assert_eq!(
            first,
            VoiceTranscriptAnnouncementRouteOutcome::ForegroundHandled
        );
        assert_eq!(
            duplicate,
            VoiceTranscriptAnnouncementRouteOutcome::DuplicateSuppressed
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "durable consumed_at must suppress duplicate Discord delivery before foreground handling"
        );

        pool.close().await;
        pg_db.drop().await;
    }
}
