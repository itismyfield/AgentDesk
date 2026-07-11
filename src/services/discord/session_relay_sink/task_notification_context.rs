//! Card-before-answer orchestration for session-bound task notifications (#4055).

use std::sync::Arc;

use serenity::model::id::{ChannelId, MessageId};
use sqlx::PgPool;

use super::super::SharedData;
use super::super::health::HealthRegistry;
use super::super::placeholder_live_events::PlaceholderLiveEvents;
use super::super::task_notification_delivery::{
    CardBot, CardDeliveryClients, CardEnsureError, CardEnsureOutcome, DiscordTaskCardTransport,
    EnsureIntent, ResponseDeliveryClaim, ResponseDeliveryClaimOutcome, ResponseDeliveryOwner,
    TaskCardTransport, TaskNotificationContext, claim_task_response_delivery,
    durable_response_turn_key, ensure_card, mark_task_response_delivered, provider_bot_key,
};
use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::cluster::stream_relay::RelaySinkError;
use crate::services::provider::ProviderKind;

/// Shared priority rule for the legacy kind marker and its richer context.
/// Keeping it with task-context orchestration avoids growing the giant sink
/// root with another task-specific policy implementation.
pub(super) fn merge_task_notification_kind(
    current: Option<TaskNotificationKind>,
    next: TaskNotificationKind,
) -> Option<TaskNotificationKind> {
    let priority = |kind: TaskNotificationKind| match kind {
        TaskNotificationKind::Subagent => 0,
        TaskNotificationKind::Background => 1,
        TaskNotificationKind::MonitorAutoTurn => 2,
    };
    match current {
        Some(existing) if priority(existing) >= priority(next) => Some(existing),
        _ => Some(next),
    }
}

/// Background notifications (for example CronCreate self-prompts) can deliver
/// without assistant text; Subagent/MonitorAutoTurn stay quiet until they have
/// user-visible assistant context (#2749).
pub(super) fn allows_delivery(
    kind: Option<TaskNotificationKind>,
    assistant_text_seen: bool,
) -> bool {
    match kind {
        None | Some(TaskNotificationKind::Background) => true,
        Some(_) => assistant_text_seen,
    }
}

pub(super) async fn ensure_task_context_card(
    health_registry: &Arc<HealthRegistry>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: u64,
    session_name: &str,
    context: Option<&TaskNotificationContext>,
) -> Result<Option<MessageId>, RelaySinkError> {
    let Some(context) = context else {
        return Ok(None);
    };
    let provider_http = shared.serenity_http_or_token_fallback();
    let notify_http = super::super::health::resolve_bot_http(health_registry.as_ref(), "notify")
        .await
        .ok();
    let clients = CardDeliveryClients::new(
        notify_http
            .map(|http| CardBot::new("notify", http))
            .into_iter()
            .chain(
                provider_http.map(|http| CardBot::new(provider_bot_key(provider.as_str()), http)),
            ),
    );
    let transport = DiscordTaskCardTransport::new(shared.clone());
    let outcome = confirm_task_context_card(
        shared.pg_pool.as_ref(),
        &clients,
        &transport,
        &shared.ui.placeholder_live_events,
        channel_id,
        provider.as_str(),
        session_name,
        Some(context),
    )
    .await
    .map_err(|error| {
        RelaySinkError::Transient(format!(
            "task-notification card must be confirmed before response delivery: {error}"
        ))
    })?;

    let Some(outcome) = outcome else {
        return Ok(None);
    };
    crate::services::tui_prompt_dedupe::record_prompt_anchor(
        provider.as_str(),
        session_name,
        channel_id,
        outcome.message_id,
    );
    tracing::info!(
        provider = provider.as_str(),
        channel_id,
        tmux_session = session_name,
        task_card_message_id = outcome.message_id,
        task_card_bot = %outcome.bot_key,
        task_card_disposition = ?outcome.disposition,
        "#4055: confirmed task context card before terminal response delivery"
    );
    Ok(Some(MessageId::new(outcome.message_id)))
}

