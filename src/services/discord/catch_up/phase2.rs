use std::collections::HashMap;
use std::time::Duration;

use crate::services::turn_orchestrator::{
    EnqueueRefusalReason, INTERVENTION_DEDUP_WINDOW, Intervention, MAX_INTERVENTIONS_PER_CHANNEL,
};
use poise::serenity_prelude::{ChannelId, MessageId};

use super::super::MailboxEnqueueOutcome;
use super::super::recovery_known_ids::RecoveryKnownIdArm;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase2EnqueueCommit {
    Accepted,
    /// `AlreadyActiveTurn` — the refusal names THIS message as the mailbox's
    /// active user turn, so a turn took it.
    DuplicateActiveTurn,
    /// `SourceIdAlreadyQueued` — the refusal names a queued entry holding this
    /// id, which says accepted, not dispatched. #5996 keeps it apart from the
    /// arm above for that reason alone; both still skip.
    DuplicateQueued,
    LastItemDedup,
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
    if outcome.persistence_error.is_none() {
        match outcome.refusal_reason {
            Some(EnqueueRefusalReason::AlreadyActiveTurn) => {
                return Phase2EnqueueCommit::DuplicateActiveTurn;
            }
            Some(EnqueueRefusalReason::SourceIdAlreadyQueued) => {
                return Phase2EnqueueCommit::DuplicateQueued;
            }
            _ => {}
        }
    }
    if outcome.persistence_error.is_none()
        && matches!(
            outcome.refusal_reason,
            Some(EnqueueRefusalReason::LastItemDedup)
        )
    {
        return Phase2EnqueueCommit::LastItemDedup;
    }
    Phase2EnqueueCommit::Deferred
}

pub(super) fn catch_up_remaining_queue_capacity(queue_len: usize) -> usize {
    MAX_INTERVENTIONS_PER_CHANNEL.saturating_sub(queue_len)
}

pub(super) fn advance_phase2_checkpoint(checkpoint: Option<u64>, message_id: u64) -> Option<u64> {
    Some(checkpoint.map_or(message_id, |saved| saved.max(message_id)))
}

/// #5996 / I20: skip and advance are not one decision. The skip is retried on
/// the next scan; the advance forecloses the message. An id no arm claims
/// resolves to no-advance — the retryable side. This is never reached for an id
/// the same scan inserted, because each message in the slice is visited once.
pub(super) fn phase2_checkpoint_after_membership_skip(
    checkpoint: Option<u64>,
    known_arms: &HashMap<u64, RecoveryKnownIdArm>,
    message_id: u64,
) -> Option<u64> {
    match known_arms.get(&message_id) {
        Some(arm) if arm.is_dispatch_evidence() => {
            advance_phase2_checkpoint(checkpoint, message_id)
        }
        _ => checkpoint,
    }
}

/// #5996: `AlreadyActiveTurn` names THIS message as the turn a slot holds, so
/// it is evidence of dispatch and advances. `SourceIdAlreadyQueued` names a
/// queued entry holding the id — membership, which does not.
pub(super) fn phase2_checkpoint_after_duplicate_commit(
    commit: Phase2EnqueueCommit,
    checkpoint: Option<u64>,
    message_id: u64,
) -> Option<u64> {
    match commit {
        Phase2EnqueueCommit::DuplicateActiveTurn => {
            advance_phase2_checkpoint(checkpoint, message_id)
        }
        _ => checkpoint,
    }
}

fn catch_up_message_id_gap(last_id: MessageId, current_id: MessageId) -> Option<Duration> {
    let last_created_at = last_id.created_at();
    let current_created_at = current_id.created_at();
    current_created_at
        .signed_duration_since(*last_created_at)
        .to_std()
        .ok()
}

pub(super) fn catch_up_last_item_dedup_is_checkpoint_safe(
    last: Option<&Intervention>,
    message_id: MessageId,
) -> bool {
    last.and_then(|last| catch_up_message_id_gap(last.message_id, message_id))
        .is_some_and(|gap| gap <= INTERVENTION_DEDUP_WINDOW)
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
