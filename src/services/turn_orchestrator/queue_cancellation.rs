use super::*;

pub(crate) struct TakeNextSoftResult {
    pub(crate) intervention: Option<Intervention>,
    pub(crate) dispatch_lease: Option<Arc<DispatchLease>>,
    pub(crate) has_more: bool,
    pub(crate) queue_len_after: usize,
    pub(crate) queue_exit_events: Vec<QueueExitEvent>,
    pub(crate) persistence_error: Option<String>,
}

pub(crate) fn has_soft_intervention_at(
    queue: &mut Vec<Intervention>,
    now: Instant,
) -> SoftInterventionProbe {
    // #3177: no age-based eviction — only the overflow cap bounds the queue.
    let _ = now;
    // #4260 defensive refactor: surface overflow events instead of a bare
    // drain; the only live caller is on a queue CLONE (see overflow.rs docs).
    let queue_exit_events = super::drain_head_overflow(queue);
    SoftInterventionProbe {
        has_pending: queue.iter().any(|item| item.mode == InterventionMode::Soft),
        queue_exit_events,
    }
}

pub(crate) fn has_soft_intervention(queue: &mut Vec<Intervention>) -> HasPendingSoftQueueResult {
    let queue_exit_events = super::prune_interventions(queue);
    HasPendingSoftQueueResult {
        has_pending: queue.iter().any(|item| item.mode == InterventionMode::Soft),
        queue_exit_events,
        persistence_error: None,
    }
}

pub(crate) fn dequeue_next_soft_intervention(
    queue: &mut Vec<Intervention>,
    primary_message_id: Option<MessageId>,
) -> TakeNextSoftResult {
    let queue_exit_events = super::prune_interventions(queue);
    let intervention = queue
        .iter()
        .position(|item| {
            item.mode == InterventionMode::Soft
                && primary_message_id.is_none_or(|message_id| item.message_id == message_id)
        })
        .map(|index| queue.remove(index));
    let has_more = queue.iter().any(|item| item.mode == InterventionMode::Soft);
    TakeNextSoftResult {
        intervention,
        dispatch_lease: None,
        has_more,
        queue_len_after: queue.len(),
        queue_exit_events,
        persistence_error: None,
    }
}

pub(crate) fn cancel_soft_intervention_by_message_id(
    queue: &mut Vec<Intervention>,
    message_id: MessageId,
) -> CancelQueuedMessageResult {
    cancel_soft_intervention_matching(queue, |item| {
        item.message_id == message_id || item.source_message_ids.contains(&message_id)
    })
}

/// Remove only a queued intervention whose primary message id exactly matches.
///
/// Explicit user controls must not treat merged source-message aliases as a
/// destructive target; legacy reaction-era callers retain alias semantics above.
pub(crate) fn cancel_soft_intervention_by_primary_message_id(
    queue: &mut Vec<Intervention>,
    message_id: MessageId,
) -> CancelQueuedMessageResult {
    cancel_soft_intervention_matching(queue, |item| item.message_id == message_id)
}

fn cancel_soft_intervention_matching(
    queue: &mut Vec<Intervention>,
    matches: impl Fn(&Intervention) -> bool,
) -> CancelQueuedMessageResult {
    let mut queue_exit_events = super::prune_interventions(queue);
    let removed = queue
        .iter()
        .position(|item| item.mode == InterventionMode::Soft && matches(item))
        .map(|index| queue.remove(index));
    if let Some(ref intervention) = removed {
        queue_exit_events.push(QueueExitEvent::new(
            intervention.clone(),
            QueueExitKind::Cancelled,
        ));
    }
    CancelQueuedMessageResult {
        removed,
        queue_exit_events,
        persistence_error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intervention(message_id: u64) -> Intervention {
        Intervention {
            author_id: UserId::new(1),
            author_is_bot: false,
            message_id: MessageId::new(message_id),
            queued_generation: 1,
            source_message_ids: vec![MessageId::new(message_id)],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: format!("message-{message_id}"),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        }
    }

    #[test]
    fn matching_dequeue_preserves_blocked_head_and_fifo_of_remaining_items() {
        let mut queue = vec![intervention(10), intervention(11), intervention(12)];

        let result = dequeue_next_soft_intervention(&mut queue, Some(MessageId::new(11)));

        assert_eq!(
            result.intervention.as_ref().map(|item| item.message_id),
            Some(MessageId::new(11))
        );
        assert!(result.has_more);
        assert_eq!(
            queue.iter().map(|item| item.message_id).collect::<Vec<_>>(),
            vec![MessageId::new(10), MessageId::new(12)],
            "selective automatic progress must park the blocked head without reordering survivors"
        );
    }

    #[test]
    fn explicit_queue_cancel_matches_only_primary_message_id() {
        let mut merged = intervention(10);
        merged.source_message_ids.push(MessageId::new(11));
        let mut queue = vec![merged];

        let alias_result =
            cancel_soft_intervention_by_primary_message_id(&mut queue, MessageId::new(11));
        assert!(
            alias_result.removed.is_none(),
            "source aliases are not controls"
        );
        assert_eq!(queue.len(), 1, "alias cancel must not mutate the queue");

        let stale_result =
            cancel_soft_intervention_by_primary_message_id(&mut queue, MessageId::new(12));
        assert!(stale_result.removed.is_none(), "stale ids are no-ops");
        assert_eq!(queue.len(), 1, "stale cancel must not mutate the queue");

        let primary_result =
            cancel_soft_intervention_by_primary_message_id(&mut queue, MessageId::new(10));
        assert_eq!(
            primary_result.removed.as_ref().map(|item| item.message_id),
            Some(MessageId::new(10))
        );
        assert!(queue.is_empty(), "primary cancel removes exactly one item");
    }
}
