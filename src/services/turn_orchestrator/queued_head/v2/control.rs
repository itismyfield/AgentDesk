//! While an actor holds a V2 owner, every queue-changing arm reaches the owner here or is refused.
//! The match has no wildcard, so a new arm cannot compile until it is classified.
use super::Ticket;
use super::*;
use crate::services::turn_orchestrator::{
    CancelQueuedMessageResult, ChannelMailboxMsg as M, ChannelMailboxState, ClearChannelResult,
    EnqueueInterventionResult, EnqueueRefusalReason, HasPendingSoftQueueResult,
    HydratePendingQueueResult, Intervention, RequeueInterventionResult, RestartDrainResult,
    TakeNextSoftResult,
    clear_channel::ActivePolicy,
    lease_release::release_active_turn_anchor,
    mark_turn_finished_signal_done,
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
        | M::CancelActiveTurnIfUserMessageWithReason { .. }
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
        | M::HardStop { .. }
        | M::FinishCancelledTurn { .. }
        | M::CloseIfIdle { .. }
        | M::Injection(_)
        // A forced purge refuses a V2 file before any write; the checked purge has a V2 form.
        | M::PurgeQueue { .. } => return Some(msg),
        #[cfg(unix)]
        M::PurgeQueueIfUnchanged(request) => request.run_v2(state, channel_id),
        #[cfg(unix)]
        M::CaptureOwnQueue(request) => request.refuse_v2(),
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
            message_id, reply, ..
        } => {
            let _ = reply.send(cancel(owner, message_id));
        }
        M::Clear {
            persistence,
            active,
            reply,
        } => {
            let mut result = clear(owner);
            if active == ActivePolicy::Release {
                let provider = Some(&persistence.provider);
                result.removed_token = release_active_turn_anchor(state, channel_id, provider);
                // Released even when the queue commit failed, so the turn's waiters wake now.
                mark_turn_finished_signal_done(channel_id);
            }
            let _ = reply.send(result);
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
                queue_len_after: owner.snapshot.items.len(),
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

fn cancel(owner: &mut Owner, message_id: MessageId) -> CancelQueuedMessageResult {
    let first = untried(owner);
    let mut next = owner.snapshot.clone();
    let found = next
        .items
        .iter()
        .skip(first)
        .position(|item| item.message_id == message_id.get());
    let (removed, persistence_error) = match found {
        None => (None, None),
        Some(at) => {
            if at + first == 0 {
                owner.revoke_front();
            }
            let item = next.items.remove(at + first);
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
        removed,
        queue_exit_events: Vec::new(),
        persistence_error,
    }
}

/// Discards only untried items; the attempted head, retained inputs and terminals stay.
fn clear(owner: &mut Owner) -> ClearChannelResult {
    let mut next = owner.snapshot.clone();
    let first = untried(owner);
    let discarded: Vec<PendingQueueItem> = next.items.drain(first..).collect();
    if first == 0 && !discarded.is_empty() {
        owner.revoke_front();
    }
    let mut result = ClearChannelResult::default();
    match owner.commit(next) {
        Ok(()) => {
            result.discarded_message_ids = discarded
                .iter()
                .flat_map(|item| sources::identity(item).sources)
                .map(|source| MessageId::new(source.message_id))
                .collect();
        }
        Err(error) => {
            tracing::warn!(%error, "V2 clear could not commit; nothing was discarded");
            result.persistence.failed.push("queue_publish");
            result.persistence.count_unknown = true;
        }
    }
    result
}

#[cfg(test)]
#[path = "control_tests.rs"]
mod tests;
