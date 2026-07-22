use crate::services::discord::inflight::{CreateNewInflightError, InflightTurnState};
use crate::services::provider::ProviderKind;

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
