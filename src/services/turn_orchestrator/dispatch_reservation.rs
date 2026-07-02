use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};

use super::pending_queue_persistence::{
    load_channel_pending_dispatch_marker, load_channel_pending_queue,
    remove_channel_pending_dispatch_marker,
};
use super::{
    ChannelMailboxState, DispatchLease, HydratePendingQueueResult, Intervention, InterventionMode,
    QueuePersistenceContext, TakeNextSoftResult, persist_queue_or_restore,
};

pub(crate) const PENDING_USER_DISPATCH_LEASE_ORPHAN_AFTER: Duration = Duration::from_secs(10);
pub(crate) const VALVE_CLEARED_DISPATCH_MARKER_GRACE: Duration = Duration::from_secs(600);

pub(super) fn set_pending_user_dispatch(
    state: &mut ChannelMailboxState,
    message_id: MessageId,
) -> Arc<DispatchLease> {
    let lease = Arc::new(DispatchLease);
    state.pending_user_dispatch = Some(message_id);
    state.pending_user_dispatch_since = Some(Instant::now());
    state.pending_user_dispatch_yield_count = 0;
    state.pending_user_dispatch_lease = Some(Arc::clone(&lease));
    lease
}

pub(super) fn clear_pending_user_dispatch(state: &mut ChannelMailboxState) -> Option<MessageId> {
    let cleared = state.pending_user_dispatch.take();
    state.pending_user_dispatch_since = None;
    state.pending_user_dispatch_yield_count = 0;
    state.pending_user_dispatch_lease = None;
    cleared
}

fn clear_pending_user_dispatch_if_matches(
    state: &mut ChannelMailboxState,
    message_id: MessageId,
) -> bool {
    if state.pending_user_dispatch != Some(message_id) {
        return false;
    }
    clear_pending_user_dispatch(state);
    true
}

pub(super) fn record_valve_cleared_pending_dispatch(
    state: &mut ChannelMailboxState,
    message_id: MessageId,
) {
    state.recently_valve_cleared_dispatch = Some((message_id, Instant::now()));
}

pub(super) fn clear_recently_valve_cleared_dispatch_if_matches(
    state: &mut ChannelMailboxState,
    message_id: MessageId,
) {
    if state
        .recently_valve_cleared_dispatch
        .is_some_and(|(cleared_id, _)| cleared_id == message_id)
    {
        state.recently_valve_cleared_dispatch = None;
    }
}

fn marker_recently_valve_cleared(state: &ChannelMailboxState, marker_id: MessageId) -> bool {
    state
        .recently_valve_cleared_dispatch
        .is_some_and(|(cleared_id, cleared_at)| {
            cleared_id == marker_id && cleared_at.elapsed() < VALVE_CLEARED_DISPATCH_MARKER_GRACE
        })
}

fn queued_intervention_contains_message_id(queue: &[Intervention], message_id: MessageId) -> bool {
    queue
        .iter()
        .any(|item| intervention_dedup_ids(item).contains(&message_id))
}

pub(super) fn delete_pending_dispatch_marker_with_persistence(
    persistence: &QueuePersistenceContext,
    channel_id: ChannelId,
    operation: &str,
) {
    if let Err(error) = remove_channel_pending_dispatch_marker(
        &persistence.provider,
        &persistence.token_hash,
        channel_id,
    ) {
        tracing::warn!(
            operation,
            provider = persistence.provider.as_str(),
            token_hash = %persistence.token_hash,
            channel_id = channel_id.get(),
            error = %error,
            "failed to remove pending dispatch marker"
        );
    }
}

pub(super) fn consume_pending_dispatch_marker_if_matches(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    user_message_id: MessageId,
    operation: &str,
) {
    let Some(persistence) = state.last_persistence.as_ref() else {
        return;
    };
    let Some((marker, _)) = load_channel_pending_dispatch_marker(
        &persistence.provider,
        &persistence.token_hash,
        channel_id,
    ) else {
        return;
    };
    if marker.message_id == user_message_id {
        delete_pending_dispatch_marker_with_persistence(persistence, channel_id, operation);
        clear_recently_valve_cleared_dispatch_if_matches(state, user_message_id);
    }
}

pub(super) fn abandon_pending_dispatch_reservation(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    user_message_id: MessageId,
    consume_marker: bool,
    operation: &str,
) {
    let matched = clear_pending_user_dispatch_if_matches(state, user_message_id);
    if consume_marker {
        consume_pending_dispatch_marker_if_matches(state, channel_id, user_message_id, operation);
    }
    if matched {
        tracing::warn!(
            operation,
            channel_id = channel_id.get(),
            user_message_id = user_message_id.get(),
            consume_marker,
            "cleared abandoned pending dispatch reservation"
        );
    }
}

