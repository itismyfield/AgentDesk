use super::*;

/// Restore a dequeued hosted-TUI follow-up at the queue front.
///
/// The item was the earliest dispatchable soft intervention before the busy
/// pre-submit failure. Front restoration preserves its position relative to
/// interventions that arrived later; a tail enqueue would reverse FIFO order.
#[allow(clippy::too_many_arguments)]
pub(super) async fn enqueue_busy_tui_followup_for_retry(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    request_owner: serenity::UserId,
    user_msg_id: serenity::MessageId,
    user_text: &str,
    preserve_on_cancel: bool,
    reply_context: Option<String>,
    has_reply_boundary: bool,
    merge_consecutive: bool,
    pending_uploads: Vec<String>,
    voice_announcement: Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
) -> MailboxEnqueueOutcome {
    super::super::super::mailbox_requeue_intervention_front(
        shared,
        provider,
        channel_id,
        build_race_requeued_intervention(
            request_owner,
            user_msg_id,
            user_text,
            preserve_on_cancel,
            reply_context,
            has_reply_boundary,
            merge_consecutive,
            pending_uploads,
            voice_announcement,
        ),
    )
    .await
}

pub(super) async fn enqueue_headless_runtime_mismatch_defer(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    user_msg_id: serenity::MessageId,
    user_text: &str,
) -> MailboxEnqueueOutcome {
    let mut intervention = build_race_requeued_intervention(
        serenity::UserId::new(1),
        user_msg_id,
        user_text,
        false,
        None,
        false,
        false,
        Vec::new(),
        None,
    );
    intervention.author_is_bot = true;
    super::super::super::mailbox_enqueue_intervention(shared, provider, channel_id, intervention)
        .await
}
