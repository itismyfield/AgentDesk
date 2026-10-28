//! Effect-free Legacy delivery preparation with execution deferred until admission.

use super::{
    DeliveryPorts, HealthRegistry, HumanInputDelivery, HumanInputError, HumanInputRequest,
    InjectMode, LivePorts, author_allowed_for_human_input, deliver_with_ports, inject,
    resolve_direct_meeting_shared,
};

impl HealthRegistry {
    /// Captures delivery state before admission; only awaiting the returned future can deliver.
    pub(crate) async fn prepare_external_human_input(
        &self,
        request: HumanInputRequest,
    ) -> Result<
        impl std::future::Future<Output = Result<HumanInputDelivery, HumanInputError>> + Send + 'static,
        HumanInputError,
    > {
        let shared = resolve_direct_meeting_shared(self, request.channel_id, &request.provider)
            .await
            .map_err(HumanInputError::RuntimeUnavailable)?;
        let allowed = {
            let settings = shared.settings.read().await;
            author_allowed_for_human_input(
                settings.owner_user_id,
                &settings.allowed_user_ids,
                request.author_id,
            )
        };
        if !allowed {
            return Err(HumanInputError::AuthorNotAllowed);
        }
        let runtime = match shared.http.cached_serenity_ctx.get().cloned() {
            None => Err("provider runtime is not ready".to_string()),
            Some(ctx) => shared
                .http
                .cached_bot_token
                .get()
                .cloned()
                .or_else(|| {
                    crate::services::discord::resolve_discord_token_by_hash(&shared.token_hash)
                })
                .map(|token| (ctx, token))
                .ok_or_else(|| "provider token unavailable".to_string()),
        };
        let ports = LivePorts {
            shared,
            runtime,
            request,
            inject_observed: Default::default(),
            reply: None,
        };
        if let Err(error) = &ports.runtime {
            let can_inject = ports.inject_mode() != InjectMode::Off
                && inject::external_ready(&ports.shared, &ports.request).await;
            if !can_inject && ports.external_hold().await.is_none() {
                return Err(HumanInputError::RuntimeUnavailable(error.clone()));
            }
        }
        Ok(async move {
            let channel_id = ports.request.channel_id.get();
            let provider = ports.request.provider.clone();
            let source = ports.request.source.clone();
            let origin_id = ports
                .request
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.pointer("/human_input/origin_id"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            let result = deliver_with_ports(&ports).await;
            let inject = ports
                .inject_observed
                .into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (outcome, turn_id, reason) = match &result {
                Ok(HumanInputDelivery::Started { turn_id }) => {
                    ("started", Some(turn_id.clone()), None)
                }
                Ok(HumanInputDelivery::Queued {
                    turn_id, reason, ..
                }) => ("queued", Some(turn_id.clone()), Some(reason.clone())),
                Ok(HumanInputDelivery::Injected { turn_id }) => ("injected", turn_id.clone(), None),
                Ok(HumanInputDelivery::Unconfirmed { turn_id, detail }) => {
                    ("unconfirmed", turn_id.clone(), Some(detail.clone()))
                }
                Err(error) => ("refused", None, Some(error.kind().to_string())),
            };
            tracing::info!(
                channel_id,
                provider = provider.as_str(),
                source = source.as_str(),
                origin_id = origin_id.as_deref(),
                turn_id = turn_id.as_deref(),
                outcome,
                reason = reason.as_deref(),
                inject_veto = inject.veto,
                inject_attempted = inject.attempted,
                "human_input_delivery"
            );
            result
        })
    }
}

#[cfg(all(test, unix))]
pub(crate) struct ReadyGatewayForTests {
    server: tokio::task::JoinHandle<()>,
}

#[cfg(all(test, unix))]
impl Drop for ReadyGatewayForTests {
    fn drop(&mut self) {
        self.server.abort();
    }
}

#[cfg(all(test, unix))]
impl super::SharedData {
    /// Supplies a real context through the existing loopback Discord fixture.
    pub(crate) async fn gateway_ready_for_tests(&self, channels: &[u64]) -> ReadyGatewayForTests {
        use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;

        let state = discord_mock::DiscordMockState::new();
        for channel in channels {
            state.allow_channel(*channel);
        }
        let (proxy, gateway, server) = discord_mock::start(state).await;
        let ctx = discord_mock::serenity_context(proxy, gateway).await;
        assert!(self.http.cached_serenity_ctx.set(ctx).is_ok());
        assert!(
            self.http
                .cached_bot_token
                .set("test-token".to_string())
                .is_ok()
        );
        ReadyGatewayForTests { server }
    }
}
