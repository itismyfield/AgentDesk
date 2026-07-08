use super::*;

pub(super) async fn requeue_claude_tui_followup_pre_submit_timeout(
    shared_owned: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    inflight_state: &InflightTurnState,
    dispatch_id: Option<&str>,
    adk_session_key: Option<&str>,
    turn_id: &str,
) {
    let requeue_outcome = super::super::mailbox_requeue_inflight_for_followup_retry(
        shared_owned,
        provider,
        channel_id,
        inflight_state,
    )
    .await;
    let requeue_refusal_reason = requeue_outcome.refusal_reason.map(|reason| reason.as_str());
    tracing::info!(
        provider = %provider.as_str(),
        channel_id = channel_id.get(),
        user_msg_id = inflight_state.user_msg_id,
        requeue_enqueued = requeue_outcome.enqueued,
        requeue_merged = requeue_outcome.merged,
        requeue_refusal_reason = requeue_refusal_reason.unwrap_or("none"),
        requeue_persistence_error = requeue_outcome.persistence_error.as_deref().unwrap_or("none"),
        "claude_tui follow-up pre-submit timeout: requeue attempt completed"
    );
    crate::services::observability::emit_inflight_lifecycle_event(
        provider.as_str(),
        channel_id.get(),
        dispatch_id,
        adk_session_key,
        Some(turn_id),
        "claude_tui_followup_pre_submit_requeue",
        serde_json::json!({
            "user_msg_id": inflight_state.user_msg_id,
            "requeue_enqueued": requeue_outcome.enqueued,
            "requeue_merged": requeue_outcome.merged,
            "requeue_refusal_reason": requeue_refusal_reason,
            "requeue_persistence_error": requeue_outcome.persistence_error,
        }),
    );

    let retry_present_or_accepted = requeue_outcome.enqueued
        || matches!(
            requeue_outcome.refusal_reason,
            Some(
                crate::services::turn_orchestrator::EnqueueRefusalReason::SourceIdAlreadyQueued
                    | crate::services::turn_orchestrator::EnqueueRefusalReason::LastItemDedup
            )
        );
    if retry_present_or_accepted {
        super::super::schedule_deferred_idle_queue_kickoff(
            shared_owned.clone(),
            provider.clone(),
            channel_id,
            "claude_tui_followup_requeue_inflight",
        );
    }
}
