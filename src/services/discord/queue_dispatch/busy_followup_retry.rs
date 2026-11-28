use serenity::model::id::{ChannelId, MessageId, UserId};

use super::super::busy_followup_retry_store::FollowupRetryRow;
use super::super::shared_data::SharedData;
use super::{MailboxEnqueueOutcome, mailbox_requeue_intervention_front};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{Intervention, SourceMessageQueuedGeneration};

/// Front-restore an inflight Claude TUI follow-up that failed before submission;
/// it predates queued interventions, and the deferred kickoff prevents hot loops.
pub(in crate::services::discord) async fn requeue_inflight_for_followup_retry(
    shared: &std::sync::Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    inflight_state: &dyn FollowupRetryRow,
) -> MailboxEnqueueOutcome {
    // Boxed so the futures of callers that await a retry stay small.
    Box::pin(requeue_inflight(
        shared,
        provider,
        channel_id,
        inflight_state,
    ))
    .await
}

async fn requeue_inflight(
    shared: &std::sync::Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    inflight_state: &dyn FollowupRetryRow,
) -> MailboxEnqueueOutcome {
    let user_msg_id = inflight_state.user_msg_id();
    let retry_user_msg_id = inflight_state.effective_busy_followup_retry_user_msg_id();
    if user_msg_id == 0 || inflight_state.user_text().trim().is_empty() {
        return MailboxEnqueueOutcome::default();
    }
    let message_id = MessageId::new(user_msg_id);
    let retry_message_id = MessageId::new(retry_user_msg_id);
    let queued_generation = shared.restart.current_generation;
    // Every merged id rides along so catch-up never recovers one twice. Row order stays because
    // the text fallback maps lines to ids by position; a missing primary or retry goes last.
    let mut source_message_ids: Vec<MessageId> = inflight_state
        .source_message_ids()
        .iter()
        .filter(|id| **id != 0)
        .map(|id| MessageId::new(*id))
        .collect();
    source_message_ids.extend([message_id, retry_message_id]);
    let mut seen = std::collections::HashSet::new();
    source_message_ids.retain(|id| seen.insert(*id));
    let source_message_queued_generations = source_message_ids
        .iter()
        .map(|&source_id| {
            if source_id == message_id && inflight_state.followup_preserve_on_cancel() {
                SourceMessageQueuedGeneration::user_instruction(source_id, queued_generation)
            } else {
                SourceMessageQueuedGeneration::new(source_id, queued_generation)
            }
        })
        .collect();
    let intervention = Intervention {
        author_id: UserId::new(inflight_state.request_owner_user_id()),
        author_is_bot: false,
        message_id,
        queued_generation,
        source_message_ids,
        source_message_queued_generations,
        source_text_segments: Vec::new(),
        text: inflight_state.user_text().to_owned(),
        mode: crate::services::turn_orchestrator::InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: inflight_state.followup_reply_context().clone(),
        has_reply_boundary: inflight_state.followup_has_reply_boundary(),
        merge_consecutive: inflight_state.followup_merge_consecutive(),
        pending_uploads: inflight_state.followup_pending_uploads().clone(),
    };
    mailbox_requeue_intervention_front(shared, provider, channel_id, intervention).await
}
