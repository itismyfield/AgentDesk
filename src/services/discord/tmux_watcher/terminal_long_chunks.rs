use std::sync::Arc;

use super::*;

use crate::services::discord::gateway::TurnGateway;
use crate::services::discord::inflight::RelayOwnerKind;
use crate::services::discord::outbound::turn_output_controller as toc;
use crate::services::discord::placeholder_controller::PlaceholderKey;
use crate::services::discord::tmux::WatcherDeliveryTarget;
use crate::services::discord::turn_finalizer::TurnKey;
use crate::services::discord::{DeliveryLeaseCell, LeaseHolder, SharedData, lease_now_ms};
use crate::services::provider::ProviderKind;

use super::controller_heartbeat::WatcherPostHeartbeat;

/// Lease-captured immutable identity for a watcher terminal delivery commit.
/// Exact receipts still require a matching fresh inflight row; the pinned user
/// message id is used only for post-persist completed-turn settlement.
#[derive(Clone, Copy)]
pub(in crate::services::discord) struct WatcherDeliveryIdentity {
    pub(in crate::services::discord) generation_mtime_ns: i64,
    pub(in crate::services::discord) lease_reset_incarnation: u64,
    pub(in crate::services::discord) ledger_user_msg_id: Option<u64>,
}

pub(in crate::services::discord) fn watcher_delivery_identity(
    source_generation_mtime_ns: i64,
    source_reset_incarnation: u64,
    lease_key: Option<&crate::services::discord::DeliveryLeaseKey>,
) -> WatcherDeliveryIdentity {
    WatcherDeliveryIdentity {
        generation_mtime_ns: source_generation_mtime_ns,
        lease_reset_incarnation: source_reset_incarnation,
        ledger_user_msg_id: lease_key
            .map(|key| key.user_msg_id)
            .filter(|user_msg_id| *user_msg_id != 0),
    }
}

pub(in crate::services::discord) struct WatcherDeliveryMutation {
    _guard: crate::services::discord::RelayFrontierMutationGuard,
    identity: WatcherDeliveryIdentity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum GuardedWatcherDeliveryResult {
    Persisted,
    AdvancedWithoutProof,
    LandedStale,
    LandedUnrecorded,
}

pub(in crate::services::discord) struct WatcherTerminalDeliveryProof {
    pub(in crate::services::discord) anchor_msg_id: Option<MessageId>,
    pub(in crate::services::discord) raw_body: String,
    /// #5071 T1 S3a: the Discord response behind `anchor_msg_id`, carrying the
    /// channel Discord actually returned. `None` on the controller/gateway path,
    /// which reports message ids only — the shadow journal then leaves the
    /// obligation open rather than synthesising a receipt that could never trip
    /// the `channel_mismatch` branch.
    pub(in crate::services::discord) receipt:
        Option<crate::services::discord::outbound::DiscordTransportReceipt>,
}

/// Admit a delivery epilogue for the captured source identity.
///
/// #4911 R10: the returned guard is an admission counter, not mutual exclusion —
/// it keeps a frontier RESET from rewinding underneath an in-flight delivery, and
/// nothing more. Two concurrent deliveries are serialized by the record flock in
/// `delivery_record.rs`, not here, so this must not be read as "the epilogue runs
/// atomically". Admission gates on the source incarnation only; a concurrent
/// advance of `committed_offset` is harmless and must not reject a live delivery.
pub(in crate::services::discord) fn begin_watcher_delivery_mutation(
    shared: &SharedData,
    channel_id: ChannelId,
    tmux_session_name: &str,
    identity: WatcherDeliveryIdentity,
) -> Option<WatcherDeliveryMutation> {
    if dr::current_generation_mtime_ns(tmux_session_name) != identity.generation_mtime_ns {
        return None;
    }
    let guard = shared.acquire_relay_frontier_mutation_for_incarnation(
        channel_id,
        identity.lease_reset_incarnation,
    )?;
    // Re-read under the guard: a reset can no longer land, so an unchanged
    // generation here holds for the rest of this epilogue.
    (dr::current_generation_mtime_ns(tmux_session_name) == identity.generation_mtime_ns).then_some(
        WatcherDeliveryMutation {
            _guard: guard,
            identity,
        },
    )
}

impl WatcherDeliveryMutation {
    pub(in crate::services::discord) fn advance(
        &self,
        target: WatcherDeliveryTarget<'_>,
        end: u64,
        context: &'static str,
    ) -> bool {
        crate::services::discord::tmux::advance_watcher_confirmed_end_for_generation(
            target,
            end,
            self.identity.generation_mtime_ns,
            context,
        )
    }

