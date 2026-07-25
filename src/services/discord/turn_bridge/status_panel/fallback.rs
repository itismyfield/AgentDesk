//! #4860 size-cap relief: status-panel completion fallback helpers extracted
//! from `turn_bridge/status_panel.rs`.

use super::super::*;
use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};

static UNRECONCILED_FALLBACK_PANELS: LazyLock<Mutex<HashSet<(String, String, u64, u64)>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

#[derive(Debug, Clone, Copy)]
pub(super) struct ConfirmedMissingPriorPanel {
    pub message_id: MessageId,
    pub generation: u64,
}

pub(super) struct CompletionFallbackRequest<'a> {
    pub shared: &'a SharedData,
    pub channel_id: ChannelId,
    pub provider: &'a ProviderKind,
    pub expected_user_msg_id: Option<u64>,
    pub confirmed_missing_prior_panel: Option<ConfirmedMissingPriorPanel>,
    pub last_status_panel_text: &'a mut String,
    pub panel_text: String,
    pub wip_warning:
        Option<crate::services::discord::turn_end_wip_warning::TurnEndWipWarningReservation>,
    pub source: &'static str,
}

fn preregister_status_panel_completion_fallback_message_id(
    request: &CompletionFallbackRequest<'_>,
    message_id: MessageId,
) -> Result<(), String> {
    let turn_identity = request
        .expected_user_msg_id
        .and_then(|expected_user_msg_id| {
            super::inflight::load_inflight_state(request.provider, request.channel_id.get())
                .filter(|state| state.user_msg_id == expected_user_msg_id)
                .map(|state| super::inflight::InflightTurnIdentity::from_state(&state))
        });
    crate::services::discord::status_panel_orphan_store::enqueue_pending_bind(
        request.provider,
        &request.shared.token_hash,
        request.channel_id.get(),
        message_id.get(),
        turn_identity,
    )
}

fn persist_status_panel_completion_fallback_message_id(
    request: &CompletionFallbackRequest<'_>,
    message_id: MessageId,
) -> super::inflight::StatusPanelBindOutcome {
    if is_synthetic_headless_message_id(message_id) {
        return super::inflight::StatusPanelBindOutcome::Missing;
    }
    let Some(expected_user_msg_id) = request.expected_user_msg_id else {
        return super::inflight::StatusPanelBindOutcome::Missing;
    };
    if request.confirmed_missing_prior_panel.is_some() {
        return super::inflight::StatusPanelBindOutcome::Missing;
    }
    let guard = super::inflight::StatusPanelBindGuard {
        require_user_msg_id: Some(expected_user_msg_id),
        skip_if_panel_already_set: true,
        ..Default::default()
    };
    let outcome = super::inflight::bind_status_panel(
        request.provider,
        request.channel_id.get(),
        message_id.get(),
        &guard,
    );
    match outcome {
        super::inflight::StatusPanelBindOutcome::SkippedPanelAlreadySet(existing_panel_id) => {
            tracing::debug!(
                channel_id = request.channel_id.get(),
                fallback_panel_message_id = message_id.get(),
                existing_panel_message_id = existing_panel_id,
                source = request.source,
                "completion fallback preserved the same turn's already-bound panel"
            );
        }
        super::inflight::StatusPanelBindOutcome::GuardMismatch => {
            tracing::debug!(
                channel_id = request.channel_id.get(),
                fallback_panel_message_id = message_id.get(),
                expected_user_msg_id,
                source = request.source,
                "skipped persisting completion fallback: inflight turn changed"
            );
        }
        super::inflight::StatusPanelBindOutcome::IoError => {
            tracing::warn!(
                channel_id = request.channel_id.get(),
                fallback_panel_message_id = message_id.get(),
                source = request.source,
                "failed to persist completion fallback panel id"
            );
        }
        super::inflight::StatusPanelBindOutcome::Bound { .. }
        | super::inflight::StatusPanelBindOutcome::AlreadyBound
        | super::inflight::StatusPanelBindOutcome::Missing => {}
    }
    outcome
}

fn finalize_fallback_binding(
    request: &mut CompletionFallbackRequest<'_>,
    message_id: MessageId,
) -> super::StatusPanelCompletionResult {
    let bind_outcome = persist_status_panel_completion_fallback_message_id(request, message_id);
    let binding_disposition = match (
        request.confirmed_missing_prior_panel,
        request.expected_user_msg_id,
    ) {
        (Some(missing), Some(expected_user_msg_id)) => {
            super::singleton::classify_completed_binding_commit(
                request.provider,
                request.channel_id,
                message_id,
                crate::services::discord::status_panel_singleton_store::commit_confirmed_missing_replacement(
                    request.provider,
                    &request.shared.token_hash,
                    request.channel_id.get(),
                    expected_user_msg_id,
                    missing.message_id.get(),
                    missing.generation,
                    message_id.get(),
                ),
            )
        }
        _ => super::singleton::commit_completed_binding(
            request.shared,
            request.provider,
            request.channel_id,
            Some(message_id),
        ),
    };
    match binding_disposition {
        super::singleton::CompletedBindingDisposition::NotApplicable
        | super::singleton::CompletedBindingDisposition::CommittedCurrent => {
            crate::services::discord::status_panel_orphan_store::remove_pending_bind(
                request.provider,
                &request.shared.token_hash,
                request.channel_id.get(),
                message_id.get(),
            );
        }
        super::singleton::CompletedBindingDisposition::Superseded => {
            crate::services::discord::status_panel_orphan_store::enqueue(
                request.provider,
                &request.shared.token_hash,
                request.channel_id.get(),
                message_id.get(),
            );
        }
        super::singleton::CompletedBindingDisposition::DurabilityFailure => {}
    }
    tracing::debug!(
        channel_id = request.channel_id.get(),
        fallback_panel_message_id = message_id.get(),
        ?bind_outcome,
        ?binding_disposition,
        "completed status-panel fallback ownership reconciliation"
    );
    if let Some(warning) = request.wip_warning.take() {
        warning.commit();
    }
    *request.last_status_panel_text = request.panel_text.clone();
    super::StatusPanelCompletionResult {
        committed: true,
        binding_disposition,
        completed_panel_message_id: Some(message_id),
        unreconciled_panel_message_id: None,
    }
}

