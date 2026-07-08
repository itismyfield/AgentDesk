//! #4260: dead-letter + operator notice for intervention-queue overflow evicts
//! (silent-loss vector 2), split out of the giant `discord` root so the
//! `apply_queue_exit_feedback` sink keeps thin call sites. A queue-overflow
//! drop-oldest evict silently discarded queued user input; here we durably
//! record it and, for evicts with no visible placeholder card, post one compact
//! channel notice.

use std::collections::HashSet;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};

use super::{QueueExitVisibleCard, SharedData, queue_exit_card_body};
use crate::services::turn_orchestrator::{QueueExitEvent, QueueExitKind};

/// Dead-letter every queue-overflow eviction so the dropped user input is
/// durably recoverable. Only `Superseded` (the drop-oldest overflow evict) is
/// recorded — `Cancelled` is a user-intended removal and `Expired` is a dead
/// variant, neither is a loss. Best-effort: a DLQ write never blocks feedback.
pub(super) async fn record_queue_overflow_dead_letters(
    shared: &SharedData,
    channel_id: ChannelId,
    queue_exit_events: &[&QueueExitEvent],
) {
    for event in queue_exit_events {
        if event.kind != QueueExitKind::Superseded {
            continue;
        }
        let intervention = &event.intervention;
        crate::db::relay_dead_letter::insert_best_effort(
            shared.pg_pool.as_ref(),
            &crate::db::relay_dead_letter::RelayDeadLetterRecord {
                kind: crate::db::relay_dead_letter::KIND_QUEUE_OVERFLOW.to_string(),
                channel_id: channel_id.to_string(),
                author_id: Some(intervention.author_id.get().to_string()),
                message_id: Some(intervention.message_id.get().to_string()),
                content: intervention.text.clone(),
                reason: "intervention queue overflow (drop-oldest, MAX_INTERVENTIONS_PER_CHANNEL)"
                    .to_string(),
            },
        )
        .await;
    }
}

/// Evicts that never had a visible `📬 대기 중` placeholder (the soft-drain
/// path) get no card rewrite in `apply_queue_exit_feedback`, so the user would
/// otherwise see only a reaction. Post ONE compact channel notice for them,
/// reusing the `⏏` `queue_exit_card_body` idiom. Placeholder-backed evicts
/// already have their own card rewritten to the same body, so they are excluded
/// here to avoid a duplicate notice. Delivery rides the outbox (dedupe-keyed
/// per channel) so consecutive evicts collapse to one card instead of spamming.
pub(super) async fn maybe_notify_orphan_queue_overflow(
    shared: &SharedData,
    channel_id: ChannelId,
    queue_exit_events: &[&QueueExitEvent],
    visible_cards_to_clear: &[QueueExitVisibleCard],
) {
    let placeholdered: HashSet<MessageId> = visible_cards_to_clear
        .iter()
        .map(|card| card.user_msg_id)
        .collect();
    let orphan_overflow = queue_exit_events
        .iter()
        .filter(|event| event.kind == QueueExitKind::Superseded)
        .filter(|event| {
            !event
                .intervention
                .source_message_ids
                .iter()
                .any(|id| placeholdered.contains(id))
        })
        .count();
    if orphan_overflow == 0 {
        return;
    }
    let base = queue_exit_card_body(QueueExitKind::Superseded);
    let body = if orphan_overflow == 1 {
        base.to_string()
    } else {
        format!("{base} ({orphan_overflow}건)")
    };
    let target = format!("channel:{channel_id}");
    crate::services::message_outbox::enqueue_outbox_best_effort(
        shared.pg_pool.as_ref(),
        crate::services::message_outbox::OutboxMessage {
            target: &target,
            content: &body,
            bot: "notify",
            source: "queue_overflow_notice",
            reason_code: Some("queue_overflow.evict"),
            session_key: Some(&target),
        },
    )
    .await;
}
