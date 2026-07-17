//! #4260: dead-letter + operator notice for intervention-queue capacity
//! overflow evicts (silent-loss vector 2), split out of the giant `discord`
//! root so the `apply_queue_exit_feedback` sink keeps thin call sites.
//!
//! Dual-review r1 (codex#2 = opus#1): only `QueueExitKind::Overflow` — the
//! genuine capacity drop-oldest evict — is dead-lettered and notified.
//! `Superseded` also flows through the same sink from BENIGN producers (the
//! Clear full drain on !clear / idle-recap / teardown, and the active-source
//! purge where the message already started processing); treating those as
//! losses produced false DLQ rows and false ⏏ notices.
//!
//! Dual-review r1 (codex#1): both the DLQ insert and the notice enqueue are
//! fire-and-forget `tokio::spawn`s — a PG pool at its acquire-timeout must not
//! stall queue-exit feedback.

use std::collections::HashSet;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};

use super::{QueueExitVisibleCard, SharedData, queue_exit_card_body};
use crate::services::turn_orchestrator::{QueueExitEvent, QueueExitKind};

/// The subset of exit events that represent genuine input loss (capacity
/// eviction). Pure so the "clear/purge must not dead-letter" contract is unit
/// testable without a DB.
fn overflow_events<'a>(queue_exit_events: &[&'a QueueExitEvent]) -> Vec<&'a QueueExitEvent> {
    queue_exit_events
        .iter()
        .filter(|event| event.kind == QueueExitKind::Overflow)
        .copied()
        .collect()
}

/// Dead-letter every capacity-overflow eviction so the dropped user input is
/// durably recoverable. Fire-and-forget: recording rides detached tasks and
/// never blocks queue-exit feedback.
pub(super) fn record_queue_overflow_dead_letters(
    shared: &SharedData,
    channel_id: ChannelId,
    queue_exit_events: &[&QueueExitEvent],
) {
    for event in overflow_events(queue_exit_events) {
        let intervention = &event.intervention;
        crate::db::relay_dead_letter::record_detached(
            shared.pg_pool.as_ref(),
            crate::db::relay_dead_letter::RelayDeadLetterRecord {
                kind: crate::db::relay_dead_letter::KIND_QUEUE_OVERFLOW.to_string(),
                channel_id: channel_id.to_string(),
                author_id: Some(intervention.author_id.get().to_string()),
                message_id: Some(intervention.message_id.get().to_string()),
                content: intervention.text.clone(),
                reason: "intervention queue overflow (drop-oldest, MAX_INTERVENTIONS_PER_CHANNEL)"
                    .to_string(),
            },
        );
    }
}

/// Count the overflow evicts that never had a visible `📬 대기 중` placeholder
/// (the soft/merge paths), i.e. the ones whose loss would otherwise surface as
/// nothing but a reaction. Pure for unit testing.
fn orphan_overflow_count(
    queue_exit_events: &[&QueueExitEvent],
    visible_cards_to_clear: &[QueueExitVisibleCard],
) -> usize {
    let placeholdered: HashSet<MessageId> = visible_cards_to_clear
        .iter()
        .map(|card| card.user_msg_id)
        .collect();
    overflow_events(queue_exit_events)
        .into_iter()
        .filter(|event| {
            !event
                .intervention
                .source_message_ids
                .iter()
                .any(|id| placeholdered.contains(id))
        })
        .count()
}

