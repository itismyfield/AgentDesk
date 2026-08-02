use std::collections::HashSet;

use poise::serenity_prelude::MessageId;

use super::{
    EnqueueInterventionResult, EnqueueRefusalReason, Intervention, ensure_source_message_ids,
    overflow, prune_interventions,
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
    match overflow::prepare_bot_front_requeue(queue, &intervention) {
        Ok(bot_exit_events) => queue_exit_events.extend(bot_exit_events),
        Err(incoming_overflow) => {
            queue_exit_events.push(incoming_overflow);
            return EnqueueInterventionResult {
                enqueued: false,
                merged: false,
                refusal_reason: None,
                queue_exit_events,
                persistence_error: None,
            };
        }
    }
    queue.insert(0, intervention);
    queue_exit_events.extend(overflow::drain_front_requeue_overflow(queue));
    EnqueueInterventionResult {
        enqueued: true,
        merged: false,
        refusal_reason: None,
        queue_exit_events,
        persistence_error: None,
    }
}
