//! Card-before-answer orchestration for session-bound task notifications (#4055).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use serenity::model::id::{ChannelId, MessageId};
use sqlx::PgPool;

use super::super::SharedData;
use super::super::health::HealthRegistry;
use super::super::placeholder_live_events::PlaceholderLiveEvents;
#[cfg(test)]
use super::super::task_notification_delivery::claim_task_response_delivery;
use super::super::task_notification_delivery::{
    CardBot, CardDeliveryClients, CardEnsureError, CardEnsureOutcome, DiscordTaskCardTransport,
    EnsureIntent, ResponseDeliveryClaim, ResponseDeliveryClaimOutcome, ResponseDeliveryOwner,
    TaskCardTransport, TaskNotificationContext, TaskResponseCommitOutcome,
    claim_existing_task_response_delivery,
    claim_task_response_delivery_with_recovery_key_and_started_at,
    commit_task_response_delivered_bounded, durable_response_turn_key, ensure_card,
    fallback_response_turn_key, provider_bot_key, rebind_task_response_card,
    record_task_response_sent_bounded,
};
use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::cluster::stream_relay::RelaySinkError;
use crate::services::provider::ProviderKind;

fn defer_task_response_to_watcher(
    turn_start_offset: Option<u64>,
    terminal_consumed_end: Option<u64>,
) -> bool {
    turn_start_offset.is_none() && terminal_consumed_end.is_none()
}

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
    .map_err(|error| match error {
        CardEnsureError::Permanent(error) => RelaySinkError::Permanent(format!(
            "task-notification card permanently rejected before response delivery: {error}"
        )),
        error => RelaySinkError::Transient(format!(
            "task-notification card must be confirmed before response delivery: {error}"
        )),
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
    let response_claim = if card.is_some()
        && delivery.task_notification_context.is_some()
        && defer_task_response_to_watcher(
            delivery.frame_turn_start_offset,
            delivery.terminal_consumed_end,
        ) {
        // A frame with no monotonic coordinate cannot be reconciled against
        // delivered tombstones without risking either suppression or replay.
        // The watcher owns the real consumed end and will retry this response.
        Some(ResponseDeliveryClaimOutcome::Wait)
    } else if let (Some(message_id), Some(context)) =
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
        let recovery_turn_key = fallback_response_turn_key(
            delivery.channel_id,
            delivery.provider.as_str(),
            &delivery.session_name,
            delivery.terminal_consumed_end.unwrap_or_default(),
            &delivery.response_text,
        );
        let mut existing = claim_existing_task_response_delivery(
            shared.pg_pool.as_ref(),
            delivery.channel_id,
            delivery.provider.as_str(),
            &delivery.session_name,
            &turn_key,
            ResponseDeliveryOwner::Sink,
        )
        .await
        .map_err(|error| {
            RelaySinkError::Transient(format!(
                "resume task-notification response turn before delivery: {error}"
            ))
        })?;
        if existing.is_none() && recovery_turn_key != turn_key {
            existing = claim_existing_task_response_delivery(
                shared.pg_pool.as_ref(),
                delivery.channel_id,
                delivery.provider.as_str(),
                &delivery.session_name,
                &recovery_turn_key,
                ResponseDeliveryOwner::Sink,
            )
            .await
            .map_err(|error| {
                RelaySinkError::Transient(format!(
                    "resume task-notification response by recovery identity: {error}"
                ))
            })?;
        }
        let outcome = if let Some(existing) = existing {
            match existing.outcome {
                ResponseDeliveryClaimOutcome::Owned(claim)
                    if existing.card_message_id != message_id.get() =>
                {
                    ResponseDeliveryClaimOutcome::Owned(
                        rebind_task_response_card(
                            shared.pg_pool.as_ref(),
                            &claim,
                            message_id.get(),
                        )
                        .await
                        .map_err(|error| {
                            RelaySinkError::Transient(format!(
                                "rebind resumed response to confirmed replacement card: {error}"
                            ))
                        })?,
                    )
                }
                outcome => outcome,
            }
        } else {
            claim_task_response_delivery_with_recovery_key_and_started_at(
                shared.pg_pool.as_ref(),
                delivery.channel_id,
                delivery.provider.as_str(),
                &delivery.session_name,
                context.event_key(),
                &turn_key,
                Some(&recovery_turn_key),
                Some(&delivery.frame_turn_started_at),
                delivery.frame_turn_start_offset,
                delivery.terminal_consumed_end,
                message_id.get(),
                ResponseDeliveryOwner::Sink,
            )
            .await
            .map_err(|error| {
                RelaySinkError::Transient(format!(
                    "task-notification response turn must be durably bound before delivery: {error}"
                ))
            })?
        };
        Some(outcome)
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
pub(super) async fn commit_response_fence(
    shared: &Arc<SharedData>,
    delivery: &super::SessionRelayDelivery,
    response_claim: Option<&ResponseDeliveryClaim>,
) -> TaskResponseCommitOutcome {
    let Some(response_claim) = response_claim else {
        return TaskResponseCommitOutcome::Delivered;
    };
    let outcome =
        commit_task_response_delivered_bounded(shared.pg_pool.as_ref(), response_claim).await;
    if let TaskResponseCommitOutcome::SentButUncommitted { error } = &outcome {
        tracing::error!(
            provider = delivery.provider.as_str(),
            channel_id = delivery.channel_id,
            tmux_session = %delivery.session_name,
            error = %error,
            "task response was sent but its final PostgreSQL delivery CAS stayed uncommitted"
        );
    }
    outcome
}

#[allow(clippy::too_many_arguments)]
impl super::SessionBoundDiscordRelaySink {
    pub(super) async fn deliver_new_message_with_task_authority(
        &self,
        http: &Arc<serenity::http::Http>,
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        channel_id: u64,
        delivery: &super::SessionRelayDelivery,
        relay_text: &str,
        task_card_message_id: Option<MessageId>,
        task_response_claim: Option<ResponseDeliveryClaim>,
        trace: &super::SessionRelayTraceContext,
        sink_lease_guard: Option<&super::SinkDeliveryLeaseGuard>,
    ) -> Result<super::SessionRelayDeliveryOutcome, RelaySinkError> {
        let channel = ChannelId::new(channel_id);
        let prompt_anchor = super::relay_format::ssh_direct_prompt_anchor_for_response(
            provider,
            &delivery.session_name,
            channel_id,
        );
        let mut task_card_message_id = task_card_message_id;
        let mut task_response_claim = task_response_claim;
        let mut response_heartbeat = None;
        let mut prompt_anchor_reference =
            answer_reference(channel, task_card_message_id, prompt_anchor);
        if task_card_message_id.is_some() {
            if let Some(claim) = task_response_claim.as_ref() {
                super::super::task_notification_delivery::renew_task_response_delivery(
                    shared.pg_pool.as_ref(),
                    claim,
                )
                .await
                .map_err(|error| {
                    RelaySinkError::Transient(format!(
                        "task response claim was lost before send: {error}"
                    ))
                })?;
            }
            let heartbeat =
                super::super::task_notification_delivery::task_response_delivery_heartbeat(
                    shared.pg_pool.as_ref(),
                    task_response_claim.as_ref(),
                );
            if task_response_claim.is_none() {
                return Err(RelaySinkError::Transient(
                    "task-card response omitted its exact delivery claim".to_string(),
                ));
            }
            let response_transport =
                super::super::task_notification_delivery::DiscordResponseChunkTransport::new(
                    http.as_ref(),
                    shared,
                );
            let context = delivery.task_notification_context.as_ref().ok_or_else(|| {
                RelaySinkError::Permanent(
                    "missing task context prevents exact missing-card repair".to_string(),
                )
            })?;
            let event = context.to_event(
                delivery.channel_id,
                provider.as_str(),
                &delivery.session_name,
            );
            let provider_http = shared.serenity_http_or_token_fallback();
            let notify_http =
                super::super::health::resolve_bot_http(self.health_registry.as_ref(), "notify")
                    .await
                    .ok();
            let clients = CardDeliveryClients::new(
                notify_http
                    .map(|http| CardBot::new("notify", http))
                    .into_iter()
                    .chain(
                        provider_http
                            .map(|http| CardBot::new(provider_bot_key(provider.as_str()), http)),
                    ),
            );
            let card_transport = DiscordTaskCardTransport::new(shared.clone());
            let (_messages, rebound) = super::super::task_notification_delivery::send_task_response_chunks_with_card_repair(
                shared.pg_pool.as_ref(),
                &clients,
                &card_transport,
                &response_transport,
                &event,
                task_response_claim.as_ref().expect("claim checked above").clone(),
                relay_text,
            )
            .await
            .map_err(|error| match error {
                super::super::task_notification_delivery::ResponseChunkDeliveryError::Permanent(_) => {
                    RelaySinkError::Permanent(error.to_string())
                }
                _ => RelaySinkError::Transient(error.to_string()),
            })?;
            task_card_message_id = Some(MessageId::new(rebound.card_message_id()));
            task_response_claim = Some(rebound);
            record_task_response_sent_bounded(
                shared.pg_pool.as_ref(),
                task_response_claim.as_ref().expect("claim checked above"),
            )
            .await
            .map_err(RelaySinkError::Transient)?;
            response_heartbeat = Some(heartbeat);
        } else {
            super::super::formatting::send_long_message_raw_with_reference(
                http,
                channel,
                relay_text,
                shared,
                prompt_anchor_reference,
            )
            .await
            .map_err(|error| RelaySinkError::Transient(error.to_string()))?;
        }
        prompt_anchor_reference = answer_reference(channel, task_card_message_id, prompt_anchor);
        if let Some(prompt_anchor) = prompt_anchor {
            super::relay_format::clear_ssh_direct_prompt_anchor(
                provider,
                &delivery.session_name,
                prompt_anchor,
            );
        }
        self.delivered_total.fetch_add(1, Ordering::AcqRel);
        // #3041 P1-4 (§4-④): lease released by the RAII guard on exit.
        tracing::info!(
            provider = provider.as_str(),
            channel_id,
            tmux_session = %delivery.session_name,
            turn_id = trace.turn_id().unwrap_or(""),
            dispatch_id = trace.dispatch_id().unwrap_or(""),
            session_key = trace.session_key().unwrap_or(""),
            relay_owner = trace.relay_owner(),
            runtime_kind = trace.runtime_kind(),
            prompt_anchor_message_id = prompt_anchor_reference
                .map(|(_, message_id)| message_id.get()),
            chars = relay_text.chars().count(),
            "session-bound relay sink delivered terminal response via new message"
        );
        crate::services::observability::emit_relay_delivery(
            provider.as_str(),
            channel_id,
            trace.dispatch_id(),
            trace.session_key(),
            trace.turn_id(),
            prompt_anchor_reference.map(|(_, message_id)| message_id.get()),
            "session_relay_sink",
            "post",
            None,
            None,
            true,
            Some("new message"),
        );
        // #3041 P1-3: post-POST fresh identity re-check before frontier advance.
        self.advance_after_confirmed_post(
            shared,
            provider,
            channel_id,
            &delivery.session_name,
            delivery,
            sink_lease_guard,
        );
        let commit_outcome =
            commit_response_fence(shared, delivery, task_response_claim.as_ref()).await;
        if let Some(heartbeat) = response_heartbeat {
            heartbeat.stop();
        }
        match commit_outcome {
            TaskResponseCommitOutcome::Delivered => {
                Ok(super::SessionRelayDeliveryOutcome::Delivered)
            }
            TaskResponseCommitOutcome::SentButUncommitted { .. } => {
                Ok(super::SessionRelayDeliveryOutcome::SentButUncommitted)
            }
        }
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
        TaskCardTransportError, mark_task_response_delivered,
    };
    use crate::services::session_backend::StreamLineState;

    #[test]
    fn coordinate_less_task_response_defers_until_watcher_has_a_consumed_end() {
        assert!(defer_task_response_to_watcher(None, None));
        assert!(!defer_task_response_to_watcher(Some(10), None));
        assert!(!defer_task_response_to_watcher(None, Some(20)));
    }

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
        let pending = claim_existing_task_response_delivery(
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
        assert_eq!(pending.card_message_id, card.message_id);
        assert!(matches!(
            pending.outcome,
            ResponseDeliveryClaimOutcome::Wait
        ));

        mark_task_response_delivered(None, &claim)
            .await
            .expect("mark response delivered");
        let delivered = claim_existing_task_response_delivery(
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
        assert_eq!(delivered.card_message_id, card.message_id);
        assert!(matches!(
            delivered.outcome,
            ResponseDeliveryClaimOutcome::Delivered { .. }
        ));
    }

    #[test]
    fn giant_sink_wires_card_gate_before_reference_send() {
        let source = include_str!("../session_relay_sink.rs");
        let gate = source
            .find("ensure_card_and_route(")
            .expect("sink must invoke task card gate");
        let delegate = source[gate..]
            .find("deliver_new_message_with_task_authority(")
            .map(|offset| gate + offset)
            .expect("sink must delegate new-message delivery to task authority");
        assert!(
            gate < delegate,
            "card gate must precede task-aware delivery"
        );

        let helper_source = include_str!("task_notification_context.rs");
        let helper = helper_source
            .split_once("\n#[cfg(test)]\nmod tests {")
            .map(|(production, _)| production)
            .expect("task authority test must inspect production code only");
        let authority = helper
            .find("deliver_new_message_with_task_authority(")
            .expect("task-aware new-message authority");
        let after_authority = &helper[authority..];
        let reference = after_authority
            .find("answer_reference(channel")
            .expect("confirmed card id must become answer reference");
        let send = after_authority
            .find("send_task_response_chunks_with_card_repair(")
            .expect("durable required-reference answer send");
        let after_send = &after_authority[send..];
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
        let heartbeat_stop = after_send
            .find("heartbeat.stop()")
            .expect("task response heartbeat must stop explicitly");
        assert!(
            unblock < heartbeat_stop,
            "sink task response heartbeat must cover the bounded final delivery CAS"
        );
    }
}
