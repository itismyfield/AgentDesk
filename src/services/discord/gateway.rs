use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId, UserId};

use super::outbound::delivery::{deliver_outbound, first_raw_message_id};
use super::outbound::message::{
    DiscordOutboundMessage, OutboundOperation, OutboundReferenceContext, OutboundTarget,
};
use super::outbound::policy::DiscordOutboundPolicy;
use super::outbound::result::DeliveryResult;
use super::outbound::{DiscordOutboundClient, OutboundDeduper};
use super::router;
use super::router::handle_text_message;
use super::turn_bridge::auto_retry_with_history;
use super::{
    Intervention, SharedData, formatting, rate_limit_wait, resolve_discord_bot_provider,
    validate_live_channel_routing,
};
use crate::services::provider::ProviderKind;
use formatting::ReplaceLongMessageOutcome;

pub(super) type GatewayFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub(super) trait TurnGateway: Send + Sync {
    fn send_message<'a>(
        &'a self,
        channel_id: ChannelId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>>;

    fn edit_message<'a>(
        &'a self,
        channel_id: ChannelId,
        message_id: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<(), String>>;

    fn replace_message_with_outcome<'a>(
        &'a self,
        channel_id: ChannelId,
        message_id: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>>;

    fn add_reaction<'a>(
        &'a self,
        channel_id: ChannelId,
        message_id: MessageId,
        emoji: char,
    ) -> GatewayFuture<'a, ()>;

    fn remove_reaction<'a>(
        &'a self,
        channel_id: ChannelId,
        message_id: MessageId,
        emoji: char,
    ) -> GatewayFuture<'a, ()>;

    fn schedule_retry_with_history<'a>(
        &'a self,
        channel_id: ChannelId,
        user_message_id: MessageId,
        user_text: &'a str,
    ) -> GatewayFuture<'a, ()>;

    fn dispatch_queued_turn<'a>(
        &'a self,
        channel_id: ChannelId,
        intervention: &'a Intervention,
        request_owner_name: &'a str,
        has_more_queued_turns: bool,
    ) -> GatewayFuture<'a, Result<(), String>>;

    fn validate_live_routing<'a>(
        &'a self,
        channel_id: ChannelId,
    ) -> GatewayFuture<'a, Result<(), String>>;

    fn requester_mention(&self) -> Option<String>;

    fn can_chain_locally(&self) -> bool;

    fn bot_owner_provider(&self) -> Option<ProviderKind>;
}

#[derive(Clone)]
pub(super) struct LiveDiscordTurnContext {
    pub(super) ctx: serenity::Context,
    pub(super) token: String,
    pub(super) request_owner: UserId,
}

pub(super) struct DiscordGateway {
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    provider: ProviderKind,
    live_turn: Option<LiveDiscordTurnContext>,
}

#[derive(Clone)]
struct SerenityTurnOutboundClient {
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
}

pub(super) struct HeadlessGateway;

impl DiscordGateway {
    pub(super) fn new(
        http: Arc<serenity::Http>,
        shared: Arc<SharedData>,
        provider: ProviderKind,
        live_turn: Option<LiveDiscordTurnContext>,
    ) -> Self {
        Self {
            http,
            shared,
            provider,
            live_turn,
        }
    }
}

fn outbound_delivery_error(result: DeliveryResult) -> Result<Option<MessageId>, String> {
    match result {
        DeliveryResult::Sent { messages, .. } => first_raw_message_id(&messages)
            .map(|message_id| parse_message_id(&message_id))
            .transpose(),
        DeliveryResult::Fallback {
            messages,
            fallback_used,
            ..
        } => {
            let message_id = first_raw_message_id(&messages).unwrap_or_default();
            tracing::info!(
                delivery_status = "fallback",
                fallback_kind = ?fallback_used,
                message_id,
                "[discord] outbound delivery used fallback"
            );
            parse_message_id(&message_id).map(Some)
        }
        DeliveryResult::Duplicate {
            existing_messages, ..
        } => {
            let message_id = first_raw_message_id(&existing_messages);
            tracing::info!(
                delivery_status = "duplicate",
                ?message_id,
                "[discord] outbound delivery deduplicated"
            );
            match message_id {
                Some(message_id) => parse_message_id(&message_id).map(Some),
                None => Ok(None),
            }
        }
        DeliveryResult::Skip { reason } => {
            tracing::info!(
                delivery_status = "skip",
                reason,
                "[discord] outbound delivery skipped"
            );
            Ok(None)
        }
        DeliveryResult::PermanentFailure { reason } => Err(reason),
    }
}

