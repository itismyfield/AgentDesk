//! While an actor holds a V2 owner, every queue-changing arm reaches the owner here or is refused.
//! The match has no wildcard, so a new arm cannot compile until it is classified.
use super::Ticket;
use super::*;
use crate::db::turns::TerminalOutcome;
use crate::services::turn_orchestrator::{
    CancelQueuedMessageResult, ChannelMailboxMsg as M, ChannelMailboxState,
    EnqueueInterventionResult, EnqueueRefusalReason, HasPendingSoftQueueResult,
    HydratePendingQueueResult, Intervention, RequeueInterventionResult, RestartDrainResult,
    TakeNextSoftResult,
    pending_queue_persistence::{
        pending_queue_item_from_intervention, pending_queue_item_to_intervention,
    },
};
use poise::serenity_prelude::{ChannelId, MessageId};
use source_admission::{AdmitError, Admitted, Custody};
use std::time::{Instant, SystemTime};

/// Answer of an arm the V2 owner does not serve: nothing was absorbed or moved.
const HELD: &str = "the V2 owner holds this queue; the legacy queue path is refused";

pub(crate) fn route(state: &mut ChannelMailboxState, channel_id: ChannelId, msg: M) -> Option<M> {
    let Some(owner) = state.queued_head.v2.as_mut() else {
        return Some(msg);
    };
    match msg {
        // Reads, turn lifecycle and injection keep the V1 path: the V1 queue is empty and every
        // V1 queue write they could attempt is refused by the legacy guard on the V2 file.
        M::CommitCapturedReadyDelivery { .. }
        | M::Snapshot { .. }
        | M::HasActiveTurn { .. }
        | M::HasBlockingActiveTurn { .. }
        | M::ActiveTurnKind { .. }
        | M::CancelToken { .. }
        | M::CancelActiveTurnWithReason { .. }
        | M::CancelActiveTurnIfCurrent { .. }
        | M::CancelActiveTurnIfCurrentWithReason { .. }
        | M::CancelActiveTurnIfCurrentUnlessHerdr { .. }
        | M::CancelActiveBackgroundTurnIfCurrent { .. }
        | M::TryStartTurn { .. }
        | M::RestoreActiveTurn { .. }
        | M::RecoveryKickoff { .. }
        | M::ClearRecoveryMarker { .. }
        | M::FinishTurn { .. }
        | M::FinishTurnIfToken { .. }
        | M::FinishTurnIfMatches { .. }
        | M::ReleaseWitnessedEpisode(_)
        | M::PolicyClose { .. }
        | M::ClaimPolicyTail(_)
        | M::FinishCancelledTurn { .. }
        | M::CloseIfIdle { .. }
        | M::Injection(_)
        // Forced purge keeps its guard; explicit clear enters the shared V2-capable clear path.
        | M::PurgeQueue { .. }
        | M::Clear { .. } => return Some(msg),
        #[cfg(unix)]
        M::PurgeQueueIfUnchanged(request) => request.run_v2(state, channel_id),
        #[cfg(unix)]
        M::CaptureOwnQueue(request) => request.refuse_v2(),
        #[cfg(test)]
        M::CancelActiveTurnIfUserMessageWithReason { .. }
        | M::HardStop { .. } => return Some(msg),
        #[cfg(test)]
        M::AgeActiveTurnForTest { .. }
        | M::AgeInboundWaitsForTest { .. }
        | M::AgeValveClearedDispatchForTest { .. } => return Some(msg),
        M::Enqueue {
            intervention,
            persistence,
            reply,
            ..
        } => {
            let role = persistence.dispatch_role_override;
            let _ = reply.send(enqueue(owner, &intervention, channel_id, role));
        }
        M::HasPendingSoftQueue { reply, .. } => {
            let _ = reply.send(HasPendingSoftQueueResult {
                has_pending: !owner.snapshot.items.is_empty(),
                queue_exit_events: Vec::new(),
                persistence_error: None,
            });
        }
        // Withheld, never dispatched: the owner bootstrap that starts a fresh turn is not wired.
        M::TakeNextSoft { reply, .. } => {
            let queue_len_after = owner.snapshot.items.len();
            let _ = reply.send(TakeNextSoftResult {
                selection_stale: false,
                intervention: None,
                dispatch_lease: None,
                has_more: queue_len_after > 0,
                queue_len_after,
                queue_exit_events: Vec::new(),
                persistence_error: None,
            });
        }
        M::CancelQueuedPrimaryMessage {
            message_id, reject_ambiguous, reply, ..
        } => {
            let _ = reply.send(cancel(owner, message_id, reject_ambiguous));
        }
        M::RestartDrain { reply, .. } => {
            let _ = reply.send(RestartDrainResult {
                queued_count: owner.snapshot.items.len(),
                persistence_error: None,
            });
        }
        M::RequeueFront { reply, .. } => {
            let _ = reply.send(RequeueInterventionResult {
                enqueued: false,
                refusal_reason: None,
                queue_exit_events: Vec::new(),
                persistence_error: Some(HELD.into()),
            });
        }
        M::AbandonPendingDispatch { reply, .. } => {
            let _ = reply.send(false);
        }
        M::HydratePendingQueueFromDisk { reply, .. }
        | M::MergeRestoredQueueItems { reply, .. }
        | M::MergeRestoredDispatchMarker { reply, .. } => {
            let _ = reply.send(HydratePendingQueueResult {
                queue_len_after: owner.waiting(),
                persistence_error: Some(HELD.into()),
                ..Default::default()
            });
        }
        // Dropping the reply is the refusal its caller observes.
        #[cfg(test)]
        M::ReplaceQueue { .. } => {}
    }
    None
}

