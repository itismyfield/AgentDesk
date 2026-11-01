//! Restore only executable legacy items; version admission remains in the caller.
use super::*;

pub(crate) fn hydrate_pending_queue_into_state(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    disk_items: Vec<Intervention>,
    persistence: QueuePersistenceContext,
    restored_override: Option<ChannelId>,
) -> HydratePendingQueueResult {
    // A V2 owner holds the queue: what a V1 read found (a foreign writer's file) is never hydrated.
    #[cfg(test)]
    if let Some(queue_len_after) = state.v2_len() {
        return HydratePendingQueueResult {
            queue_len_after,
            persistence_error: Some("the V2 owner holds this queue; V1 hydrate refused".into()),
            ..Default::default()
        };
    }
    state.last_persistence = Some(persistence.clone());
    // Items read from a file a clear could not rewrite were discarded by that clear.
    let mut disk_items = match queue_trusted(state, &persistence) {
        true => disk_items,
        false => Vec::new(),
    };
    purge_sources_from_queue(&mut disk_items, |_, id| {
        state.remint_fence.refuses_discarded(&[id])
    });
    let previous_queue = state.intervention_queue.clone();
    let mut existing_ids: HashSet<MessageId> = state
        .intervention_queue
        .iter()
        .flat_map(intervention_dedup_ids)
        .collect();
    let mut absorbed = 0usize;
    for mut item in disk_items.into_iter().rev() {
        let item_ids = intervention_dedup_ids(&item);
        let mut stripped_existing_source = false;
        for existing_id in item_ids
            .iter()
            .copied()
            .filter(|id| existing_ids.contains(id))
        {
            strip_source_message_id_from_intervention(&mut item, existing_id);
            stripped_existing_source = true;
        }
        if stripped_existing_source
            && (item.source_message_ids.is_empty()
                || (item.text.trim().is_empty() && item.pending_uploads.is_empty()))
        {
            continue;
        }
        let remaining_ids = intervention_dedup_ids(&item);
        existing_ids.extend(remaining_ids);
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
