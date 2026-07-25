//! #4860 size-cap relief: status-panel completion fallback helpers extracted
//! from `turn_bridge/status_panel.rs`.

use super::super::*;

pub(super) struct CompletionFallbackRequest<'a> {
    pub shared: &'a SharedData,
    pub channel_id: ChannelId,
    pub provider: &'a ProviderKind,
    pub expected_user_msg_id: Option<u64>,
    pub expected_prior:
        Option<crate::services::discord::status_panel_singleton_store::StatusPanelSingletonBinding>,
    pub last_status_panel_text: &'a mut String,
    pub panel_text: String,
    pub wip_warning:
        Option<crate::services::discord::turn_end_wip_warning::TurnEndWipWarningReservation>,
    pub source: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MissingPanelRecoveryFence {
    panel_message_id: u64,
    generation: u64,
    identity: Option<super::inflight::InflightTurnIdentity>,
}

impl MissingPanelRecoveryFence {
    pub(super) fn capture(
        shared: &SharedData,
        provider: &ProviderKind,
        channel_id: ChannelId,
        panel_message_id: Option<MessageId>,
        inflight: Option<&super::super::InflightTurnState>,
    ) -> Option<Self> {
        let panel_message_id = super::normalize_status_panel_message_id(panel_message_id)?.get();
        let singleton = crate::services::discord::status_panel_singleton_store::load(
            provider,
            &shared.token_hash,
            channel_id.get(),
        )?;
        if singleton.panel_message_id != panel_message_id {
            return None;
        }
        let identity = inflight
            .filter(|state| state.status_message_id == Some(panel_message_id))
            .map(super::inflight::InflightTurnIdentity::from_state);
        Some(Self {
            panel_message_id,
            generation: singleton.generation,
            identity,
        })
    }

    pub(super) fn prior_binding(
        &self,
    ) -> Option<crate::services::discord::status_panel_singleton_store::StatusPanelSingletonBinding>
    {
        Some(
            crate::services::discord::status_panel_singleton_store::StatusPanelSingletonBinding {
                panel_message_id: self.panel_message_id,
                generation: self.generation,
            },
        )
    }

    fn still_current(&self, request: &CompletionFallbackRequest<'_>) -> bool {
        let singleton = crate::services::discord::status_panel_singleton_store::load(
            request.provider,
            &request.shared.token_hash,
            request.channel_id.get(),
        );
        if singleton != self.prior_binding() {
            return false;
        }
        match self.identity.as_ref() {
            Some(identity) => {
                super::inflight::load_inflight_state(request.provider, request.channel_id.get())
                    .is_some_and(|state| {
                        identity.matches_state(&state)
                            && state.status_message_id == Some(self.panel_message_id)
                            && state.status_panel_generation == self.generation
                    })
            }
            None => true,
        }
    }
}

fn reject_stale_missing_panel_recovery(
    request: &CompletionFallbackRequest<'_>,
) -> super::StatusPanelCompletionResult {
    tracing::warn!(
        channel_id = request.channel_id.get(),
        source = request.source,
        "status-panel 10008 recovery fence is stale; preserving newer authority"
    );
    super::StatusPanelCompletionResult::not_applicable(false)
}

pub(super) async fn recover_missing_status_panel_with_gateway<G: TurnGateway + ?Sized>(
    gateway: &G,
    request: CompletionFallbackRequest<'_>,
    fence: Option<MissingPanelRecoveryFence>,
) -> super::StatusPanelCompletionResult {
    let Some(fence) = fence.filter(|fence| fence.still_current(&request)) else {
        return reject_stale_missing_panel_recovery(&request);
    };
    complete_status_panel_v2_fallback_with_gateway_fenced(gateway, request, Some(fence)).await
}

pub(super) async fn recover_missing_status_panel_with_http(
    http: &serenity::Http,
    request: CompletionFallbackRequest<'_>,
    fence: Option<MissingPanelRecoveryFence>,
) -> super::StatusPanelCompletionResult {
    let Some(fence) = fence.filter(|fence| fence.still_current(&request)) else {
        return reject_stale_missing_panel_recovery(&request);
    };
    complete_status_panel_v2_fallback_with_http_fenced(http, request, Some(fence)).await
}

