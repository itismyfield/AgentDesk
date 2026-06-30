use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId, UserId};

use super::gateway_voice_queue::queued_intervention_request_owner;
use super::outbound::delivery::{deliver_outbound, first_raw_message_id};
use super::outbound::message::{
    DiscordOutboundMessage, OutboundOperation, OutboundReferenceContext, OutboundTarget,
};
use super::outbound::policy::DiscordOutboundPolicy;
use super::outbound::result::DeliveryResult;
use super::outbound::{DiscordOutboundClient, shared_outbound_deduper};
use super::router;
use super::router::handle_text_message;
use super::turn_bridge::{auto_retry_with_history, release_retry_pending};
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

    fn send_long_message_with_rollback<'a>(
        &'a self,
        channel_id: ChannelId,
        rollback_anchor_msg_id: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<Vec<MessageId>, String>> {
        Box::pin(async move {
            let chunks = formatting::split_message(content);
            if chunks.is_empty() {
                return Ok(Vec::new());
            }

            let mut sent_message_ids = Vec::new();
            for (index, chunk) in chunks.iter().enumerate() {
                match TurnGateway::send_message(self, channel_id, chunk).await {
                    Ok(message_id) => sent_message_ids.push(message_id),
                    Err(error) => {
                        let mut rollback_errors = Vec::new();
                        for message_id in sent_message_ids.iter().rev() {
                            if let Err(rollback_error) =
                                self.delete_message(channel_id, *message_id).await
                            {
                                rollback_errors.push(format!(
                                    "{}: {}",
                                    message_id.get(),
                                    rollback_error
                                ));
                            }
                        }
                        let attempted = index + 1;
                        let total = chunks.len();
                        let anchor = rollback_anchor_msg_id.get();
                        if rollback_errors.is_empty() {
                            return Err(format!(
                                "send chunk {attempted}/{total} failed for anchor {anchor} in channel {}; sent chunks cleaned before retry: {error}",
                                channel_id.get()
                            ));
                        }
                        return Err(format!(
                            "send chunk {attempted}/{total} failed for anchor {anchor} in channel {}; cleanup incomplete after error {error}: {}",
                            channel_id.get(),
                            rollback_errors.join("; ")
                        ));
                    }
                }
            }

            Ok(sent_message_ids)
        })
    }

    fn edit_message<'a>(
        &'a self,
        channel_id: ChannelId,
        message_id: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<(), String>>;

    fn delete_message<'a>(
        &'a self,
        _channel_id: ChannelId,
        _message_id: MessageId,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async move { Ok(()) })
    }

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

    /// #2452 H6 graduation: variant of `schedule_retry_with_history` that
    /// returns a `oneshot::Sender<()>` to the caller via the
    /// `completion_tx` parameter; the implementor MUST signal completion
    /// (success OR failure) on this channel when scheduling has finished
    /// so the caller can release any pending-retry lockout immediately
    /// instead of waiting on a fixed wall-clock timer.
    ///
    /// Default implementation delegates to `schedule_retry_with_history`
    /// and immediately drops `completion_tx`, which causes the
    /// `recv().await` on the matching `oneshot::Receiver` to resolve with
    /// `Err(RecvError)` — semantically equivalent to "completion signal
    /// arrived" for the lockout-release path. Implementors that can
    /// observe the actual retry-turn completion edge should override this
    /// to send `()` only after the retry truly completes.
    fn schedule_retry_with_history_with_completion<'a>(
        &'a self,
        channel_id: ChannelId,
        user_message_id: MessageId,
        user_text: &'a str,
        completion_tx: tokio::sync::oneshot::Sender<()>,
    ) -> GatewayFuture<'a, ()> {
        Box::pin(async move {
            self.schedule_retry_with_history(channel_id, user_message_id, user_text)
                .await;
            let _ = completion_tx.send(());
        })
    }

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

