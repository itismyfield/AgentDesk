//! Durable task-card and response-delivery authority for watcher fallback.

use std::sync::Arc;

use super::*;

use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::discord::SharedData;
use crate::services::discord::inflight::opt_message_id;
use crate::services::discord::task_notification_delivery as task_delivery;
use crate::services::provider::ProviderKind;

struct PreparedWatcherTaskResponse {
    claim: task_delivery::ResponseDeliveryClaimOutcome,
    card_message_id: MessageId,
    event: task_delivery::TaskCardEvent,
    clients: task_delivery::CardDeliveryClients,
}

#[derive(Debug, thiserror::Error)]
enum PrepareWatcherTaskResponseError {
    #[error("{0}")]
    Transient(String),
    #[error("{0}")]
    Permanent(String),
}

pub(super) struct WatcherTaskResponseLocals<'a> {
    pub(super) placeholder_msg_id: &'a mut Option<MessageId>,
    pub(super) placeholder_from_restored_inflight: &'a mut bool,
    pub(super) last_edit_text: &'a mut String,
    pub(super) retry_terminal_delivery_from_offset: &'a mut bool,
    pub(super) tui_direct_anchor_terminal_body_visible: &'a mut bool,
    pub(super) tui_direct_anchor_or_lease_present_for_lifecycle: &'a mut bool,
    pub(super) task_response_claim: &'a mut Option<task_delivery::ResponseDeliveryClaim>,
}

pub(super) struct WatcherTaskResponseOutcome {
    pub(super) relay_ok: bool,
    pub(super) direct_send_delivered: bool,
    pub(super) external_input_lease_consumed_by_relay: bool,
}

