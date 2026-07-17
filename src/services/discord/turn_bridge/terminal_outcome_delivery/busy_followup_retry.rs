use super::empty_response_recovery::{
    EmptyResponseRecoveryOutcome, handle_empty_response_recovery,
};
use super::*;

/// Run empty-response recovery and normalize its outcome into the tuple the
/// terminal-delivery loop consumes. The recovery call writes
/// `claude_tui_busy_requeue_pending` back through its state borrow, so the
/// caller reads that flag AFTER this returns and then runs
/// [`apply_busy_requeue_if_pending`] — sequencing the two so the shared
/// mutable locals are never borrowed simultaneously.
pub(super) async fn handle_empty_response_and_busy_requeue(
    message: EmptyResponseRecoveryMessage,
    recovery_ctx: EmptyResponseRecoveryContext<'_>,
    recovery_state: EmptyResponseRecoveryState<'_>,
) -> (String, String, bool, bool) {
    match handle_empty_response_recovery(message, recovery_ctx, recovery_state).await {
        EmptyResponseRecoveryOutcome::ContinueDelivery {
            delivery_response,
            spoken_delivery_response,
            resume_retry_queued,
        } => (
            delivery_response,
            spoken_delivery_response,
            resume_retry_queued,
            false,
        ),
        EmptyResponseRecoveryOutcome::SilentTurnHandled {
            delivery_response,
            spoken_delivery_response,
            resume_retry_queued,
        } => (
            delivery_response,
            spoken_delivery_response,
            resume_retry_queued,
            true,
        ),
    }
}

/// Requeue a Claude-TUI pre-submit busy timeout, preserving the inflight turn.
/// Returns the user-facing delivery response and whether the inflight must be
/// preserved for cleanup retry (only on requeue failure — a failed requeue must
/// NOT be reported as success, #4605). Called only when the recovery step set
/// `claude_tui_busy_requeue_pending`, after its state borrow is released.
#[allow(clippy::too_many_arguments)]
pub(super) async fn apply_busy_requeue_if_pending(
    claude_tui_busy_requeue_pending: bool,
    shared_owned: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    inflight_state: &InflightTurnState,
    dispatch_id: Option<&str>,
    adk_session_key: Option<&str>,
    turn_id: &str,
    delivery_response: &mut String,
    preserve_inflight_for_cleanup_retry: &mut bool,
) {
    if !claude_tui_busy_requeue_pending {
        return;
    }
    let requeued = followup_requeue::requeue_claude_tui_followup_pre_submit_timeout(
        shared_owned,
        provider,
        channel_id,
        inflight_state,
        dispatch_id,
        adk_session_key,
        turn_id,
    )
    .await;
    if requeued {
        *delivery_response = "⏳ Claude TUI가 이전 턴을 처리 중이라 메시지를 아직 주입하지 못했습니다. 기존 세션은 유지하고 메시지를 큐에 다시 넣어, TUI가 한가해지면 자동으로 처리합니다.".to_string();
    } else {
        *preserve_inflight_for_cleanup_retry = true;
        *delivery_response = "⚠ Claude TUI가 이전 턴을 처리 중이라 메시지 재큐에 실패했습니다. 기존 세션은 유지했으니 잠시 후 다시 보내 주세요.".to_string();
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            user_msg_id = inflight_state.user_msg_id,
            "Claude TUI busy follow-up requeue failed; preserving inflight instead of reporting success"
        );
    }
}
