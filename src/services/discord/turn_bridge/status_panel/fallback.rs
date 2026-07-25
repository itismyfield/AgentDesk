//! #4860 size-cap relief: the status-panel completion FALLBACK helpers, moved
//! verbatim from the 700-capped `turn_bridge/status_panel.rs` (behavior
//! unchanged): the guarded fallback-message-id persist and the two
//! fallback-send transports (gateway + raw HTTP).

use super::super::*;

pub(super) fn preregister_status_panel_completion_fallback_message_id(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    expected_user_msg_id: Option<u64>,
    message_id: MessageId,
) {
    let turn_identity = expected_user_msg_id.and_then(|expected_user_msg_id| {
        super::inflight::load_inflight_state(provider, channel_id.get())
            .filter(|state| state.user_msg_id == expected_user_msg_id)
            .map(|state| super::inflight::InflightTurnIdentity::from_state(&state))
    });
    crate::services::discord::status_panel_orphan_store::enqueue_pending_bind(
        provider,
        &shared.token_hash,
        channel_id.get(),
        message_id.get(),
        turn_identity,
    );
}

pub(super) fn persist_status_panel_completion_fallback_message_id(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    expected_user_msg_id: Option<u64>,
    message_id: MessageId,
    source: &'static str,
) -> super::inflight::StatusPanelBindOutcome {
    if is_synthetic_headless_message_id(message_id) {
        return super::inflight::StatusPanelBindOutcome::Missing;
    }
    let Some(expected_user_msg_id) = expected_user_msg_id else {
        return super::inflight::StatusPanelBindOutcome::Missing;
    };
    // #3077: route the load-modify-save through the typed bind op so the
    // user_msg_id guard and the field set are serialized under the inflight
    // flock (no TOCTOU with a concurrent turn rebinding the row). Behavior is
    // preserved: bind only when the on-disk row still belongs to this turn.
    let guard = super::inflight::StatusPanelBindGuard {
        require_user_msg_id: Some(expected_user_msg_id),
        skip_if_panel_already_set: false,
        ..Default::default()
    };
    let outcome =
        super::inflight::bind_status_panel(provider, channel_id.get(), message_id.get(), &guard);
    match outcome {
        super::inflight::StatusPanelBindOutcome::Bound { .. }
        | super::inflight::StatusPanelBindOutcome::AlreadyBound => {
            crate::services::discord::status_panel_orphan_store::remove_pending_bind(
                provider,
                &shared.token_hash,
                channel_id.get(),
                message_id.get(),
            );
        }
        super::inflight::StatusPanelBindOutcome::SkippedPanelAlreadySet(_)
        | super::inflight::StatusPanelBindOutcome::Missing => {}
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
    outcome
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

#[allow(clippy::too_many_arguments)]
pub(super) async fn complete_status_panel_v2_fallback_with_gateway<G: TurnGateway + ?Sized>(
    shared: &SharedData,
    gateway: &G,
    channel_id: ChannelId,
    provider: &ProviderKind,
    expected_user_msg_id: u64,
    last_status_panel_text: &mut String,
    panel_text: String,
    wip_warning: Option<
        crate::services::discord::turn_end_wip_warning::TurnEndWipWarningReservation,
    >,
    source: &'static str,
) -> super::StatusPanelCompletionResult {
    match send_status_panel_v2_completion_fallback(shared, gateway, channel_id, &panel_text).await {
        Ok(message_id) => {
            if let Some(warning) = wip_warning {
                warning.commit();
            }
            preregister_status_panel_completion_fallback_message_id(
                shared,
                provider,
                channel_id,
                Some(expected_user_msg_id),
                message_id,
            );
            persist_status_panel_completion_fallback_message_id(
                shared,
                provider,
                channel_id,
                Some(expected_user_msg_id),
                message_id,
                source,
            );
            let binding_disposition = super::singleton::commit_completed_binding(
                shared,
                provider,
                channel_id,
                Some(message_id),
            );
            *last_status_panel_text = panel_text;
            super::StatusPanelCompletionResult {
                committed: true,
                binding_disposition,
                completed_panel_message_id: Some(message_id),
            }
        }
        Err(error) => {
            tracing::warn!(
                "[turn_bridge] failed to send fallback status-panel-v2 completion in channel {} from {}: {}",
                channel_id,
                source,
                error
            );
            super::StatusPanelCompletionResult::not_applicable(false)
        }
    }
}
