use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId, UserId};

use super::gateway_queue_owner::queued_intervention_request_owner;
use super::outbound::delivery::{deliver_outbound, first_raw_message_id};
use super::outbound::message::{
    DiscordOutboundMessage, OutboundOperation, OutboundReferenceContext, OutboundTarget,
};
use super::outbound::policy::DiscordOutboundPolicy;
use super::outbound::{
    DiscordOutboundClient, post_serenity_message_with_nonce, shared_outbound_deduper,
};
use super::shared_state::{PortFuture, QueuedAdmission, QueuedRun};
use super::turn_bridge::{auto_retry_with_history, release_retry_pending};
use super::{
    Intervention, QueuedCardDisposition, QueuedCardTeardown, SharedData, formatting, queue_marker,
    queued_card_gate, rate_limit_wait, resolve_discord_bot_provider, validate_live_channel_routing,
};
use crate::domain::outbound::port::{DispatchMessagePostError, DispatchMessagePostErrorKind};
use crate::domain::outbound::result::DeliveryResult;
use crate::services::cluster::intake_turn::IntakeExecution;
use crate::services::provider::ProviderKind;
use formatting::ReplaceLongMessageOutcome;

mod outbound_messages;
#[cfg(test)]
use self::outbound_messages::await_answer_flush_if_queued_notice;
pub(super) use self::outbound_messages::{
    ClassifiedOutboundEditError, ClassifiedOutboundPostError, edit_intake_placeholder,
    edit_outbound_message, edit_outbound_message_classified, send_intake_placeholder,
    send_outbound_message, send_outbound_message_with_nonce_classified,
};
use outbound_messages::outbound_delivery_error;

pub(super) type GatewayFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

fn watcher_classified_failure_message(
    class: super::replace_outcome_policy::WatcherSendFailureClass,
    message: impl std::fmt::Display,
) -> String {
    super::replace_outcome_policy::watcher_send_failure_classified_message(class, message)
}