fn fallback_pending_bind_write_failed(
    request: &CompletionFallbackRequest<'_>,
    message_id: MessageId,
    error: &str,
) {
    tracing::warn!(
        channel_id = request.channel_id.get(),
        fallback_panel_message_id = message_id.get(),
        source = request.source,
        error,
        "completion fallback panel was sent but its pending-bind record could not be persisted"
    );
}

fn retain_unreconciled_fallback_in_memory(
    request: &CompletionFallbackRequest<'_>,
    message_id: MessageId,
) {
    UNRECONCILED_FALLBACK_PANELS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert((
            request.provider.as_str().to_string(),
            request.shared.token_hash.clone(),
            request.channel_id.get(),
            message_id.get(),
        ));
}

fn clear_unreconciled_fallback_in_memory(
    request: &CompletionFallbackRequest<'_>,
    message_id: MessageId,
) {
    UNRECONCILED_FALLBACK_PANELS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(&(
            request.provider.as_str().to_string(),
            request.shared.token_hash.clone(),
            request.channel_id.get(),
            message_id.get(),
        ));
}

#[cfg(test)]
pub(super) fn unreconciled_fallback_is_retained(
    request: &CompletionFallbackRequest<'_>,
    message_id: MessageId,
) -> bool {
    UNRECONCILED_FALLBACK_PANELS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .contains(&(
            request.provider.as_str().to_string(),
            request.shared.token_hash.clone(),
            request.channel_id.get(),
            message_id.get(),
        ))
}

pub(super) async fn complete_status_panel_v2_fallback_with_http(
    http: &serenity::Http,
    mut request: CompletionFallbackRequest<'_>,
) -> super::StatusPanelCompletionResult {
    match super::http::send_channel_message(http, request.channel_id, request.panel_text.as_str())
        .await
    {
        Ok(message) => {
            if let Err(error) =
                preregister_status_panel_completion_fallback_message_id(&request, message.id)
            {
                fallback_pending_bind_write_failed(&request, message.id, &error);
                if super::http::delete_channel_message(http, request.channel_id, message.id)
                    .await
                    .is_err()
                {
                    retain_unreconciled_fallback_in_memory(&request, message.id);
                }
                return super::StatusPanelCompletionResult::unreconciled(message.id);
            }
            clear_unreconciled_fallback_in_memory(&request, message.id);
            finalize_fallback_binding(&mut request, message.id)
        }
        Err(error) => {
            tracing::warn!(
                channel_id = request.channel_id.get(),
                source = request.source,
                error = %error,
                "failed to send status-panel completion fallback"
            );
            super::StatusPanelCompletionResult::not_applicable(false)
        }
    }
}

pub(super) async fn complete_status_panel_v2_fallback_with_gateway<G: TurnGateway + ?Sized>(
    gateway: &G,
    mut request: CompletionFallbackRequest<'_>,
) -> super::StatusPanelCompletionResult {
    let send_result = if gateway.can_chain_locally() {
        gateway
            .send_message(request.channel_id, request.panel_text.as_str())
            .await
    } else if let Some(http) = request.shared.serenity_http_or_token_fallback() {
        super::http::send_channel_message(&http, request.channel_id, request.panel_text.as_str())
            .await
            .map(|message| message.id)
            .map_err(|error| error.to_string())
    } else {
        Err("no Discord HTTP available for status-panel-v2 completion fallback".to_string())
    };

    match send_result {
        Ok(message_id) => {
            if let Err(error) =
                preregister_status_panel_completion_fallback_message_id(&request, message_id)
            {
                fallback_pending_bind_write_failed(&request, message_id, &error);
                if gateway
                    .delete_message(request.channel_id, message_id)
                    .await
                    .is_err()
                {
                    retain_unreconciled_fallback_in_memory(&request, message_id);
                }
                return super::StatusPanelCompletionResult::unreconciled(message_id);
            }
            clear_unreconciled_fallback_in_memory(&request, message_id);
            finalize_fallback_binding(&mut request, message_id)
        }
        Err(error) => {
            tracing::warn!(
                channel_id = request.channel_id.get(),
                source = request.source,
                error,
                "failed to send status-panel completion fallback"
            );
            super::StatusPanelCompletionResult::not_applicable(false)
        }
    }
}