fn parse_message_id(message_id: &str) -> Result<MessageId, String> {
    message_id
        .parse::<u64>()
        .map(MessageId::new)
        .map_err(|error| format!("invalid Discord message id {message_id}: {error}"))
}

fn outbound_policy() -> DiscordOutboundPolicy {
    DiscordOutboundPolicy::preserve_inline_content().without_idempotency()
}

fn gateway_outbound_message(
    channel_id: ChannelId,
    content: impl Into<String>,
) -> DiscordOutboundMessage {
    DiscordOutboundMessage::new(
        format!("gateway:{}", channel_id.get()),
        "gateway:no-idempotency",
        content,
        OutboundTarget::Channel(channel_id),
        outbound_policy(),
    )
}

fn gateway_deduper() -> &'static OutboundDeduper {
    static DEDUPER: std::sync::OnceLock<OutboundDeduper> = std::sync::OnceLock::new();
    DEDUPER.get_or_init(OutboundDeduper::new)
}

impl DiscordOutboundClient for SerenityTurnOutboundClient {
    async fn post_message(
        &self,
        target_channel: &str,
        content: &str,
    ) -> Result<String, crate::server::routes::dispatches::discord_delivery::DispatchMessagePostError>
    {
        let channel_id = parse_channel_id(target_channel)?;
        rate_limit_wait(&self.shared, channel_id).await;
        channel_id
            .send_message(&self.http, serenity::CreateMessage::new().content(content))
            .await
            .map(|message| message.id.get().to_string())
            .map_err(dispatch_post_error)
    }

    async fn post_message_with_reference(
        &self,
        target_channel: &str,
        content: &str,
        reference_channel: &str,
        reference_message: &str,
    ) -> Result<String, crate::server::routes::dispatches::discord_delivery::DispatchMessagePostError>
    {
        let channel_id = parse_channel_id(target_channel)?;
        let reference_channel_id = parse_channel_id(reference_channel)?;
        let reference_message_id = parse_message_id(reference_message).map_err(|error| {
            crate::server::routes::dispatches::discord_delivery::DispatchMessagePostError::new(
                crate::server::routes::dispatches::discord_delivery::DispatchMessagePostErrorKind::Other,
                error,
            )
        })?;
        rate_limit_wait(&self.shared, channel_id).await;
        channel_id
            .send_message(
                &self.http,
                serenity::CreateMessage::new()
                    .reference_message((reference_channel_id, reference_message_id))
                    .content(content),
            )
            .await
            .map(|message| message.id.get().to_string())
            .map_err(dispatch_post_error)
    }

    async fn edit_message(
        &self,
        target_channel: &str,
        message_id: &str,
        content: &str,
    ) -> Result<String, crate::server::routes::dispatches::discord_delivery::DispatchMessagePostError>
    {
        let channel_id = parse_channel_id(target_channel)?;
        let message_id = parse_message_id(message_id).map_err(|error| {
            crate::server::routes::dispatches::discord_delivery::DispatchMessagePostError::new(
                crate::server::routes::dispatches::discord_delivery::DispatchMessagePostErrorKind::Other,
                error,
            )
        })?;
        rate_limit_wait(&self.shared, channel_id).await;
        channel_id
            .edit_message(
                &self.http,
                message_id,
                serenity::EditMessage::new().content(content),
            )
            .await
            .map(|message| message.id.get().to_string())
            .map_err(dispatch_post_error)
    }
}

fn parse_channel_id(
    raw: &str,
) -> Result<ChannelId, crate::server::routes::dispatches::discord_delivery::DispatchMessagePostError>
{
    raw.parse::<u64>()
        .map(ChannelId::new)
        .map_err(|error| {
            crate::server::routes::dispatches::discord_delivery::DispatchMessagePostError::new(
                crate::server::routes::dispatches::discord_delivery::DispatchMessagePostErrorKind::Other,
                format!("invalid Discord channel id {raw}: {error}"),
            )
        })
}

fn dispatch_post_error(
    error: serenity::Error,
) -> crate::server::routes::dispatches::discord_delivery::DispatchMessagePostError {
    let detail = error.to_string();
    let lowered = detail.to_ascii_lowercase();
    let kind = if detail.contains("BASE_TYPE_MAX_LENGTH")
        || lowered.contains("2000 or fewer in length")
        || lowered.contains("length")
    {
        crate::server::routes::dispatches::discord_delivery::DispatchMessagePostErrorKind::MessageTooLong
    } else {
        crate::server::routes::dispatches::discord_delivery::DispatchMessagePostErrorKind::Other
    };
    crate::server::routes::dispatches::discord_delivery::DispatchMessagePostError::new(kind, detail)
}

