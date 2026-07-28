use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serenity::model::id::{ChannelId, MessageId};

use super::{
    SessionRelayDelivery, SessionRelayDeliveryOutcome, SessionRelayTraceContext, SinkPostHeartbeat,
    delivery_frontier, delivery_outcome_classify,
};
use crate::services::cluster::stream_relay::RelaySinkError;
use crate::services::discord::inflight::RelayOwnerKind;
use crate::services::discord::outbound::turn_output_controller as toc;
use crate::services::discord::placeholder_controller::{PlaceholderKey, PlaceholderLifecycle};
use crate::services::provider::ProviderKind;

/// Every input one cut-over short-replace delivery needs. Bundled so the entry
/// point stays within the argument-count ratchet instead of carrying an `allow`.
pub(super) struct SinkShortReplaceCtx<'a> {
    pub(super) shared: &'a Arc<crate::services::discord::SharedData>,
    pub(super) provider: &'a ProviderKind,
    pub(super) channel: ChannelId,
    pub(super) channel_id: u64,
    pub(super) msg_id: MessageId,
    pub(super) relay_text: &'a str,
    pub(super) delivered_fingerprint_body: &'a str,
    pub(super) delivery: &'a SessionRelayDelivery,
    pub(super) sink_lease_key: crate::services::discord::DeliveryLeaseKey,
    pub(super) sink_delivery_authority: delivery_frontier::SinkDeliveryAuthority,
    pub(super) trace: &'a SessionRelayTraceContext,
    pub(super) range: (u64, u64),
    pub(super) delivered_total: &'a AtomicU64,
}

pub(super) async fn deliver_short_replace_via_controller<
    G: crate::services::discord::gateway::TurnGateway + ?Sized,
>(
    gateway: &G,
    ctx: SinkShortReplaceCtx<'_>,
) -> Result<SessionRelayDeliveryOutcome, RelaySinkError> {
    let SinkShortReplaceCtx {
        shared,
        provider,
        channel,
        channel_id,
        msg_id,
        relay_text,
        delivered_fingerprint_body,
        delivery,
        sink_lease_key,
        sink_delivery_authority,
        trace,
        range: (start, end),
        delivered_total,
    } = ctx;
    let cell = shared.delivery_lease(channel);
    cell.reclaim_if_expired(crate::services::discord::lease_now_ms());
    let heartbeat = SinkPostHeartbeat { cell: cell.clone() };
    let sink_delivery_ctx = delivery_frontier::SinkDeliveryCtx {
        shared,
        provider,
        channel,
        delivery,
        authority: sink_delivery_authority,
    };
    let delivery_mutation = Mutex::new(None);
    let landed_stale = AtomicBool::new(false);
    let advance = |_range: (u64, u64)| -> bool {
        let Some(mutation) = delivery_frontier::begin_sink_delivery_mutation(
            sink_delivery_ctx,
            "src/services/discord/session_relay_sink/short_controller.rs:sink_short_controller_advance",
        ) else {
            landed_stale.store(true, Ordering::Release);
            // The POST landed. Settle Delivered so stale source work is never retried.
            return true;
        };
        *delivery_mutation
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(mutation);
        true
    };
    let outcome = toc::deliver_turn_output(
        gateway,
        toc::TurnOutputCtx {
            turn: crate::services::discord::turn_finalizer::TurnKey::new(
                channel,
                delivery.frame_turn_user_msg_id,
                shared.restart.current_generation,
            ),
            lease_key: Some(sink_lease_key),
            owner: RelayOwnerKind::SessionBoundRelay,
            holder: crate::services::discord::LeaseHolder::Sink,
            lease: &*cell,
            channel_id: channel,
            placeholder_controller: &shared.ui.placeholder_controller,
            placeholder: toc::PlaceholderSlot::Active {
                message_id: msg_id,
                key: PlaceholderKey {
                    provider: provider.clone(),
                    channel_id: channel,
                    message_id: msg_id,
                },
            },
            body: relay_text,
            send_range: (start, end),
            plan: toc::OutputPlan::Replace {
                lifecycle: PlaceholderLifecycle::Active,
            },
            edit_fail_policy: toc::EditFailPlaceholderPolicy::PreserveAlways,
            fallback_commit_policy: toc::FallbackCommitPolicy::CommitOnFallback,
            acquire_failure_mode: toc::AcquireFailureMode::Transient,
            advance: Some(&advance),
            heartbeat: Some(&heartbeat),
        },
    )
    .await;

    match outcome {
        toc::DeliveryOutcome::Delivered { replace_kind, .. } => {
            if landed_stale.load(Ordering::Acquire) {
                return Ok(SessionRelayDeliveryOutcome::LandedStale);
            }
            let anchor = match replace_kind {
                Some(toc::ReplaceDeliveryKind::FreshFallbackAfterEditFailure {
                    replacement_anchor,
                    ..
                }) => replacement_anchor.map(|anchor| anchor.get()),
                _ => Some(msg_id.get()),
            };
            let Some(mutation) = delivery_mutation
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
            else {
                return Ok(SessionRelayDeliveryOutcome::LandedUnrecorded);
            };
            if delivery_frontier::persist_sink_delivery(
                mutation,
                sink_delivery_ctx,
                anchor,
                delivered_fingerprint_body,
            ) != delivery_frontier::SinkDeliveryProofResult::Persisted
            {
                return Ok(SessionRelayDeliveryOutcome::LandedUnrecorded);
            }
            delivered_total.fetch_add(1, Ordering::AcqRel);
            tracing::info!(
                provider = provider.as_str(),
                channel_id,
                message = msg_id.get(),
                tmux_session = %delivery.session_name,
                turn_id = trace.turn_id().unwrap_or(""),
                dispatch_id = trace.dispatch_id().unwrap_or(""),
                session_key = trace.session_key().unwrap_or(""),
                relay_owner = trace.relay_owner(),
                runtime_kind = trace.runtime_kind(),
                chars = relay_text.chars().count(),
                "session-bound relay sink delivered terminal response via placeholder edit (controller #3089 A2b)"
            );
            crate::services::observability::emit_relay_delivery(
                provider.as_str(),
                channel_id,
                trace.dispatch_id(),
                trace.session_key(),
                trace.turn_id(),
                Some(msg_id.get()),
                "session_relay_sink",
                "edit",
                None,
                None,
                true,
                Some("placeholder edit (controller)"),
            );
            Ok(SessionRelayDeliveryOutcome::Delivered)
        }
        toc::DeliveryOutcome::NotDelivered { .. } => {
            Ok(SessionRelayDeliveryOutcome::LandedUnrecorded)
        }
        toc::DeliveryOutcome::FreshDelivered {
            committed_to,
            persistence_recorded,
        } => Ok(SessionRelayDeliveryOutcome::FreshDelivered {
            committed_to,
            persistence_recorded,
        }),
        non_delivery @ (toc::DeliveryOutcome::Transient { .. }
        | toc::DeliveryOutcome::Unknown { .. }
        | toc::DeliveryOutcome::Skipped) => Err(
            delivery_outcome_classify::short_replace_non_delivery_error(&non_delivery),
        ),
    }
}