#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn commit_watcher_task_response_fence(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    frontier_committed: bool,
    claim: Option<&task_delivery::ResponseDeliveryClaim>,
) {
    if frontier_committed && let Some(claim) = claim {
        let heartbeat =
            task_delivery::task_response_delivery_heartbeat(shared.pg_pool.as_ref(), Some(claim));
        let outcome =
            task_delivery::commit_task_response_delivered_bounded(shared.pg_pool.as_ref(), claim)
                .await;
        heartbeat.stop();
        if let task_delivery::TaskResponseCommitOutcome::SentButUncommitted { error } = outcome {
            tracing::error!(
                provider = provider.as_str(),
                channel_id = channel_id.get(),
                tmux_session = %tmux_session_name,
                error = %error,
                "watcher advanced the task response frontier but its sent response stayed uncommitted"
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn prepare_watcher_task_response(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    kind: TaskNotificationKind,
    context: Option<&task_delivery::TaskNotificationContext>,
    response_turn_key: &str,
    turn_started_at: Option<&str>,
    turn_start_offset: Option<u64>,
    turn_end_offset: u64,
) -> Result<PreparedWatcherTaskResponse, PrepareWatcherTaskResponseError> {
    let mut event = context.map_or_else(
        || {
            task_delivery::TaskCardEvent::from_recovered_terminal(
                channel_id.get(),
                provider.as_str(),
                tmux_session_name,
                kind,
                response_turn_key,
            )
        },
        |context| context.to_event(channel_id.get(), provider.as_str(), tmux_session_name),
    );
    if let Some(existing) = task_delivery::claim_existing_task_response_delivery(
        shared.pg_pool.as_ref(),
        channel_id.get(),
        provider.as_str(),
        tmux_session_name,
        response_turn_key,
        task_delivery::ResponseDeliveryOwner::Watcher,
    )
    .await
    .map_err(|error| {
        PrepareWatcherTaskResponseError::Transient(format!("resume watcher task response: {error}"))
    })? {
        event = event.with_persisted_event_key(existing.event_key);
        let clients =
            watcher_card_clients(http, shared, provider, Some(existing.card_bot_key.as_str()))
                .await?;
        if let Some(context) = context {
            let event = context.to_event(channel_id.get(), provider.as_str(), tmux_session_name);
            shared
                .ui
                .placeholder_live_events
                .claim_terminal_slot_for_card(channel_id, event.kind(), event.tool_use_id());
        }
        let Some(card_message_id) = opt_message_id(existing.card_message_id) else {
            return Err(PrepareWatcherTaskResponseError::Transient(
                "persisted task response card message id is zero".to_string(),
            ));
        };
        return Ok(PreparedWatcherTaskResponse {
            claim: existing.outcome,
            card_message_id,
            event,
            clients,
        });
    }
    if context.is_none() {
        return Err(PrepareWatcherTaskResponseError::Transient(
            "recovered watcher turn has no durable response identity; refusing to create a second response fence"
                .to_string(),
        ));
    }
    let clients = watcher_card_clients(http, shared, provider, None).await?;
    let transport = task_delivery::DiscordTaskCardTransport::new(shared.clone());
    let card = task_delivery::ensure_card_with_shared(
        shared.as_ref(),
        &clients,
        &transport,
        &event,
        task_delivery::EnsureIntent::Promotion,
    )
    .await
    .map_err(|error| match error {
        task_delivery::CardEnsureError::Permanent(error) => {
            PrepareWatcherTaskResponseError::Permanent(format!(
                "confirm watcher task card: {error}"
            ))
        }
        error => PrepareWatcherTaskResponseError::Transient(format!(
            "confirm watcher task card: {error}"
        )),
    })?;
    shared
        .ui
        .placeholder_live_events
        .claim_terminal_slot_for_card(channel_id, event.kind(), event.tool_use_id());
    let claim = task_delivery::claim_task_response_delivery_with_recovery_key_and_started_at(
        shared.pg_pool.as_ref(),
        channel_id.get(),
        provider.as_str(),
        tmux_session_name,
        event.event_key(),
        response_turn_key,
        Some(response_turn_key),
        turn_started_at,
        turn_start_offset,
        Some(turn_end_offset),
        card.message_id,
        task_delivery::ResponseDeliveryOwner::Watcher,
    )
    .await
    .map_err(|error| {
        PrepareWatcherTaskResponseError::Transient(format!("claim watcher task response: {error}"))
    })?;
    Ok(PreparedWatcherTaskResponse {
        claim,
        card_message_id: MessageId::new(card.message_id),
        event,
        clients,
    })
}

async fn watcher_card_clients(
    provider_http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    persisted_bot_key: Option<&str>,
) -> Result<task_delivery::CardDeliveryClients, PrepareWatcherTaskResponseError> {
    let provider_key = task_delivery::provider_bot_key(provider.as_str());
    let notify_role = crate::services::discord::bot_role::UtilityBotRole::Notify;
    let notify_alias = notify_role.alias();
    let mut bots = Vec::new();
    let needs_notify = persisted_bot_key.is_none() || persisted_bot_key == Some(notify_alias);
    if needs_notify && let Some(registry) = shared.health_registry() {
        match crate::services::discord::health::resolve_utility_bot_http(
            registry.as_ref(),
            notify_role,
        )
        .await
        {
            Ok(http) => bots.push(task_delivery::CardBot::new(notify_alias, http)),
            Err((_status, error)) if persisted_bot_key == Some(notify_alias) => {
                return Err(PrepareWatcherTaskResponseError::Transient(format!(
                    "persisted notify task-card bot is unavailable during watcher recovery: {error}"
                )));
            }
            Err(_) => {}
        }
    } else if needs_notify && persisted_bot_key == Some(notify_alias) {
        return Err(PrepareWatcherTaskResponseError::Transient(
            "persisted notify task-card bot cannot be resolved without a health registry"
                .to_string(),
        ));
    }
    if persisted_bot_key.is_none() || persisted_bot_key == Some(provider_key.as_str()) {
        bots.push(task_delivery::CardBot::new(
            provider_key.clone(),
            provider_http.clone(),
        ));
    }
    if let Some(persisted) = persisted_bot_key
        && persisted != notify_alias
        && persisted != provider_key
    {
        return Err(PrepareWatcherTaskResponseError::Permanent(format!(
            "persisted task-card bot identity {persisted} is not valid for provider {}",
            provider.as_str()
        )));
    }
    Ok(task_delivery::CardDeliveryClients::new(bots))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn apply_watcher_task_response(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    kind: TaskNotificationKind,
    context: Option<&task_delivery::TaskNotificationContext>,
    response_turn_key: &str,
    turn_started_at: Option<&str>,
    turn_start_offset: Option<u64>,
    turn_end_offset: u64,
    relay_text: &str,
    external_input_lease_before_relay: bool,
    locals: WatcherTaskResponseLocals<'_>,
) -> WatcherTaskResponseOutcome {
    let WatcherTaskResponseLocals {
        placeholder_msg_id,
        placeholder_from_restored_inflight,
        last_edit_text,
        retry_terminal_delivery_from_offset,
        tui_direct_anchor_terminal_body_visible,
        tui_direct_anchor_or_lease_present_for_lifecycle,
        task_response_claim,
    } = locals;
    let mut relay_ok = true;
    let mut direct_send_delivered = false;
    let mut external_input_lease_consumed_by_relay = false;
    match prepare_watcher_task_response(
        http,
        shared,
        provider,
        channel_id,
        tmux_session_name,
        kind,
        context,
        response_turn_key,
        turn_started_at,
        turn_start_offset,
        turn_end_offset,
    )
    .await
    {
        Err(PrepareWatcherTaskResponseError::Transient(error)) => {
            tracing::warn!(
                provider = provider.as_str(),
                channel_id = channel_id.get(),
                tmux_session = tmux_session_name,
                error = %error,
                "watcher task response preparation failed; preserving the delivery frontier"
            );
            relay_ok = false;
            *retry_terminal_delivery_from_offset = true;
        }
        Err(PrepareWatcherTaskResponseError::Permanent(error)) => {
            tracing::error!(
                provider = provider.as_str(),
                channel_id = channel_id.get(),
                tmux_session = tmux_session_name,
                error = %error,
                "watcher task response preparation failed permanently; bounded give-up without rewinding"
            );
            relay_ok = false;
            *retry_terminal_delivery_from_offset = false;
        }
        Ok(mut prepared) => {
            use task_delivery::ResponseDeliveryClaimOutcome;
            match prepared.claim {
                ResponseDeliveryClaimOutcome::Wait => {
                    tracing::info!(
                        provider = provider.as_str(),
                        channel_id = channel_id.get(),
                        tmux_session = tmux_session_name,
                        "watcher task response waits for the live sink claim"
                    );
                    relay_ok = false;
                    *retry_terminal_delivery_from_offset = true;
                }
                ResponseDeliveryClaimOutcome::Delivered { .. } => {
                    direct_send_delivered = true;
                    *tui_direct_anchor_terminal_body_visible = true;
                    *tui_direct_anchor_or_lease_present_for_lifecycle = true;
                    external_input_lease_consumed_by_relay = external_input_lease_before_relay;
                }
                ResponseDeliveryClaimOutcome::SentUncommitted { card_message_id } => {
                    tracing::error!(
                        provider = provider.as_str(),
                        channel_id = channel_id.get(),
                        tmux_session = tmux_session_name,
                        task_card_message_id = card_message_id,
                        "watcher found a sent-but-uncommitted response and refused a duplicate POST"
                    );
                    direct_send_delivered = true;
                    *tui_direct_anchor_terminal_body_visible = true;
                    *tui_direct_anchor_or_lease_present_for_lifecycle = true;
                    external_input_lease_consumed_by_relay = external_input_lease_before_relay;
                }
                ResponseDeliveryClaimOutcome::Owned(mut claim) => {
                    let renewed = task_delivery::renew_task_response_delivery(
                        shared.pg_pool.as_ref(),
                        &claim,
                    )
                    .await;
                    if let Err(error) = renewed {
                        tracing::warn!(
                            provider = provider.as_str(),
                            channel_id = channel_id.get(),
                            tmux_session = tmux_session_name,
                            error = %error,
                            "watcher lost its task response claim before send"
                        );
                        relay_ok = false;
                        *retry_terminal_delivery_from_offset = true;
                    } else {
                        let heartbeat = task_delivery::task_response_delivery_heartbeat(
                            shared.pg_pool.as_ref(),
                            Some(&claim),
                        );
                        let response_transport = task_delivery::DiscordResponseChunkTransport::new(
                            http.as_ref(),
                            shared,
                        );
                        let card_transport =
                            task_delivery::DiscordTaskCardTransport::new(shared.clone());
                        let send_result =
                            task_delivery::send_task_response_chunks_with_card_repair(
                                shared.pg_pool.as_ref(),
                                &prepared.clients,
                                &card_transport,
                                &response_transport,
                                &prepared.event,
                                claim.clone(),
                                relay_text,
                            )
                            .await;
                        let send_result = match send_result {
                            Ok((messages, rebound)) => {
                                claim = rebound;
                                prepared.card_message_id = MessageId::new(claim.card_message_id());
                                Ok(messages)
                            }
                            Err(error) => Err(error),
                        };
                        match send_result {
                            Ok(_) => {
                                let sent_state = task_delivery::record_task_response_sent_bounded(
                                    shared.pg_pool.as_ref(),
                                    &claim,
                                )
                                .await;
                                heartbeat.stop();
                                match sent_state {
                                    Ok(()) => {
                                        *task_response_claim = Some(claim);
                                        direct_send_delivered = true;
                                        *tui_direct_anchor_terminal_body_visible = true;
                                        *tui_direct_anchor_or_lease_present_for_lifecycle = true;
                                        external_input_lease_consumed_by_relay =
                                            external_input_lease_before_relay;
                                    }
                                    Err(error) => {
                                        tracing::error!(
                                            provider = provider.as_str(),
                                            channel_id = channel_id.get(),
                                            tmux_session = tmux_session_name,
                                            error = %error,
                                            "watcher sent a task response but could not persist its sent-state fence"
                                        );
                                        relay_ok = false;
                                        *retry_terminal_delivery_from_offset = false;
                                    }
                                }
                            }
                            Err(error) => {
                                heartbeat.stop();
                                info_watcher_failed_relay(&error);
                                let failure_class = match &error {
                                    task_delivery::ResponseChunkDeliveryError::Permanent(_) => {
                                        crate::services::discord::replace_outcome_policy::WatcherSendFailureClass::Permanent
                                    }
                                    task_delivery::ResponseChunkDeliveryError::Transient(_)
                                    | task_delivery::ResponseChunkDeliveryError::Ambiguous { .. }
                                    | task_delivery::ResponseChunkDeliveryError::UnknownReference { .. } => {
                                        crate::services::discord::replace_outcome_policy::WatcherSendFailureClass::Transient
                                    }
                                };
                                let plan = watcher_send_failure_plan_warned(
                                    failure_class,
                                    WatcherNoRewindWarnSite::PlaceholderlessFull,
                                    provider,
                                    channel_id,
                                    tmux_session_name,
                                    &error,
                                );
                                relay_ok = plan.relay_ok;
                                *retry_terminal_delivery_from_offset = plan.retry_offset;
                            }
                        }
                    }
                }
            }
            if direct_send_delivered && let Some(stale_placeholder) = *placeholder_msg_id {
                if stale_placeholder == prepared.card_message_id {
                    *placeholder_msg_id = None;
                    *placeholder_from_restored_inflight = false;
                    last_edit_text.clear();
                } else {
                    let cleanup = delete_terminal_placeholder(
                        http,
                        channel_id,
                        shared,
                        provider,
                        tmux_session_name,
                        stale_placeholder,
                        "watcher_task_response_placeholder_cleanup",
                    )
                    .await;
                    if cleanup.is_committed() {
                        *placeholder_msg_id = None;
                        *placeholder_from_restored_inflight = false;
                        last_edit_text.clear();
                        drop_placeholder_orphan_record(
                            provider,
                            shared,
                            channel_id,
                            stale_placeholder,
                        );
                    } else {
                        tracing::warn!(
                            provider = provider.as_str(),
                            channel_id = channel_id.get(),
                            message_id = stale_placeholder.get(),
                            "task response delivered; stale placeholder cleanup will retry independently"
                        );
                    }
                }
            }
        }
    }
    WatcherTaskResponseOutcome {
        relay_ok,
        direct_send_delivered,
        external_input_lease_consumed_by_relay,
    }
}
