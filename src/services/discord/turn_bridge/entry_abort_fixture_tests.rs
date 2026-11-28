use crate::services::discord::gateway::TurnGateway;
use crate::services::discord::inflight::InflightTurnState;
use crate::services::discord::turn_bridge::TurnBridgeContext;
#[cfg(unix)]
use crate::services::discord::{
    self, formatting::ReplaceLongMessageOutcome, gateway::GatewayFuture,
};
use crate::services::memory::TokenUsage;
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::ChannelId;
#[cfg(unix)]
use poise::serenity_prelude::MessageId;
#[cfg(unix)]
use std::sync::{Arc, Mutex};

pub(in crate::services::discord) fn seed_context(
    seed: &str,
    row: InflightTurnState,
) -> TurnBridgeContext {
    let gateway: std::sync::Arc<dyn TurnGateway> =
        std::sync::Arc::new(crate::services::discord::gateway::HeadlessGateway);
    TurnBridgeContext {
        provider: ProviderKind::Codex,
        gateway,
        channel_id: ChannelId::new(row.channel_id),
        user_msg_id: None,
        user_text_owned: String::new(),
        request_owner_name: String::new(),
        role_binding: None,
        adk_session_key: None,
        adk_session_name: None,
        adk_session_info: None,
        adk_cwd: None,
        dispatch_id: None,
        dispatch_kind: None,
        memory_recall_usage: TokenUsage::default(),
        context_window_tokens: 0,
        context_compact_percent: 0,
        current_msg_id: None,
        response_sent_offset: 0,
        full_response: seed.to_string(),
        tmux_last_offset: None,
        new_session_id: None,
        defer_watcher_resume: false,
        reuse_status_panel_message: false,
        completion_tx: None,
        is_external_input_tui_direct: false,
        inflight_state: row,
    }
}

#[cfg(unix)]
/// Every body the bridge showed on Discord, in order; a send never awaited shows nothing.
#[derive(Default)]
pub(in crate::services::discord) struct Shown(pub(in crate::services::discord) Mutex<Vec<String>>);

#[cfg(unix)]
impl Shown {
    pub(in crate::services::discord) fn bodies(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

#[cfg(unix)]
impl TurnGateway for Shown {
    fn send_message<'a>(
        &'a self,
        _: ChannelId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>> {
        Box::pin(async move {
            self.0.lock().unwrap().push(content.to_owned());
            Ok(MessageId::new(5_340_990))
        })
    }
    fn edit_message<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.0.lock().unwrap().push(content.to_owned());
            Ok(())
        })
    }
    fn replace_message_with_outcome<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
        Box::pin(async move {
            self.0.lock().unwrap().push(content.to_owned());
            Ok(ReplaceLongMessageOutcome::EditedOriginal)
        })
    }
    fn schedule_retry_with_history<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
        _: &'a str,
    ) -> GatewayFuture<'a, ()> {
        Box::pin(async {})
    }
    fn dispatch_queued_turn<'a>(
        &'a self,
        _: ChannelId,
        _: &'a discord::Intervention,
        _: &'a str,
        _: bool,
        _: Option<Arc<crate::services::turn_orchestrator::DispatchLease>>,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn validate_live_routing<'a>(&'a self, _: ChannelId) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn requester_mention(&self) -> Option<String> {
        None
    }
    fn can_chain_locally(&self) -> bool {
        false
    }
    fn can_deliver_directly(&self) -> bool {
        true
    }
    fn bot_owner_provider(&self) -> Option<ProviderKind> {
        None
    }
}
