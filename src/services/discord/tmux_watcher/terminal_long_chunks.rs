use std::sync::Arc;

use super::*;

use crate::services::discord::gateway::TurnGateway;
use crate::services::discord::inflight::RelayOwnerKind;
use crate::services::discord::outbound::turn_output_controller as toc;
use crate::services::discord::placeholder_controller::PlaceholderKey;
use crate::services::discord::turn_finalizer::TurnKey;
use crate::services::discord::{DeliveryLeaseCell, LeaseHolder, SharedData, lease_now_ms};
use crate::services::provider::ProviderKind;

use super::controller_heartbeat::WatcherPostHeartbeat;

#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn deliver_long_chunks_via_controller<
    G: TurnGateway + ?Sized,
>(
    gateway: &G,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    msg_id: MessageId,
    relay_text: &str,
    cell: &Arc<DeliveryLeaseCell>,
    turn: TurnKey,
    lease_key: Option<crate::services::discord::DeliveryLeaseKey>,
    instance_id: u64,
    start: u64,
    end: u64,
) -> toc::DeliveryOutcome {
    let holder = LeaseHolder::Watcher { instance_id };
    cell.reclaim_if_expired(lease_now_ms());
    let heartbeat = WatcherPostHeartbeat { cell: cell.clone() };
    let advance = |range: (u64, u64)| -> bool {
        debug_assert_eq!(range, (start, end));
        crate::services::discord::tmux::advance_watcher_confirmed_end(
            shared,
            provider,
            channel_id,
            tmux_session_name,
            end,
            "src/services/discord/tmux_watcher/terminal_long_chunks.rs:watcher_long_chunks_controller_advance",
        );
        true
    };
    let outcome = toc::deliver_turn_output(
        gateway,
        toc::TurnOutputCtx {
            turn,
            lease_key,
            owner: RelayOwnerKind::Watcher,
            holder,
            lease: &**cell,
            channel_id,
            placeholder_controller: &shared.ui.placeholder_controller,
            placeholder: toc::PlaceholderSlot::Active {
                message_id: msg_id,
                key: PlaceholderKey {
                    provider: provider.clone(),
                    channel_id,
                    message_id: msg_id,
                },
            },
            body: relay_text,
            send_range: (start, end),
            plan: toc::OutputPlan::SendNewChunks {
                chunk_count: crate::services::discord::formatting::split_message(relay_text).len(),
                delete_anchor: true,
            },
            edit_fail_policy: toc::EditFailPlaceholderPolicy::PreserveAlways,
            fallback_commit_policy: toc::FallbackCommitPolicy::CommitOnFallback,
            acquire_failure_mode: toc::AcquireFailureMode::Transient,
            advance: Some(&advance),
            heartbeat: Some(&heartbeat),
        },
    )
    .await;
    if let toc::DeliveryOutcome::Delivered {
        new_chunks: Some(chunks),
        ..
    } = &outcome
    {
        super::terminal_send::record_watcher_long_chunk_terminal_delivery(
            shared,
            provider,
            channel_id,
            (start, end),
            chunks.tail_message_id.map(|m| m.get()),
        );
    }
    outcome
}

pub(in crate::services::discord) struct WatcherLongChunksLocals<'a> {
    pub(in crate::services::discord) relay_ok: &'a mut bool,
    pub(in crate::services::discord) direct_send_delivered: &'a mut bool,
    pub(in crate::services::discord) tui_direct_anchor_terminal_body_visible: &'a mut bool,
    pub(in crate::services::discord) external_input_lease_consumed_by_relay: &'a mut bool,
    pub(in crate::services::discord) placeholder_msg_id: &'a mut Option<MessageId>,
    pub(in crate::services::discord) placeholder_from_restored_inflight: &'a mut bool,
    pub(in crate::services::discord) last_edit_text: &'a mut String,
}