/// codex review P2 (#1332 follow-up): drain the `queued_placeholders` /
/// `placeholder_controller` bookkeeping for every non-head source message id
/// of a merged intervention. The dispatch path uses `intervention.message_id`
/// (the merged tail) as the Active card, so the head id's mapping must be
/// preserved here — only the *other* source ids leak. Returns the placeholder
/// Discord message ids whose visible cards the caller should delete (kept as
/// a return value to keep the helper independent of `serenity::Http` so the
/// test harness can invoke it without a real Discord client).
pub(super) async fn drain_merged_queued_placeholders(
    shared: &SharedData,
    channel_id: ChannelId,
    head_message_id: MessageId,
    source_message_ids: &[MessageId],
) -> Vec<MessageId> {
    // codex review round-4 P2 + round-5 P2: serialize the merged-source
    // drain with every other `queued_placeholders` mutation on the same
    // channel via the per-channel async persistence mutex. Otherwise an
    // `insert_queued_placeholder` for the head id could race this drain and
    // let the older snapshot overwrite the newer disk file, resurrecting
    // non-head source mappings on restart. The lock is async so this helper
    // can be safely awaited from both the live dispatch path and the
    // restart-induced kickoff path (round-5 P2 finding 3) without blocking
    // the runtime worker.
    let persist_lock = shared.queued_placeholders_persist_lock(channel_id);
    let _persist_guard = persist_lock.lock().await;
    let mut to_delete = Vec::new();
    let mut mutated = false;
    for message_id in source_message_ids {
        if *message_id == head_message_id {
            continue;
        }
        if let Some((_, placeholder_msg_id)) = shared
            .queued_placeholders
            .remove(&(channel_id, *message_id))
        {
            shared
                .placeholder_controller
                .detach_by_message(channel_id, placeholder_msg_id);
            to_delete.push(placeholder_msg_id);
            mutated = true;
        }
    }
    // codex review round-3 P2: persist the write-through after the batch
    // drain so a restart sees the same state as memory.
    if mutated {
        super::queued_placeholders_store::persist_channel_from_map(
            &shared.queued_placeholders,
            &shared.provider,
            &shared.token_hash,
            channel_id,
        );
    }
    to_delete
}

pub(super) async fn send_intake_placeholder(
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    channel_id: ChannelId,
    reference: Option<(ChannelId, MessageId)>,
) -> Result<MessageId, String> {
    let client = SerenityTurnOutboundClient { http, shared };
    let mut msg = gateway_outbound_message(channel_id, "...");
    if let Some((reference_channel, reference_message)) = reference {
        msg = msg.with_reference(OutboundReferenceContext::reply_to(
            reference_channel,
            reference_message,
        ));
    }
    outbound_delivery_error(deliver_outbound(&client, gateway_deduper(), msg).await)?
        .ok_or_else(|| "intake placeholder delivery was skipped".to_string())
}

pub(super) async fn edit_outbound_message(
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    channel_id: ChannelId,
    message_id: MessageId,
    content: &str,
) -> Result<(), String> {
    let client = SerenityTurnOutboundClient { http, shared };
    let msg = gateway_outbound_message(channel_id, content)
        .with_operation(OutboundOperation::Edit { message_id });
    outbound_delivery_error(deliver_outbound(&client, gateway_deduper(), msg).await).map(|_| ())
}

fn live_bot_owner_provider(live_turn: Option<&LiveDiscordTurnContext>) -> Option<ProviderKind> {
    let live_turn = live_turn?;
    Some(resolve_discord_bot_provider(&live_turn.token))
}

fn next_headless_message_id() -> MessageId {
    static HEADLESS_MESSAGE_ID_SEQ: AtomicU64 = AtomicU64::new(9_000_000_000_000_000_000);
    MessageId::new(HEADLESS_MESSAGE_ID_SEQ.fetch_add(1, Ordering::Relaxed))
}

