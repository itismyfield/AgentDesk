//! The mailbox `Clear` arm: release the active turn, drain the queue and
//! reservation, and report every discarded inbound id.

use poise::serenity_prelude::{ChannelId, MessageId};

use super::{
    ChannelMailboxState, ClearChannelResult, QueueExitEvent, QueueExitKind,
    QueuePersistenceContext, clear_pending_user_dispatch,
    delete_pending_dispatch_marker_with_persistence, persist_queue_or_restore, queued_head,
    release_active_turn_anchor,
};

pub(super) fn clear_channel_state(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: QueuePersistenceContext,
) -> ClearChannelResult {
    state.last_persistence = Some(persistence.clone());
    // An explicit clear discards the queued head too, whatever its phase.
    let previous_head = match queued_head::release_for_clear(state, channel_id, &persistence) {
        Ok(previous) => previous,
        Err(error) => {
            return ClearChannelResult {
                persistence_error: Some(error),
                ..Default::default()
            };
        }
    };
    let mut discarded_message_ids: Vec<MessageId> =
        state.active_user_message_id.into_iter().collect();
    let removed_token = release_active_turn_anchor(state, channel_id, Some(&persistence.provider));
    let previous_queue = state.intervention_queue.clone();
    let queue_exit_events: Vec<QueueExitEvent> = state
        .intervention_queue
        .drain(..)
        .map(|intervention| QueueExitEvent::new(intervention, QueueExitKind::Superseded))
        .collect();
    if let Err(error) =
        persist_queue_or_restore(state, channel_id, &persistence, previous_queue, "clear")
    {
        // The head stays held too, so nothing behind it runs before a retried clear.
        state.queued_head = previous_head;
        return ClearChannelResult {
            removed_token,
            queue_exit_events: Vec::new(),
            discarded_message_ids,
            persistence_error: Some(error),
        };
    }
    // Liveness-blind: an orphaned reservation is discarded all the same.
    discarded_message_ids.extend(state.pending_user_dispatch);
    discarded_message_ids.extend(state.pending_user_dispatch_source_ids.iter().copied());
    for event in &queue_exit_events {
        let item = &event.intervention;
        discarded_message_ids.push(item.message_id);
        discarded_message_ids.extend(item.source_message_ids.iter().copied());
    }
    clear_pending_user_dispatch(state);
    state.recently_valve_cleared_dispatch = None;
    delete_pending_dispatch_marker_with_persistence(&persistence, channel_id, "clear");
    ClearChannelResult {
        removed_token,
        queue_exit_events,
        discarded_message_ids,
        persistence_error: None,
    }
}
