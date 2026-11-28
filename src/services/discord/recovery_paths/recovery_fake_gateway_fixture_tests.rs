use std::sync::atomic::{AtomicUsize, Ordering};

use poise::serenity_prelude::{ChannelId, MessageId};

use crate::services::discord::formatting::ReplaceLongMessageOutcome;
use crate::services::discord::gateway::{GatewayFuture, TurnGateway};
use crate::services::provider::ProviderKind;

type ReplaceHook = Box<dyn FnOnce() -> GatewayFuture<'static, ()> + Send>;

// Records recovery replacements and deletes so tests can check the original anchor is preserved.
pub(in crate::services::discord) struct RecoveryFakeGateway {
    outcome: ReplaceLongMessageOutcome,
    ok: bool,
    pub(super) replace_calls: AtomicUsize,
    pub(super) delete_calls: AtomicUsize,
    pub(in crate::services::discord) replacements: std::sync::Mutex<Vec<(MessageId, String)>>,
    replace_hook: std::sync::Mutex<Option<ReplaceHook>>,
}

impl RecoveryFakeGateway {
    pub(in crate::services::discord) fn new(outcome: ReplaceLongMessageOutcome, ok: bool) -> Self {
        Self {
            outcome,
            ok,
            replace_calls: AtomicUsize::new(0),
            delete_calls: AtomicUsize::new(0),
            replacements: std::sync::Mutex::new(Vec::new()),
            replace_hook: std::sync::Mutex::new(None),
        }
    }
}

impl RecoveryFakeGateway {
    pub(in crate::services::discord) fn before_replace_returns(
        self,
        hook: impl FnOnce() -> GatewayFuture<'static, ()> + Send + 'static,
    ) -> Self {
        *self.replace_hook.lock().expect("replace hook") = Some(Box::new(hook));
        self
    }
}

impl TurnGateway for RecoveryFakeGateway {
    fn replace_message_with_outcome<'a>(
        &'a self,
        _c: ChannelId,
        message: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
        Box::pin(async move {
            self.replace_calls.fetch_add(1, Ordering::SeqCst);
            let hook = self.replace_hook.lock().expect("replace hook").take();
            if let Some(hook) = hook {
                hook().await;
            }
            if self.ok {
                self.replacements
                    .lock()
                    .expect("record replacements")
                    .push((message, content.to_string()));
                Ok(self.outcome.clone())
            } else {
                Err("fake transport failure".to_string())
            }
        })
    }
    fn delete_message<'a>(
        &'a self,
        _c: ChannelId,
        _m: MessageId,
    ) -> GatewayFuture<'a, Result<(), String>> {
        self.delete_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(()) })
    }
    fn send_message<'a>(
        &'a self,
        _c: ChannelId,
        _x: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>> {
        panic!("recovery short-replace path never sends a new message")
    }
    fn edit_message<'a>(
        &'a self,
        _c: ChannelId,
        _m: MessageId,
        _x: &'a str,
    ) -> GatewayFuture<'a, Result<(), String>> {
        panic!("Active lifecycle → post_send_finalize no-op → no edit")
    }

    fn schedule_retry_with_history<'a>(
        &'a self,
        _c: ChannelId,
        _u: MessageId,
        _t: &'a str,
    ) -> GatewayFuture<'a, ()> {
        panic!("unused TurnGateway method on the recovery short-replace path")
    }
    fn dispatch_queued_turn<'a>(
        &'a self,
        _c: ChannelId,
        _i: &'a crate::services::discord::Intervention,
        _o: &'a str,
        _h: bool,
        _dispatch_lease: Option<std::sync::Arc<crate::services::turn_orchestrator::DispatchLease>>,
    ) -> GatewayFuture<'a, Result<(), String>> {
        panic!("unused TurnGateway method on the recovery short-replace path")
    }
    fn validate_live_routing<'a>(&'a self, _c: ChannelId) -> GatewayFuture<'a, Result<(), String>> {
        panic!("unused TurnGateway method on the recovery short-replace path")
    }
    fn requester_mention(&self) -> Option<String> {
        None
    }
    fn can_chain_locally(&self) -> bool {
        false
    }
    fn bot_owner_provider(&self) -> Option<ProviderKind> {
        None
    }
}
