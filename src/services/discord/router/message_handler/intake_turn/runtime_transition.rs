use super::*;
use crate::services::discord::router::turn_start::IntakeRuntimeTransition;

pub(super) async fn acquire_after_redirect_or_requeue(
    runtime: (
        &Arc<serenity::http::Http>,
        &Arc<SharedData>,
        &str,
        &ProviderKind,
    ),
    channels: (ChannelId, ChannelId),
    request: (TurnKind, UserId, MessageId, &str),
    reply: (&Option<String>, bool, bool),
    uploads: (
        &mut crate::services::cluster::attachment_transfer::uploads::PendingUploads,
        &Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
    ),
    requeue: (
        bool,
        &Option<String>,
        Option<crate::services::discord::turn_view_reconciler::TurnStartAttempt>,
        bool,
        (bool, super::adk_thread::Route),
    ),
    fallback_state: (Option<String>, bool, String),
) -> Result<Option<IntakeRuntimeTransition>, Error> {
    let (http, shared, token, provider) = runtime;
    let (channel_id, original_channel_id) = channels;
    let (turn_kind, original_request_owner, user_msg_id, user_text) = request;
    let (reply_context, has_reply_boundary, merge_consecutive) = reply;
    let (pending_uploads, voice_announcement) = uploads;
    let (
        reply_to_user_message,
        dispatch_id_for_thread,
        turn_start_attempt,
        preserve_on_cancel,
        (queued_drain, route),
    ) = requeue;
    // Busy queued drains return their already owned uploads to the restore owner.
    // Fresh input keeps the durable tail enqueue and slow wake policy.
    let admits = crate::services::discord::commands::native_clear_admits;
    let admitted = match try_intake_runtime_transition_after_redirect(
        shared,
        channel_id,
        fallback_state,
    )
    .await
    {
        Ok(mut t) => admits(
            http,
            shared,
            provider,
            channel_id,
            &mut t.state,
            &mut t.recovered_fresh,
        )
        .await
        .then_some(t),
        Err(_) => None,
    };
    match admitted {
        Some(transition) => Ok(Some(transition)),
        None => {
            if queued_drain {
                return Err(Box::new(
                    super::super::super::intake_dispatch::QueuedTransitionBusyHandoff {
                        owned_uploads: std::mem::take(pending_uploads),
                    },
                ));
            }
            tracing::warn!(
                channel_id = channel_id.get(),
                "session transition is busy; preserving intake immediately as a durable queued intervention"
            );
            // Ordinary input already owns its R1 uploads; later arrivals stay
            // in the session. Locally completable input leaves them there too.
            race_loss::handle_race_loss_enqueue(
                http,
                shared,
                token,
                provider,
                channel_id,
                original_channel_id,
                turn_kind,
                original_request_owner,
                user_msg_id,
                user_text,
                reply_context,
                has_reply_boundary,
                merge_consecutive,
                pending_uploads,
                voice_announcement,
                reply_to_user_message,
                dispatch_id_for_thread,
                turn_start_attempt,
                preserve_on_cancel,
                race_loss::QueuedIntakeCause::SessionTransitionBusy,
                route,
            )
            .await?;
            Ok(None)
        }
    }
}

