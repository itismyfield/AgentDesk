use poise::serenity_prelude::MessageId;

use super::{
    Intervention, QueueExitEvent, QueueExitKind, SourceMessageQueuedGeneration,
    ensure_source_message_ids, join_source_text_segments,
};

pub(super) fn intervention_sources_all_match_active(
    intervention: &Intervention,
    active_user_message_id: Option<MessageId>,
) -> bool {
    active_user_message_id.is_some_and(|active_id| {
        !intervention.source_message_ids.is_empty()
            && intervention
                .source_message_ids
                .iter()
                .all(|source_id| *source_id == active_id)
    })
}

pub(super) fn intervention_has_active_source(
    intervention: &Intervention,
    active_user_message_id: Option<MessageId>,
) -> Option<MessageId> {
    active_user_message_id.filter(|active_id| intervention.source_message_ids.contains(active_id))
}

fn source_generation_for(
    intervention: &Intervention,
    message_id: MessageId,
) -> SourceMessageQueuedGeneration {
    intervention
        .source_message_queued_generations()
        .into_iter()
        .find(|source| source.message_id == message_id)
        .unwrap_or_else(|| {
            SourceMessageQueuedGeneration::new(message_id, intervention.queued_generation)
        })
}

fn queue_exit_event_for_source(
    intervention: &Intervention,
    message_id: MessageId,
) -> QueueExitEvent {
    let mut removed = intervention.clone();
    let source_text_segment = intervention
        .source_text_segments()
        .into_iter()
        .find(|segment| segment.message_id == message_id);
    removed.message_id = message_id;
    removed.source_message_ids = vec![message_id];
    removed.source_message_queued_generations =
        vec![source_generation_for(intervention, message_id)];
    removed.text = source_text_segment
        .as_ref()
        .map(|segment| segment.text.clone())
        .unwrap_or_default();
    removed.source_text_segments = source_text_segment.into_iter().collect();
    QueueExitEvent {
        intervention: removed,
        kind: QueueExitKind::Superseded,
    }
}

pub(super) fn strip_source_message_id_from_intervention(
    intervention: &mut Intervention,
    message_id: MessageId,
) {
    ensure_source_message_ids(intervention);
    let mut source_text_segments = intervention.source_text_segments();
    intervention
        .source_message_ids
        .retain(|source_id| *source_id != message_id);
    intervention
        .source_message_queued_generations
        .retain(|source| source.message_id != message_id);
    source_text_segments.retain(|segment| segment.message_id != message_id);
    intervention.source_text_segments = source_text_segments;
    intervention.text = join_source_text_segments(&intervention.source_text_segments);

    if intervention.message_id == message_id
        && let Some(replacement) = intervention.source_message_ids.last().copied()
    {
        intervention.message_id = replacement;
        if let Some(source) = intervention
            .source_message_queued_generations
            .iter()
            .find(|source| source.message_id == replacement)
        {
            intervention.queued_generation = source.queued_generation;
        }
    }

    if !intervention.source_message_ids.is_empty() {
        ensure_source_message_ids(intervention);
    }
}

pub(super) fn purge_active_source_from_queue(
    queue: &mut Vec<Intervention>,
    active_user_message_id: MessageId,
) -> Vec<QueueExitEvent> {
    let mut queue_exit_events = Vec::new();
    let mut index = 0;
    while index < queue.len() {
        ensure_source_message_ids(&mut queue[index]);
        if !queue[index]
            .source_message_ids
            .contains(&active_user_message_id)
        {
            index += 1;
            continue;
        }

        if intervention_sources_all_match_active(&queue[index], Some(active_user_message_id)) {
            let removed = queue.remove(index);
            queue_exit_events.push(QueueExitEvent {
                intervention: removed,
                kind: QueueExitKind::Superseded,
            });
        } else {
            let event = queue_exit_event_for_source(&queue[index], active_user_message_id);
            strip_source_message_id_from_intervention(&mut queue[index], active_user_message_id);
            queue_exit_events.push(event);
            index += 1;
        }
    }
    queue_exit_events
}
