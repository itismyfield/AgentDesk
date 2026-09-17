use std::sync::Arc;
use std::sync::LazyLock;

use poise::serenity_prelude as serenity;
use serenity::ChannelId;

use crate::services::provider::ProviderKind;

use super::SharedData;
use super::gateway::{DiscordGateway, TurnGateway};

const RETRY_PENDING_RELEASE_SAFETY_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(120);

pub(super) static PROVIDER_OVERLOAD_RETRY_STATE: LazyLock<
    dashmap::DashMap<u64, ProviderOverloadRetryState>,
> = LazyLock::new(dashmap::DashMap::new);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProviderOverloadRetryState {
    pub(super) fingerprint: String,
    pub(super) attempts: u8,
}

pub(super) fn clear_provider_overload_retry_state(channel_id: ChannelId) {
    PROVIDER_OVERLOAD_RETRY_STATE.remove(&channel_id.get());
}

/// #2426 H6: schedule retry through the completion-aware gateway path so
/// `RETRY_PENDING` is released when retry scheduling completes instead of
/// relying on `auto_retry_with_history`'s legacy 30s fallback task.
pub(super) fn schedule_retry_with_history_completion_release(
    gateway: Arc<dyn TurnGateway>,
    channel_id: ChannelId,
    user_message_id: serenity::MessageId,
    retry_text: String,
) {
    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        gateway
            .schedule_retry_with_history_with_completion(
                channel_id,
                user_message_id,
                &retry_text,
                completion_tx,
            )
            .await;
    });

    tokio::spawn(async move {
        let _ = tokio::time::timeout(RETRY_PENDING_RELEASE_SAFETY_TIMEOUT, completion_rx).await;
        super::turn_bridge::release_retry_pending(channel_id);
    });
}

fn discord_retry_gateway(
    shared: Arc<SharedData>,
    http: Arc<serenity::Http>,
    provider: ProviderKind,
) -> Arc<dyn TurnGateway> {
    Arc::new(DiscordGateway::new(http, shared, provider, None))
}

pub(super) fn schedule_discord_retry_with_history_completion_release(
    shared: Arc<SharedData>,
    http: Arc<serenity::Http>,
    provider: ProviderKind,
    channel_id: ChannelId,
    user_message_id: serenity::MessageId,
    retry_text: String,
) {
    schedule_retry_with_history_completion_release(
        discord_retry_gateway(shared, http, provider),
        channel_id,
        user_message_id,
        retry_text,
    );
}

#[cfg(test)]
mod completion_release_tests {
    use super::*;
    use crate::services::discord::Intervention;
    use crate::services::discord::formatting::ReplaceLongMessageOutcome;
    use crate::services::discord::gateway::{GatewayFuture, TurnGateway};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    struct CompletionOnlyGateway {
        completion_calls: Arc<AtomicUsize>,
        legacy_calls: Arc<AtomicUsize>,
        seen_text: Arc<Mutex<Option<String>>>,
        completion_observed: Arc<tokio::sync::Notify>,
        bot_owner_provider: Option<ProviderKind>,
    }

    impl CompletionOnlyGateway {
        fn new(bot_owner_provider: Option<ProviderKind>) -> Self {
            Self {
                completion_calls: Arc::new(AtomicUsize::new(0)),
                legacy_calls: Arc::new(AtomicUsize::new(0)),
                seen_text: Arc::new(Mutex::new(None)),
                completion_observed: Arc::new(tokio::sync::Notify::new()),
                bot_owner_provider,
            }
        }
    }

    impl TurnGateway for CompletionOnlyGateway {
        fn send_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<serenity::MessageId, String>> {
            Box::pin(async { Ok(serenity::MessageId::new(1)) })
        }

        fn edit_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: serenity::MessageId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn replace_message_with_outcome<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: serenity::MessageId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
            Box::pin(async { Ok(ReplaceLongMessageOutcome::EditedOriginal) })
        }

        fn schedule_retry_with_history<'a>(
            &'a self,
            _channel_id: ChannelId,
            _user_message_id: serenity::MessageId,
            _user_text: &'a str,
        ) -> GatewayFuture<'a, ()> {
            let legacy_calls = self.legacy_calls.clone();
            Box::pin(async move {
                legacy_calls.fetch_add(1, Ordering::Relaxed);
            })
        }

        fn schedule_retry_with_history_with_completion<'a>(
            &'a self,
            _channel_id: ChannelId,
            _user_message_id: serenity::MessageId,
            user_text: &'a str,
            completion_tx: tokio::sync::oneshot::Sender<()>,
        ) -> GatewayFuture<'a, ()> {
            let completion_calls = self.completion_calls.clone();
            let seen_text = self.seen_text.clone();
            let completion_observed = self.completion_observed.clone();
            let text = user_text.to_string();
            Box::pin(async move {
                completion_calls.fetch_add(1, Ordering::Relaxed);
                *seen_text.lock().expect("seen_text lock") = Some(text);
                let _ = completion_tx.send(());
                completion_observed.notify_one();
            })
        }

        fn dispatch_queued_turn<'a>(
            &'a self,
            _channel_id: ChannelId,
            _intervention: &'a Intervention,
            _request_owner_name: &'a str,
            _has_more_queued_turns: bool,
            _dispatch_lease: Option<
                std::sync::Arc<crate::services::turn_orchestrator::DispatchLease>,
            >,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn validate_live_routing<'a>(
            &'a self,
            _channel_id: ChannelId,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn requester_mention(&self) -> Option<String> {
            None
        }

        fn can_chain_locally(&self) -> bool {
            true
        }

        fn bot_owner_provider(&self) -> Option<ProviderKind> {
            self.bot_owner_provider.clone()
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn retry_release_helper_uses_completion_gateway_variant() {
        let fake = Arc::new(CompletionOnlyGateway::new(None));
        let gateway: Arc<dyn TurnGateway> = fake.clone();
        let channel = ChannelId::new(999_000_378_900);

        schedule_retry_with_history_completion_release(
            gateway,
            channel,
            serenity::MessageId::new(999_000_378_901),
            "retry payload".to_string(),
        );

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            fake.completion_observed.notified(),
        )
        .await
        .expect("completion-aware retry path should be invoked");

        assert_eq!(fake.completion_calls.load(Ordering::Relaxed), 1);
        assert_eq!(fake.legacy_calls.load(Ordering::Relaxed), 0);
        assert_eq!(
            fake.seen_text.lock().expect("seen_text lock").as_deref(),
            Some("retry payload")
        );
    }
}