fn enqueue(
    owner: &mut Owner,
    intervention: &Intervention,
    channel_id: ChannelId,
    role: Option<u64>,
) -> EnqueueInterventionResult {
    let (now, instant) = (SystemTime::now(), Instant::now());
    let item = pending_queue_item_from_intervention(intervention, channel_id, role, now, instant);
    let result = |enqueued, refusal_reason, persistence_error| EnqueueInterventionResult {
        enqueued,
        merged: false,
        refusal_reason,
        queue_exit_events: Vec::new(),
        persistence_error,
    };
    // Only a real source names one logical input, so only its Pending retries can share a receipt;
    // a synthetic id may name several inputs and keeps one custody per call.
    let real = (sources::identity(&item).sources.iter())
        .all(|s| crate::services::tui_input::input_key::is_discord_key(s.message_id));
    let mut custody = match real {
        true => Custody::with_ticket(&item, Ticket::of_sources(&item)),
        false => Custody::new(&item),
    };
    match custody.advance(owner) {
        Ok(Admitted::Queued | Admitted::RetainedOpaque) => result(true, None, None),
        // Its sources are already admitted, under this receipt or with another body.
        Ok(Admitted::Duplicate) | Err(AdmitError::Conflict(_) | AdmitError::SourceAlreadyKnown) => {
            result(
                false,
                Some(EnqueueRefusalReason::SourceIdAlreadyQueued),
                None,
            )
        }
        Err(error) => result(
            false,
            None,
            Some(format!("V2 admission refused: {error:?}")),
        ),
    }
}

/// Items behind an attempted head are untried; every original the head carries stays in custody.
fn untried(owner: &Owner) -> usize {
    (owner.snapshot.head_injection.as_ref()).map_or(0, |head| head.members().count())
}

/// Requeues the proven-unsent retained inputs `pick` names in the snapshot a cancel or clear
/// commits, so neither leaves an original it should have taken to come back later.
fn requeued(owner: &mut Owner, pick: impl Fn(&RetainedInput) -> bool) -> SnapshotV2 {
    let mut next = owner.snapshot.clone();
    if !next.requeue_known_not_sent(pick).is_empty() && untried(owner) == 0 {
        owner.revoke_front();
    }
    next
}

/// Takes only the cancelled original; an unrelated retained input stays where it is.
fn cancel(
    owner: &mut Owner,
    message_id: MessageId,
    reject_ambiguous: bool,
) -> CancelQueuedMessageResult {
    if reject_ambiguous
        && owner
            .cancelable_primary_counts()
            .get(&message_id.get())
            .is_some_and(|count| *count > 1)
    {
        return CancelQueuedMessageResult {
            removed: None,
            queue_exit_events: Vec::new(),
            persistence_error: Some(
                "모호한 ID: 취소 가능한 원본이 여러 개여서 단일 취소를 거절합니다.".into(),
            ),
        };
    }
    let first = untried(owner);
    let named = |entry: &RetainedInput| {
        (entry.input.members()).any(|member| member.message_id == message_id.get())
    };
    let mut next = requeued(owner, named);
    let found = next
        .items
        .iter()
        .skip(first)
        .position(|item| item.message_id == message_id.get());
    let (removed, persistence_error) = match found {
        None => (None, None),
        Some(at) => {
            if first == 0 && at <= owner.followers().len() {
                owner.revoke_front();
            }
            next.owe_queued(
                [at + first],
                TerminalOutcome::Cancelled,
                super::origin::now_ms(),
            );
            let item = next.items.remove(at + first);
            if next
                .unfinished_handoff
                .as_ref()
                .is_some_and(|j| j.original == sources::identity(&item))
            {
                next.unfinished_handoff = None;
            }
            next.unbind_item(at + first);
            match owner.commit(next) {
                Ok(()) => {
                    let (now, instant) = (SystemTime::now(), Instant::now());
                    (
                        Some(pending_queue_item_to_intervention(item, now, instant)),
                        None,
                    )
                }
                Err(error) => (None, Some(error)),
            }
        }
    };
    CancelQueuedMessageResult {
        queue_exit_events: removed
            .iter()
            .cloned()
            .map(|item| {
                crate::services::turn_orchestrator::QueueExitEvent::new(
                    item,
                    crate::services::turn_orchestrator::QueueExitKind::Cancelled,
                )
            })
            .collect(),
        removed,
        persistence_error,
    }
}

