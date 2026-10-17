//! The provider failure notice on a channel whose answer body O posts.

use super::*;

/// Replaces the placeholder with the bridge's failure notice. The notice is bridge chrome, not
/// O's native body, so it is sent outside O's body claim. Returns (committed, posted by fallback):
/// only a confirmed replace counts as delivered.
#[allow(clippy::too_many_arguments)]
pub(super) async fn replace_on_o_channel(
    shared: &SharedData,
    gateway: &dyn TurnGateway,
    provider: &ProviderKind,
    channel_id: ChannelId,
    current_msg_id: MessageId,
    inflight_state: &InflightTurnState,
    dispatch_id: Option<&str>,
    adk_session_key: Option<&str>,
    turn_id: &str,
    notice: &str,
) -> (bool, bool) {
    let outcome = gateway
        .replace_message_with_outcome(channel_id, current_msg_id, notice)
        .await;
    let fallback = matches!(
        &outcome,
        Ok(super::super::super::formatting::ReplaceLongMessageOutcome::SentFallbackAfterEditFailure { .. })
    );
    let committed = turn_bridge_replace_outcome_committed(
        shared,
        provider,
        channel_id,
        current_msg_id,
        inflight_state.tmux_session_name.as_deref(),
        outcome,
        dispatch_id,
        adk_session_key,
        Some(turn_id),
        "turn_bridge_provider_failure_replace",
    );
    (committed, fallback)
}
