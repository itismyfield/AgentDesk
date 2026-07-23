use poise::serenity_prelude as serenity;

use crate::services::discord::inflight::{CreateNewInflightError, InflightTurnState};
use crate::services::provider::ProviderKind;

pub(crate) async fn record_turn_start_origin(
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    state: &InflightTurnState,
) {
    crate::services::discord::adk_session::record_turn_start_origin(
        state.session_key.as_deref(),
        provider,
        channel_id,
        state.turn_nonce.as_deref(),
        state.dispatch_id.is_some(),
    )
    .await;
}

pub(crate) fn log_create_new_inflight_outcome(
    result: Result<(), CreateNewInflightError>,
    provider: &ProviderKind,
    state: &InflightTurnState,
) {
    let channel_id = state.channel_id;
    let user_msg_id = state.user_msg_id;
    match result {
        Ok(()) => {}
        Err(CreateNewInflightError::AlreadyExists) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id,
                user_msg_id,
                "inflight create skipped because a durable row already exists; continuing fail-closed"
            );
        }
        Err(CreateNewInflightError::Internal(error)) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id,
                user_msg_id,
                %error,
                "inflight create failed internally; continuing without durable row"
            );
        }
    }
}
