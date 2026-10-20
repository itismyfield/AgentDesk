//! A live arrival may reuse its sole plain queued source, preserving the queued-head transaction.
use super::*;
use crate::services::turn_orchestrator::{SourceMessageQueuedGeneration, prefix};
use std::time::Instant;

pub(super) async fn own_queued_head(
    shared: &Arc<SharedData>,
    request: &HumanInputRequest,
    origin: Origin,
) -> Result<InjectAttempt, Origin> {
    let Origin::Discord { message, .. } = &origin else {
        return Err(origin);
    };
    if !Origin::queued_head_ready(&request.provider, request.channel_id) {
        return Err(origin);
    }
    let snapshot = crate::services::discord::mailbox_snapshot(shared, request.channel_id).await;
    let [head] = snapshot.intervention_queue.as_slice() else {
        return Err(origin);
    };
    if snapshot.pending_user_dispatch.is_some()
        || snapshot.active_user_message_id == Some(*message)
        || snapshot.active_absorbed_source_ids.contains(message)
        || head.message_id != *message
        || head.author_id.get() != request.author_id
        || head.text != request.text
        || !prefix::eligible(head)
    {
        return Err(origin);
    }
    let expected = QueueIdentity::of(head);
    let Origin::Discord { name, guard, .. } = origin else {
        unreachable!("the origin was checked before the mailbox read")
    };
    // Discord origins already passed the live channel gate; the actor still applies its queued gate.
    let admitted = |_: ChannelId, _: Option<ChannelId>| true;
    let attempt = queued_owner::queued_attempt(
        shared,
        request,
        expected,
        None,
        admitted,
        Some((name, guard)),
    )
    .await;
    Ok(match attempt {
        // This source already has queue custody, even when no queued-head reservation was taken.
        InjectAttempt::NotSent(_) => InjectAttempt::NotSent(SOURCE_OWNED),
        other => other,
    })
}

pub(super) async fn confirmed(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    sources: &[SourceMessageQueuedGeneration],
) {
    let http = shared.serenity_http_or_token_fallback();
    for source in sources {
        inject_disposition::note_terminal(
            provider,
            channel,
            Some(source.message_id),
            InjectionOutcome::Observed,
            Instant::now(),
        );
        record_terminal(
            provider,
            channel,
            source.message_id,
            InjectionOutcome::Observed,
        )
        .await;
        if let Some(http) = &http {
            for emoji in crate::services::discord::queue_reactions::QUEUE_PENDING_REACTION_EMOJIS {
                crate::services::discord::queue_marker::note_removed_queued_generation(
                    shared,
                    http,
                    channel,
                    source.message_id,
                    emoji,
                    source.queued_generation,
                    "busy_inject_confirmed",
                )
                .await;
            }
        }
    }
}
