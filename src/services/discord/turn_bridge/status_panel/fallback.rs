//! #4860 size-cap relief: the status-panel completion FALLBACK helpers, moved
//! verbatim from the 700-capped `turn_bridge/status_panel.rs` (behavior
//! unchanged): the guarded fallback-message-id persist and the two
//! fallback-send transports (gateway + raw HTTP).

use super::super::*;

pub(super) fn persist_status_panel_completion_fallback_message_id(
    provider: &ProviderKind,
    channel_id: ChannelId,
    expected_user_msg_id: Option<u64>,
    message_id: MessageId,
    source: &'static str,
) {
    if is_synthetic_headless_message_id(message_id) {
        return;
    }
    let Some(expected_user_msg_id) = expected_user_msg_id else {
        return;
    };
    // #3077: route the load-modify-save through the typed bind op so the
    // user_msg_id guard and the field set are serialized under the inflight
    // flock (no TOCTOU with a concurrent turn rebinding the row). Behavior is
    // preserved: bind only when the on-disk row still belongs to this turn.
    let guard = super::inflight::StatusPanelBindGuard {
        require_user_msg_id: Some(expected_user_msg_id),
        ..Default::default()
    };
    match super::inflight::bind_status_panel(provider, channel_id.get(), message_id.get(), &guard) {
        super::inflight::StatusPanelBindOutcome::Bound { .. }
        | super::inflight::StatusPanelBindOutcome::AlreadyBound
        | super::inflight::StatusPanelBindOutcome::SkippedPanelAlreadySet(_) => {}
        super::inflight::StatusPanelBindOutcome::Missing => {}
        super::inflight::StatusPanelBindOutcome::GuardMismatch => {
            tracing::debug!(
                "[turn_bridge] skipped persisting status-panel-v2 fallback id {} in channel {} from {}: inflight user_msg_id != expected {}",
                message_id,
                channel_id,
                source,
                expected_user_msg_id
            );
        }
        super::inflight::StatusPanelBindOutcome::IoError => {
            tracing::warn!(
                "[turn_bridge] failed to persist fallback status-panel-v2 message {} in channel {} from {}",
                message_id,
                channel_id,
                source
            );
        }
    }
}

pub(super) async fn send_status_panel_v2_completion_fallback_http(
    http: &serenity::Http,
    channel_id: ChannelId,
    panel_text: &str,
) -> Result<MessageId, String> {
    super::http::send_channel_message(http, channel_id, panel_text)
        .await
        .map(|message| message.id)
        .map_err(|error| error.to_string())
}

pub(super) async fn send_status_panel_v2_completion_fallback<G: TurnGateway + ?Sized>(
    shared: &SharedData,
    gateway: &G,
    channel_id: ChannelId,
    panel_text: &str,
) -> Result<MessageId, String> {
    if gateway.can_chain_locally() {
        return gateway.send_message(channel_id, panel_text).await;
    }
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        return Err(
            "no Discord HTTP available for status-panel-v2 completion fallback".to_string(),
        );
    };
    super::http::send_channel_message(&http, channel_id, panel_text)
        .await
        .map(|message| message.id)
        .map_err(|error| error.to_string())
}
