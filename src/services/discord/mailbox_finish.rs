use poise::serenity_prelude as serenity;
use serenity::ChannelId;

use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::FinishTurnResult;

use super::{
    SharedData, apply_queue_exit_feedback, queue_persistence_context, turn_completion_events,
};

fn unavailable_finish_turn_result() -> FinishTurnResult {
    FinishTurnResult {
        removed_token: None,
        has_pending: false,
        mailbox_online: false,
        queue_exit_events: Vec::new(),
        persistence_error: None,
    }
}

/// Recovery-only non-creating finish. Runtime selection must resolve an
/// instance-local mailbox; a process-global mirror is not runtime identity.
pub(in crate::services::discord) async fn mailbox_finish_owned_turn(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> FinishTurnResult {
    let Some(handle) = shared.mailbox_peek(channel_id) else {
        return unavailable_finish_turn_result();
    };
    let result = handle
        .finish_turn(queue_persistence_context(shared, provider, channel_id))
        .await;
    apply_queue_exit_feedback(shared, channel_id, &result.queue_exit_events).await;
    shared.mailboxes.recovery_done(channel_id).mark_done();
    turn_completion_events::publish_mailbox_release_completion_event(
        shared, channel_id, None, &result,
    );
    result
}

pub(in crate::services::discord) async fn mailbox_finish_cancelled_turn(
    shared: &SharedData,
    channel_id: ChannelId,
) -> FinishTurnResult {
    let Some(handle) = shared.mailbox_peek(channel_id) else {
        return unavailable_finish_turn_result();
    };
    let result = handle.finish_cancelled_turn().await;
    apply_queue_exit_feedback(shared, channel_id, &result.queue_exit_events).await;
    if result.removed_token.is_some() {
        shared.mailboxes.recovery_done(channel_id).mark_done();
    }
    turn_completion_events::publish_mailbox_release_completion_event(
        shared, channel_id, None, &result,
    );
    result
}

pub(in crate::services::discord) async fn mailbox_finish_turn(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> FinishTurnResult {
    let result = shared
        .mailbox(channel_id)
        .finish_turn(queue_persistence_context(shared, provider, channel_id))
        .await;
    apply_queue_exit_feedback(shared, channel_id, &result.queue_exit_events).await;
    // #2443 — finish_turn is the success-path exit for the recovery engine
    // (recovery_engine.rs L648). Marking `recovery_done` here covers the
    // "recovery completed normally" branch so the watcher waiting on
    // `recovery_done.wait()` can proceed without waiting for the 60s timeout
    // that the legacy heuristic depended on. The latch is idempotent — if
    // `mailbox_clear_recovery_marker` already ran, this is a no-op.
    shared.mailboxes.recovery_done(channel_id).mark_done();
    turn_completion_events::publish_mailbox_release_completion_event(
        shared, channel_id, None, &result,
    );
    result
}

/// #3016 — identity-guarded variant of [`mailbox_finish_turn`]. Finalizes the
/// channel's active turn ONLY when the mailbox's current
/// `active_user_message_id` still matches `expected_user_message_id`. Used by
/// the `TurnFinalizer` when the terminal carries a real `user_msg_id` so a
/// stale / channel-only terminal arriving in the narrow window between one
/// turn finalizing and the next turn's `try_start_turn` (or after ledger GC)
/// cannot release the WRONG (newer) turn's token or decrement `global_active`.
/// On mismatch it returns `removed_token = None`, exactly like an idempotent
/// second `mailbox_finish_turn`, so the finalizer's counter-decrement gate is
/// a no-op.
pub(in crate::services::discord) async fn mailbox_finish_turn_if_matches(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    expected_user_message_id: serenity::model::id::MessageId,
) -> FinishTurnResult {
    let result = shared
        .mailbox(channel_id)
        .finish_turn_if_matches(
            expected_user_message_id,
            queue_persistence_context(shared, provider, channel_id),
        )
        .await;
    apply_queue_exit_feedback(shared, channel_id, &result.queue_exit_events).await;
    // Mirror `mailbox_finish_turn`: a successful guarded finish is also a
    // recovery-engine success exit. Only mark `recovery_done` when this call
    // actually finalized (removed a token); a mismatch no-op must not free a
    // watcher waiting on a turn that is still live.
    if result.removed_token.is_some() {
        shared.mailboxes.recovery_done(channel_id).mark_done();
    }
    turn_completion_events::publish_mailbox_release_completion_event(
        shared,
        channel_id,
        Some(expected_user_message_id.get()),
        &result,
    );
    result
}

async fn mailbox_finish_turn_if_matches_episode_started_before_inner(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    expected_user_message_id: serenity::model::id::MessageId,
    expected_turn_nonce: Option<String>,
    active_started_before: std::time::Instant,
    publish_completion: bool,
) -> FinishTurnResult {
    let result = shared
        .mailbox(channel_id)
        .finish_turn_if_matches_episode_started_before(
            expected_user_message_id,
            expected_turn_nonce,
            active_started_before,
            queue_persistence_context(shared, provider, channel_id),
        )
        .await;
    apply_queue_exit_feedback(shared, channel_id, &result.queue_exit_events).await;
    if result.removed_token.is_some() {
        shared.mailboxes.recovery_done(channel_id).mark_done();
    }
    if publish_completion {
        turn_completion_events::publish_mailbox_release_completion_event(
            shared,
            channel_id,
            Some(expected_user_message_id.get()),
            &result,
        );
    }
    result
}

pub(in crate::services::discord) async fn mailbox_finish_turn_if_matches_episode_started_before(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    expected_user_message_id: serenity::model::id::MessageId,
    expected_turn_nonce: Option<String>,
    active_started_before: std::time::Instant,
) -> FinishTurnResult {
    mailbox_finish_turn_if_matches_episode_started_before_inner(
        shared,
        provider,
        channel_id,
        expected_user_message_id,
        expected_turn_nonce,
        active_started_before,
        true,
    )
    .await
}

pub(in crate::services::discord) async fn mailbox_finish_turn_if_matches_episode_started_before_without_completion(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    expected_user_message_id: serenity::model::id::MessageId,
    expected_turn_nonce: Option<String>,
    active_started_before: std::time::Instant,
) -> FinishTurnResult {
    mailbox_finish_turn_if_matches_episode_started_before_inner(
        shared,
        provider,
        channel_id,
        expected_user_message_id,
        expected_turn_nonce,
        active_started_before,
        false,
    )
    .await
}

#[cfg(test)]
mod relay_state_contract_refs {
    #[test]
    fn contract_symbols_exist() {
        let _ = super::mailbox_finish_turn_if_matches_episode_started_before;
        let _ = crate::services::provider::CancelToken::turn_nonce;
        let _ = |snapshot: &crate::services::turn_orchestrator::ChannelMailboxSnapshot| {
            let _ = &snapshot.active_turn_nonce;
        };
    }
}