#[cfg(all(test, unix))]
mod native_hold_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use pre_admission_control::{
        pre_admission_control_tests::{fixture, session_with},
        take_channel_input_state,
    };
    const RUNTIME_TRANSITION: &str = include_str!("runtime_transition.rs");
    const FORWARD_OWNED: &str = "                pending_uploads,";

    /// Classification 2: production text before the unit tests, comments excluded. The
    /// busy handoff forwards its owned uploads and never takes session input state.
    fn handoff_ownership_gaps(source: &str) -> Vec<String> {
        use crate::services::discord::router::source_guard_support::Lexed;
        let lexed = Lexed::new(source);
        let Some(end) = lexed.find("#[cfg(test)]") else {
            return vec!["missing the unit-test boundary".to_string()];
        };
        let src = lexed.slice(0..end);
        let mut gaps = Vec::new();
        if src.text().contains("take_channel_input_state") {
            gaps.push("handoff must not take session state".to_string());
        }
        if !src.contains(FORWARD_OWNED) {
            gaps.push("handoff must forward its owned pending uploads".to_string());
        }
        gaps
    }

    #[test]
    fn handoff_ownership_guard_ignores_comments() {
        use crate::services::discord::router::source_guard_support::forge;
        use crate::test_support::CommentForgery;
        for forgery in CommentForgery::ALL {
            let forged = forge(RUNTIME_TRANSITION, FORWARD_OWNED, forgery);
            assert!(forged.contains(FORWARD_OWNED));
            assert_eq!(handoff_ownership_gaps(&forged).len(), 1, "{forgery:?}");
        }
        let take = "take_channel_input_state(shared, channel_id)";
        let prose = RUNTIME_TRANSITION.replacen(
            FORWARD_OWNED,
            &format!("{FORWARD_OWNED} // {take}\n/* outer /* {take} */ */"),
            1,
        );
        assert_eq!(handoff_ownership_gaps(&prose), Vec::<String>::new());
        let coded =
            RUNTIME_TRANSITION.replacen(FORWARD_OWNED, &format!("{FORWARD_OWNED} {take},"), 1);
        assert_eq!(handoff_ownership_gaps(&coded).len(), 1);
    }

    fn route(target: ChannelId) -> super::super::adk_thread::Route {
        super::super::adk_thread::Route {
            target,
            parent: None,
            role: None,
        }
    }
    #[tokio::test]
    #[rustfmt::skip]
    async fn runtime_transition_busy_forwards_owned_uploads_without_taking_session_state() {
        let root = tempfile::tempdir().unwrap();
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
        let (shared, channel) = fixture(Some(session_with(&["U1"], true))).await;
        let (mut owned, was_cleared) = take_channel_input_state(&shared, channel).await;
        assert!(was_cleared);
        let mut core = shared.core.lock().await;
        assert!(!core.sessions[&channel].cleared, "R1 consumes the original cleared state");
        // State arriving after R1 belongs to the session, not this handoff.
        *core.sessions.get_mut(&channel).unwrap() = session_with(&["U2"], true);
        drop(core);
        let _held = shared.session_transition_lock(channel).lock_owned().await;
        let http = Arc::new(serenity::HttpBuilder::new("test-token")
            .proxy("http://127.0.0.1:1").ratelimiter_disabled(true).build());
        // S3-19b: the held transition must reach the real busy enqueue path.
        let transition = tokio::time::timeout(std::time::Duration::from_secs(5),
            acquire_after_redirect_or_requeue(
                (&http, &shared, "test-token", &ProviderKind::Codex), (channel, channel),
                (TurnKind::Foreground, UserId::new(1), MessageId::new(566_003), "A"),
                (&None, false, false), (&mut owned, &None),
                (false, &None, None, false, (false, route(channel))), (None, false, String::new()),
            )).await.expect("busy handoff must finish").expect("busy enqueue must succeed");
        assert!(transition.is_none(), "held transition must requeue, not acquire");
        let queued = crate::services::discord::mailbox_snapshot(&shared, channel).await;
        assert_eq!(queued.intervention_queue.len(), 1);
        assert_eq!(queued.intervention_queue[0].message_id, MessageId::new(566_003));
        assert_eq!(queued.intervention_queue[0].pending_uploads, ["U1"]);
        assert_eq!(owned, ["U1"]);
        let core = shared.core.lock().await;
        assert_eq!(core.sessions[&channel].pending_uploads, ["U2"]);
        assert!(core.sessions[&channel].cleared, "busy handoff must preserve cleared");
        assert_eq!(handoff_ownership_gaps(RUNTIME_TRANSITION), Vec::<String>::new());
    }
    #[tokio::test]
    async fn queued_busy_returns_only_owned_vector_and_keeps_later_session_uploads() {
        let (shared, channel) = fixture(Some(session_with(&["U2"], true))).await;
        let _held = shared.session_transition_lock(channel).lock_owned().await;
        let http = Arc::new(serenity::Http::new("test-token"));
        let mut owned = vec!["U1".into(), "Q".into()];
        let result = acquire_after_redirect_or_requeue(
            (&http, &shared, "test-token", &ProviderKind::Codex),
            (channel, channel),
            (
                TurnKind::Foreground,
                UserId::new(1),
                MessageId::new(566_004),
                "A",
            ),
            (&None, false, false),
            (&mut owned, &None),
            (false, &None, None, false, (true, route(channel))),
            (None, false, String::new()),
        )
        .await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("queued busy returns owned handoff"),
        };
        let handoff = error
            .downcast::<super::super::super::super::intake_dispatch::QueuedTransitionBusyHandoff>()
            .expect("typed handoff");
        assert_eq!(handoff.owned_uploads, ["U1", "Q"]);
        assert!(owned.is_empty());
        assert_eq!(
            shared.core.lock().await.sessions[&channel].pending_uploads,
            ["U2"]
        );
        assert!(
            shared
                .mailbox(channel)
                .snapshot()
                .await
                .intervention_queue
                .is_empty()
        );
        shared.mailboxes.remove_fixture_for_test(channel);
    }
}
