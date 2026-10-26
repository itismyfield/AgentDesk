//! The stale bridge has one protected decision; the legacy arm also consumes the durable fence.

use super::*;

pub(in crate::services::discord::turn_bridge) async fn retry_stale_resume(
    point: StaleRetry<'_>,
    state: StaleRetryState<'_>,
) {
    use crate::services::discord::replay_policy::{self, StaleResume};
    let pool = point.shared.pg_pool.as_ref();
    match replay_policy::stale_resume(pool, point.cancel_token, state.inflight_state).await {
        StaleResume::Legacy => {
            if !crate::services::discord::router::replay_producers_dormant(
                &point.channel_id.get().to_string(),
            ) {
                let Some(provider) = ProviderKind::from_str(&state.inflight_state.provider) else {
                    return;
                };
                if super::super::recovery_text::replay::blocked(
                    point.shared,
                    &provider,
                    point.channel_id.get(),
                    point.user_msg_id.map_or(0, |id| id.get()),
                )
                .await
                {
                    return;
                }
            }
            let reset = reset_session_for_auto_retry(
                point.shared,
                point.channel_id,
                point.cancel_token,
                point.adk_session_key,
                state.new_session_id,
                state.new_raw_provider_session_id,
                state.inflight_state,
                point.reason,
            )
            .await;
            // Retry with history only for an anchored user message whose session was cleared.
            if let Some(user_msg_id) = state.auto_retry.queue(reset, point.user_msg_id) {
                spawn_retry_with_history_with_release(
                    point.gateway.clone(),
                    point.channel_id,
                    user_msg_id,
                    point.user_text.to_string(),
                );
            }
            state.full_response.clear();
        }
        StaleResume::Exact(permit) => {
            if let Some(user_msg_id) = point.user_msg_id {
                let inflight = &state.inflight_state;
                let retry = crate::services::discord::replay_policy::ExactRetry {
                    user_message_id: user_msg_id,
                    sources: inflight
                        .source_message_ids
                        .iter()
                        .map(|&id| MessageId::new(id))
                        .collect(),
                    request_owner: UserId::new(inflight.request_owner_user_id),
                    text: point.user_text.to_string(),
                    reply_context: inflight.followup_reply_context.clone(),
                    permit,
                };
                spawn_retry(
                    point.gateway.clone(),
                    point.channel_id,
                    user_msg_id,
                    String::new(),
                    Some(retry),
                );
                state.auto_retry.queued = true;
            }
            state.full_response.clear();
        }
        StaleResume::Hold => {}
    }
}
