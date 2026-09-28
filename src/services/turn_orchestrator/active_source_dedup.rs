use poise::serenity_prelude::MessageId;

use super::{
    ChannelMailboxState, EnqueueInterventionResult, EnqueueRefusalReason, Intervention,
    QueueExitEvent, QueueExitKind, SourceMessageQueuedGeneration, ensure_source_message_ids,
    join_source_text_segments,
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

/// Pre-hydrate refusal of work the active turn already runs: its own message, or
/// (#6035) only ids its merged head absorbed, which would otherwise run twice.
pub(super) fn active_turn_enqueue_refusal(
    state: &ChannelMailboxState,
    intervention: &Intervention,
) -> Option<EnqueueRefusalReason> {
    if intervention_sources_all_match_active(intervention, state.active_user_message_id) {
        return Some(EnqueueRefusalReason::AlreadyActiveTurn);
    }
    let sources = &intervention.source_message_ids;
    let absorbed = &state.active_absorbed_source_ids;
    (!sources.is_empty() && sources.iter().all(|source| absorbed.contains(source)))
        .then_some(EnqueueRefusalReason::AbsorbedByActiveTurn)
}

impl EnqueueInterventionResult {
    pub(super) fn refused(
        reason: EnqueueRefusalReason,
        queue_exit_events: Vec<QueueExitEvent>,
    ) -> Self {
        Self {
            enqueued: false,
            merged: false,
            refusal_reason: Some(reason),
            queue_exit_events,
            persistence_error: None,
        }
    }
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
    purge_sources_from_queue(queue, |_, source_id| source_id == active_user_message_id)
}

/// Takes the next soft head after dropping sources whose completed-turn ledger commit
/// is later than the row's enqueue; an earlier commit belongs to a previous episode.
pub(super) fn take_unsettled(
    state: &mut super::ChannelMailboxState,
    channel_id: poise::serenity_prelude::ChannelId,
    primary_message_id: Option<MessageId>,
) -> super::TakeNextSoftResult {
    let provider = state.last_persistence.as_ref().map(|p| p.provider.clone());
    let committed = provider.as_ref().map_or_else(Default::default, |provider| {
        crate::services::discord::outbound::completed_turn_ledger::settled_commit_ms_by_user_msg_id(
            provider,
            channel_id.get(),
        )
    });
    let now_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_micros());
    let queue = &mut state.intervention_queue;
    let settled_exits = purge_sources_from_queue(queue, |row, id| {
        let enqueued_us = now_us.saturating_sub(row.created_at.elapsed().as_micros());
        committed
            .get(&id.get())
            .is_some_and(|commit_ms| u128::from(*commit_ms) * 1000 > enqueued_us)
    });
    for event in &settled_exits {
        crate::services::observability::record_invariant_check_with_severity(
            false,
            crate::services::observability::InvariantViolation {
                provider: provider.as_ref().map(|provider| provider.as_str()),
                channel_id: Some(channel_id.get()),
                dispatch_id: None,
                session_key: None,
                turn_id: None,
                invariant: QUEUE_ROW_SETTLED_BY_COMPLETED_TURN_LEDGER,
                code_location: "src/services/turn_orchestrator/active_source_dedup.rs:take_unsettled",
                message: "queued source already has a confirmed terminal delivery; removed instead of re-dispatching",
                details: serde_json::json!({
                    "message_id": event.intervention.message_id.get(),
                    "source_count": event.intervention.source_message_ids.len(),
                    "queued_generation": event.intervention.queued_generation,
                }),
            },
            crate::services::observability::InvariantSeverity::Warn,
        );
    }
    let mut result = super::dequeue_next_soft_intervention(queue, primary_message_id);
    result.queue_exit_events.splice(0..0, settled_exits);
    result
}

const QUEUE_ROW_SETTLED_BY_COMPLETED_TURN_LEDGER: &str =
    "queue_row_settled_by_completed_turn_ledger";

fn purge_sources_from_queue(
    queue: &mut Vec<Intervention>,
    is_settled: impl Fn(&Intervention, MessageId) -> bool,
) -> Vec<QueueExitEvent> {
    let mut queue_exit_events = Vec::new();
    let mut index = 0;
    while index < queue.len() {
        ensure_source_message_ids(&mut queue[index]);
        let settled_sources: Vec<MessageId> = queue[index]
            .source_message_ids
            .iter()
            .copied()
            .filter(|source_id| is_settled(&queue[index], *source_id))
            .collect();
        if settled_sources.is_empty() {
            index += 1;
            continue;
        }

        if settled_sources.len() == queue[index].source_message_ids.len() {
            let removed = queue.remove(index);
            queue_exit_events.push(QueueExitEvent {
                intervention: removed,
                kind: QueueExitKind::Superseded,
            });
        } else {
            for source_id in settled_sources {
                let event = queue_exit_event_for_source(&queue[index], source_id);
                strip_source_message_id_from_intervention(&mut queue[index], source_id);
                queue_exit_events.push(event);
            }
            index += 1;
        }
    }
    queue_exit_events
}