/// Overflow evicts without a placeholder card get ONE compact channel notice,
/// reusing the `⏏` `queue_exit_card_body` idiom. Placeholder-backed evicts
/// already have their card rewritten to the same body, so they are excluded to
/// avoid a duplicate notice. Delivery rides the outbox (dedupe-keyed per
/// channel) so consecutive evicts collapse to one card; the enqueue itself is a
/// detached spawn (codex#1) so feedback never awaits a pool acquire.
pub(super) fn maybe_notify_orphan_queue_overflow(
    shared: &SharedData,
    channel_id: ChannelId,
    queue_exit_events: &[&QueueExitEvent],
    visible_cards_to_clear: &[QueueExitVisibleCard],
) {
    let orphan_overflow = orphan_overflow_count(queue_exit_events, visible_cards_to_clear);
    if orphan_overflow == 0 {
        return;
    }
    let Some(pool) = shared.pg_pool.clone() else {
        return;
    };
    let base = queue_exit_card_body(QueueExitKind::Overflow);
    let body = if orphan_overflow == 1 {
        base.to_string()
    } else {
        format!("{base} ({orphan_overflow}건)")
    };
    let target = format!("channel:{channel_id}");
    tokio::spawn(async move {
        let enqueued = crate::services::message_outbox::enqueue_outbox_pg(
            &pool,
            crate::services::message_outbox::OutboxMessage {
                target: &target,
                content: &body,
                bot: super::bot_role::UtilityBotRole::Notify.alias(),
                source: "queue_overflow_notice",
                reason_code: Some("queue_overflow.evict"),
                session_key: Some(&target),
            },
        )
        .await;
        if let Err(error) = enqueued {
            tracing::warn!("[dlq] failed to enqueue queue-overflow notice (best-effort): {error}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::turn_orchestrator::{Intervention, InterventionMode};
    use poise::serenity_prelude::UserId;
    use std::time::Instant;

    fn event(message_id: u64, kind: QueueExitKind) -> QueueExitEvent {
        QueueExitEvent {
            intervention: Intervention {
                author_id: UserId::new(7),
                author_is_bot: false,
                message_id: MessageId::new(message_id),
                queued_generation: 1,
                source_message_ids: vec![MessageId::new(message_id)],
                source_message_queued_generations: Vec::new(),
                source_text_segments: Vec::new(),
                text: "queued text".to_string(),
                mode: InterventionMode::Soft,
                created_at: Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
                pending_uploads: Vec::new(),
                voice_announcement: None,
            },
            kind,
        }
    }

    /// #4260 dual r1 (codex#2 = opus#1): only `Overflow` is loss. `Superseded`
    /// (Clear full drain / active-source purge) and `Cancelled` must yield ZERO
    /// dead-letter candidates and ZERO notices — the pre-rework conflation
    /// false-DLQ'd every `!clear`.
    #[test]
    fn clear_and_purge_superseded_events_are_never_dead_lettered() {
        let clear_drain = event(1, QueueExitKind::Superseded);
        let purge = event(2, QueueExitKind::Superseded);
        let cancel = event(3, QueueExitKind::Cancelled);
        let events = [&clear_drain, &purge, &cancel];
        assert!(
            overflow_events(&events).is_empty(),
            "Superseded/Cancelled must not be DLQ candidates"
        );
        assert_eq!(
            orphan_overflow_count(&events, &[]),
            0,
            "Superseded/Cancelled must not trigger the ⏏ overflow notice"
        );
    }

    #[test]
    fn overflow_events_are_dead_letter_and_notice_candidates() {
        let overflow_a = event(10, QueueExitKind::Overflow);
        let overflow_b = event(11, QueueExitKind::Overflow);
        let benign = event(12, QueueExitKind::Superseded);
        let events = [&overflow_a, &benign, &overflow_b];
        let candidates = overflow_events(&events);
        assert_eq!(candidates.len(), 2, "both Overflow evicts must be recorded");
        assert_eq!(
            orphan_overflow_count(&events, &[]),
            2,
            "placeholder-less overflow evicts must be notice-counted"
        );
    }

    #[test]
    fn placeholder_backed_overflow_is_excluded_from_the_orphan_notice() {
        let with_card = event(20, QueueExitKind::Overflow);
        let without_card = event(21, QueueExitKind::Overflow);
        let events = [&with_card, &without_card];
        let cards = [QueueExitVisibleCard {
            user_msg_id: MessageId::new(20),
            placeholder_msg_id: MessageId::new(920),
            kind: QueueExitKind::Overflow,
        }];
        assert_eq!(
            orphan_overflow_count(&events, &cards),
            1,
            "card-backed evicts already get a card rewrite — no duplicate notice"
        );
    }
}