fn watcher_classified_error_string(error: &(dyn std::error::Error + 'static)) -> String {
    watcher_classified_failure_message(
        super::replace_outcome_policy::classify_watcher_send_failure(error),
        error,
    )
}

#[allow(dead_code)]
/// A synthetic id's edit error, classified like Discord's own missing message.
pub(in crate::services::discord) const SYNTHETIC_MESSAGE_EDIT_ERROR: &str =
    "Unknown Message: synthetic id has no Discord message";

pub(super) trait TurnGateway: Send + Sync {
    fn output_pool(&self) -> Option<&sqlx::PgPool> {
        None
    }

    fn has_raw_output_boundaries(&self) -> bool {
        false
    }

    fn output_provider(&self) -> Option<ProviderKind> {
        self.bot_owner_provider()
    }

    fn send_message<'a>(
        &'a self,
        channel_id: ChannelId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>>;

    /// Terminal notices consume their caller's latch before attempting delivery.
    fn send_notice_once<'a>(
        &'a self,
        channel_id: ChannelId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>> {
        TurnGateway::send_message(self, channel_id, content)
    }

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
                        let failure_class =
                            super::replace_outcome_policy::classify_watcher_send_failure_message(
                                &error,
                            );
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
                            return Err(watcher_classified_failure_message(
                                failure_class,
                                format!(
                                    "send chunk {attempted}/{total} failed for anchor {anchor} in channel {}; sent chunks cleaned before retry: {error}",
                                    channel_id.get()
                                ),
                            ));
                        }
                        return Err(watcher_classified_failure_message(
                            super::replace_outcome_policy::WatcherSendFailureClass::RollbackIncomplete,
                            format!(
                                "send chunk {attempted}/{total} failed for anchor {anchor} in channel {}; cleanup incomplete after error {error}: {}",
                                channel_id.get(),
                                rollback_errors.join("; ")
                            ),
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

    /// Return typed edit failures so the range owner can revalidate authority before fallback.
    fn replace_message_deferred<'a>(
        &'a self,
        channel_id: ChannelId,
        message_id: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<formatting::DeferredReplaceLongMessageOutcome, String>> {
        Box::pin(async move {
            self.replace_message_with_outcome(channel_id, message_id, content)
                .await
                .map(formatting::DeferredReplaceLongMessageOutcome::Edited)
        })
    }

    fn schedule_retry_with_history<'a>(
        &'a self,
        channel_id: ChannelId,
        user_message_id: MessageId,
        user_text: &'a str,
    ) -> GatewayFuture<'a, ()>;

    /// Signal completion after scheduling so pending-retry lockout can end immediately.
    /// Override when the actual retry-turn completion can be observed.
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

    /// Hand off the exact protected retry; refuse without a start path so it stays held.
    fn schedule_exact_retry_with_history<'a>(
        &'a self,
        channel_id: ChannelId,
        retry: crate::services::discord::replay_policy::ExactRetry,
    ) -> GatewayFuture<'a, crate::services::discord::replay_policy::ExactHandoff> {
        Box::pin(async move {
            tracing::warn!(
                channel_id = channel_id.get(),
                user_message_id = retry.user_message_id.get(),
                receipt = retry.permit.receipt_id(),
                "exact replay retry has no start path to re-enter; the request stays held"
            );
            crate::services::discord::replay_policy::ExactHandoff::Refused
        })
    }

    fn dispatch_queued_turn<'a>(
        &'a self,
        channel_id: ChannelId,
        intervention: &'a Intervention,
        request_owner_name: &'a str,
        has_more_queued_turns: bool,
        dispatch_lease: Option<std::sync::Arc<crate::services::turn_orchestrator::DispatchLease>>,
    ) -> GatewayFuture<'a, Result<(), String>>;

    fn validate_live_routing<'a>(
        &'a self,
        channel_id: ChannelId,
    ) -> GatewayFuture<'a, Result<(), String>>;

    fn requester_mention(&self) -> Option<String>;

    fn can_chain_locally(&self) -> bool;

    /// REST can edit a worker's placeholder while the leader owns queue admission.
    fn can_deliver_directly(&self) -> bool {
        self.can_chain_locally()
    }

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

mod headless;
mod headless_exact;
pub(super) use headless::HeadlessGateway;
pub(super) use headless_exact::HeadlessExactGateway;

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

fn task_card_outbound_message(
    channel_id: ChannelId,
    content: &str,
    nonce: &str,
) -> DiscordOutboundMessage {
    gateway_outbound_message(channel_id, content).with_create_nonce(nonce, true)
}

