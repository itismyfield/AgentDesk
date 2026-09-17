//! One mailbox/token admission boundary shared by chat, headless and routine turns.
use super::*;
use crate::services::turn_orchestrator::TurnAdmissionOrder;

pub(in crate::services::discord) async fn mailbox_try_start_turn_kinded_with_feedback(
    shared: &SharedData,
    channel_id: ChannelId,
    cancel_token: Arc<CancelToken>,
    request_owner: UserId,
    user_message_id: MessageId,
    turn_kind: ActiveTurnKind,
) -> bool {
    mailbox_try_start_turn_ordered(
        shared,
        channel_id,
        cancel_token,
        request_owner,
        user_message_id,
        turn_kind,
        TurnAdmissionOrder::Immediate,
    )
    .await
}

/// #5937 — the Discord text-intake claim. Unlike recovery, reaper and healing
/// claims, this one is inbound traffic, so it waits behind anything already
/// queued for the channel instead of taking an idle slot ahead of it.
pub(in crate::services::discord) async fn mailbox_try_start_turn_behind_queue(
    shared: &SharedData,
    channel_id: ChannelId,
    cancel_token: Arc<CancelToken>,
    request_owner: UserId,
    user_message_id: MessageId,
) -> bool {
    mailbox_try_start_turn_ordered(
        shared,
        channel_id,
        cancel_token,
        request_owner,
        user_message_id,
        ActiveTurnKind::UserOrAgent,
        TurnAdmissionOrder::BehindQueue,
    )
    .await
}

async fn mailbox_try_start_turn_ordered(
    shared: &SharedData,
    channel_id: ChannelId,
    cancel_token: Arc<CancelToken>,
    request_owner: UserId,
    user_message_id: MessageId,
    turn_kind: ActiveTurnKind,
    admission_order: TurnAdmissionOrder,
) -> bool {
    let _recovery_admission = match crate::services::agent_recovery::admission::admit(
        &channel_id.get().to_string(),
        &shared.provider,
    )
    .await
    {
        Ok(guard) => guard,
        Err(error) => {
            tracing::warn!(
                channel_id = channel_id.get(),
                provider = shared.provider.as_str(),
                error,
                "recovery mailbox admission refused"
            );
            return false;
        }
    };
    let result = shared
        .mailbox(channel_id)
        .try_start_turn_kinded_with_persistence(
            cancel_token,
            request_owner,
            user_message_id,
            turn_kind,
            admission_order,
            queue_persistence_context(shared, &shared.provider, channel_id),
        )
        .await;
    apply_queue_exit_feedback(shared, channel_id, &result.queue_exit_events).await;
    if let Some(error) = result.persistence_error.as_ref() {
        tracing::error!(
            provider = shared.provider.as_str(),
            channel_id = channel_id.get(),
            user_message_id = user_message_id.get(),
            turn_kind = ?turn_kind,
            error = %error,
            "mailbox try-start failed durable active-source queue purge"
        );
    }
    result.started
}

pub(in crate::services::discord) async fn mailbox_recovery_kickoff(
    shared: &SharedData,
    channel_id: ChannelId,
    cancel_token: Arc<CancelToken>,
    request_owner: UserId,
    // `None` when the recovery turn has no anchored user message
    // (user_msg_id == 0, e.g. a TUI-direct turn).
    user_message_id: Option<MessageId>,
) -> RecoveryKickoffResult {
    let Ok(_admission) = crate::services::agent_recovery::admission::admit(
        &channel_id.get().to_string(),
        &shared.provider,
    )
    .await
    else {
        return RecoveryKickoffResult {
            activated_turn: false,
            refused_closed: false,
        };
    };
    // #2443 — reset the per-channel `recovery_done` latch BEFORE recovery
    // starts; a stale "done" flag would let `watchers/lifecycle.rs` graduate
    // its skip early and race the ongoing recovery. Idempotent and cheap.
    shared.mailboxes.recovery_done(channel_id).reset();
    // #3297 r3 — tombstone refusal ⇒ retry on a fresh registered actor.
    let result = shared
        .mailboxes
        .recovery_kickoff_with_closed_retry(
            channel_id,
            cancel_token,
            request_owner,
            user_message_id,
        )
        .await;
    if result.activated_turn {
        increment_global_active(shared, "recovery_kickoff");
    }
    result
}
