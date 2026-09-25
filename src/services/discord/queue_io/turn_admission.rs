//! One mailbox/token admission boundary shared by chat, headless and routine turns.
use super::*;
use crate::services::turn_orchestrator::{
    ClaimObservation, TryStartTurnResult, TurnAdmissionOrder,
};

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
        AdmissionClaim::Kinded(turn_kind, TurnAdmissionOrder::Immediate),
    )
    .await
    .started
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
        AdmissionClaim::Kinded(ActiveTurnKind::UserOrAgent, TurnAdmissionOrder::BehindQueue),
    )
    .await
    .started
}

/// Recovery re-mint of a persisted episode: refused by the mailbox actor, in
/// the same step as the claim, when that episode did not start after the last
/// exact release.
pub(in crate::services::discord) async fn mailbox_try_start_turn_unless_released(
    shared: &SharedData,
    channel_id: ChannelId,
    cancel_token: Arc<CancelToken>,
    request_owner: UserId,
    user_message_id: MessageId,
) -> TryStartTurnResult {
    mailbox_try_start_turn_ordered(
        shared,
        channel_id,
        cancel_token,
        request_owner,
        user_message_id,
        AdmissionClaim::UnlessReleased,
    )
    .await
}

/// The mailbox claim an admitted turn issues.
#[derive(Clone, Copy, Debug)]
enum AdmissionClaim {
    Kinded(ActiveTurnKind, TurnAdmissionOrder),
    /// An immediate user claim the actor's recovery fence may refuse.
    UnlessReleased,
}

async fn mailbox_try_start_turn_ordered(
    shared: &SharedData,
    channel_id: ChannelId,
    cancel_token: Arc<CancelToken>,
    request_owner: UserId,
    user_message_id: MessageId,
    claim: AdmissionClaim,
) -> TryStartTurnResult {
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
            return TryStartTurnResult::default();
        }
    };
    let mailbox = shared.mailbox(channel_id);
    let persistence = queue_persistence_context(shared, &shared.provider, channel_id);
    let result = match claim {
        AdmissionClaim::Kinded(turn_kind, admission_order) => {
            mailbox
                .try_start_turn_kinded_with_persistence(
                    cancel_token,
                    request_owner,
                    user_message_id,
                    turn_kind,
                    admission_order,
                    persistence,
                )
                .await
        }
        AdmissionClaim::UnlessReleased => {
            mailbox
                .try_start_turn_unless_released(
                    cancel_token,
                    request_owner,
                    user_message_id,
                    persistence,
                )
                .await
        }
    };
    apply_queue_exit_feedback(shared, channel_id, &result.queue_exit_events).await;
    if let Some(error) = result.persistence_error.as_ref() {
        tracing::error!(
            provider = shared.provider.as_str(),
            channel_id = channel_id.get(),
            user_message_id = user_message_id.get(),
            claim = ?claim,
            error = %error,
            "mailbox try-start failed durable active-source queue purge"
        );
    }
    result
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
        return RecoveryKickoffResult::Unavailable;
    };
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
    if result.activated_turn() {
        increment_global_active(shared, "recovery_kickoff");
    } else {
        // Only `Activated` gives the caller a slot to drive; the occupant keeps
        // its lease and the recovered inflight row is not adopted.
        tracing::warn!(
            channel_id = channel_id.get(),
            outcome = ?result,
            "recovery kickoff did not claim the mailbox slot"
        );
    }
    result
}

#[cfg(test)]
#[path = "turn_admission_tests.rs"]
mod tests;

/// `observed` is the claim observation of the snapshot that
/// classified `intervention` (catch-up); `None` for live intake.
pub(in crate::services::discord) async fn mailbox_enqueue_observed_intervention(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    intervention: Intervention,
    observed: Option<ClaimObservation>,
) -> MailboxEnqueueOutcome {
    // #3297 r3 — tombstone refusal ⇒ retry on a fresh registered actor
    // instead of orphaning the queue on a purged one.
    let result = shared
        .mailboxes
        .enqueue_with_closed_retry(
            channel_id,
            intervention,
            queue_persistence_context(shared, provider, channel_id),
            observed,
        )
        .await;
    apply_queue_exit_feedback(shared, channel_id, &result.queue_exit_events).await;
    if let Some(error) = result.persistence_error.as_ref() {
        tracing::error!(
            provider = provider.as_str(),
            channel_id = channel_id.get(),
            error = %error,
            "mailbox enqueue failed durable pending-queue persistence"
        );
    }
    if result.enqueued && result.persistence_error.is_none() {
        schedule_post_enqueue_idle_queue_kick(shared.clone(), provider.clone(), channel_id);
    }
    MailboxEnqueueOutcome {
        enqueued: result.enqueued,
        merged: result.merged,
        refusal_reason: result.refusal_reason,
        persistence_error: result.persistence_error,
    }
}