impl DiscordOutboundClient for SerenityTurnOutboundClient {
    async fn post_message(
        &self,
        target_channel: &str,
        content: &str,
    ) -> Result<String, crate::services::dispatches::discord_delivery::DispatchMessagePostError>
    {
        let channel_id = parse_channel_id(target_channel)?;
        rate_limit_wait(&self.shared, channel_id).await;
        channel_id
            .send_message(
                &self.http,
                serenity::CreateMessage::new()
                    .content(content)
                    .allowed_mentions(super::http::relay_allowed_mentions()),
            )
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
    ) -> Result<String, crate::services::dispatches::discord_delivery::DispatchMessagePostError>
    {
        let channel_id = parse_channel_id(target_channel)?;
        let reference_channel_id = parse_channel_id(reference_channel)?;
        let reference_message_id = parse_message_id(reference_message).map_err(|error| {
            crate::services::dispatches::discord_delivery::DispatchMessagePostError::new(
                crate::services::dispatches::discord_delivery::DispatchMessagePostErrorKind::Other,
                error,
            )
        })?;
        rate_limit_wait(&self.shared, channel_id).await;
        // #740 steer / robustness: degrade to a normal (non-reply) send when the
        // referenced message no longer exists. A queued `/steer` intervention
        // carries the slash *interaction* id as its `message_id` (a dedup/cancel
        // token, not a real channel message), so replying to it with the Discord
        // default `fail_if_not_exists=true` would 10008 and bubble an Err up
        // through `handle_text_message`, requeue-looping the steer so it never
        // reaches the agent. `fail_if_not_exists(false)` also hardens every other
        // reply against a since-deleted target.
        channel_id
            .send_message(
                &self.http,
                serenity::CreateMessage::new()
                    .reference_message(
                        serenity::MessageReference::from((
                            reference_channel_id,
                            reference_message_id,
                        ))
                        .fail_if_not_exists(false),
                    )
                    .content(content)
                    .allowed_mentions(super::http::relay_allowed_mentions()),
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
    ) -> Result<String, crate::services::dispatches::discord_delivery::DispatchMessagePostError>
    {
        let channel_id = parse_channel_id(target_channel)?;
        let message_id = parse_message_id(message_id).map_err(|error| {
            crate::services::dispatches::discord_delivery::DispatchMessagePostError::new(
                crate::services::dispatches::discord_delivery::DispatchMessagePostErrorKind::Other,
                error,
            )
        })?;
        rate_limit_wait(&self.shared, channel_id).await;
        channel_id
            .edit_message(
                &self.http,
                message_id,
                serenity::EditMessage::new()
                    .content(content)
                    .allowed_mentions(super::http::relay_allowed_mentions()),
            )
            .await
            .map(|message| message.id.get().to_string())
            .map_err(dispatch_post_error)
    }
}

fn parse_channel_id(
    raw: &str,
) -> Result<ChannelId, crate::services::dispatches::discord_delivery::DispatchMessagePostError> {
    raw.parse::<u64>().map(ChannelId::new).map_err(|error| {
        crate::services::dispatches::discord_delivery::DispatchMessagePostError::new(
            crate::services::dispatches::discord_delivery::DispatchMessagePostErrorKind::Other,
            format!("invalid Discord channel id {raw}: {error}"),
        )
    })
}

fn dispatch_post_error(
    error: serenity::Error,
) -> crate::services::dispatches::discord_delivery::DispatchMessagePostError {
    let detail = crate::utils::redact::redact_known_secrets(&error.to_string());
    let lowered = detail.to_ascii_lowercase();
    let kind = if detail.contains("BASE_TYPE_MAX_LENGTH")
        || lowered.contains("2000 or fewer in length")
        || lowered.contains("length")
    {
        crate::services::dispatches::discord_delivery::DispatchMessagePostErrorKind::MessageTooLong
    } else {
        crate::services::dispatches::discord_delivery::DispatchMessagePostErrorKind::Other
    };
    crate::services::dispatches::discord_delivery::DispatchMessagePostError::new(kind, detail)
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
            .queued
            .queued_placeholders
            .remove(&(channel_id, *message_id))
        {
            shared
                .ui
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
            &shared.queued.queued_placeholders,
            &shared.provider,
            &shared.token_hash,
            channel_id,
        );
    }
    to_delete
}

/// #3082 part B (codex P2-3): the answer-flush wait gate for intake
/// placeholders, factored out so the queued-only gating is unit-testable
/// without a live Discord HTTP client.
///
/// Only the queued-turn "📬" notice path waits behind an in-flight multi-chunk
/// answer flush — so the notice lands as a single TRAILING card after the last
/// chunk, never interleaved between answer chunks. Active-turn placeholders (a
/// turn starting NOW, or a TUI idle-response card) pass `is_queued_notice =
/// false` and return immediately, never delayed behind a flush.
///
/// The wait is bounded (progress-aware inactivity grace + absolute hard
/// ceiling) and the barrier guard is RAII-cleared, so a stuck/errored flush can
/// never permanently suppress the queued card — we proceed regardless once it
/// elapses (logged, no deadlock).
async fn await_answer_flush_if_queued_notice(
    barrier: &Arc<super::answer_flush_barrier::AnswerFlushBarrier>,
    channel_id: ChannelId,
    is_queued_notice: bool,
) {
    if !is_queued_notice {
        return;
    }
    if !barrier
        .wait_for_flush(
            channel_id,
            super::answer_flush_barrier::ANSWER_FLUSH_WAIT_TIMEOUT,
            super::answer_flush_barrier::ANSWER_FLUSH_WAIT_HARD_CEILING,
        )
        .await
    {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⏱ INTAKE: answer-flush barrier timed out for channel {}; posting queued card anyway (no deadlock)",
            channel_id
        );
    }
}

