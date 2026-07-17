//! Anchor-less fresh-send verb for the turn-output controller (#4046 S1r-1).

use poise::serenity_prelude::MessageId;

use super::{ControllerLeaseGuard, DeliveryLease, DeliveryOutcome, OutputPlan, TurnOutputCtx};
use crate::services::discord::gateway::TurnGateway;
use crate::services::discord::outbound::delivery_record;
use crate::services::discord::{LeaseOutcome, lease_now_ms};

/// The concrete fresh-send inputs live on the verb so later owner cutovers cannot
/// accidentally omit the durable generation/fingerprint authority.
#[derive(Clone)]
pub(in crate::services::discord) struct RecordContext {
    pub(in crate::services::discord) provider: crate::services::provider::ProviderKind,
    pub(in crate::services::discord) record_channel_id: poise::serenity_prelude::ChannelId,
    pub(in crate::services::discord) tmux_session_name: String,
    pub(in crate::services::discord) attempts: u32,
}

pub(super) async fn deliver<G, L>(gateway: &G, ctx: TurnOutputCtx<'_, L>) -> DeliveryOutcome
where
    G: TurnGateway + ?Sized,
    L: DeliveryLease + ?Sized,
{
    let OutputPlan::SendFresh {
        range,
        reference,
        record,
    } = &ctx.plan
    else {
        unreachable!("fresh-send child received a different output plan");
    };
    debug_assert!(reference.is_none(), "S1r-1 fresh-send is anchor-less");
    if record.record_channel_id != ctx.channel_id {
        tracing::warn!(
            post_channel_id = ctx.channel_id.get(),
            record_channel_id = record.record_channel_id.get(),
            "fresh-send refused mismatched POST and record channels"
        );
        return DeliveryOutcome::Skipped;
    }
    if reference.is_some() || ctx.body.is_empty() {
        return DeliveryOutcome::Skipped;
    }

    let Some(key) = ctx.lease_key.as_ref() else {
        tracing::warn!(
            channel_id = ctx.channel_id.get(),
            "fresh-send refused without a delivery-lease key"
        );
        return DeliveryOutcome::Transient {
            retry_from_offset: range.map_or(ctx.send_range.0, |value| value.0),
        };
    };

    let (start, end) = match range {
        Some((start, end)) if end > start => (*start, *end),
        Some((start, _)) => {
            tracing::warn!(
                channel_id = ctx.channel_id.get(),
                range = ?range,
                "fresh-send refused an empty durable range"
            );
            return DeliveryOutcome::Transient {
                retry_from_offset: *start,
            };
        }
        None => pseudo_range(ctx.send_range.0, ctx.body),
    };

    if range.is_none()
        && delivery_record::recent_fresh_send_content_matches(
            &record.provider,
            record.record_channel_id,
            &record.tmux_session_name,
            ctx.body,
        )
    {
        tracing::warn!(
            provider = record.provider.as_str(),
            channel_id = ctx.channel_id.get(),
            body_len = ctx.body.len(),
            "fresh-send suppressed a NoRange duplicate by content fingerprint"
        );
        return DeliveryOutcome::Skipped;
    }

    // D1 parity: reclaim an expired holder before trying the real/pseudo range.
    ctx.lease.reclaim_if_expired(lease_now_ms());
    let deadline_ms = lease_now_ms().saturating_add(super::TURN_OUTPUT_LEASE_TTL_MS);
    if !ctx
        .lease
        .try_acquire(key.clone(), ctx.holder, start, end, deadline_ms)
    {
        return DeliveryOutcome::Transient {
            retry_from_offset: start,
        };
    }
    let mut lease_guard = ControllerLeaseGuard::arm(ctx.lease, ctx.holder, key.clone(), start, end);
    let heartbeat_guard = ctx
        .heartbeat
        .map(|heartbeat| heartbeat.start(ctx.holder, key.clone()));

    let sent = gateway.send_message(ctx.channel_id, ctx.body).await;
    drop(heartbeat_guard);
    let message_id = match sent {
        Ok(message_id) => message_id,
        Err(error) => {
            tracing::warn!(
                channel_id = ctx.channel_id.get(),
                error = %error,
                "fresh-send transport failed"
            );
            lease_guard.release_and_disarm();
            return DeliveryOutcome::Unknown { fell_back: false };
        }
    };

    // A real range owns offset authority. NoRange is deliver-without-advance:
    // its pseudo-range exists only until POST + retry-fingerprint persistence
    // complete, and must never be presented to the owner's advance callback.
    let Some(range) = *range else {
        let persistence_recorded = record_success(record, None, message_id, ctx.body);
        lease_guard.release_and_disarm();
        return DeliveryOutcome::FreshDelivered {
            committed_to: None,
            persistence_recorded,
        };
    };

    // I1: advance, lease commit, durable record, then release. There is no await
    // after the transport before this whole authority sequence completes.
    let advanced = ctx.advance.is_none_or(|advance| advance(range));
    let lease_outcome = if advanced {
        LeaseOutcome::Delivered
    } else {
        LeaseOutcome::NotDelivered
    };
    let committed = ctx
        .lease
        .commit(ctx.holder, key.clone(), start, end, lease_outcome);
    debug_assert!(committed, "fresh-send commit must match its acquired lease");

    let persistence_recorded =
        advanced && committed && record_success(record, Some(range), message_id, ctx.body);
    lease_guard.release_and_disarm();

    if advanced && !committed {
        return DeliveryOutcome::Unknown { fell_back: false };
    }
    if advanced {
        DeliveryOutcome::FreshDelivered {
            committed_to: Some(range.1),
            persistence_recorded,
        }
    } else {
        DeliveryOutcome::NotDelivered {
            committed_from: range.0,
        }
    }
}

pub(super) fn pseudo_range(start_hint: u64, body: &str) -> (u64, u64) {
    let width = u64::try_from(body.len().max(1)).unwrap_or(u64::MAX);
    (start_hint, start_hint.saturating_add(width))
}

fn record_success(
    record: &RecordContext,
    range: Option<(u64, u64)>,
    message_id: MessageId,
    body: &str,
) -> bool {
    let generation_mtime_ns =
        delivery_record::current_generation_mtime_ns(&record.tmux_session_name);
    if generation_mtime_ns == 0 {
        tracing::warn!(
            provider = record.provider.as_str(),
            channel_id = record.record_channel_id.get(),
            tmux_session_name = record.tmux_session_name.as_str(),
            "fresh-send delivered without a readable generation marker"
        );
        return false;
    }

    let result = match range {
        Some(range) => {
            let commit = delivery_record::DeliveredCommit {
                range,
                generation_mtime_ns,
                attempts: record.attempts,
                panel_msg_id: Some(message_id.get()),
                panel_channel_id: Some(record.record_channel_id.get()),
            };
            delivery_record::write_delivered_frontier(
                &record.provider,
                record.record_channel_id.get(),
                commit,
            )
        }
        None => {
            // F3: NoRange never claims frontier or watcher suppression authority.
            // Its generation-scoped fingerprint is isolated retry metadata; the
            // pseudo-range remains process-local lease mutual exclusion only.
            delivery_record::record_fresh_send_content_fingerprint(
                &record.provider,
                record.record_channel_id.get(),
                body,
                generation_mtime_ns,
            )
        }
    };

    match result {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                provider = record.provider.as_str(),
                channel_id = record.record_channel_id.get(),
                error = %error,
                range = ?range,
                "fresh-send persistence write failed"
            );
            false
        }
    }
}