pub(super) async fn ensure_card_and_route(
    health_registry: &Arc<HealthRegistry>,
    shared: &Arc<SharedData>,
    delivery: &super::SessionRelayDelivery,
    route: super::SessionBoundTerminalDeliveryRoute,
) -> Result<
    (
        super::SessionBoundTerminalDeliveryRoute,
        Option<MessageId>,
        Option<ResponseDeliveryClaimOutcome>,
    ),
    RelaySinkError,
> {
    let card = ensure_task_context_card(
        health_registry,
        shared,
        &delivery.provider,
        delivery.channel_id,
        &delivery.session_name,
        delivery.task_notification_context.as_ref(),
    )
    .await?;
    let response_claim =
        if let (Some(message_id), Some(context)) =
            (card, delivery.task_notification_context.as_ref())
        {
            let turn_key = durable_response_turn_key(
                delivery.channel_id,
                delivery.provider.as_str(),
                &delivery.session_name,
                delivery.frame_turn_user_msg_id,
                &delivery.frame_turn_started_at,
                delivery.frame_turn_start_offset,
                delivery.terminal_consumed_end.unwrap_or_default(),
                &delivery.response_text,
            );
            Some(claim_task_response_delivery(
            shared.pg_pool.as_ref(),
            delivery.channel_id,
            delivery.provider.as_str(),
            &delivery.session_name,
            context.event_key(),
            &turn_key,
            message_id.get(),
            ResponseDeliveryOwner::Sink,
        )
        .await
        .map_err(|error| {
            RelaySinkError::Transient(format!(
                "task-notification response turn must be durably bound before delivery: {error}"
            ))
        })?)
        } else {
            None
        };
    let route = if card.is_some() {
        super::SessionBoundTerminalDeliveryRoute::NewMessage
    } else {
        route
    };
    Ok((route, card, response_claim))
}

pub(super) fn answer_reference(
    channel: ChannelId,
    task_card_message_id: Option<MessageId>,
    prompt_anchor: Option<crate::services::tui_prompt_dedupe::TuiPromptAnchor>,
) -> Option<(ChannelId, MessageId)> {
    task_card_message_id
        .map(|message_id| (channel, message_id))
        .or_else(|| super::relay_format::prompt_anchor_reference(prompt_anchor))
}

/// Release the watcher fail-closed gate only after the referenced response has
/// been confirmed and the sink's commit-fence decision has run. Card
/// confirmation by itself is not response confirmation.
pub(super) async fn mark_response_delivered(
    pool: Option<&PgPool>,
    response_claim: Option<&ResponseDeliveryClaim>,
) -> Result<(), String> {
    match response_claim {
        Some(claim) => mark_task_response_delivered(pool, claim).await,
        None => Ok(()),
    }
}

pub(super) async fn commit_response_fence(
    shared: &Arc<SharedData>,
    delivery: &super::SessionRelayDelivery,
    response_claim: Option<&ResponseDeliveryClaim>,
) {
    if let Err(error) = mark_response_delivered(shared.pg_pool.as_ref(), response_claim).await {
        tracing::error!(
            provider = delivery.provider.as_str(),
            channel_id = delivery.channel_id,
            tmux_session = %delivery.session_name,
            error = %error,
            "task response was delivered but its PostgreSQL fallback fence stayed fail-closed"
        );
    }
}

