use super::*;
use crate::services::discord::router::turn_start::IntakeRuntimeTransition;

#[allow(clippy::too_many_arguments)]
pub(super) async fn acquire_after_redirect_or_requeue(
    http: &Arc<serenity::http::Http>,
    shared: &Arc<SharedData>,
    token: &str,
    provider: &ProviderKind,
    channel_id: ChannelId,
    original_channel_id: ChannelId,
    turn_kind: TurnKind,
    original_request_owner: UserId,
    user_msg_id: MessageId,
    user_text: &str,
    reply_context: &Option<String>,
    has_reply_boundary: bool,
    merge_consecutive: bool,
    pending_uploads: &[String],
    voice_announcement: &Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
    reply_to_user_message: bool,
    dispatch_id_for_thread: &Option<String>,
    turn_start_attempt: Option<crate::services::discord::turn_view_reconciler::TurnStartAttempt>,
    preserve_on_cancel: bool,
    fallback_state: (Option<String>, bool, String),
) -> Result<Option<IntakeRuntimeTransition>, Error> {
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
