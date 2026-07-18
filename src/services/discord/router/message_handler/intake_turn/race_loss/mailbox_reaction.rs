use super::*;

pub(super) async fn clear_rejected_attempt_pending(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    turn_start_attempt: Option<crate::services::discord::turn_view_reconciler::TurnStartAttempt>,
) {
    crate::services::discord::turn_view_reconciler::note_intake_turn_cleared_current_if_attempt_matches(
        shared,
        http,
        channel_id,
        user_msg_id,
        turn_start_attempt,
        "race_loss_enqueue_rejected",
    )
    .await;
}

pub(in crate::services::discord::router::message_handler::intake_turn) async fn note_busy_tui_pre_submit_queue_pending(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    merged: bool,
    turn_start_attempt: Option<crate::services::discord::turn_view_reconciler::TurnStartAttempt>,
) {
    let emoji = if merged {
        crate::services::discord::queue_reactions::QUEUE_MERGED_PENDING_REACTION
    } else {
        crate::services::discord::queue_reactions::QUEUE_STANDALONE_PENDING_REACTION
    };
    note_queue_pending(
        shared,
        http,
        channel_id,
        user_msg_id,
        emoji,
        turn_start_attempt,
        "tui_busy_pre_submit_message_queued",
    )
    .await;
}

pub(super) async fn note_queue_pending(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    emoji: char,
    turn_start_attempt: Option<crate::services::discord::turn_view_reconciler::TurnStartAttempt>,
    source: &'static str,
) {
    if emoji == crate::services::discord::queue_reactions::QUEUE_STANDALONE_PENDING_REACTION
        && let Some(turn_start_attempt) = turn_start_attempt
    {
        crate::services::discord::turn_view_reconciler::note_intake_start_rolled_back_to_queued_current(
            shared,
            channel_id,
            user_msg_id,
            turn_start_attempt,
            source,
        )
        .await;
    }
    // #4554: rollback is attempt-scoped and may legitimately be unavailable or
    // refused after a newer start attempt. The mailbox enqueue is authoritative,
    // so always publish the desired queue marker through the reconciler. When the
    // rollback already installed 📬 this coalesces without a second HTTP add.
    let delivered = crate::services::discord::queue_marker::note_added_current(
        shared,
        http,
        channel_id,
        user_msg_id,
        emoji,
        source,
    )
    .await;
    crate::services::discord::outbound::reaction_control::ensure_queue_reaction_or_fallback_http(
        http,
        channel_id,
        shared,
        user_msg_id,
        delivered,
    )
    .await;
}
