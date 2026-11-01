use super::*;

pub(super) fn present_or_accepted(outcome: &MailboxEnqueueOutcome) -> bool {
    outcome.enqueued
        || matches!(
            outcome.refusal_reason,
            Some(
                crate::services::turn_orchestrator::EnqueueRefusalReason::SourceIdAlreadyQueued
                    | crate::services::turn_orchestrator::EnqueueRefusalReason::SourceIdPendingOrActive
            )
        )
}

pub(super) struct FinalizeEnqueueContext<'a> {
    pub(super) shared: &'a Arc<SharedData>,
    pub(super) http: &'a Arc<serenity::http::Http>,
    pub(super) provider: &'a crate::services::provider::ProviderKind,
    pub(super) channel_id: ChannelId,
    pub(super) user_msg_id: MessageId,
    pub(super) placeholder_msg_id: Option<MessageId>,
    pub(super) turn_start_attempt:
        Option<crate::services::discord::turn_view_reconciler::TurnStartAttempt>,
    pub(super) session_retry_context:
        Option<&'a crate::services::discord::router::turn_start::FormattedSessionRetryContext>,
    pub(super) feedback_reminder: Option<&'a str>,
    pub(super) wip_warning: Option<&'a str>,
}

/// The V2 front already owns its original. Roll back only this unstarted turn, without
/// publishing a completion that would wake the same front or handing it to the legacy queue.
#[cfg(unix)]
pub(super) async fn defer_owned_front(
    context: FinalizeEnqueueContext<'_>,
    cancel: &Arc<CancelToken>,
) {
    let FinalizeEnqueueContext {
        shared,
        http,
        provider,
        channel_id,
        user_msg_id,
        placeholder_msg_id,
        turn_start_attempt,
        session_retry_context,
        feedback_reminder,
        wip_warning,
    } = context;
    let started = shared.turn_start_times.get(&channel_id).map(|at| *at);
    let finish = crate::services::discord::mailbox_finish::mailbox_finish_turn_if_matches_episode_started_before_with_actor_without_completion(
        shared, provider, channel_id, user_msg_id, cancel.turn_nonce().map(str::to_owned),
        std::time::Instant::now(), Some(cancel.clone()),
    ).await;
    let released = finish.removed_token.is_some();
    if let Some(removed) = finish.removed_token {
        removed.mark_completion_cleanup();
        removed
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        crate::services::discord::saturating_decrement_global_active(shared);
        if let Some(started) = started {
            shared
                .turn_start_times
                .remove_if(&channel_id, |_, at| *at == started);
        }
        put_back_session_retry_context(
            shared,
            channel_id,
            session_retry_context,
            Some("owned_busy_pre_submit"),
        );
        put_back_turn_end_wip_warning(
            shared,
            provider,
            channel_id,
            wip_warning,
            Some("owned_busy_pre_submit"),
        );
        put_back_voluntary_feedback_reminder(
            shared,
            provider,
            channel_id,
            feedback_reminder,
            Some("owned_busy_pre_submit"),
        );
        super::intake_turn::race_loss::mailbox_reaction::note_busy_tui_pre_submit_queue_pending(
            shared,
            http,
            channel_id,
            user_msg_id,
            false,
            turn_start_attempt,
        )
        .await;
        if let Some(placeholder) = placeholder_msg_id {
            let _ = channel_id.delete_message(http, placeholder).await;
        }
    }
    tracing::info!(
        channel_id = channel_id.get(),
        user_msg_id = user_msg_id.get(),
        released,
        "owner busy pre-submit held without completion or legacy requeue"
    );
}

pub(super) async fn finalize_enqueue(
    context: FinalizeEnqueueContext<'_>,
    outcome: &MailboxEnqueueOutcome,
) -> bool {
    let FinalizeEnqueueContext {
        shared,
        http,
        provider,
        channel_id,
        user_msg_id,
        placeholder_msg_id,
        turn_start_attempt,
        session_retry_context,
        feedback_reminder,
        wip_warning,
    } = context;
    let accepted = present_or_accepted(outcome);
    if outcome.enqueued {
        super::intake_turn::race_loss::mailbox_reaction::note_busy_tui_pre_submit_queue_pending(
            shared,
            http,
            channel_id,
            user_msg_id,
            outcome.merged,
            turn_start_attempt,
        )
        .await;
        if let Some(placeholder_msg_id) = placeholder_msg_id {
            let _ = channel_id.delete_message(http, placeholder_msg_id).await;
        }
    } else if accepted {
        tracing::info!(
            channel_id = channel_id.get(),
            user_msg_id = user_msg_id.get(),
            refusal_reason = outcome
                .refusal_reason
                .map(|reason| reason.as_str())
                .unwrap_or("none"),
            "claude_tui follow-up retry already queued or in progress"
        );
        if let Some(placeholder_msg_id) = placeholder_msg_id {
            let _ = channel_id.delete_message(http, placeholder_msg_id).await;
        }
    } else {
        super::tui_followup::apply_tui_busy_enqueue_refusal(
            shared,
            http,
            provider,
            (channel_id, user_msg_id, placeholder_msg_id),
            session_retry_context,
            feedback_reminder,
            wip_warning,
            outcome.refusal_reason,
        )
        .await;
        crate::services::discord::turn_view_reconciler::note_intake_turn_cleared_current(
            shared,
            http,
            channel_id,
            user_msg_id,
            "intake_busy_queue",
        )
        .await;
    }
    accepted
}
