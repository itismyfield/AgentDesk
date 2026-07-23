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
    pub(super) placeholder_msg_id: MessageId,
    pub(super) turn_start_attempt:
        Option<crate::services::discord::turn_view_reconciler::TurnStartAttempt>,
    pub(super) session_retry_context:
        Option<&'a crate::services::discord::router::turn_start::FormattedSessionRetryContext>,
    pub(super) feedback_reminder: Option<&'a str>,
    pub(super) wip_warning: Option<&'a str>,
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
        let _ = channel_id.delete_message(http, placeholder_msg_id).await;
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
        let _ = channel_id.delete_message(http, placeholder_msg_id).await;
    } else {
        super::tui_followup::apply_tui_busy_enqueue_refusal(
            shared,
            http,
            provider,
            channel_id,
            placeholder_msg_id,
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
