//! Recovery-retry fallback for terminal outcome delivery.

use std::sync::Arc;

use super::*;

pub(super) enum RecoveryRetryMessage {
    SessionDiedDuringRecovery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RecoveryRetryOutcome {
    Continue,
}

pub(super) struct RecoveryRetryContext<'a> {
    pub(super) shared_owned: &'a Arc<SharedData>,
    pub(super) gateway: &'a Arc<dyn TurnGateway>,
    pub(super) cancel_token: &'a Arc<crate::services::provider::CancelToken>,
    pub(super) channel_id: ChannelId,
    pub(super) user_msg_id: Option<MessageId>,
    pub(super) current_msg_id: MessageId,
    pub(super) adk_session_key: &'a Option<String>,
    pub(super) user_text_owned: &'a String,
}

pub(super) struct RecoveryRetryState<'a> {
    pub(super) full_response: &'a mut String,
    pub(super) new_session_id: &'a mut Option<String>,
    pub(super) new_raw_provider_session_id: &'a mut Option<String>,
    pub(super) inflight_state: &'a mut InflightTurnState,
}

#[rustfmt::skip]
pub(super) async fn handle_recovery_retry(
    message: RecoveryRetryMessage,
    ctx: RecoveryRetryContext<'_>,
    state: RecoveryRetryState<'_>,
) -> RecoveryRetryOutcome {
    let shared_owned = Arc::clone(ctx.shared_owned);
    let gateway = Arc::clone(ctx.gateway);
    let cancel_token = Arc::clone(ctx.cancel_token);
    let channel_id = ctx.channel_id;
    let user_msg_id = ctx.user_msg_id;
    let current_msg_id = ctx.current_msg_id;
    let adk_session_key = ctx.adk_session_key;
    let user_text_owned = ctx.user_text_owned;

    let mut full_response = std::mem::take(state.full_response);
    let mut new_session_id = state.new_session_id.take();
    let mut new_raw_provider_session_id = state.new_raw_provider_session_id.take();
    let mut inflight_state = &mut *state.inflight_state;

    match message {
        RecoveryRetryMessage::SessionDiedDuringRecovery => {
    // Recovery auto-retry: session died during restart recovery
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ↻ Recovery session died — triggering auto-retry with history (channel {})",
            channel_id
        );
        reset_session_for_auto_retry(
            &shared_owned,
            channel_id,
            &cancel_token,
            adk_session_key.as_deref(),
            &mut new_session_id,
            &mut new_raw_provider_session_id,
            &mut inflight_state,
            "recovery session died",
        )
        .await;
        // #2452 H6: schedule the auto-retry via the explicit
        // completion path so the dedup lockout is released as soon
        // as scheduling resolves (≤ 120s safety net inside helper).
        // A recovery turn with no anchored user message (user_msg_id == 0)
        // has no message to retry-with-history against, so skip scheduling.
        if let Some(user_msg_id) = user_msg_id {
            spawn_retry_with_history_with_release(
                gateway.clone(),
                channel_id,
                user_msg_id,
                user_text_owned.clone(),
            );
        }
        // Replace placeholder with recovery notice (don't delete — avoids visual gap)
        let _ = gateway
            .edit_message(
                channel_id,
                current_msg_id,
                "↻ 세션 복구 중... 잠시 후 자동으로 이어갑니다.",
            )
            .await;
        full_response = String::new();
        }
    }

    *state.full_response = full_response;
    *state.new_session_id = new_session_id;
    *state.new_raw_provider_session_id = new_raw_provider_session_id;

    RecoveryRetryOutcome::Continue
}
