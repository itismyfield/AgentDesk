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
    async fn durable_duplicate_voice_announcement_pg_invokes_foreground_once() {
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn direct_dispatch_carry_forward_admits_announcement_and_still_dedups() {
        // #3905: one process hosts many agent-bot gateways and the announce-bot
        // message is delivered to all of them. The intake gate resolves the
        // durable row NON-consumingly, then one gateway wins the durable claim
        // and dispatches. A racing gateway's INDEPENDENT re-derivation inside
        // `handle_text_message` then loads the LIVE row (`consumed_at IS NULL`),
        // finds None, and would WARN-drop ("ignoring voice transcript
        // announcement without authorized durable metadata") a message the gate
        // already authorized. The fix carries the gate's resolved announcement
        // forward into direct dispatch so it stays ADMITTED, while the
        // per-message durable claim still dedups it (no second foreground
        // dispatch / no multi-agent reply storm — #3464 preserved).
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let channel_id = ChannelId::new(44_004);
        let message_id = MessageId::new(55_004);
        let announcement = voice_announcement_fixture("긴 작업 시작해줘", "utt-carry-forward");
        let pending_key = crate::voice::announce_meta::durable_voice_announcement_pending_key(
            "voice:1:44004:utt-carry-forward",
            "announce:generation:1",
        );
        crate::voice::announce_meta::persist_voice_announcement_reservation_durable(
            &pool,
            &pending_key,
            channel_id,
            "🎙️ \"긴 작업 시작해줘\"",
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

        // The first gateway carries the gate-resolved announcement forward,
        // wins the per-message durable claim, and dispatches: ADMITTED.
        let winner_calls = Arc::new(AtomicUsize::new(0));
        let winner = route_voice_transcript_announcement_once(
            Some(&pool),
            channel_id,
            message_id,
            false,
            Some(&announcement),
            {
                let calls = Arc::clone(&winner_calls);
                move |seen| {
                    let calls = Arc::clone(&calls);
                    async move {
                        assert_eq!(seen.utterance_id, "utt-carry-forward");
                        calls.fetch_add(1, Ordering::SeqCst);
                        true
                    }
                }
            },
        )
        .await;
        assert_eq!(
            winner,
            VoiceTranscriptAnnouncementRouteOutcome::ForegroundHandled,
            "the carry-forward winner must be admitted and dispatched, not dropped"
        );
        assert_eq!(winner_calls.load(Ordering::SeqCst), 1);

        // The live durable row is now consumed, so the direct path's INDEPENDENT
        // re-derivation observes None — exactly the state that previously
        // produced the unauthorized-metadata WARN drop.
        assert!(
            crate::voice::announce_meta::load_voice_announcement_durable(&pool, message_id)
                .await
                .expect("durable load after consume")
                .is_none(),
            "a consumed row leaves no LIVE durable row for independent re-derivation"
        );
        let announce_bot = Some(1_479_017_284_805_722_200_u64);
        let owner = UserId::new(1_479_017_284_805_722_200);
        assert!(
            super::super::voice_announcement_scope::should_drop_unauthorized_voice_announcement(
                false,
                false,
                true,
                /* voice_announcement_resolved = */ false,
                announce_bot,
                owner,
            ),
            "without the carry-forward the re-derived None drops with a WARN"
        );
        assert!(
            !super::super::voice_announcement_scope::should_drop_unauthorized_voice_announcement(
                false,
                false,
                true,
                /* voice_announcement_resolved = */ true,
                announce_bot,
                owner,
            ),
            "#3905: the gate carry-forward keeps the announcement admitted (no WARN drop)"
        );

        // A racing gateway also carries the gate-resolved announcement forward
        // (already_accepted = false, so the per-message durable claim still
        // runs). Because the row is already consumed, it is DuplicateSuppressed
        // — the owner does NOT double-dispatch, so #3464 dedup holds.
        let loser_calls = Arc::new(AtomicUsize::new(0));
        let loser = route_voice_transcript_announcement_once(
            Some(&pool),
            channel_id,
            message_id,
            false,
            Some(&announcement),
            {
                let calls = Arc::clone(&loser_calls);
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
            loser,
            VoiceTranscriptAnnouncementRouteOutcome::DuplicateSuppressed,
            "the carry-forward must not re-dispatch an utterance another gateway already claimed"
        );
        assert_eq!(
            loser_calls.load(Ordering::SeqCst),
            0,
            "the per-message durable claim dedups the carry-forward (no reply storm)"
        );

        pool.close().await;
        pg_db.drop().await;
    }
}
