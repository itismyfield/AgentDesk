//! #4260/#5034: intervention-queue capacity eviction, split out of the giant
//! `turn_orchestrator` root. Capacity eviction when the queue exceeds
//! `MAX_INTERVENTIONS_PER_CHANNEL` is the genuine
//! input-loss vector (silent-loss vector 2), so it emits the dedicated
//! `QueueExitKind::Overflow` — the only kind the sink
//! (`apply_queue_exit_feedback`) dead-letters + notifies on. Benign producers
//! (Clear full drain, active-source purge) keep `Superseded` and are never
//! dead-lettered. Bot entries absorb admission/capacity pressure first so an
//! operational turn trigger cannot evict queued user input. Every drain site routes through here so a capacity evict
//! always produces exit events — never a bare `queue.drain(..)`.

use super::{
    Intervention, MAX_BOT_INTERVENTIONS_PER_CHANNEL, MAX_INTERVENTIONS_PER_CHANNEL, QueueExitEvent,
    QueueExitKind,
};

fn remove_overflow_at(queue: &mut Vec<Intervention>, index: usize) -> QueueExitEvent {
    QueueExitEvent::new(queue.remove(index), QueueExitKind::Overflow)
}

/// Enforce the bot admission ceiling without sweeping a pre-existing queue.
///
/// One tail enqueue can add at most one bot entry, so removing one oldest bot
/// prevents new bot traffic from growing an already over-limit restored queue
/// while avoiding a destructive bulk cleanup of legacy backlog.
pub(super) fn drain_bot_admission_overflow(
    queue: &mut Vec<Intervention>,
    incoming_author_is_bot: bool,
) -> Vec<QueueExitEvent> {
    if !incoming_author_is_bot
        || queue.iter().filter(|item| item.author_is_bot).count()
            <= MAX_BOT_INTERVENTIONS_PER_CHANNEL
    {
        return Vec::new();
    }

    let oldest_bot = queue
        .iter()
        .position(|item| item.author_is_bot)
        .expect("bot count above zero has a bot eviction candidate");
    vec![remove_overflow_at(queue, oldest_bot)]
}

/// Make room for a bot-authored front requeue without displacing user input.
/// A full human-only queue rejects the bot retry itself; otherwise the oldest
/// already-queued bot absorbs either the bot ceiling or shared-capacity pressure.
pub(super) fn prepare_bot_front_requeue(
    queue: &mut Vec<Intervention>,
    incoming: &Intervention,
) -> Result<Vec<QueueExitEvent>, QueueExitEvent> {
    if !incoming.author_is_bot {
        return Ok(Vec::new());
    }

    let bot_count = queue.iter().filter(|item| item.author_is_bot).count();
    let must_make_room = bot_count >= MAX_BOT_INTERVENTIONS_PER_CHANNEL
        || queue.len() >= MAX_INTERVENTIONS_PER_CHANNEL;
    if !must_make_room {
        return Ok(Vec::new());
    }
    if let Some(oldest_bot) = queue.iter().position(|item| item.author_is_bot) {
        return Ok(vec![remove_overflow_at(queue, oldest_bot)]);
    }

    Err(QueueExitEvent::new(
        incoming.clone(),
        QueueExitKind::Overflow,
    ))
}

/// Preserve the front-restored intervention, preferring an existing bot over
/// the tail user when shared capacity must be reclaimed.
pub(super) fn drain_front_requeue_overflow(queue: &mut Vec<Intervention>) -> Vec<QueueExitEvent> {
    let mut evicted = Vec::new();
    while queue.len() > MAX_INTERVENTIONS_PER_CHANNEL {
        let index = queue
            .iter()
            .enumerate()
            .skip(1)
            .find_map(|(index, item)| item.author_is_bot.then_some(index))
            .unwrap_or(queue.len() - 1);
        evicted.push(remove_overflow_at(queue, index));
    }
    evicted
}

/// Drain `queue.len() - MAX` entries as `Overflow` exit events, choosing the
/// oldest bot first. The legacy name remains because all callers share this
/// capacity primitive; when no bot exists its behavior is still head eviction.
pub(super) fn drain_head_overflow(queue: &mut Vec<Intervention>) -> Vec<QueueExitEvent> {
    let mut evicted = Vec::new();
    while queue.len() > MAX_INTERVENTIONS_PER_CHANNEL {
        let index = queue
            .iter()
            .position(|item| item.author_is_bot)
            .unwrap_or(0);
        evicted.push(remove_overflow_at(queue, index));
    }
    evicted
}