impl TurnGateway for DiscordGateway {
    fn send_message<'a>(
        &'a self,
        channel_id: ChannelId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>> {
        Box::pin(async move {
            let client = SerenityTurnOutboundClient {
                http: self.http.clone(),
                shared: self.shared.clone(),
            };
            let msg = gateway_outbound_message(channel_id, content);
            outbound_delivery_error(deliver_outbound(&client, gateway_deduper(), msg).await)?
                .ok_or_else(|| "message delivery was skipped".to_string())
        })
    }

    fn edit_message<'a>(
        &'a self,
        channel_id: ChannelId,
        message_id: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let client = SerenityTurnOutboundClient {
                http: self.http.clone(),
                shared: self.shared.clone(),
            };
            let msg = gateway_outbound_message(channel_id, content)
                .with_operation(OutboundOperation::Edit { message_id });
            outbound_delivery_error(deliver_outbound(&client, gateway_deduper(), msg).await)
                .map(|_| ())
        })
    }

    fn replace_message_with_outcome<'a>(
        &'a self,
        channel_id: ChannelId,
        message_id: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
        Box::pin(async move {
            formatting::replace_long_message_raw_with_outcome(
                &self.http,
                channel_id,
                message_id,
                content,
                &self.shared,
            )
            .await
            .map_err(|e| e.to_string())
        })
    }

    fn add_reaction<'a>(
        &'a self,
        channel_id: ChannelId,
        message_id: MessageId,
        emoji: char,
    ) -> GatewayFuture<'a, ()> {
        Box::pin(async move {
            formatting::add_reaction_raw(&self.http, channel_id, message_id, emoji).await;
        })
    }

    fn remove_reaction<'a>(
        &'a self,
        channel_id: ChannelId,
        message_id: MessageId,
        emoji: char,
    ) -> GatewayFuture<'a, ()> {
        Box::pin(async move {
            formatting::remove_reaction_raw(&self.http, channel_id, message_id, emoji).await;
        })
    }

    fn schedule_retry_with_history<'a>(
        &'a self,
        channel_id: ChannelId,
        user_message_id: MessageId,
        user_text: &'a str,
    ) -> GatewayFuture<'a, ()> {
        Box::pin(async move {
            auto_retry_with_history(
                &self.http,
                &self.shared,
                &self.provider,
                channel_id,
                user_message_id,
                user_text,
            )
            .await;
        })
    }

    fn dispatch_queued_turn<'a>(
        &'a self,
        channel_id: ChannelId,
        intervention: &'a Intervention,
        request_owner_name: &'a str,
        has_more_queued_turns: bool,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let Some(live_turn) = self.live_turn.as_ref() else {
                return Err("missing live Discord context".to_string());
            };

            for message_id in &intervention.source_message_ids {
                // Both the standalone-queue (📬) and merged-queue (➕) reactions
                // must be cleaned up — `source_message_ids` collects every
                // message that contributed to this intervention.
                formatting::remove_reaction_raw(&self.http, channel_id, *message_id, '📬').await;
                formatting::remove_reaction_raw(&self.http, channel_id, *message_id, '➕').await;
            }

            // codex review P2 (#1332 follow-up): merged interventions can carry
            // several `source_message_ids`, each of which had registered its own
            // `📬 메시지 대기 중` placeholder when it lost the start-turn race.
            // `handle_text_message` only consumes `queued_placeholders` for the
            // intervention's HEAD message id (the last merged id, used as the
            // Active card). The remaining source ids would otherwise leak both
            // a `queued_placeholders` mapping and a stale `📬` Discord card for
            // a turn that is now actively running. Drain them here, before
            // dispatch enters `handle_text_message`. The head id is excluded
            // because the dispatch hand-off path will own its transition.
            let drained = drain_merged_queued_placeholders(
                &self.shared,
                channel_id,
                intervention.message_id,
                &intervention.source_message_ids,
            )
            .await;
            for placeholder_msg_id in drained {
                let _ = channel_id
                    .delete_message(&self.http, placeholder_msg_id)
                    .await;
            }

            let deps = router::IntakeDeps {
                ctx: &live_turn.ctx,
                shared: &self.shared,
                token: &live_turn.token,
            };
            handle_text_message(
                &deps,
                channel_id,
                intervention.message_id,
                live_turn.request_owner,
                request_owner_name,
                &intervention.text,
                true,
                has_more_queued_turns,
                true,
                intervention.merge_consecutive,
                intervention.reply_context.clone(),
                intervention.has_reply_boundary,
                None,
                // Queued turn kickoff: the prior turn already finished, so
                // this dispatch is not racing the placeholder-delete path.
                router::TurnKind::Foreground,
            )
            .await
            .map_err(|e| e.to_string())
        })
    }

    fn validate_live_routing<'a>(
        &'a self,
        channel_id: ChannelId,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let Some(live_turn) = self.live_turn.as_ref() else {
                return Err("missing live Discord context".to_string());
            };

            let bot_owner_provider = resolve_discord_bot_provider(&live_turn.token);
            let settings_snapshot = self.shared.settings.read().await.clone();
            validate_live_channel_routing(
                &live_turn.ctx,
                &bot_owner_provider,
                &settings_snapshot,
                channel_id,
            )
            .await
            .map_err(|e| e.to_string())
        })
    }

    fn requester_mention(&self) -> Option<String> {
        self.live_turn
            .as_ref()
            .map(|live_turn| format!("<@{}>", live_turn.request_owner.get()))
    }

    fn can_chain_locally(&self) -> bool {
        self.live_turn.is_some()
    }

    fn bot_owner_provider(&self) -> Option<ProviderKind> {
        live_bot_owner_provider(self.live_turn.as_ref())
    }
}

