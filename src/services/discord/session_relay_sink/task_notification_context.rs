//! Card-before-answer orchestration for session-bound task notifications (#4055).

use std::sync::Arc;

use serenity::model::id::{ChannelId, MessageId};
use sqlx::PgPool;

use super::super::SharedData;
use super::super::health::HealthRegistry;
use super::super::placeholder_live_events::PlaceholderLiveEvents;
use super::super::task_notification_delivery::{
    CardBot, CardDeliveryClients, CardEnsureError, CardEnsureOutcome, DiscordTaskCardTransport,
    EnsureIntent, TaskCardTransport, TaskNotificationContext,
    block_unanchored_task_response_fallback, clear_unanchored_task_response_fallback, ensure_card,
    provider_bot_key,
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
    block_unanchored_task_response_fallback(provider.as_str(), session_name, channel_id);
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
) -> Result<(super::SessionBoundTerminalDeliveryRoute, Option<MessageId>), RelaySinkError> {
    let card = ensure_task_context_card(
        health_registry,
        shared,
        &delivery.provider,
        delivery.channel_id,
        &delivery.session_name,
        delivery.task_notification_context.as_ref(),
    )
    .await?;
    let route = if card.is_some() {
        super::SessionBoundTerminalDeliveryRoute::NewMessage
    } else {
        route
    };
    Ok((route, card))
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
/// been confirmed and its delivery frontier has advanced. Card confirmation by
/// itself is not response confirmation.
pub(super) fn mark_response_delivered(
    delivery: &super::SessionRelayDelivery,
    task_card_message_id: Option<MessageId>,
) {
    if task_card_message_id.is_some() {
        clear_unanchored_task_response_fallback(
            delivery.provider.as_str(),
            &delivery.session_name,
            delivery.channel_id,
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
    use crate::services::discord::task_notification_delivery::TaskCardTransportError;
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
            .find("formatting::send_long_message_raw_with_reference(")
            .expect("referenced answer send");
        let after_send = &after_gate[send..];
        let advance = after_send
            .find("self.advance_after_confirmed_post(")
            .expect("confirmed answer must advance its delivery frontier");
        let unblock = after_send
            .find("task_notification_context::mark_response_delivered(")
            .expect("watcher fallback gate must be released after answer commit");
        assert!(
            reference < send,
            "card reference must be selected before answer send"
        );
        assert!(
            advance < unblock,
            "watcher fallback must stay blocked through answer confirmation and frontier advance"
        );
    }
}