    pub(in crate::services::discord) fn persist(
        self,
        target: WatcherDeliveryTarget<'_>,
        range: (u64, u64),
        terminal_anchor_msg_id: Option<u64>,
        delivered_body: &str,
    ) -> bool {
        let WatcherDeliveryTarget {
            shared,
            provider,
            channel_id,
            tmux_session_name,
        } = target;
        dr::record_watcher_terminal_delivery(
            shared,
            provider,
            channel_id,
            tmux_session_name,
            dr::WatcherDeliveryRecordAuthority {
                lease_reset_incarnation: self.identity.lease_reset_incarnation,
                generation_mtime_ns: self.identity.generation_mtime_ns,
                ledger_user_msg_id: self.identity.ledger_user_msg_id,
            },
            range,
            terminal_anchor_msg_id,
            delivered_body,
        )
    }
}

pub(in crate::services::discord) fn advance_watcher_terminal_delivery(
    target: WatcherDeliveryTarget<'_>,
    identity: WatcherDeliveryIdentity,
    end: u64,
) -> GuardedWatcherDeliveryResult {
    let Some(mutation) = begin_watcher_delivery_mutation(
        target.shared,
        target.channel_id,
        target.tmux_session_name,
        identity,
    ) else {
        return GuardedWatcherDeliveryResult::LandedStale;
    };
    if mutation.advance(
        target,
        end,
        "src/services/discord/tmux_watcher/terminal_long_chunks.rs:guarded_watcher_advance_without_proof",
    ) {
        GuardedWatcherDeliveryResult::AdvancedWithoutProof
    } else {
        GuardedWatcherDeliveryResult::LandedStale
    }
}

/// Record only after watcher transport, lease commit, and in-memory advance all
/// succeed. The mutation guard prevents a reset from crossing the lease-time
/// identity snapshot and durable record mutation.
pub(in crate::services::discord) fn record_watcher_terminal_delivery(
    target: WatcherDeliveryTarget<'_>,
    identity: WatcherDeliveryIdentity,
    range: (u64, u64),
    last_chunk_anchor_msg_id: Option<u64>,
    delivered_body: &str,
) -> GuardedWatcherDeliveryResult {
    let Some(mutation) = begin_watcher_delivery_mutation(
        target.shared,
        target.channel_id,
        target.tmux_session_name,
        identity,
    ) else {
        tracing::warn!(
            provider = target.provider.as_str(),
            channel_id = target.channel_id.get(),
            range = ?range,
            "watcher frontier reset after delivery lease capture"
        );
        return GuardedWatcherDeliveryResult::LandedStale;
    };
    if !mutation.advance(
        target,
        range.1,
        "src/services/discord/tmux_watcher/terminal_long_chunks.rs:guarded_watcher_advance",
    ) {
        tracing::warn!(
            provider = target.provider.as_str(),
            channel_id = target.channel_id.get(),
            range = ?range,
            "watcher frontier changed before durable record"
        );
        return GuardedWatcherDeliveryResult::LandedStale;
    }
    if mutation.persist(target, range, last_chunk_anchor_msg_id, delivered_body) {
        GuardedWatcherDeliveryResult::Persisted
    } else {
        GuardedWatcherDeliveryResult::LandedUnrecorded
    }
}

/// #5071 T1 S3a: returns the guarded result rather than a bare bool so the caller
/// can both keep its legacy `frontier_committed` decision (via
/// [`legacy_watcher_delivery_committed`]) and journal the honest distinction
/// between a durably recorded delivery and a proof-less advance.
pub(in crate::services::discord) fn commit_legacy_watcher_delivery(
    target: WatcherDeliveryTarget<'_>,
    identity: WatcherDeliveryIdentity,
    range: (u64, u64),
    proof: Option<&WatcherTerminalDeliveryProof>,
) -> GuardedWatcherDeliveryResult {
    proof.map_or_else(
        || advance_watcher_terminal_delivery(target, identity, range.1),
        |proof| {
            record_watcher_terminal_delivery(
                target,
                identity,
                range,
                proof.anchor_msg_id.map(|anchor| anchor.get()),
                &proof.raw_body,
            )
        },
    )
}

/// The legacy frontier-committed predicate, unchanged in meaning: both a durable
/// record and a proof-less advance moved the watermark.
pub(in crate::services::discord) fn legacy_watcher_delivery_committed(
    result: GuardedWatcherDeliveryResult,
) -> bool {
    matches!(
        result,
        GuardedWatcherDeliveryResult::Persisted
            | GuardedWatcherDeliveryResult::AdvancedWithoutProof
    )
}

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
    delivered_body: &str,
    cell: &Arc<DeliveryLeaseCell>,
    turn: TurnKey,
    lease_key: Option<crate::services::discord::DeliveryLeaseKey>,
    instance_id: u64,
    source_authority: WatcherSourceAuthority,
    start: u64,
    end: u64,
) -> WatcherLongChunksResult {
    let delivery_identity = watcher_delivery_identity(
        source_authority.generation_mtime_ns,
        source_authority.reset_incarnation,
        lease_key.as_ref(),
    );
    let delivery_target = WatcherDeliveryTarget {
        shared,
        provider,
        channel_id,
        tmux_session_name,
    };
    let delivery_mutation = std::sync::Mutex::new(None);
    let landed_stale = std::sync::atomic::AtomicBool::new(false);
    let holder = LeaseHolder::Watcher { instance_id };
    cell.reclaim_if_expired(lease_now_ms());
    let heartbeat = WatcherPostHeartbeat { cell: cell.clone() };
    let advance = |range: (u64, u64)| -> bool {
        debug_assert_eq!(range, (start, end));
        let Some(mutation) = begin_watcher_delivery_mutation(
            shared,
            channel_id,
            tmux_session_name,
            delivery_identity,
        ) else {
            landed_stale.store(true, Ordering::Release);
            return true;
        };
        if !mutation.advance(
            delivery_target,
            end,
            "src/services/discord/tmux_watcher/terminal_long_chunks.rs:watcher_long_chunks_controller_advance",
        ) {
            landed_stale.store(true, Ordering::Release);
            return true;
        }
        *delivery_mutation
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(mutation);
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
        if landed_stale.load(Ordering::Acquire) {
            return WatcherLongChunksResult::LandedStale;
        }
        let mutation = delivery_mutation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let persisted = mutation.is_some_and(|mutation| {
            mutation.persist(
                delivery_target,
                (start, end),
                chunks.tail_message_id.map(|m| m.get()),
                delivered_body,
            )
        });
        if !persisted {
            return WatcherLongChunksResult::LandedUnrecorded;
        }
    }
    WatcherLongChunksResult::Outcome(outcome)
}