impl TurnGateway for HeadlessGateway {
    fn send_message<'a>(
        &'a self,
        _channel_id: ChannelId,
        _content: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>> {
        Box::pin(async move { Ok(next_headless_message_id()) })
    }

    fn edit_message<'a>(
        &'a self,
        _channel_id: ChannelId,
        _message_id: MessageId,
        _content: &'a str,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async move { Ok(()) })
    }

    fn replace_message_with_outcome<'a>(
        &'a self,
        _channel_id: ChannelId,
        _message_id: MessageId,
        _content: &'a str,
    ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
        Box::pin(async move { Ok(ReplaceLongMessageOutcome::EditedOriginal) })
    }

    fn add_reaction<'a>(
        &'a self,
        _channel_id: ChannelId,
        _message_id: MessageId,
        _emoji: char,
    ) -> GatewayFuture<'a, ()> {
        Box::pin(async move {})
    }

    fn remove_reaction<'a>(
        &'a self,
        _channel_id: ChannelId,
        _message_id: MessageId,
        _emoji: char,
    ) -> GatewayFuture<'a, ()> {
        Box::pin(async move {})
    }

    fn schedule_retry_with_history<'a>(
        &'a self,
        channel_id: ChannelId,
        _user_message_id: MessageId,
        user_text: &'a str,
    ) -> GatewayFuture<'a, ()> {
        Box::pin(async move {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 📦 Headless retry suppressed for channel {}: {}",
                channel_id,
                user_text
            );
        })
    }

    fn dispatch_queued_turn<'a>(
        &'a self,
        _channel_id: ChannelId,
        _intervention: &'a Intervention,
        _request_owner_name: &'a str,
        _has_more_queued_turns: bool,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(
            async move { Err("headless turns do not dispatch queued turns locally".to_string()) },
        )
    }

    fn validate_live_routing<'a>(
        &'a self,
        _channel_id: ChannelId,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async move { Ok(()) })
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::{drain_merged_queued_placeholders, live_bot_owner_provider, outbound_policy};
    use crate::services::discord::outbound::policy::{FallbackPolicy, LengthStrategy};
    use poise::serenity_prelude::{ChannelId, MessageId};
    use std::time::Duration;

    #[test]
    fn live_bot_owner_provider_requires_live_turn_context() {
        assert!(live_bot_owner_provider(None).is_none());
    }

    #[test]
    fn gateway_outbound_policy_preserves_streaming_chunks() {
        let policy = outbound_policy();

        assert_eq!(policy.length_strategy, LengthStrategy::RejectOverLimit);
        assert_eq!(policy.fallback, FallbackPolicy::None);
        assert_eq!(policy.idempotency_window, Duration::ZERO);
    }

    // codex review round-6 P2 (#1332): the two `drain_merged_queued_placeholders`
    // tests below now write to disk via `persist_channel_from_map` whenever
    // the drain mutates the map. Without isolation they could pollute the
    // developer's real `~/.adk/release` runtime directory or race a
    // parallel test that mutates `AGENTDESK_ROOT_DIR`. Wrap each test in
    // `lock_test_env` + a `tempfile::tempdir` `AGENTDESK_ROOT_DIR` so the
    // write-through lands in a per-test temp directory.
    fn with_isolated_runtime_root<F: FnOnce(&std::path::Path)>(f: F) {
        let _lock = crate::services::discord::runtime_store::lock_test_env();
        let tmp = tempfile::tempdir().expect("create temp runtime dir for queued placeholder test");
        unsafe {
            std::env::set_var(
                "AGENTDESK_ROOT_DIR",
                tmp.path().to_str().expect("temp path must be valid utf-8"),
            );
        }
        f(tmp.path());
        unsafe {
            std::env::remove_var("AGENTDESK_ROOT_DIR");
        }
    }

    // codex review P2 (#1332 follow-up): when a merged intervention is
    // dispatched, the queued placeholders for every NON-head source id must
    // be drained and returned for visible-card cleanup. The head id is left
    // in place because the dispatch hand-off path consumes it directly.
    //
    // codex review round-6 P2 (#1332): isolated under temp `AGENTDESK_ROOT_DIR`
    // so the persistence write-through cannot pollute the dev runtime dir
    // or race other parallel tests that mutate the env var. Run the async
    // body on a freshly-built runtime inside the env-locked block — using
    // `#[tokio::test]` would acquire the lock AFTER the runtime started,
    // and other tests on the runtime's worker threads could observe a
    // half-set `AGENTDESK_ROOT_DIR`.
    #[test]
    fn drain_merged_queued_placeholders_drops_non_head_source_ids() {
        with_isolated_runtime_root(|_root| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build single-thread tokio runtime");
            rt.block_on(async {
                let shared = crate::services::discord::make_shared_data_for_tests();
                let channel_id = ChannelId::new(940_000_000_000_001);
                let head_msg = MessageId::new(840_000_000_000_001);
                let head_card = MessageId::new(740_000_000_000_001);
                let merged_a_msg = MessageId::new(840_000_000_000_002);
                let merged_a_card = MessageId::new(740_000_000_000_002);
                let merged_b_msg = MessageId::new(840_000_000_000_003);
                let merged_b_card = MessageId::new(740_000_000_000_003);

                shared
                    .queued_placeholders
                    .insert((channel_id, head_msg), head_card);
                shared
                    .queued_placeholders
                    .insert((channel_id, merged_a_msg), merged_a_card);
                shared
                    .queued_placeholders
                    .insert((channel_id, merged_b_msg), merged_b_card);

                let drained = drain_merged_queued_placeholders(
                    &shared,
                    channel_id,
                    head_msg,
                    &[merged_a_msg, merged_b_msg, head_msg],
                )
                .await;

                // Head id must remain in queued_placeholders so the dispatch hand-off
                // path can consume it; the two merged ids must be drained AND
                // returned so the caller can delete their stale Discord cards.
                assert_eq!(drained.len(), 2);
                let drained_set: std::collections::HashSet<MessageId> =
                    drained.into_iter().collect();
                assert!(drained_set.contains(&merged_a_card));
                assert!(drained_set.contains(&merged_b_card));
                assert!(!drained_set.contains(&head_card));
                assert_eq!(shared.queued_placeholders.len(), 1);
                assert_eq!(
                    shared
                        .queued_placeholders
                        .get(&(channel_id, head_msg))
                        .map(|entry| *entry.value()),
                    Some(head_card),
                    "head id mapping must survive the drain"
                );
            });
        });
    }

    // codex review P2: a non-merged intervention (single source id == head)
    // should produce an empty drain — there is nothing to clean up.
    //
    // codex review round-6 P2 (#1332): even though this drain takes the
    // `mutated == false` branch and does NOT write to disk, the helper
    // still acquires the per-channel persistence mutex which DashMap-stores
    // the lock entry inside `SharedData::queued_placeholders_persist_locks`.
    // Wrap the test for symmetry with the mutating sibling and to defend
    // against future drain-helper changes that would start persisting on
    // the no-op path.
    #[test]
    fn drain_merged_queued_placeholders_noop_for_non_merged_intervention() {
        with_isolated_runtime_root(|_root| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build single-thread tokio runtime");
            rt.block_on(async {
                let shared = crate::services::discord::make_shared_data_for_tests();
                let channel_id = ChannelId::new(950_000_000_000_001);
                let head_msg = MessageId::new(850_000_000_000_001);
                let head_card = MessageId::new(750_000_000_000_001);

                shared
                    .queued_placeholders
                    .insert((channel_id, head_msg), head_card);

                let drained =
                    drain_merged_queued_placeholders(&shared, channel_id, head_msg, &[head_msg])
                        .await;

                assert!(drained.is_empty());
                assert_eq!(shared.queued_placeholders.len(), 1);
            });
        });
    }
}
