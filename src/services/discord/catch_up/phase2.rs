use crate::services::turn_orchestrator::{EnqueueRefusalReason, MAX_INTERVENTIONS_PER_CHANNEL};
use poise::serenity_prelude::{ChannelId, MessageId};

use super::super::MailboxEnqueueOutcome;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase2EnqueueCommit {
    Accepted,
    Duplicate,
    Deferred,
}

pub(super) fn catch_up_enqueue_accepted(outcome: &MailboxEnqueueOutcome) -> bool {
    outcome.enqueued && outcome.persistence_error.is_none()
}

pub(super) fn classify_phase2_enqueue_commit(
    outcome: &MailboxEnqueueOutcome,
) -> Phase2EnqueueCommit {
    if catch_up_enqueue_accepted(outcome) {
        return Phase2EnqueueCommit::Accepted;
    }
    if outcome.persistence_error.is_none()
        && matches!(
            outcome.refusal_reason,
            Some(EnqueueRefusalReason::AlreadyActiveTurn)
                | Some(EnqueueRefusalReason::SourceIdAlreadyQueued)
                | Some(EnqueueRefusalReason::LastItemDedup)
        )
    {
        return Phase2EnqueueCommit::Duplicate;
    }
    Phase2EnqueueCommit::Deferred
}

pub(super) fn catch_up_remaining_queue_capacity(queue_len: usize) -> usize {
    MAX_INTERVENTIONS_PER_CHANNEL.saturating_sub(queue_len)
}

pub(super) fn advance_phase2_checkpoint(checkpoint: Option<u64>, message_id: u64) -> Option<u64> {
    Some(checkpoint.map_or(message_id, |saved| saved.max(message_id)))
}

pub(super) fn phase2_retry_after_checkpoint(
    max_recovered_id: Option<u64>,
    phase2_checkpoint: Option<u64>,
    last_bot_response_id: u64,
) -> u64 {
    match (max_recovered_id, phase2_checkpoint) {
        (Some(recovered), Some(checkpoint)) => recovered.max(checkpoint),
        (Some(recovered), None) => recovered,
        (None, Some(checkpoint)) => checkpoint,
        (None, None) => last_bot_response_id,
    }
}

pub(super) fn log_catch_up_enqueue_not_accepted(
    phase: &'static str,
    channel_id: ChannelId,
    message_id: MessageId,
    outcome: &MailboxEnqueueOutcome,
) {
    let ts = chrono::Local::now().format("%H:%M:%S");
    let refusal = outcome
        .refusal_reason
        .map(|reason| reason.as_str())
        .unwrap_or("none");
    let persistence_error = outcome.persistence_error.as_deref().unwrap_or("none");
    tracing::warn!(
        "  [{ts}] ⚠ catch-up {phase}: message {} in channel {} was not committed to queue (enqueued={} merged={} refusal={} persistence_error={})",
        message_id,
        channel_id,
        outcome.enqueued,
        outcome.merged,
        refusal,
        persistence_error
    );
}

#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Phase2RecoveryStats {
    pub(super) returned: usize,
    pub(super) discovered: usize,
    pub(super) eligible: usize,
    pub(super) duplicate: usize,
    pub(super) skipped: usize,
    pub(super) enqueued: usize,
    pub(super) deferred: usize,
}