pub(in crate::services::discord) enum WatcherLongChunksResult {
    Outcome(toc::DeliveryOutcome),
    LandedStale,
    LandedUnrecorded,
}

pub(super) fn remember_ordered_long_chunks_footer_target(
    enabled: bool,
    target: &mut Option<super::WatcherCompletionFooterTerminalTarget>,
    tail_message_id: Option<MessageId>,
    relay_text: &str,
) {
    let Some(tail_message_id) = tail_message_id else {
        return;
    };
    let tail = crate::services::discord::formatting::split_message(relay_text)
        .pop()
        .unwrap_or_else(|| relay_text.to_string());
    super::remember_watcher_completion_footer_terminal_target(
        enabled,
        target,
        tail_message_id,
        &tail,
    );
}

pub(in crate::services::discord) struct WatcherLongChunksLocals<'a> {
    pub(in crate::services::discord) relay_ok: &'a mut bool,
    pub(in crate::services::discord) direct_send_delivered: &'a mut bool,
    pub(in crate::services::discord) tui_direct_anchor_terminal_body_visible: &'a mut bool,
    pub(in crate::services::discord) external_input_lease_consumed_by_relay: &'a mut bool,
    pub(in crate::services::discord) placeholder_msg_id: &'a mut Option<MessageId>,
    pub(in crate::services::discord) placeholder_from_restored_inflight: &'a mut bool,
    pub(in crate::services::discord) last_edit_text: &'a mut String,
    pub(in crate::services::discord) single_message_panel_footer_mode: bool,
    pub(in crate::services::discord) completion_footer_terminal_target:
        &'a mut Option<super::WatcherCompletionFooterTerminalTarget>,
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
    delivered_body: &str,
    cell: &Arc<DeliveryLeaseCell>,
    turn: TurnKey,
    lease_key: Option<crate::services::discord::DeliveryLeaseKey>,
    instance_id: u64,
    source_authority: WatcherSourceAuthority,
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
        delivered_body,
        cell,
        turn,
        lease_key,
        instance_id,
        source_authority,
        range.0,
        range.1,
    )
    .await;
    if let WatcherLongChunksResult::Outcome(outcome) = outcome {
        apply_watcher_long_chunks_result(
            outcome,
            http,
            shared,
            provider,
            channel_id,
            tmux_session_name,
            msg_id,
            relay_text,
            session_bound_fallback_uses_full_body,
            frozen_rollover_msg_ids,
            inflight_before_relay,
            locals,
        )
        .await;
    }
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
    // #5071 T1 S3a: the receipt for the same chunk `watcher_long_chunk_anchor_msg_id`
    // names, so the journal's `T` carries Discord's returned channel.
    watcher_long_chunk_anchor_receipt: &mut Option<
        crate::services::discord::outbound::DiscordTransportReceipt,
    >,
    locals: WatcherLongChunksLocals<'_>,
) {
    match crate::services::discord::formatting::send_long_message_raw_with_rollback_returning_receipts(
        http, channel_id, msg_id, relay_text, shared,
    )
    .await
    .and_then(|receipts| {
        crate::services::discord::formatting::message_ids_from_receipts(receipts.clone())
            .map(|message_ids| (message_ids, receipts))
    }) {
        Ok((message_ids, receipts)) => {
            *watcher_long_chunk_anchor_receipt = receipts.last().cloned();
            *locals.direct_send_delivered = true;
            *locals.tui_direct_anchor_terminal_body_visible = true;
            *locals.external_input_lease_consumed_by_relay =
                super::watcher_inflight_represents_external_input(inflight_before_relay);
            *watcher_long_chunk_anchor_msg_id = message_ids.last().copied();
            remember_ordered_long_chunks_footer_target(
                locals.single_message_panel_footer_mode,
                locals.completion_footer_terminal_target,
                *watcher_long_chunk_anchor_msg_id,
                relay_text,
            );
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
            let error = error.to_string();
            let display_error =
                crate::services::discord::replace_outcome_policy::strip_watcher_send_failure_class_marker(
                    &error,
                );
            tracing::info!("  [{ts}] 👁 Failed to relay ordered terminal chunks: {display_error}");
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
    relay_text: &str,
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
            remember_ordered_long_chunks_footer_target(
                locals.single_message_panel_footer_mode,
                locals.completion_footer_terminal_target,
                chunks.tail_message_id,
                relay_text,
            );
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