impl DiscordOutboundClient for SerenityTurnOutboundClient {
    async fn post_message(
        &self,
        target_channel: &str,
        content: &str,
    ) -> Result<String, DispatchMessagePostError> {
        let channel_id = parse_channel_id(target_channel)?;
        rate_limit_wait(&self.shared, channel_id).await;
        super::outbound::raw_output::request(
            channel_id,
            super::outbound::raw_output::Operation::Post,
            channel_id.send_message(
                &self.http,
                serenity::CreateMessage::new()
                    .content(content)
                    .allowed_mentions(
                        crate::services::message_outbox::mentions::relay_allowed_mentions(),
                    ),
            ),
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
    ) -> Result<String, DispatchMessagePostError> {
        let channel_id = parse_channel_id(target_channel)?;
        let reference_channel_id = parse_channel_id(reference_channel)?;
        let reference_message_id = parse_message_id(reference_message).map_err(|error| {
            DispatchMessagePostError::new(DispatchMessagePostErrorKind::Other, error)
        })?;
        rate_limit_wait(&self.shared, channel_id).await;
        // Degrade to a normal (non-reply) send when the referenced message no
        // longer exists. This also hardens every reply against a since-deleted
        // target.
        super::outbound::raw_output::request(
            channel_id,
            super::outbound::raw_output::Operation::Post,
            channel_id.send_message(
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
                    .allowed_mentions(
                        crate::services::message_outbox::mentions::relay_allowed_mentions(),
                    ),
            ),
        )
        .await
        .map(|message| message.id.get().to_string())
        .map_err(dispatch_post_error)
    }

    async fn post_message_with_nonce(
        &self,
        target_channel: &str,
        content: &str,
        nonce: &str,
        enforce_nonce: bool,
    ) -> Result<String, DispatchMessagePostError> {
        let channel_id = parse_channel_id(target_channel)?;
        post_serenity_message_with_nonce(
            &self.http,
            &self.shared,
            channel_id,
            content,
            None,
            nonce,
            enforce_nonce,
        )
        .await
        .map_err(dispatch_post_error)
    }

    async fn post_message_with_reference_and_nonce(
        &self,
        target_channel: &str,
        content: &str,
        reference_channel: &str,
        reference_message: &str,
        nonce: &str,
        enforce_nonce: bool,
    ) -> Result<String, DispatchMessagePostError> {
        let channel_id = parse_channel_id(target_channel)?;
        let reference_channel_id = parse_channel_id(reference_channel)?;
        let reference_message_id = parse_message_id(reference_message).map_err(|error| {
            DispatchMessagePostError::new(DispatchMessagePostErrorKind::Other, error)
        })?;
        post_serenity_message_with_nonce(
            &self.http,
            &self.shared,
            channel_id,
            content,
            Some((reference_channel_id, reference_message_id)),
            nonce,
            enforce_nonce,
        )
        .await
        .map_err(dispatch_post_error)
    }

    async fn edit_message(
        &self,
        target_channel: &str,
        message_id: &str,
        content: &str,
    ) -> Result<String, DispatchMessagePostError> {
        let channel_id = parse_channel_id(target_channel)?;
        let message_id = parse_message_id(message_id).map_err(|error| {
            DispatchMessagePostError::new(DispatchMessagePostErrorKind::Other, error)
        })?;
        rate_limit_wait(&self.shared, channel_id).await;
        super::outbound::raw_output::request(
            channel_id,
            super::outbound::raw_output::Operation::Edit,
            channel_id.edit_message(
                &self.http,
                message_id,
                serenity::EditMessage::new()
                    .content(content)
                    .allowed_mentions(
                        crate::services::message_outbox::mentions::relay_allowed_mentions(),
                    ),
            ),
        )
        .await
        .map(|message| message.id.get().to_string())
        .map_err(dispatch_post_error)
    }
}

fn parse_channel_id(raw: &str) -> Result<ChannelId, DispatchMessagePostError> {
    raw.parse::<u64>().map(ChannelId::new).map_err(|error| {
        DispatchMessagePostError::new(
            DispatchMessagePostErrorKind::Other,
            format!("invalid Discord channel id {raw}: {error}"),
        )
    })
}

fn dispatch_post_error(error: serenity::Error) -> DispatchMessagePostError {
    let detail = crate::utils::redact::redact_known_secrets(&error.to_string());
    let lowered = detail.to_ascii_lowercase();
    let kind = if detail.contains("BASE_TYPE_MAX_LENGTH")
        || lowered.contains("2000 or fewer in length")
        || lowered.contains("length")
    {
        DispatchMessagePostErrorKind::MessageTooLong
    } else {
        DispatchMessagePostErrorKind::Other
    };
    if let serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(response)) = &error
        && let Ok(status) = reqwest::StatusCode::from_u16(response.status_code.as_u16())
    {
        return DispatchMessagePostError::http(
            kind,
            status,
            i64::try_from(response.error.code).ok(),
            detail,
        );
    }
    DispatchMessagePostError::new(kind, detail)
}

