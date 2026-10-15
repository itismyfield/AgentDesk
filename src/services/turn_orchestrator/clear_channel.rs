//! The mailbox `Clear` arm: release the active turn, drain the queue, its head and
//! reservation, and report every discarded inbound id.

use poise::serenity_prelude::{ChannelId, MessageId};

use super::dispatch_reservation::reclaim_pending_dispatch_marker;
use super::pending_queue_persistence::{pending_queue_file_path, persist_queue};
use super::{
    ChannelMailboxState, ClearChannelResult, QueueExitEvent, QueueExitKind,
    QueuePersistenceContext, clear_pending_user_dispatch, log_queue_persistence_rollback,
    queued_head, release_active_turn_anchor,
};
use crate::services::discord::runtime_store::{AtomicWriteContext, critical_atomic_write};

pub(super) fn clear_channel_state(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: QueuePersistenceContext,
) -> ClearChannelResult {
    state.last_persistence = Some(persistence.clone());
    let refused = |error| ClearChannelResult {
        persistence_error: Some(error),
        ..Default::default()
    };
    if let Err(error) = reclaim_pending_dispatch_marker(state, channel_id, &persistence) {
        return refused(error);
    }
    // An explicit clear discards the queued head too, whatever its phase.
    let previous_head = match queued_head::release_for_clear(state, channel_id, &persistence) {
        Ok(previous) => previous,
        Err(error) => return refused(error),
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
    if let Err(error) = persist_cleared_queue(state, channel_id, &persistence) {
        log_queue_persistence_rollback("clear", channel_id, &persistence, &error);
        // The head stays held too, so nothing behind it runs before a retried clear.
        state.intervention_queue = previous_queue;
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
    ClearChannelResult {
        removed_token,
        queue_exit_events,
        discarded_message_ids,
        persistence_error: None,
    }
}

/// The clear's write: an empty queue without metadata is published as an empty snapshot by atomic
/// replace rather than by removing the file, so an error leaves the previous snapshot in place.
fn persist_cleared_queue(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: &QueuePersistenceContext,
) -> Result<(), String> {
    if !state.intervention_queue.is_empty() || state.queued_head.meta.is_set() {
        return persist_queue(state, channel_id, persistence);
    }
    queued_head::prepare_write(state, channel_id, persistence)?;
    let (provider, token_hash) = (&persistence.provider, persistence.token_hash.as_str());
    let path = pending_queue_file_path(provider, token_hash, channel_id)
        .ok_or("pending queue root unavailable")?;
    crate::services::discord::input_runtime::fence::write(provider, channel_id.get(), || {
        #[cfg(test)]
        if super::pending_queue_persistence::save_fault::take(channel_id) {
            return Err(format!(
                "injected pending queue save failure {}",
                path.display()
            ));
        }
        let context = AtomicWriteContext::new("discord_pending_queue")
            .provider(provider.as_str())
            .token_hash(token_hash)
            .channel_id(channel_id.get());
        critical_atomic_write(&path, "[]", context)
    })?;
    queued_head::record_write(state);
    Ok(())
}