/// Result of the soft-queue probe. Carries the overflow `QueueExitEvent`s
/// instead of draining eventlessly. Defensive refactor (#4260 dual-review r1):
/// the previous bare `queue.drain(..)` in `has_soft_intervention_at` never
/// caused a real loss — its only live caller (diagnostics `reports.rs`)
/// operates on a throwaway CLONE of the queue — but an eventless drain
/// primitive was one new caller away from becoming one, so the probe now
/// surfaces the events and lets clone-path callers discard them explicitly.
pub(crate) struct SoftInterventionProbe {
    pub(crate) has_pending: bool,
    pub(crate) queue_exit_events: Vec<QueueExitEvent>,
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use poise::serenity_prelude::{MessageId, UserId};

    use super::*;
    use crate::services::turn_orchestrator::{
        InterventionMode, SourceMessageQueuedGeneration, enqueue_intervention,
        front_requeue::requeue_intervention_front,
    };

    fn intervention(message_id: u64, author_is_bot: bool) -> Intervention {
        Intervention {
            author_id: UserId::new(if author_is_bot { 9001 } else { message_id }),
            author_is_bot,
            message_id: MessageId::new(message_id),
            queued_generation: 1,
            source_message_ids: vec![MessageId::new(message_id)],
            source_message_queued_generations: vec![SourceMessageQueuedGeneration::new(
                MessageId::new(message_id),
                1,
            )],
            source_text_segments: Vec::new(),
            text: format!("queue-entry-{message_id}"),
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
    fn bot_capacity_pressure_never_evicts_queued_user_input() {
        let mut queue = vec![intervention(1, false)];
        queue.extend((2..=MAX_INTERVENTIONS_PER_CHANNEL as u64).map(|id| intervention(id, true)));

        let outcome = enqueue_intervention(&mut queue, intervention(100, false), None);

        assert!(
            queue
                .iter()
                .any(|item| item.message_id == MessageId::new(1))
        );
        assert!(
            queue
                .iter()
                .any(|item| item.message_id == MessageId::new(100))
        );
        assert_eq!(queue.len(), MAX_INTERVENTIONS_PER_CHANNEL);
        assert_eq!(outcome.queue_exit_events.len(), 1);
        assert!(outcome.queue_exit_events[0].intervention.author_is_bot);
        assert_eq!(
            outcome.queue_exit_events[0].intervention.message_id,
            MessageId::new(2),
            "the oldest bot, not the oldest user, must absorb shared-capacity overflow"
        );
    }

    #[test]
    fn bot_admission_cap_replaces_the_oldest_bot_entry() {
        let mut queue = Vec::new();
        for id in 1..=MAX_BOT_INTERVENTIONS_PER_CHANNEL as u64 {
            assert!(enqueue_intervention(&mut queue, intervention(id, true), None).enqueued);
        }

        let newest_id = MAX_BOT_INTERVENTIONS_PER_CHANNEL as u64 + 1;
        let outcome = enqueue_intervention(&mut queue, intervention(newest_id, true), None);

        assert!(outcome.enqueued);
        assert_eq!(queue.len(), MAX_BOT_INTERVENTIONS_PER_CHANNEL);
        assert_eq!(outcome.queue_exit_events.len(), 1);
        assert_eq!(
            outcome.queue_exit_events[0].intervention.message_id,
            MessageId::new(1)
        );
        assert!(
            queue
                .iter()
                .any(|item| item.message_id == MessageId::new(newest_id))
        );
    }

    #[test]
    fn bot_front_requeue_yields_to_a_full_human_queue() {
        let mut queue: Vec<_> = (1..=MAX_INTERVENTIONS_PER_CHANNEL as u64)
            .map(|id| intervention(id, false))
            .collect();

        let outcome =
            requeue_intervention_front(&mut queue, intervention(100, true), None, None, None);

        assert!(!outcome.enqueued);
        assert_eq!(queue.len(), MAX_INTERVENTIONS_PER_CHANNEL);
        assert!(queue.iter().all(|item| !item.author_is_bot));
        assert_eq!(outcome.queue_exit_events.len(), 1);
        assert_eq!(
            outcome.queue_exit_events[0].intervention.message_id,
            MessageId::new(100)
        );
    }
}