#[cfg(test)]
#[path = "control_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "requeue_tests.rs"]
mod requeue_tests;

#[cfg(all(test, unix))]
impl Owner {
    pub(crate) fn cancel_sequence_for_tests(&self) -> u64 {
        self.last_success_sequence
    }
    pub(crate) fn cancel_fault_for_tests(&mut self, before: bool) {
        self.fault = if before { Fault::Before } else { Fault::Sync };
    }
}

pub(crate) struct ClearedWaiting {
    pub(crate) queued: Vec<PendingQueueItem>,
    pub(crate) retained: Vec<PendingQueueItem>,
    pub(crate) uncertain: usize,
    pub(crate) kept: usize,
}

impl Owner {
    fn cancelable_items(&self) -> impl Iterator<Item = (&PendingQueueItem, bool)> {
        let queued = self
            .snapshot
            .items
            .iter()
            .skip(untried(self))
            .map(|item| (item, false));
        let retained = self
            .snapshot
            .retained_inputs
            .iter()
            .filter(|entry| super::retained::requeueable(entry, &self.snapshot.terminal_identities))
            .flat_map(|entry| entry.input.members().map(|item| (item, true)));
        queued.chain(retained)
    }

    fn cancelable_primary_counts(&self) -> std::collections::BTreeMap<u64, usize> {
        let mut counts = std::collections::BTreeMap::new();
        for (item, _) in self.cancelable_items() {
            *counts.entry(item.message_id).or_default() += 1;
        }
        counts
    }

    /// Counts all cancelable originals before text search can hide an ID collision.
    pub(crate) fn cancelable(&self) -> Vec<(u64, u64, Option<u64>, String, bool)> {
        let counts = self.cancelable_primary_counts();
        self.cancelable_items()
            .filter(|(item, _)| counts.get(&item.message_id) == Some(&1))
            .map(|(item, retained)| {
                (
                    item.author_id,
                    item.message_id,
                    item.created_at_wall_time_ms,
                    item.text.clone(),
                    retained,
                )
            })
            .collect()
    }

    /// Takes the claimed front and the followers its one input carried once that input finished
    /// without a provider turn; an origin waiting on them is owed `Completed`.
    pub(crate) fn complete_claimed_locally(
        &mut self,
        (front, number): (&Front, u64),
        followers: usize,
    ) -> Result<(), String> {
        let claimed = number == self.claim_number
            && (self.front_claim.as_ref()).is_some_and(|c| is_front(c, front))
            && self.front().is_some_and(|now| is_front(&now, front));
        if !claimed {
            return Err("NotSent(\"front_not_granted\")".into());
        }
        let mut next = self.snapshot.clone();
        let taken = (1 + followers).min(next.items.len());
        next.owe_queued(
            0..taken,
            TerminalOutcome::Completed,
            super::origin::now_ms(),
        );
        for _ in 0..taken {
            next.items.remove(0);
            next.unbind_item(0);
        }
        self.commit(next)?;
        self.revoke_front();
        Ok(())
    }

    /// Clears waiting inputs in one commit; in-flight evidence remains available to late settlement.
    pub(crate) fn clear_waiting(&mut self) -> Result<ClearedWaiting, String> {
        let mut next = requeued(self, |_| true);
        let first = untried(self);
        let at = super::origin::now_ms();
        next.owe_queued(first..next.items.len(), TerminalOutcome::Cancelled, at);
        let queued: Vec<_> = next.items.drain(first..).collect();
        if first == 0 {
            next.unfinished_handoff = None;
        }
        if first == 0 && !queued.is_empty() {
            self.revoke_front();
        }
        let (mut retained, mut owed, mut uncertain) = (Vec::new(), Vec::new(), 0);
        for entry in &mut next.retained_inputs {
            if entry.notice.kind == super::retained::CLEARED_NOTICE {
                continue;
            }
            retained.extend(entry.input.members().cloned());
            if matches!(entry.reason, RetainedReason::Unconfirmed) {
                uncertain += entry.input.members().count();
            }
            if matches!(entry.disposition, Some(RetainedDisposition::KnownNotSent))
                && entry.input.attempt.is_some()
                && !entry.notice.enqueued
                && !super::retained::permanently_refused(entry)
            {
                owed.push(entry.input.clone());
            }
            entry.notice.enqueued = true;
            entry.notice.kind = super::retained::CLEARED_NOTICE.into();
        }
        for input in owed {
            next.owe_attempt(&input, TerminalOutcome::Cancelled, at);
        }
        self.commit(next)?;
        self.project_slots();
        Ok(ClearedWaiting {
            queued,
            retained,
            uncertain,
            kept: first,
        })
    }
}