pub(super) async fn send_intake_placeholder(
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    channel_id: ChannelId,
    reference: Option<(ChannelId, MessageId)>,
    // #3082 part B (codex P2-3): only the queued-turn notice path must wait on
    // the answer-flush barrier. Active-turn placeholders (a turn starting NOW,
    // or a TUI idle-response card) are NOT a trailing "📬 queued" notice and
    // must NOT be delayed behind a multi-chunk answer flush — set this `false`
    // for those callers.
    is_queued_notice: bool,
) -> Result<MessageId, String> {
    // codex P2-3: gate the answer-flush wait to the queued-notice path only;
    // unrelated active-turn placeholders skip the barrier entirely.
    await_answer_flush_if_queued_notice(&shared.answer_flush_barrier, channel_id, is_queued_notice)
        .await;

    let client = SerenityTurnOutboundClient { http, shared };
    let mut msg = gateway_outbound_message(channel_id, "...");
    if let Some((reference_channel, reference_message)) = reference {
        msg = msg.with_reference(OutboundReferenceContext::reply_to(
            reference_channel,
            reference_message,
        ));
    }
    outbound_delivery_error(deliver_outbound(&client, shared_outbound_deduper(), msg, None).await)?
        .ok_or_else(|| "intake placeholder delivery was skipped".to_string())
}