/// Drain non-head merged-source mappings, preserving the head and cards with surviving owners.
/// Return teardown tokens so the caller performs guarded HTTP deletions.
pub(super) async fn drain_merged_queued_placeholders(
    shared: &SharedData,
    channel_id: ChannelId,
    head_message_id: MessageId,
    source_message_ids: &[MessageId],
) -> Vec<QueuedCardTeardown> {
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
    // #5035: complete departing hint (head ∪ sources) — re-key ordering only.
    let departing: Vec<MessageId> = std::iter::once(head_message_id)
        .chain(source_message_ids.iter().copied())
        .collect();
    for message_id in source_message_ids {
        if *message_id == head_message_id {
            continue;
        }
        if let Some((_, placeholder_msg_id)) = shared
            .queued
            .queued_placeholders
            .remove(&(channel_id, *message_id))
        {
            mutated = true;
            if let QueuedCardDisposition::Released(teardown) =
                queued_card_gate::release_or_rekey_locked(
                    shared,
                    channel_id,
                    placeholder_msg_id,
                    &departing,
                    &_persist_guard,
                )
                .await
            {
                to_delete.push(teardown);
            }
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

fn live_bot_owner_provider(live_turn: Option<&LiveDiscordTurnContext>) -> Option<ProviderKind> {
    let live_turn = live_turn?;
    Some(resolve_discord_bot_provider(&live_turn.token))
}

fn next_headless_message_id() -> MessageId {
    static HEADLESS_MESSAGE_ID_SEQ: AtomicU64 = AtomicU64::new(super::INTERNAL_MESSAGE_ID_START);
    MessageId::new(HEADLESS_MESSAGE_ID_SEQ.fetch_add(1, Ordering::Relaxed))
}

impl TurnGateway for DiscordGateway {
    fn has_raw_output_boundaries(&self) -> bool {
        true
    }

    fn output_pool(&self) -> Option<&sqlx::PgPool> {
        self.shared.pg_pool.as_ref()
    }

    fn output_provider(&self) -> Option<ProviderKind> {
        Some(self.provider.clone())
    }

    fn send_notice_once<'a>(
        &'a self,
        channel_id: ChannelId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>> {
        Box::pin(
            super::outbound::confirmation::send_terminal_notice_http_once(
                &self.http,
                &self.shared,
                channel_id,
                content,
            ),
        )
    }

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
            // A synthetic id is a turn's in-memory absence: there is no message to edit.
            if super::synthetic_headless_message_id::is_synthetic_headless_message_id_raw(
                message_id.get(),
            ) {
                return Err(SYNTHETIC_MESSAGE_EDIT_ERROR.to_string());
            }
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
                &mut None,
            )
            .await
            .map_err(|error| watcher_classified_error_string(error.as_ref()))
        })
    }

    fn replace_message_deferred<'a>(
        &'a self,
        channel_id: ChannelId,
        message_id: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<formatting::DeferredReplaceLongMessageOutcome, String>> {
        Box::pin(async move {
            formatting::replace_long_message_raw_deferred(
                &self.http,
                channel_id,
                message_id,
                content,
                &self.shared,
                &mut None,
            )
            .await
            .map_err(|error| watcher_classified_error_string(error.as_ref()))
        })
    }

    fn delete_message<'a>(
        &'a self,
        channel_id: ChannelId,
        message_id: MessageId,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async move {
            rate_limit_wait(&self.shared, channel_id).await;
            super::outbound::raw_output::request(
                channel_id,
                super::outbound::raw_output::Operation::Delete,
                channel_id.delete_message(&self.http, message_id),
            )
            .await
            .map_err(|e| e.to_string())
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
            // Release after scheduling so stale-resume detection need not wait for the timer.
            // The completion signal releases this turn's pending-retry lockout.
            release_retry_pending(channel_id);
            let _ = completion_tx.send(());
        })
    }

    fn schedule_exact_retry_with_history<'a>(
        &'a self,
        channel_id: ChannelId,
        retry: crate::services::discord::replay_policy::ExactRetry,
    ) -> GatewayFuture<'a, crate::services::discord::replay_policy::ExactHandoff> {
        (self.shared.router_ports.start_exact_retry)(&self.http, &self.shared, channel_id, retry)
    }

    fn dispatch_queued_turn<'a>(
        &'a self,
        channel_id: ChannelId,
        intervention: &'a Intervention,
        request_owner_name: &'a str,
        has_more_queued_turns: bool,
        dispatch_lease: Option<std::sync::Arc<crate::services::turn_orchestrator::DispatchLease>>,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async move {
            // #4270 A — pre-drain hosted-TUI readiness gate: a busy TUI defers
            // the promotion (front-requeue + slow backstop, zero view teardown)
            // BEFORE the marker drain / merged-card deletion below. `Ok` keeps
            // the epilogue from re-arming the fast ~2s kickoff (the #4270 spin).
            if (self.shared.router_ports.defer_promoted_dispatch)(
                &self.shared,
                &self.provider,
                channel_id,
                intervention,
                dispatch_lease
                    .clone()
                    .ok_or_else(|| "queued dispatch is missing its dispatch lease".to_string())?,
            )
            .await
            {
                return Ok(());
            }

            let Some(live_turn) = self.live_turn.as_ref() else {
                return Err("missing live Discord context".to_string());
            };

            // Queued human input keeps its verified author; other items take the live-turn owner.
            let dispatch_request_owner =
                queued_intervention_request_owner(intervention, live_turn.request_owner);
            let deps = super::queue_io::transport::IntakeDeps {
                http: &live_turn.ctx.http,
                cache: Some(&live_turn.ctx.cache),
                ctx_for_chained_dispatch: Some(&live_turn.ctx),
                shared: &self.shared,
                token: &live_turn.token,
            };
            let after_admission: PortFuture<'_, bool> = Box::pin(async move {
                let late_settlement = (self.shared.router_ports.checkpoint_waits)(channel_id);
                if !late_settlement {
                    let source_message_generations =
                        intervention.source_message_queued_generations();
                    queue_marker::drain_dispatched_queue_markers(
                        &self.shared,
                        &self.http,
                        channel_id,
                        intervention.message_id,
                        &source_message_generations,
                    )
                    .await;

                    // Dormant queued dispatch keeps the legacy marker/card handoff order.
                    let drained = drain_merged_queued_placeholders(
                        &self.shared,
                        channel_id,
                        intervention.message_id,
                        &intervention.source_message_ids,
                    )
                    .await;
                    for teardown in drained {
                        let placeholder_msg_id = teardown.card();
                        let result =
                            queued_card_gate::teardown_delete(&self.http, &self.shared, teardown)
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
                }
                late_settlement
            });
            let admission = QueuedAdmission {
                provider: self.provider.clone(),
                channel_id,
                intervention,
                request_owner: dispatch_request_owner,
                request_owner_name: request_owner_name.to_string(),
                defer_watcher_resume: has_more_queued_turns,
                wait_for_completion: true,
                backstop_reason: "intake_admission_pre_drain_defer",
                dispatch_lease: dispatch_lease.clone(),
            };
            let admit = self.shared.router_ports.admit_and_finish_queued;
            let (late_settlement, result) = match admit(&deps, admission, after_admission).await {
                QueuedRun::Finished {
                    after_admission,
                    result,
                } => (after_admission, result),
                QueuedRun::NotAdmitted => return Ok(()),
                QueuedRun::RejectedRestore => {
                    return Err("queued admission failed to restore dequeued head".to_string());
                }
            };
            let result = result.map_err(|e| e.to_string())?;
            if late_settlement && result == IntakeExecution::Executed {
                super::queue_dispatch::settle_executed_queued_intake(
                    &self.shared,
                    &self.http,
                    &self.provider,
                    channel_id,
                    intervention,
                    "gateway_queued_placeholder_drain",
                )
                .await;
            }
            Ok(())
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

    fn can_deliver_directly(&self) -> bool {
        true
    }

    fn bot_owner_provider(&self) -> Option<ProviderKind> {
        live_bot_owner_provider(self.live_turn.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::DISCORD_MSG_LIMIT;
    use std::sync::Mutex;

    #[test]
    fn task_notification_card_outbound_message_enforces_nonce_reconciliation() {
        let message = task_card_outbound_message(ChannelId::new(4_055), "card", "adktn123");
        assert_eq!(message.create_nonce.as_deref(), Some("adktn123"));
        assert!(
            message.enforce_nonce,
            "ambiguous task-card retries must ask Discord to return the same message"
        );
    }

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