pub(super) fn clear_stale_pending_dispatch_reservation(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
) -> Option<MessageId> {
    let reserved_id = state.pending_user_dispatch?;
    let Some(stored_lease) = state.pending_user_dispatch_lease.as_ref() else {
        return None;
    };
    let reservation_age = state
        .pending_user_dispatch_since
        .map(|since| since.elapsed())
        .unwrap_or_default();
    if state.cancel_token.is_some()
        || Arc::strong_count(stored_lease) > 1
        || reservation_age < PENDING_USER_DISPATCH_LEASE_ORPHAN_AFTER
    {
        return None;
    }
    clear_pending_user_dispatch(state);
    tracing::warn!(
        channel_id = channel_id.get(),
        user_message_id = reserved_id.get(),
        reservation_age_ms = reservation_age.as_millis(),
        "cleared orphaned pending dispatch reservation before marker self-heal"
    );
    Some(reserved_id)
}

pub(super) fn pending_dispatch_lease_is_orphaned(state: &ChannelMailboxState) -> bool {
    let Some(stored_lease) = state.pending_user_dispatch_lease.as_ref() else {
        return false;
    };
    let reservation_age = state
        .pending_user_dispatch_since
        .map(|since| since.elapsed())
        .unwrap_or_default();
    state.pending_user_dispatch.is_some()
        && state.cancel_token.is_none()
        && Arc::strong_count(stored_lease) == 1
        && reservation_age >= PENDING_USER_DISPATCH_LEASE_ORPHAN_AFTER
}

fn intervention_dedup_ids(item: &Intervention) -> Vec<MessageId> {
    let mut ids: Vec<MessageId> = item.source_message_ids.clone();
    if !ids.contains(&item.message_id) {
        ids.push(item.message_id);
    }
    ids
}

pub(super) fn hydrate_pending_queue_into_state(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    disk_items: Vec<Intervention>,
    persistence: QueuePersistenceContext,
    restored_override: Option<ChannelId>,
) -> HydratePendingQueueResult {
    state.last_persistence = Some(persistence.clone());
    let previous_queue = state.intervention_queue.clone();
    let mut existing_ids: HashSet<MessageId> = state
        .intervention_queue
        .iter()
        .flat_map(intervention_dedup_ids)
        .collect();
    let mut absorbed = 0usize;
    for item in disk_items.into_iter().rev() {
        let item_ids = intervention_dedup_ids(&item);
        if item_ids.iter().all(|id| existing_ids.contains(id)) {
            continue;
        }
        existing_ids.extend(item_ids);
        state.intervention_queue.insert(0, item);
        absorbed += 1;
    }
    if absorbed > 0
        && let Err(error) = persist_queue_or_restore(
            state,
            channel_id,
            &persistence,
            previous_queue,
            "hydrate_pending_queue_from_disk",
        )
    {
        return HydratePendingQueueResult {
            absorbed: 0,
            queue_len_after: state.intervention_queue.len(),
            restored_override,
            persistence_error: Some(error),
        };
    }
    HydratePendingQueueResult {
        absorbed,
        queue_len_after: state.intervention_queue.len(),
        restored_override,
        persistence_error: None,
    }
}

pub(super) fn merge_pending_dispatch_marker_into_state(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    marker: Intervention,
    persistence: QueuePersistenceContext,
    restored_override: Option<ChannelId>,
    operation: &'static str,
) -> HydratePendingQueueResult {
    state.last_persistence = Some(persistence.clone());
    let marker_id = marker.message_id;
    if queued_intervention_contains_message_id(&state.intervention_queue, marker_id)
        || state.active_user_message_id == Some(marker_id)
    {
        delete_pending_dispatch_marker_with_persistence(&persistence, channel_id, operation);
        clear_recently_valve_cleared_dispatch_if_matches(state, marker_id);
        return HydratePendingQueueResult {
            absorbed: 0,
            queue_len_after: state.intervention_queue.len(),
            restored_override,
            persistence_error: None,
        };
    }
    if marker_recently_valve_cleared(state, marker_id) {
        return HydratePendingQueueResult {
            absorbed: 0,
            queue_len_after: state.intervention_queue.len(),
            restored_override,
            persistence_error: None,
        };
    }

    let previous_queue = state.intervention_queue.clone();
    state.intervention_queue.insert(0, marker);
    if let Err(error) =
        persist_queue_or_restore(state, channel_id, &persistence, previous_queue, operation)
    {
        return HydratePendingQueueResult {
            absorbed: 0,
            queue_len_after: state.intervention_queue.len(),
            restored_override,
            persistence_error: Some(error),
        };
    }
    delete_pending_dispatch_marker_with_persistence(&persistence, channel_id, operation);
    clear_recently_valve_cleared_dispatch_if_matches(state, marker_id);
    HydratePendingQueueResult {
        absorbed: 1,
        queue_len_after: state.intervention_queue.len(),
        restored_override,
        persistence_error: None,
    }
}

