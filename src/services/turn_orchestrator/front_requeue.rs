use std::collections::HashSet;

use poise::serenity_prelude::MessageId;

use super::{
    EnqueueInterventionResult, EnqueueRefusalReason, Intervention, MAX_INTERVENTIONS_PER_CHANNEL,
    QueueExitEvent, QueueExitKind, ensure_source_message_ids, prune_interventions,
};

pub(super) fn intervention_identity_ids(intervention: &Intervention) -> HashSet<MessageId> {
    let mut ids: HashSet<_> = intervention.source_message_ids.iter().copied().collect();
    ids.insert(intervention.message_id);
    ids
}

pub(super) fn requeue_intervention_front(
    queue: &mut Vec<Intervention>,
    mut intervention: Intervention,
    pending_user_dispatch: Option<MessageId>,
    active_user_message_id: Option<MessageId>,
    authorized_pending_restore: Option<MessageId>,
) -> EnqueueInterventionResult {
    let mut queue_exit_events = prune_interventions(queue);
    ensure_source_message_ids(&mut intervention);
    let incoming_ids = intervention_identity_ids(&intervention);
    let pending_conflict = pending_user_dispatch
        .filter(|pending| incoming_ids.contains(pending))
        .is_some_and(|pending| authorized_pending_restore != Some(pending));
    let active_conflict =
        active_user_message_id.is_some_and(|active| incoming_ids.contains(&active));
    let queued_conflict = queue
        .iter()
        .any(|queued| !incoming_ids.is_disjoint(&intervention_identity_ids(queued)));
    let refusal_reason = if pending_conflict || active_conflict {
        Some(EnqueueRefusalReason::SourceIdPendingOrActive)
    } else if queued_conflict {
        Some(EnqueueRefusalReason::SourceIdAlreadyQueued)
    } else {
        None
    };
    if let Some(refusal_reason) = refusal_reason {
        return EnqueueInterventionResult {
            enqueued: false,
            merged: false,
            refusal_reason: Some(refusal_reason),
            queue_exit_events,
            persistence_error: None,
        };
    }
    queue.insert(0, intervention);
    if queue.len() > MAX_INTERVENTIONS_PER_CHANNEL {
        queue_exit_events.extend(
            queue
                .drain(MAX_INTERVENTIONS_PER_CHANNEL..)
                .map(|intervention| QueueExitEvent::new(intervention, QueueExitKind::Overflow)),
        );
    }
    EnqueueInterventionResult {
        enqueued: true,
        merged: false,
        refusal_reason: None,
        queue_exit_events,
        persistence_error: None,
    }
}

/// Restores a handed-out head only while its sources have not been discarded.
pub(super) fn restore_dequeued(
    state: &mut super::ChannelMailboxState,
    channel_id: poise::serenity_prelude::ChannelId,
    intervention: Intervention,
    persistence: super::QueuePersistenceContext,
    dispatch_lease: Option<std::sync::Arc<super::DispatchLease>>,
) -> super::RequeueInterventionResult {
    use super::{
        RequeueInterventionResult, absorb_disk_queue_error, clear_pending_user_dispatch,
        consume_pending_dispatch_marker_if_matches, inbound_order, persist_queue_or_restore,
    };
    use std::sync::Arc;
    state.last_persistence = Some(persistence.clone());
    if dispatch_lease
        .as_ref()
        .is_some_and(|lease| lease.superseded_by_clear())
        || super::clear_fence::refuses(state, &intervention)
    {
        return RequeueInterventionResult {
            enqueued: false,
            refusal_reason: Some(EnqueueRefusalReason::SupersededByClear),
            queue_exit_events: Vec::new(),
            persistence_error: None,
        };
    }
    if let Some(error) = absorb_disk_queue_error(state, channel_id, &persistence) {
        return RequeueInterventionResult::absorb_failed(error);
    }
    let identity_ids = intervention_identity_ids(&intervention);
    let authorized_pending_restore = dispatch_lease.as_ref().and_then(|lease| {
        let pending = state.pending_user_dispatch?;
        let stored = state.pending_user_dispatch_lease.as_ref()?;
        (identity_ids.contains(&pending) && Arc::ptr_eq(lease, stored)).then_some(pending)
    });
    let previous_queue = state.intervention_queue.clone();
    let requeue_result = requeue_intervention_front(
        &mut state.intervention_queue,
        intervention,
        state.pending_user_dispatch,
        state.active_user_message_id,
        authorized_pending_restore,
    );
    if !requeue_result.enqueued {
        RequeueInterventionResult {
            enqueued: false,
            refusal_reason: requeue_result.refusal_reason,
            queue_exit_events: requeue_result.queue_exit_events,
            persistence_error: None,
        }
    } else if let Err(error) = persist_queue_or_restore(
        state,
        channel_id,
        &persistence,
        previous_queue,
        "requeue_front",
    ) {
        RequeueInterventionResult {
            enqueued: false,
            refusal_reason: None,
            queue_exit_events: Vec::new(),
            persistence_error: Some(error),
        }
    } else {
        if let Some(pending) = authorized_pending_restore {
            consume_pending_dispatch_marker_if_matches(
                state,
                channel_id,
                pending,
                "restore_dequeued_head",
            );
            clear_pending_user_dispatch(state);
        }
        // A restore ends a drain round, up to a cap of unclaimed returns per head.
        inbound_order::discount_restored_head(state, &identity_ids);
        RequeueInterventionResult {
            enqueued: true,
            refusal_reason: None,
            queue_exit_events: requeue_result.queue_exit_events,
            persistence_error: None,
        }
    }
}
