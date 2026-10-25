//! Headless presentation stays unchanged; only the exact continuation has a start adapter.
use super::*;
use crate::services::discord::replay_policy::{ExactHandoff, ExactRetry};

type Restart = dyn Fn(ChannelId, ExactRetry) -> GatewayFuture<'static, ExactHandoff> + Send + Sync;

pub(in crate::services::discord) struct HeadlessExactGateway {
    base: HeadlessGateway,
    restart: Arc<Restart>,
}

impl HeadlessExactGateway {
    pub(in crate::services::discord) fn new(
        restart: impl Fn(ChannelId, ExactRetry) -> GatewayFuture<'static, ExactHandoff>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            base: HeadlessGateway,
            restart: Arc::new(restart),
        }
    }
}

impl TurnGateway for HeadlessExactGateway {
    fn send_message<'a>(
        &'a self,
        channel: ChannelId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>> {
        self.base.send_message(channel, content)
    }
    fn edit_message<'a>(
        &'a self,
        channel: ChannelId,
        message: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<(), String>> {
        self.base.edit_message(channel, message, content)
    }
    fn delete_message<'a>(
        &'a self,
        channel: ChannelId,
        message: MessageId,
    ) -> GatewayFuture<'a, Result<(), String>> {
        self.base.delete_message(channel, message)
    }
    fn send_long_message_with_rollback<'a>(
        &'a self,
        channel: ChannelId,
        anchor: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<Vec<MessageId>, String>> {
        self.base
            .send_long_message_with_rollback(channel, anchor, content)
    }
    fn replace_message_with_outcome<'a>(
        &'a self,
        channel: ChannelId,
        message: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
        self.base
            .replace_message_with_outcome(channel, message, content)
    }
    fn schedule_retry_with_history<'a>(
        &'a self,
        channel: ChannelId,
        message: MessageId,
        text: &'a str,
    ) -> GatewayFuture<'a, ()> {
        self.base
            .schedule_retry_with_history(channel, message, text)
    }
    fn schedule_exact_retry_with_history<'a>(
        &'a self,
        channel: ChannelId,
        retry: ExactRetry,
    ) -> GatewayFuture<'a, ExactHandoff> {
        (self.restart)(channel, retry)
    }
    fn dispatch_queued_turn<'a>(
        &'a self,
        channel: ChannelId,
        intervention: &'a Intervention,
        owner: &'a str,
        more: bool,
        lease: Option<Arc<crate::services::turn_orchestrator::DispatchLease>>,
    ) -> GatewayFuture<'a, Result<(), String>> {
        self.base
            .dispatch_queued_turn(channel, intervention, owner, more, lease)
    }
    fn validate_live_routing<'a>(
        &'a self,
        channel: ChannelId,
    ) -> GatewayFuture<'a, Result<(), String>> {
        self.base.validate_live_routing(channel)
    }
    fn requester_mention(&self) -> Option<String> {
        self.base.requester_mention()
    }
    fn can_chain_locally(&self) -> bool {
        self.base.can_chain_locally()
    }
    fn bot_owner_provider(&self) -> Option<ProviderKind> {
        self.base.bot_owner_provider()
    }
}
