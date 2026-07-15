use super::*;

pub(super) async fn note_queue_pending(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    emoji: char,
    turn_start_attempt: Option<crate::services::discord::turn_view_reconciler::TurnStartAttempt>,
) {
    if emoji == crate::services::discord::queue_reactions::QUEUE_STANDALONE_PENDING_REACTION
        && let Some(turn_start_attempt) = turn_start_attempt
    {
        crate::services::discord::turn_view_reconciler::note_intake_start_rolled_back_to_queued_current(
            shared,
            channel_id,
            user_msg_id,
            turn_start_attempt,
            "race_loss_message_queued",
        )
        .await;
    }
    // #4554: rollback is attempt-scoped and may legitimately be unavailable or
    // refused after a newer start attempt. The mailbox enqueue is authoritative,
    // so always publish the desired queue marker through the reconciler. When the
    // rollback already installed 📬 this coalesces without a second HTTP add.
    crate::services::discord::queue_marker::note_added_current(
        shared,
        http,
        channel_id,
        user_msg_id,
        emoji,
        "race_loss_message_queued",
    )
    .await;
}
