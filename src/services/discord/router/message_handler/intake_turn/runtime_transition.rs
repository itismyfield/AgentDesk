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
        &[String],
        &Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
    ),
    requeue: (
        bool,
        &Option<String>,
        Option<crate::services::discord::turn_view_reconciler::TurnStartAttempt>,
        bool,
    ),
    fallback_state: (Option<String>, bool, String),
) -> Result<Option<IntakeRuntimeTransition>, Error> {
    let (http, shared, token, provider) = runtime;
    let (channel_id, original_channel_id) = channels;
    let (turn_kind, original_request_owner, user_msg_id, user_text) = request;
    let (reply_context, has_reply_boundary, merge_consecutive) = reply;
    let (pending_uploads, voice_announcement) = uploads;
    let (reply_to_user_message, dispatch_id_for_thread, turn_start_attempt, preserve_on_cancel) =
        requeue;
    // Redirect resolution is complete. Never wait outside durable storage for a
    // concurrent `/resume`: if the channel transition is already held, enqueue
    // immediately and let the normal queued consumer retry after the transition.
    // This removes the process-crash loss window that existed while intake waited
    // up to three seconds with the event only on this task's stack.
    //
    // #5170: the enqueue is unchanged, but it is NOT a race loss. Nothing
    // claimed the mailbox here — the transition lock was simply held at this
    // instant — so the requeue is tagged `SessionTransitionBusy` and takes the
    // deferred wake policy (the fixed 60s fail-open backstop) rather than the race-loss
    // edge-trigger recheck, whose own transition wait re-entered intake against
    // a lock it had itself made unacquirable, requeueing on every rotation.
    match try_intake_runtime_transition_after_redirect(shared, channel_id, fallback_state).await {
        Ok(transition) => Ok(Some(transition)),
        Err(_) => {
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
            )
            .await?;
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pre_admission_control::{
        pre_admission_control_tests::{fixture, session_with},
        take_channel_input_state,
    };
    #[tokio::test]
    #[rustfmt::skip]
    async fn runtime_transition_busy_forwards_owned_uploads_without_taking_session_state() {
        let root = tempfile::tempdir().unwrap();
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
        let (shared, channel) = fixture(Some(session_with(&["U1"], true))).await;
        let (owned, was_cleared) = take_channel_input_state(&shared, channel).await;
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
                (&None, false, false), (&owned, &None),
                (false, &None, None, false), (None, false, String::new()),
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
        let src = include_str!("runtime_transition.rs").split("#[cfg(test)]").next().unwrap();
        assert!(!src.contains("take_channel_input_state"), "handoff must not take session state");
        assert!(!src.contains("mem::take"), "handoff forwards only already-owned uploads");
        assert!(src.contains("                pending_uploads,"));
    }
}
