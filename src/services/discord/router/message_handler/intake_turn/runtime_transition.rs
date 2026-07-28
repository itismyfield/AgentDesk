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
    match try_intake_runtime_transition_after_redirect(shared, channel_id, fallback_state).await {
        Ok(transition) => Ok(Some(transition)),
        Err(_) => {
            tracing::warn!(
                channel_id = channel_id.get(),
                "session transition is busy; preserving intake immediately as a durable queued intervention"
            );
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
            )
            .await?;
            Ok(None)
        }
    }
}