fn completion_fallback_identity(
    request: &CompletionFallbackRequest<'_>,
) -> Option<super::inflight::InflightTurnIdentity> {
    request
        .expected_user_msg_id
        .and_then(|expected_user_msg_id| {
            super::inflight::load_inflight_state(request.provider, request.channel_id.get())
                .filter(|state| state.user_msg_id == expected_user_msg_id)
                .map(|state| super::inflight::InflightTurnIdentity::from_state(&state))
        })
}

fn cancel_prepared_if_stale(
    request: &CompletionFallbackRequest<'_>,
    prepared: &crate::services::discord::status_panel_transition::PreparedStatusPanelTransition,
    fence: Option<&MissingPanelRecoveryFence>,
) -> bool {
    if fence.is_none_or(|fence| fence.still_current(request)) {
        return false;
    }
    if let Err(error) = crate::services::discord::status_panel_transition::cancel_prepared_candidate(
        request.provider,
        &request.shared.token_hash,
        request.channel_id.get(),
        prepared,
    ) {
        tracing::warn!(
            channel_id = request.channel_id.get(),
            source = request.source,
            error = %error,
            "failed to cancel stale prepared status-panel recovery"
        );
    }
    true
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
    let transition = crate::services::discord::status_panel_transition::settle_completed_candidate(
        request.provider,
        &request.shared.token_hash,
        request.channel_id.get(),
        message_id.get(),
    );
    let binding_disposition = match transition {
        crate::services::discord::status_panel_transition::StatusPanelTransitionAction::KeepCurrent {
            generation: Some(_),
        } => super::singleton::CompletedBindingDisposition::CommittedCurrent,
        crate::services::discord::status_panel_transition::StatusPanelTransitionAction::RetireCandidate => {
            crate::services::discord::status_panel_orphan_store::enqueue(
                request.provider,
                &request.shared.token_hash,
                request.channel_id.get(),
                message_id.get(),
            );
            super::singleton::CompletedBindingDisposition::Superseded
        }
        crate::services::discord::status_panel_transition::StatusPanelTransitionAction::DeferDurability { .. }
        | crate::services::discord::status_panel_transition::StatusPanelTransitionAction::RecoverUnreconciled
        | crate::services::discord::status_panel_transition::StatusPanelTransitionAction::KeepCurrent { generation: None } => {
            super::singleton::CompletedBindingDisposition::DurabilityFailure
        }
    };
    tracing::debug!(
        channel_id = request.channel_id.get(),
        fallback_panel_message_id = message_id.get(),
        ?bind_outcome,
        ?transition,
        ?binding_disposition,
        "completed status-panel fallback ownership reconciliation"
    );
    let committed =
        binding_disposition == super::singleton::CompletedBindingDisposition::CommittedCurrent;
    if committed {
        if let Some(warning) = request.wip_warning.take() {
            warning.commit();
        }
        *request.last_status_panel_text = request.panel_text.clone();
    }
    super::StatusPanelCompletionResult {
        committed,
        binding_disposition,
        completed_panel_message_id: committed.then_some(message_id),
        unreconciled_panel_message_id: (!committed).then_some(message_id),
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

pub(super) async fn complete_status_panel_v2_fallback_with_http(
    http: &serenity::Http,
    request: CompletionFallbackRequest<'_>,
) -> super::StatusPanelCompletionResult {
    complete_status_panel_v2_fallback_with_http_fenced(http, request, None).await
}

async fn complete_status_panel_v2_fallback_with_http_fenced(
    http: &serenity::Http,
    mut request: CompletionFallbackRequest<'_>,
    missing_panel_fence: Option<MissingPanelRecoveryFence>,
) -> super::StatusPanelCompletionResult {
    let identity = completion_fallback_identity(&request);
    let prepared = match crate::services::discord::status_panel_transition::prepare_candidate(
        request.provider,
        &request.shared.token_hash,
        request.channel_id.get(),
        request.expected_prior,
        identity,
        crate::services::discord::status_panel_transition::StatusPanelTransitionOperation::CompletionFallback,
        request.panel_text.as_str(),
    ) {
        Ok(prepared) => prepared,
        Err(error) => {
            tracing::warn!(channel_id = request.channel_id.get(), source = request.source, error = %error, "failed to persist completion fallback intent before send");
            return super::StatusPanelCompletionResult::not_applicable(false);
        }
    };
    if cancel_prepared_if_stale(&request, &prepared, missing_panel_fence.as_ref()) {
        return reject_stale_missing_panel_recovery(&request);
    }
    match super::http::send_channel_message_with_nonce(
        http,
        request.channel_id,
        request.panel_text.as_str(),
        &prepared.nonce,
    )
    .await
    {
        Ok(message) => {
            let transition =
                crate::services::discord::status_panel_transition::acknowledge_candidate(
                    request.provider,
                    &request.shared.token_hash,
                    request.channel_id.get(),
                    &prepared,
                    message.id.get(),
                );
            if !matches!(transition, crate::services::discord::status_panel_transition::StatusPanelTransitionAction::KeepCurrent { .. }) {
                fallback_pending_bind_write_failed(&request, message.id, "transition intent pending recovery");
                return super::StatusPanelCompletionResult::unreconciled(message.id);
            }
            finalize_fallback_binding(&mut request, message.id)
        }
        Err(error) => {
            tracing::warn!(
                channel_id = request.channel_id.get(),
                source = request.source,
                nonce = %prepared.nonce,
                error = %error,
                "status-panel completion fallback ACK is unknown; durable nonce intent will retry"
            );
            super::StatusPanelCompletionResult::durable_recovery()
        }
    }
}

pub(super) async fn complete_status_panel_v2_fallback_with_gateway<G: TurnGateway + ?Sized>(
    gateway: &G,
    request: CompletionFallbackRequest<'_>,
) -> super::StatusPanelCompletionResult {
    complete_status_panel_v2_fallback_with_gateway_fenced(gateway, request, None).await
}

async fn complete_status_panel_v2_fallback_with_gateway_fenced<G: TurnGateway + ?Sized>(
    gateway: &G,
    mut request: CompletionFallbackRequest<'_>,
    missing_panel_fence: Option<MissingPanelRecoveryFence>,
) -> super::StatusPanelCompletionResult {
    let identity = completion_fallback_identity(&request);
    let prepared = match crate::services::discord::status_panel_transition::prepare_candidate(
        request.provider,
        &request.shared.token_hash,
        request.channel_id.get(),
        request.expected_prior,
        identity,
        crate::services::discord::status_panel_transition::StatusPanelTransitionOperation::CompletionFallback,
        request.panel_text.as_str(),
    ) {
        Ok(prepared) => prepared,
        Err(error) => {
            tracing::warn!(channel_id = request.channel_id.get(), source = request.source, error = %error, "failed to persist completion fallback intent before send");
            return super::StatusPanelCompletionResult::not_applicable(false);
        }
    };
    if cancel_prepared_if_stale(&request, &prepared, missing_panel_fence.as_ref()) {
        return reject_stale_missing_panel_recovery(&request);
    }
    let send_result = if gateway.can_chain_locally() {
        gateway
            .send_message_with_nonce(
                request.channel_id,
                request.panel_text.as_str(),
                &prepared.nonce,
            )
            .await
    } else if let Some(http) = request.shared.serenity_http_or_token_fallback() {
        super::http::send_channel_message_with_nonce(
            &http,
            request.channel_id,
            request.panel_text.as_str(),
            &prepared.nonce,
        )
        .await
        .map(|message| message.id)
        .map_err(|error| error.to_string())
    } else {
        Err("no Discord HTTP available for status-panel-v2 completion fallback".to_string())
    };

    match send_result {
        Ok(message_id) => {
            let transition =
                crate::services::discord::status_panel_transition::acknowledge_candidate(
                    request.provider,
                    &request.shared.token_hash,
                    request.channel_id.get(),
                    &prepared,
                    message_id.get(),
                );
            if !matches!(transition, crate::services::discord::status_panel_transition::StatusPanelTransitionAction::KeepCurrent { .. }) {
                fallback_pending_bind_write_failed(&request, message_id, "transition intent pending recovery");
                return super::StatusPanelCompletionResult::unreconciled(message_id);
            }
            finalize_fallback_binding(&mut request, message_id)
        }
        Err(error) => {
            tracing::warn!(
                channel_id = request.channel_id.get(),
                source = request.source,
                nonce = %prepared.nonce,
                error,
                "status-panel completion fallback ACK is unknown; durable nonce intent will retry"
            );
            super::StatusPanelCompletionResult::durable_recovery()
        }
    }
}