#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn apply_watcher_long_chunks_controller(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    msg_id: MessageId,
    relay_text: &str,
    cell: &Arc<DeliveryLeaseCell>,
    turn: TurnKey,
    lease_key: Option<crate::services::discord::DeliveryLeaseKey>,
    instance_id: u64,
    range: (u64, u64),
    session_bound_fallback_uses_full_body: bool,
    frozen_rollover_msg_ids: &mut Vec<MessageId>,
    inflight_before_relay: Option<&crate::services::discord::InflightTurnState>,
    locals: WatcherLongChunksLocals<'_>,
) {
    let gateway = crate::services::discord::gateway::DiscordGateway::new(
        http.clone(),
        shared.clone(),
        provider.clone(),
        None,
    );
    let outcome = deliver_long_chunks_via_controller(
        &gateway,
        shared,
        provider,
        channel_id,
        tmux_session_name,
        msg_id,
        relay_text,
        cell,
        turn,
        lease_key,
        instance_id,
        range.0,
        range.1,
    )
    .await;
    apply_watcher_long_chunks_result(
        outcome,
        http,
        shared,
        provider,
        channel_id,
        tmux_session_name,
        msg_id,
        session_bound_fallback_uses_full_body,
        frozen_rollover_msg_ids,
        inflight_before_relay,
        locals,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn apply_watcher_long_chunks_legacy(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    msg_id: MessageId,
    relay_text: &str,
    session_bound_fallback_uses_full_body: bool,
    frozen_rollover_msg_ids: &mut Vec<MessageId>,
    inflight_before_relay: Option<&crate::services::discord::InflightTurnState>,
    watcher_long_chunk_anchor_msg_id: &mut Option<MessageId>,
    locals: WatcherLongChunksLocals<'_>,
) {
    match crate::services::discord::formatting::send_long_message_raw_with_rollback(
        http, channel_id, msg_id, relay_text, shared,
    )
    .await
    {
        Ok(message_ids) => {
            *locals.direct_send_delivered = true;
            *locals.tui_direct_anchor_terminal_body_visible = true;
            *locals.external_input_lease_consumed_by_relay =
                super::watcher_inflight_represents_external_input(inflight_before_relay);
            *watcher_long_chunk_anchor_msg_id = message_ids.last().copied();
            let cleanup = super::delete_terminal_placeholder(
                http,
                channel_id,
                shared,
                provider,
                tmux_session_name,
                msg_id,
                "watcher_terminal_relay_full_body_fallback_cleanup",
            )
            .await;
            if cleanup.is_committed() {
                *locals.placeholder_msg_id = None;
                *locals.placeholder_from_restored_inflight = false;
                locals.last_edit_text.clear();
                drop_placeholder_orphan_record(provider, shared, channel_id, msg_id);
            }
            super::delete_watcher_rollover_frozen_prefixes(
                http,
                channel_id,
                shared,
                provider,
                tmux_session_name,
                session_bound_fallback_uses_full_body,
                std::mem::take(frozen_rollover_msg_ids),
            )
            .await;
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 ✓ relayed full terminal response after session-bound fallback (ordered chunks) channel {} msg {} ({} chars)",
                channel_id.get(),
                msg_id.get(),
                relay_text.len()
            );
        }
        Err(error) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] 👁 Failed to relay ordered terminal chunks: {error}");
            *locals.relay_ok = false;
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn apply_watcher_long_chunks_result(
    outcome: toc::DeliveryOutcome,
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    msg_id: MessageId,
    session_bound_fallback_uses_full_body: bool,
    frozen_rollover_msg_ids: &mut Vec<MessageId>,
    inflight_before_relay: Option<&crate::services::discord::InflightTurnState>,
    locals: WatcherLongChunksLocals<'_>,
) {
    match outcome {
        toc::DeliveryOutcome::Delivered {
            new_chunks: Some(chunks),
            ..
        } => {
            *locals.direct_send_delivered = true;
            *locals.tui_direct_anchor_terminal_body_visible = true;
            *locals.external_input_lease_consumed_by_relay =
                super::watcher_inflight_represents_external_input(inflight_before_relay);
            let cleanup_outcome = match chunks.anchor_delete_error {
                Some(error) => {
                    crate::services::discord::placeholder_cleanup::classify_delete_error(&error)
                }
                None => {
                    crate::services::discord::placeholder_cleanup::PlaceholderCleanupOutcome::Succeeded
                }
            };
            let cleanup_committed = cleanup_outcome.is_committed();
            super::super::record_placeholder_cleanup(
                shared,
                provider,
                channel_id,
                msg_id,
                tmux_session_name,
                crate::services::discord::placeholder_cleanup::PlaceholderCleanupOperation::DeleteTerminal,
                cleanup_outcome,
                "watcher_terminal_relay_full_body_controller_cleanup",
            );
            if cleanup_committed {
                *locals.placeholder_msg_id = None;
                *locals.placeholder_from_restored_inflight = false;
                locals.last_edit_text.clear();
                drop_placeholder_orphan_record(provider, shared, channel_id, msg_id);
            }
            super::delete_watcher_rollover_frozen_prefixes(
                http,
                channel_id,
                shared,
                provider,
                tmux_session_name,
                session_bound_fallback_uses_full_body,
                std::mem::take(frozen_rollover_msg_ids),
            )
            .await;
        }
        _ => {
            *locals.relay_ok = false;
        }
    }
}