pub(super) fn hydrate_pending_queue_from_disk_if_present(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: &QueuePersistenceContext,
) -> HydratePendingQueueResult {
    let (disk_items, mut restored_override) =
        load_channel_pending_queue(&persistence.provider, &persistence.token_hash, channel_id);

    let mut effective_persistence = persistence.clone();
    if effective_persistence.dispatch_role_override.is_none() {
        effective_persistence.dispatch_role_override =
            restored_override.map(|channel| channel.get());
    }
    let mut result = if disk_items.is_empty() {
        HydratePendingQueueResult {
            absorbed: 0,
            queue_len_after: state.intervention_queue.len(),
            restored_override,
            persistence_error: None,
        }
    } else {
        hydrate_pending_queue_into_state(
            state,
            channel_id,
            disk_items,
            effective_persistence.clone(),
            restored_override,
        )
    };
    if result.persistence_error.is_some() || state.pending_user_dispatch.is_some() {
        return result;
    }
    restored_override = result.restored_override;
    let Some((marker, marker_override)) = load_channel_pending_dispatch_marker(
        &persistence.provider,
        &persistence.token_hash,
        channel_id,
    ) else {
        return result;
    };
    if restored_override.is_none() {
        restored_override = marker_override;
    }
    if effective_persistence.dispatch_role_override.is_none() {
        effective_persistence.dispatch_role_override =
            restored_override.map(|channel| channel.get());
    }
    let marker_result = merge_pending_dispatch_marker_into_state(
        state,
        channel_id,
        marker,
        effective_persistence,
        restored_override,
        "hydrate_pending_dispatch_marker",
    );
    result.absorbed += marker_result.absorbed;
    result.queue_len_after = marker_result.queue_len_after;
    result.restored_override = marker_result.restored_override;
    result.persistence_error = marker_result.persistence_error;
    result
}

pub(super) fn reconcile_pending_dispatch_marker_before_take_next(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: &QueuePersistenceContext,
) -> Option<TakeNextSoftResult> {
    if state.pending_user_dispatch.is_some() {
        return Some(TakeNextSoftResult {
            intervention: None,
            dispatch_lease: None,
            has_more: state
                .intervention_queue
                .iter()
                .any(|item| item.mode == InterventionMode::Soft),
            queue_len_after: state.intervention_queue.len(),
            queue_exit_events: Vec::new(),
            persistence_error: None,
        });
    }

    let Some((marker, marker_override)) = load_channel_pending_dispatch_marker(
        &persistence.provider,
        &persistence.token_hash,
        channel_id,
    ) else {
        return None;
    };
    let marker_id = marker.message_id;
    let stale_duplicate =
        queued_intervention_contains_message_id(&state.intervention_queue, marker_id)
            || state.active_user_message_id == Some(marker_id);
    if stale_duplicate {
        delete_pending_dispatch_marker_with_persistence(
            persistence,
            channel_id,
            "take_next_soft_stale_marker",
        );
        clear_recently_valve_cleared_dispatch_if_matches(state, marker_id);
        return None;
    }
    if marker_recently_valve_cleared(state, marker_id) {
        return None;
    }

    let mut effective_persistence = persistence.clone();
    if effective_persistence.dispatch_role_override.is_none() {
        effective_persistence.dispatch_role_override = marker_override.map(|channel| channel.get());
    }
    let marker_result = merge_pending_dispatch_marker_into_state(
        state,
        channel_id,
        marker,
        effective_persistence,
        marker_override,
        "take_next_soft_restore_marker",
    );
    if let Some(error) = marker_result.persistence_error {
        return Some(TakeNextSoftResult {
            intervention: None,
            dispatch_lease: None,
            has_more: state
                .intervention_queue
                .iter()
                .any(|item| item.mode == InterventionMode::Soft),
            queue_len_after: state.intervention_queue.len(),
            queue_exit_events: Vec::new(),
            persistence_error: Some(error),
        });
    }
    None
}
