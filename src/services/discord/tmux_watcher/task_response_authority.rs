//! Durable task-card and response-delivery authority for watcher fallback.

use std::sync::Arc;

use super::*;

use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::discord::SharedData;
use crate::services::discord::formatting::long_send_rollback;
use crate::services::discord::task_notification_delivery as task_delivery;
use crate::services::provider::ProviderKind;

struct PreparedWatcherTaskResponse {
    claim: task_delivery::ResponseDeliveryClaimOutcome,
    card_message_id: MessageId,
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
    if frontier_committed
        && let Some(claim) = claim
        && let Err(error) =
            task_delivery::mark_task_response_delivered(shared.pg_pool.as_ref(), claim).await
    {
        tracing::error!(
            provider = provider.as_str(),
            channel_id = channel_id.get(),
            tmux_session = %tmux_session_name,
            error = %error,
            "watcher advanced the task response frontier but could not commit its exact delivery claim"
        );
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
) -> Result<PreparedWatcherTaskResponse, String> {
    if let Some((claim, card_message_id)) = task_delivery::claim_existing_task_response_delivery(
        shared.pg_pool.as_ref(),
        channel_id.get(),
        provider.as_str(),
        tmux_session_name,
        response_turn_key,
        task_delivery::ResponseDeliveryOwner::Watcher,
    )
    .await
    .map_err(|error| format!("resume watcher task response: {error}"))?
    {
        if let Some(context) = context {
            let event = context.to_event(channel_id.get(), provider.as_str(), tmux_session_name);
            shared
                .ui
                .placeholder_live_events
                .claim_terminal_slot_for_card(channel_id, event.kind(), event.tool_use_id());
        }
        return Ok(PreparedWatcherTaskResponse {
            claim,
            card_message_id: MessageId::new(card_message_id),
        });
    }
    let event = context.map_or_else(
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
    let clients = task_delivery::CardDeliveryClients::new([task_delivery::CardBot::new(
        task_delivery::provider_bot_key(provider.as_str()),
        http.clone(),
    )]);
    let transport = task_delivery::DiscordTaskCardTransport::new(shared.clone());
    let card = task_delivery::ensure_card(
        shared.pg_pool.as_ref(),
        &clients,
        &transport,
        &event,
        task_delivery::EnsureIntent::Promotion,
    )
    .await
    .map_err(|error| format!("confirm watcher task card: {error}"))?;
    shared
        .ui
        .placeholder_live_events
        .claim_terminal_slot_for_card(channel_id, event.kind(), event.tool_use_id());
    let claim = task_delivery::claim_task_response_delivery(
        shared.pg_pool.as_ref(),
        channel_id.get(),
        provider.as_str(),
        tmux_session_name,
        event.event_key(),
        response_turn_key,
        card.message_id,
        task_delivery::ResponseDeliveryOwner::Watcher,
    )
    .await
    .map_err(|error| format!("claim watcher task response: {error}"))?;
    Ok(PreparedWatcherTaskResponse {
        claim,
        card_message_id: MessageId::new(card.message_id),
    })
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
    )
    .await
    {
        Err(error) => {
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
        Ok(prepared) => {
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
                ResponseDeliveryClaimOutcome::Owned(claim) => {
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
                        let send_result = long_send_rollback::send_long_message_raw_with_required_reference_rollback(
                            http,
                            channel_id,
                            prepared.card_message_id,
                            relay_text,
                            shared,
                            (channel_id, prepared.card_message_id),
                        )
                        .await;
                        heartbeat.stop();
                        match send_result {
                            Ok(_) => {
                                *task_response_claim = Some(claim);
                                direct_send_delivered = true;
                                *tui_direct_anchor_terminal_body_visible = true;
                                *tui_direct_anchor_or_lease_present_for_lifecycle = true;
                                external_input_lease_consumed_by_relay =
                                    external_input_lease_before_relay;
                            }
                            Err(error) => {
                                info_watcher_failed_relay(error.as_ref());
                                let plan = watcher_send_failure_plan_warned(
                                    classify_watcher_send_failure(error.as_ref()),
                                    WatcherNoRewindWarnSite::PlaceholderlessFull,
                                    provider,
                                    channel_id,
                                    tmux_session_name,
                                    error.as_ref(),
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
