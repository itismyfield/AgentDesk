use super::active_source_dedup::settle_completed_sources;
use super::*;

pub(super) fn enqueue_with_settlement(
    queue: &mut Vec<Intervention>,
    mut intervention: Intervention,
    active_user_message_id: Option<MessageId>,
    settlement: Option<(&ProviderKind, ChannelId)>,
) -> EnqueueInterventionResult {
    let mut queue_exit_events = prune_interventions(queue);
    ensure_source_message_ids(&mut intervention);

    if intervention_sources_all_match_active(&intervention, active_user_message_id) {
        return EnqueueInterventionResult::refused(
            EnqueueRefusalReason::AlreadyActiveTurn,
            queue_exit_events,
        );
    }
    if let Some(active_id) = intervention_has_active_source(&intervention, active_user_message_id) {
        strip_source_message_id_from_intervention(&mut intervention, active_id);
    }

    if queue
        .iter()
        .any(|item| item.source_message_ids.contains(&intervention.message_id))
    {
        return EnqueueInterventionResult::refused(
            EnqueueRefusalReason::SourceIdAlreadyQueued,
            queue_exit_events,
        );
    }

    if let Some(last) = queue.last() {
        if last.author_id == intervention.author_id
            && last.text == intervention.text
            && last.reply_context == intervention.reply_context
            && last.has_reply_boundary == intervention.has_reply_boundary
            && last.pending_uploads == intervention.pending_uploads
            && intervention_age_since(last, &intervention) <= INTERVENTION_DEDUP_WINDOW
        {
            return EnqueueInterventionResult::refused(
                EnqueueRefusalReason::LastItemDedup,
                queue_exit_events,
            );
        }
    }

    let merge_index = queue.len().checked_sub(1);
    if let Some(index) = merge_index
        && should_merge_intervention(&queue[index], &intervention)
        && let Some((provider, channel_id)) = settlement
    {
        // Retire already visible completions before appending the new input.
        let mut candidate = queue.split_off(index);
        queue_exit_events.extend(settle_completed_sources(
            &mut candidate,
            Some(provider),
            channel_id,
        ));
        queue.extend(candidate);
    }

    if let Some(last) = merge_index.and_then(|index| queue.get_mut(index)) {
        ensure_source_message_ids(last);
        if should_merge_intervention(last, &intervention) {
            let incoming_text_segments = intervention.source_text_segments();
            last.message_id = intervention.message_id;
            last.queued_generation = intervention.queued_generation;
            push_unique_message_ids(
                &mut last.source_message_ids,
                intervention.source_message_ids.into_iter(),
            );
            push_unique_source_message_queued_generations(
                &mut last.source_message_queued_generations,
                intervention.source_message_queued_generations.into_iter(),
            );
            push_unique_source_text_segments(
                &mut last.source_text_segments,
                incoming_text_segments,
            );
            last.text = join_source_text_segments(&last.source_text_segments);
            last.created_at = intervention.created_at;
            // Voice metadata follows the new primary used by dispatch.
            if intervention.voice_announcement.is_some() {
                last.voice_announcement = intervention.voice_announcement;
            }
            last.pending_uploads.extend(intervention.pending_uploads);
            return EnqueueInterventionResult {
                enqueued: true,
                merged: true,
                refusal_reason: None,
                queue_exit_events,
                persistence_error: None,
            };
        }
    }

    queue.push(intervention);
    queue_exit_events.extend(drain_head_overflow(queue));
    EnqueueInterventionResult {
        enqueued: true,
        merged: false,
        refusal_reason: None,
        queue_exit_events,
        persistence_error: None,
    }
}

#[cfg(test)]
pub(crate) fn enqueue_intervention(
    queue: &mut Vec<Intervention>,
    intervention: Intervention,
    active_user_message_id: Option<MessageId>,
) -> EnqueueInterventionResult {
    enqueue_with_settlement(queue, intervention, active_user_message_id, None)
}