pub(super) async fn send_outbound_message(
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    channel_id: ChannelId,
    content: &str,
) -> Result<MessageId, String> {
    let client = SerenityTurnOutboundClient { http, shared };
    let msg = gateway_outbound_message(channel_id, content);
    outbound_delivery_error(deliver_outbound(&client, shared_outbound_deduper(), msg, None).await)?
        .ok_or_else(|| "message delivery was skipped".to_string())
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
    outbound_delivery_error(deliver_outbound(&client, shared_outbound_deduper(), msg, None).await)
        .map(|_| ())
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
            send_outbound_message(self.http.clone(), self.shared.clone(), channel_id, content).await
        })
    }

    fn send_long_message_with_rollback<'a>(
        &'a self,
        channel_id: ChannelId,
        rollback_anchor_msg_id: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<Vec<MessageId>, String>> {
        Box::pin(async move {
            formatting::send_long_message_raw_with_rollback(
                &self.http,
                channel_id,
                rollback_anchor_msg_id,
                content,
                &self.shared,
            )
            .await
            .map_err(|error| error.to_string())
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
            outbound_delivery_error(
                deliver_outbound(&client, shared_outbound_deduper(), msg, None).await,
            )
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

    fn delete_message<'a>(
        &'a self,
        channel_id: ChannelId,
        message_id: MessageId,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async move {
            rate_limit_wait(&self.shared, channel_id).await;
            channel_id
                .delete_message(&self.http, message_id)
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

    fn schedule_retry_with_history_with_completion<'a>(
        &'a self,
        channel_id: ChannelId,
        user_message_id: MessageId,
        user_text: &'a str,
        completion_tx: tokio::sync::oneshot::Sender<()>,
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
            // #2452 H6: explicit release path — once scheduling has
            // resolved, drop the dedup lockout immediately so a
            // subsequent stale-resume detection on the same channel can
            // schedule another retry without waiting on the 30s sleep
            // fallback inside `auto_retry_with_history`.
            release_retry_pending(channel_id);
            let _ = completion_tx.send(());
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
                let result = channel_id
                    .delete_message(&self.http, placeholder_msg_id)
                    .await;
                // #3607: observe the merged-queued-placeholder drain delete.
                crate::services::observability::emit_relay_delete_result(
                    self.provider.as_str(),
                    channel_id.get(),
                    placeholder_msg_id.get(),
                    "gateway_queued_placeholder_drain",
                    "delete_nonterminal",
                    &result,
                );
            }

            let deps = router::IntakeDeps {
                http: &live_turn.ctx.http,
                cache: Some(&live_turn.ctx.cache),
                ctx_for_chained_dispatch: Some(&live_turn.ctx),
                shared: &self.shared,
                token: &live_turn.token,
            };
            // #2266 compatibility: older queued interventions may already
            // carry a voice-transcript accepted-replay payload. If present,
            // reinsert it into the per-process `voice::announce_meta` store
            // keyed by the intervention's HEAD `message_id`. New queue commits
            // strip that payload and rely on the readable announcement text to
            // resolve and claim the durable row at dispatch time.
            if let Some(announcement) = intervention.voice_announcement.as_ref() {
                crate::voice::announce_meta::global_store()
                    .insert_accepted_replay(intervention.message_id, announcement.clone());
            }
            // #2266: for voice-transcript queued items, the
            // `handle_text_message` voice-author authorization check at
            // line ~2274 requires `announce_bot_id == Some(request_owner)`.
            // The queued `Intervention.author_id` was captured at intake or
            // race-loss enqueue time as the ORIGINAL Discord author (the
            // announce bot), so pass it through here instead of
            // `live_turn.request_owner` (which is the previous turn's
            // owner). Non-voice queued items kept the legacy behavior of
            // routing via the live-turn owner so the user attribution does
            // not silently swap mid-chain; we only override the
            // request_owner when the intervention is voice-tagged. New queue
            // commits deliberately avoid embedding the full voice payload; the
            // readable announce text still needs the announce-bot author so
            // `handle_text_message` can resolve and claim the durable row at
            // processing time.
            let dispatch_request_owner =
                queued_intervention_request_owner(intervention, live_turn.request_owner);
            if !intervention.pending_uploads.is_empty() {
                let mut data = self.shared.core.lock().await;
                if let Some(session) = data.sessions.get_mut(&channel_id) {
                    session
                        .pending_uploads
                        .extend(intervention.pending_uploads.iter().cloned());
                }
            }
            handle_text_message(
                &deps,
                channel_id,
                intervention.message_id,
                dispatch_request_owner,
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
                Vec::new(),
                // #3905: queued dispatch carries the voice payload via the
                // accepted-replay store reinsert above, not the gate carry-forward.
                None,
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

    fn send_long_message_with_rollback<'a>(
        &'a self,
        _channel_id: ChannelId,
        _rollback_anchor_msg_id: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<Vec<MessageId>, String>> {
        Box::pin(async move {
            let chunks = formatting::split_message(content);
            let count = chunks.len().max(1);
            Ok((0..count).map(|_| next_headless_message_id()).collect())
        })
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

    fn delete_message<'a>(
        &'a self,
        _channel_id: ChannelId,
        _message_id: MessageId,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async move { Ok(()) })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::DISCORD_MSG_LIMIT;
    use std::sync::Mutex;

    #[derive(Default)]
    struct DefaultLongGateway {
        sent: Mutex<Vec<(MessageId, String)>>,
        deleted: Mutex<Vec<MessageId>>,
        fail_on_chunk: Option<usize>,
    }

    impl TurnGateway for DefaultLongGateway {
        fn send_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            content: &'a str,
        ) -> GatewayFuture<'a, Result<MessageId, String>> {
            Box::pin(async move {
                let mut sent = self.sent.lock().expect("sent lock");
                let chunk_index = sent.len() + 1;
                if self.fail_on_chunk == Some(chunk_index) {
                    return Err("simulated send failure".to_string());
                }
                let message_id = MessageId::new(8_000 + chunk_index as u64);
                sent.push((message_id, content.to_string()));
                Ok(message_id)
            })
        }

        fn edit_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn delete_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            message_id: MessageId,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async move {
                self.deleted.lock().expect("deleted lock").push(message_id);
                Ok(())
            })
        }

        fn replace_message_with_outcome<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
            Box::pin(async { Ok(ReplaceLongMessageOutcome::EditedOriginal) })
        }

        fn add_reaction<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _emoji: char,
        ) -> GatewayFuture<'a, ()> {
            Box::pin(async {})
        }

        fn remove_reaction<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _emoji: char,
        ) -> GatewayFuture<'a, ()> {
            Box::pin(async {})
        }

        fn schedule_retry_with_history<'a>(
            &'a self,
            _channel_id: ChannelId,
            _user_message_id: MessageId,
            _user_text: &'a str,
        ) -> GatewayFuture<'a, ()> {
            Box::pin(async {})
        }

        fn dispatch_queued_turn<'a>(
            &'a self,
            _channel_id: ChannelId,
            _intervention: &'a Intervention,
            _request_owner_name: &'a str,
            _has_more_queued_turns: bool,
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
            None
        }
    }

    #[tokio::test]
    async fn default_long_message_delivery_chunks_over_limit_body() {
        let gateway = DefaultLongGateway::default();
        let body = format!("{}{}", "A".repeat(2500), "B".repeat(2500));

        let ids = gateway
            .send_long_message_with_rollback(ChannelId::new(7), MessageId::new(42), &body)
            .await
            .expect("long message delivery");

        assert!(ids.len() > 1);
        let sent = gateway.sent.lock().expect("sent lock");
        assert_eq!(
            sent.iter()
                .map(|(_, chunk)| {
                    chunk
                        .split_once('\n')
                        .filter(|(prefix, _)| {
                            prefix.starts_with('[') && prefix.ends_with(']') && prefix.contains('/')
                        })
                        .map(|(_, body)| body)
                        .unwrap_or(chunk.as_str())
                })
                .collect::<String>(),
            body
        );
        assert!(
            sent.iter()
                .all(|(_, chunk)| chunk.len() <= DISCORD_MSG_LIMIT)
        );
        assert!(gateway.deleted.lock().expect("deleted lock").is_empty());
    }

    #[tokio::test]
    async fn default_long_message_delivery_rolls_back_sent_chunks_on_failure() {
        let gateway = DefaultLongGateway {
            fail_on_chunk: Some(2),
            ..DefaultLongGateway::default()
        };
        let body = format!("{}{}", "A".repeat(2500), "B".repeat(2500));

        let error = gateway
            .send_long_message_with_rollback(ChannelId::new(7), MessageId::new(42), &body)
            .await
            .expect_err("second chunk should fail");

        assert!(error.contains("sent chunks cleaned before retry"));
        assert_eq!(
            gateway.deleted.lock().expect("deleted lock").as_slice(),
            &[MessageId::new(8001)]
        );
    }
}

