//! Actor-side manual-steer head claim.
//!
//! The manual card may claim only the oldest soft intervention while a live
//! foreground owner exists. The actor persists the dequeue and keeps one queue
//! slot reserved until its lease is either restored or consumed.

use super::*;

async fn request_take_soft(
    handle: &ChannelMailboxHandle,
    persistence: QueuePersistenceContext,
    primary_message_id: Option<MessageId>,
    require_queue_head: bool,
    require_active_turn: bool,
) -> TakeNextSoftResult {
    handle
        .request(
            |reply| ChannelMailboxMsg::TakeNextSoft {
                persistence,
                primary_message_id,
                require_queue_head,
                require_active_turn,
                reply,
            },
            TakeNextSoftResult {
                intervention: None,
                dispatch_lease: None,
                has_more: false,
                queue_len_after: 0,
                queue_exit_events: Vec::new(),
                persistence_error: None,
            },
        )
        .await
}

pub(super) async fn take_next_soft(
    handle: &ChannelMailboxHandle,
    persistence: QueuePersistenceContext,
) -> TakeNextSoftResult {
    request_take_soft(handle, persistence, None, false, false).await
}

pub(super) async fn take_soft_matching(
    handle: &ChannelMailboxHandle,
    persistence: QueuePersistenceContext,
    primary_message_id: Option<MessageId>,
) -> TakeNextSoftResult {
    request_take_soft(handle, persistence, primary_message_id, false, false).await
}

pub(super) async fn take_queue_head_matching_while_active(
    handle: &ChannelMailboxHandle,
    persistence: QueuePersistenceContext,
    primary_message_id: MessageId,
) -> TakeNextSoftResult {
    request_take_soft(handle, persistence, Some(primary_message_id), true, true).await
}

pub(super) fn take_next_soft_actor(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: QueuePersistenceContext,
    primary_message_id: Option<MessageId>,
    require_queue_head: bool,
    require_active_turn: bool,
) -> TakeNextSoftResult {
    state.last_persistence = Some(persistence.clone());
    if require_active_turn && state.cancel_token.is_none() {
        return reject_without_active_turn(state);
    }
    let _ = clear_stale_pending_dispatch_reservation(state, channel_id);
    if let Some(result) =
        reconcile_pending_dispatch_marker_before_take_next(state, channel_id, &persistence)
    {
        return result;
    }
    let previous_queue = state.intervention_queue.clone();
    let next_result = dequeue_next_soft_intervention(
        &mut state.intervention_queue,
        primary_message_id,
        require_queue_head,
    );
    let queue_len_after = state.intervention_queue.len();
    let dispatched_head = next_result.intervention.as_ref().map(|i| i.message_id);
    let marker_error = if let Some(intervention) = next_result.intervention.as_ref() {
        save_channel_pending_dispatch_marker(
            &persistence.provider,
            &persistence.token_hash,
            channel_id,
            intervention,
            persistence.dispatch_role_override,
        )
        .err()
    } else {
        None
    };
    if let Some(error) = marker_error {
        state.intervention_queue = previous_queue;
        log_queue_persistence_rollback("take_next_soft_marker", channel_id, &persistence, &error);
        return failed_claim(state, error);
    }
    if let Err(error) = persist_queue_or_restore(
        state,
        channel_id,
        &persistence,
        previous_queue,
        "take_next_soft",
    ) {
        return failed_claim(state, error);
    }
    if let Some(head) = dispatched_head {
        let dispatch_lease = set_pending_user_dispatch(state, head);
        if require_queue_head {
            reserve_capacity_for_manual_claim(state, &dispatch_lease);
        }
        TakeNextSoftResult {
            intervention: next_result.intervention,
            dispatch_lease: Some(dispatch_lease),
            has_more: next_result.has_more,
            queue_len_after,
            queue_exit_events: next_result.queue_exit_events,
            persistence_error: None,
        }
    } else {
        TakeNextSoftResult {
            intervention: next_result.intervention,
            dispatch_lease: None,
            has_more: next_result.has_more,
            queue_len_after,
            queue_exit_events: next_result.queue_exit_events,
            persistence_error: None,
        }
    }
}

fn failed_claim(state: &ChannelMailboxState, error: String) -> TakeNextSoftResult {
    TakeNextSoftResult {
        intervention: None,
        dispatch_lease: None,
        has_more: state
            .intervention_queue
            .iter()
            .any(|item| item.mode == InterventionMode::Soft),
        queue_len_after: state.intervention_queue.len(),
        queue_exit_events: Vec::new(),
        persistence_error: Some(error),
    }
}

pub(super) fn reject_without_active_turn(state: &ChannelMailboxState) -> TakeNextSoftResult {
    TakeNextSoftResult {
        intervention: None,
        dispatch_lease: None,
        has_more: state
            .intervention_queue
            .iter()
            .any(|item| item.mode == InterventionMode::Soft),
        queue_len_after: state.intervention_queue.len(),
        queue_exit_events: Vec::new(),
        persistence_error: None,
    }
}

pub(super) fn reserve_capacity_for_manual_claim(
    state: &mut ChannelMailboxState,
    lease: &Arc<DispatchLease>,
) {
    state.manual_steer_capacity_lease = Some(Arc::clone(lease));
}

pub(super) fn clear_manual_capacity_if_matches(
    state: &mut ChannelMailboxState,
    candidate: Option<&Arc<DispatchLease>>,
) {
    if state
        .manual_steer_capacity_lease
        .as_ref()
        .is_some_and(|lease| candidate.is_some_and(|candidate| Arc::ptr_eq(lease, candidate)))
    {
        state.manual_steer_capacity_lease = None;
    }
}

pub(super) fn queue_capacity_is_reserved(state: &ChannelMailboxState) -> bool {
    state.intervention_queue.len() >= MAX_INTERVENTIONS_PER_CHANNEL - 1
        && state.manual_steer_capacity_lease.is_some()
}
