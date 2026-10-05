//! One mailbox/token admission boundary shared by chat, headless and routine turns.
use super::*;
use crate::services::turn_orchestrator::{
    ClaimObservation, EnqueueRefusalReason, TryStartTurnResult, TurnAdmissionOrder,
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
    let episode_nonce = cancel_token.turn_nonce().map(str::to_owned);
    mailbox_try_start_turn_adopting(
        shared,
        channel_id,
        cancel_token,
        request_owner,
        user_message_id,
        ActiveTurnKind::UserOrAgent,
        episode_nonce,
    )
    .await
}

/// Row -> lease re-adoption: the claim's own step refuses the ROW's episode unless it
/// started after the last exact release, and records the installed token as started.
pub(in crate::services::discord) async fn mailbox_try_start_turn_adopting(
    shared: &SharedData,
    channel_id: ChannelId,
    cancel_token: Arc<CancelToken>,
    request_owner: UserId,
    user_message_id: MessageId,
    kind: ActiveTurnKind,
    episode_nonce: Option<String>,
) -> TryStartTurnResult {
    mailbox_try_start_turn_ordered(
        shared,
        channel_id,
        cancel_token,
        request_owner,
        user_message_id,
        AdmissionClaim::Adopt(kind, episode_nonce),
    )
    .await
}

/// The mailbox claim an admitted turn issues.
#[derive(Clone, Debug)]
enum AdmissionClaim {
    Kinded(ActiveTurnKind, TurnAdmissionOrder),
    /// An immediate claim the actor's recovery fence may refuse for the named episode.
    Adopt(ActiveTurnKind, Option<String>),
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
    let turn_nonce = cancel_token.turn_nonce().map(str::to_owned);
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
        AdmissionClaim::Adopt(kind, ref episode_nonce) => {
            mailbox
                .try_start_turn_adopting(
                    cancel_token,
                    request_owner,
                    user_message_id,
                    kind,
                    episode_nonce.clone(),
                    persistence,
                )
                .await
        }
    };
    // The alias lands before the claim is reported, hence before any delivery
    // of this episode can append `(user_message_id, turn_nonce)`.
    if result.started
        && !result.absorbed_source_ids.is_empty()
        && let Some(turn_nonce) = turn_nonce.as_deref()
    {
        let absorbed: Vec<u64> = result
            .absorbed_source_ids
            .iter()
            .map(|id| id.get())
            .collect();
        crate::services::discord::outbound::completed_turn_ledger::record_merged_alias(
            &shared.provider,
            channel_id.get(),
            user_message_id.get(),
            turn_nonce,
            &absorbed,
        );
    }
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

pub(in crate::services::discord) const INPUT_PENDING_NOTICE: &str =
    "보류됨 — 입력 전환이 끝나면 자동 접수됩니다(5분 이내 원본). 다시 보내지 않아도 됩니다.";
async fn input_refusal_notice(
    shared: &SharedData,
    channel: ChannelId,
    sources: &[u64],
    failure: super::super::input_runtime::fence::Failure,
) {
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        return;
    };
    let text = if matches!(
        failure,
        super::super::input_runtime::fence::Failure::Mode(_)
    ) {
        INPUT_PENDING_NOTICE
    } else {
        "입력을 저장하지 못했습니다. 원본을 유지하고 다시 시도해 주세요."
    };
    for source in sources {
        if super::super::http::send_channel_message_with_reference(
            &http,
            channel,
            text,
            channel,
            MessageId::new(*source),
        )
        .await
        .is_err()
        {
            tracing::warn!(
                channel = channel.get(),
                source,
                ?failure,
                "input refusal notice failed; responsibility retained"
            );
        }
    }
}

/// `observed` is the claim observation of the snapshot that
/// classified `intervention` (catch-up); `None` for live intake.
pub(in crate::services::discord) async fn mailbox_enqueue_observed_intervention(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    intervention: Intervention,
    observed: Option<ClaimObservation>,
) -> MailboxEnqueueOutcome {
    let mut sources: Vec<u64> = intervention
        .source_message_ids
        .iter()
        .map(|id| id.get())
        .collect();
    if !sources.contains(&intervention.message_id.get()) {
        sources.push(intervention.message_id.get());
    }
    sources.sort_unstable();
    sources.dedup();
    // Tombstone refusal ⇒ retry on a fresh registered actor
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
    use crate::services::discord::input_runtime::fence::{self, Failure};
    let failure = match result.refusal_reason {
        Some(EnqueueRefusalReason::InputModeFenced(mode)) => Some(Failure::Mode(mode)),
        Some(EnqueueRefusalReason::LockTimeout) => Some(Failure::LockTimeout),
        Some(EnqueueRefusalReason::InputPersistence) => Some(Failure::Persistence),
        _ => None,
    };
    if let Some(failure) = failure {
        fence::record_failure(provider, channel_id.get(), &sources, failure);
        input_refusal_notice(shared, channel_id, &sources, failure).await;
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