/// #3082 part B (codex P2-3): gate-behavior tests for the answer-flush wait.
/// These do not need a live Discord HTTP client (they exercise only the
/// `await_answer_flush_if_queued_notice` seam against a real barrier), so they
/// are compiled unconditionally rather than behind removed integration fixtures.
#[cfg(test)]
mod answer_flush_gate_tests {
    use super::await_answer_flush_if_queued_notice;
    use crate::services::discord::answer_flush_barrier::AnswerFlushBarrier;
    use poise::serenity_prelude::ChannelId;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// P2-3: a non-queued active-turn placeholder must NOT wait on the barrier,
    /// even while a multi-chunk answer flush is in flight on the same channel.
    #[tokio::test]
    async fn active_turn_placeholder_does_not_wait_on_barrier() {
        let barrier = Arc::new(AnswerFlushBarrier::default());
        let channel = ChannelId::new(101);
        // A multi-chunk answer flush is in flight (guard held) for this channel.
        let _flush_guard = barrier.begin_flush(channel);

        // An ACTIVE-turn placeholder (is_queued_notice = false) must return
        // immediately, never blocking behind the in-flight flush.
        let start = Instant::now();
        await_answer_flush_if_queued_notice(&barrier, channel, false).await;
        assert!(
            start.elapsed() < Duration::from_millis(150),
            "an active-turn placeholder must NOT wait on the answer-flush barrier"
        );
    }

    /// P2-3 (counterpart): the queued-notice path DOES wait behind an in-flight
    /// flush and only proceeds once the flush ends — proving the gate routes the
    /// two callers differently.
    #[tokio::test]
    async fn queued_notice_placeholder_waits_for_flush() {
        let barrier = Arc::new(AnswerFlushBarrier::default());
        let channel = ChannelId::new(102);
        let flush_guard = barrier.begin_flush(channel);

        let barrier_for_card = barrier.clone();
        let card = tokio::spawn(async move {
            // is_queued_notice = true — must block behind the flush.
            await_answer_flush_if_queued_notice(&barrier_for_card, channel, true).await;
        });

        // Hold the flush briefly; the queued notice must still be waiting.
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(
            !card.is_finished(),
            "the queued-notice placeholder must wait while the flush is in flight"
        );

        // Flush ends — the queued notice proceeds.
        drop(flush_guard);
        card.await.expect("queued-notice wait task must complete");
    }

    /// P2-3: with no flush in flight, even the queued-notice path returns
    /// immediately (the wait is only paid when there is something to wait for).
    #[tokio::test]
    async fn queued_notice_returns_immediately_when_no_flush() {
        let barrier = Arc::new(AnswerFlushBarrier::default());
        let channel = ChannelId::new(103);
        let start = Instant::now();
        await_answer_flush_if_queued_notice(&barrier, channel, true).await;
        assert!(
            start.elapsed() < Duration::from_millis(150),
            "with no flush in flight the queued-notice path must not block"
        );
    }
}