async fn confirm_task_context_card<T: TaskCardTransport>(
    pool: Option<&PgPool>,
    clients: &CardDeliveryClients,
    transport: &T,
    live_events: &PlaceholderLiveEvents,
    channel_id: u64,
    provider: &str,
    session_name: &str,
    context: Option<&TaskNotificationContext>,
) -> Result<Option<CardEnsureOutcome>, CardEnsureError> {
    let Some(context) = context else {
        return Ok(None);
    };
    let event = context.to_event(channel_id, provider, session_name);
    let outcome = ensure_card(pool, clients, transport, &event, EnsureIntent::Promotion).await?;
    live_events.claim_terminal_slot_for_card(
        ChannelId::new(channel_id),
        event.kind(),
        event.tool_use_id(),
    );
    Ok(Some(outcome))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use poise::serenity_prelude as serenity;

    use super::*;
    use crate::services::discord::task_notification_delivery::{
        TaskCardTransportError, claim_existing_task_response_delivery,
    };
    use crate::services::session_backend::StreamLineState;

    struct OrderedTransport {
        fail: AtomicBool,
        next_id: AtomicU64,
        events: Arc<Mutex<Vec<String>>>,
    }

    impl TaskCardTransport for OrderedTransport {
        async fn post_card(
            &self,
            _bot: &CardBot,
            _channel_id: u64,
            _content: &str,
            nonce: &str,
        ) -> Result<u64, TaskCardTransportError> {
            self.events
                .lock()
                .expect("event log")
                .push(format!("card:{nonce}"));
            if self.fail.load(Ordering::Acquire) {
                return Err(TaskCardTransportError::Transient("503".to_string()));
            }
            Ok(self.next_id.fetch_add(1, Ordering::AcqRel))
        }

        async fn edit_card(
            &self,
            _bot: &CardBot,
            _channel_id: u64,
            _message_id: u64,
            _content: &str,
        ) -> Result<(), TaskCardTransportError> {
            Ok(())
        }
    }

    fn context(task_id: &str) -> TaskNotificationContext {
        TaskNotificationContext::from_stream_json(
            &serde_json::json!({
                "type": "system",
                "subtype": "task_notification",
                "task_id": task_id,
                "tool_use_id": format!("toolu-{task_id}"),
                "status": "completed",
                "summary": "background work",
                "task_notification_kind": "background"
            }),
            &StreamLineState::new(),
        )
        .expect("task context")
    }

    fn clients() -> CardDeliveryClients {
        CardDeliveryClients::new([CardBot::new(
            "notify",
            Arc::new(serenity::Http::new("test-token")),
        )])
    }

    #[tokio::test]
    async fn card_is_confirmed_before_referenced_answer_is_allowed() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let transport = OrderedTransport {
            fail: AtomicBool::new(false),
            next_id: AtomicU64::new(40_550),
            events: events.clone(),
        };
        let context = context("sink-order");
        let outcome = confirm_task_context_card(
            None,
            &clients(),
            &transport,
            &PlaceholderLiveEvents::default(),
            4_055,
            "claude",
            "AgentDesk-claude-4055",
            Some(&context),
        )
        .await
        .expect("card gate")
        .expect("task card");
        events
            .lock()
            .expect("event log")
            .push(format!("answer:reference={}", outcome.message_id));

        let recorded = events.lock().expect("event log");
        assert_eq!(recorded.len(), 2);
        assert!(recorded[0].starts_with("card:adktn"));
        assert_eq!(
            recorded[1],
            format!("answer:reference={}", outcome.message_id)
        );
    }

    #[tokio::test]
    async fn transient_card_failure_blocks_answer_and_frontier() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let transport = OrderedTransport {
            fail: AtomicBool::new(true),
            next_id: AtomicU64::new(40_560),
            events: events.clone(),
        };
        let context = context("sink-transient");
        let mut frontier_advanced = false;
        let result = confirm_task_context_card(
            None,
            &clients(),
            &transport,
            &PlaceholderLiveEvents::default(),
            4_056,
            "claude",
            "AgentDesk-claude-4056",
            Some(&context),
        )
        .await;
        if result.is_ok() {
            events.lock().expect("event log").push("answer".to_string());
            frontier_advanced = true;
        }
        assert!(result.is_err());
        assert!(!frontier_advanced);
        assert!(
            events
                .lock()
                .expect("event log")
                .iter()
                .all(|event| event != "answer")
        );
    }

    #[tokio::test]
    async fn task_notification_fallback_gate_releases_only_after_referenced_answer_delivery() {
        let context = context("response-commit");
        let delivery = super::super::SessionRelayDelivery {
            provider: ProviderKind::Claude,
            channel_id: 4_055_902,
            session_name: "AgentDesk-claude-4055-response-commit".to_string(),
            response_text: "answer".to_string(),
            task_notification_kind: Some(TaskNotificationKind::Background),
            task_notification_context: Some(context.clone()),
            terminal_consumed_end: None,
            frame_turn_user_msg_id: 0,
            frame_turn_started_at: "2026-07-11T01:37:00Z".to_string(),
            frame_turn_start_offset: Some(4055),
        };
        let transport = OrderedTransport {
            fail: AtomicBool::new(false),
            next_id: AtomicU64::new(4_055_902),
            events: Arc::new(Mutex::new(Vec::new())),
        };
        let card = confirm_task_context_card(
            None,
            &clients(),
            &transport,
            &PlaceholderLiveEvents::default(),
            delivery.channel_id,
            delivery.provider.as_str(),
            &delivery.session_name,
            Some(&context),
        )
        .await
        .expect("confirm response card")
        .expect("response card");
        let turn_key = durable_response_turn_key(
            delivery.channel_id,
            delivery.provider.as_str(),
            &delivery.session_name,
            delivery.frame_turn_user_msg_id,
            &delivery.frame_turn_started_at,
            delivery.frame_turn_start_offset,
            delivery.terminal_consumed_end.unwrap_or_default(),
            &delivery.response_text,
        );
        let claim = claim_task_response_delivery(
            None,
            delivery.channel_id,
            delivery.provider.as_str(),
            &delivery.session_name,
            context.event_key(),
            &turn_key,
            card.message_id,
            ResponseDeliveryOwner::Sink,
        )
        .await
        .expect("claim response turn");
        let ResponseDeliveryClaimOutcome::Owned(claim) = claim else {
            panic!("first response claimant must own the turn")
        };
        let (pending, pending_card) = claim_existing_task_response_delivery(
            None,
            delivery.channel_id,
            delivery.provider.as_str(),
            &delivery.session_name,
            &turn_key,
            ResponseDeliveryOwner::Watcher,
        )
        .await
        .expect("load pending response claim")
        .expect("response row");
        assert_eq!(pending_card, card.message_id);
        assert!(matches!(pending, ResponseDeliveryClaimOutcome::Wait));

        mark_response_delivered(None, Some(&claim))
            .await
            .expect("mark response delivered");
        let (delivered, delivered_card) = claim_existing_task_response_delivery(
            None,
            delivery.channel_id,
            delivery.provider.as_str(),
            &delivery.session_name,
            &turn_key,
            ResponseDeliveryOwner::Watcher,
        )
        .await
        .expect("load delivered response claim")
        .expect("response row");
        assert_eq!(delivered_card, card.message_id);
        assert!(matches!(
            delivered,
            ResponseDeliveryClaimOutcome::Delivered { .. }
        ));
    }

    #[test]
    fn giant_sink_wires_card_gate_before_reference_send() {
        let source = include_str!("../session_relay_sink.rs");
        let gate = source
            .find("ensure_card_and_route(")
            .expect("sink must invoke task card gate");
        let after_gate = &source[gate..];
        let reference = after_gate
            .find("answer_reference(channel")
            .expect("confirmed card id must become answer reference");
        let send = after_gate
            .find("formatting::send_long_message_raw_with_required_reference_rollback(")
            .expect("required-reference answer send");
        let after_send = &after_gate[send..];
        let advance = after_send
            .find("self.advance_after_confirmed_post(")
            .expect("confirmed answer must advance its delivery frontier");
        let unblock = after_send
            .find("commit_response_fence(")
            .expect("watcher fallback gate must be released after answer commit");
        assert!(
            reference < send,
            "card reference must be selected before answer send"
        );
        assert!(
            advance < unblock,
            "watcher fallback must stay blocked through answer confirmation and commit-fence decision"
        );
    }
}
