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
        && delivery_record::recent_delivered_content_matches(
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

    // I1: advance, lease commit, durable record, then release. There is no await
    // after the transport before this whole authority sequence completes.
    let advanced = ctx.advance.is_none_or(|advance| advance((start, end)));
    let lease_outcome = if advanced {
        LeaseOutcome::Delivered
    } else {
        LeaseOutcome::NotDelivered
    };
    let committed = ctx
        .lease
        .commit(ctx.holder, key.clone(), start, end, lease_outcome);
    debug_assert!(committed, "fresh-send commit must match its acquired lease");

    if advanced && committed {
        record_success(record, *range, message_id, ctx.body);
    }
    lease_guard.release_and_disarm();

    if advanced && !committed {
        return DeliveryOutcome::Unknown { fell_back: false };
    }
    if advanced {
        DeliveryOutcome::Delivered {
            committed_to: range.map_or(ctx.send_range.0, |value| value.1),
            replace_kind: None,
            new_chunks: None,
        }
    } else {
        DeliveryOutcome::NotDelivered {
            committed_from: range.map_or(ctx.send_range.0, |value| value.0),
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
) {
    let generation_mtime_ns =
        delivery_record::current_generation_mtime_ns(&record.tmux_session_name);
    if generation_mtime_ns == 0 {
        tracing::warn!(
            provider = record.provider.as_str(),
            channel_id = record.record_channel_id.get(),
            tmux_session_name = record.tmux_session_name.as_str(),
            "fresh-send delivered without a readable generation marker"
        );
    }

    match range {
        Some(range) if generation_mtime_ns != 0 => {
            let commit = delivery_record::DeliveredCommit {
                range,
                generation_mtime_ns,
                attempts: record.attempts,
                panel_msg_id: Some(message_id.get()),
                panel_channel_id: Some(record.record_channel_id.get()),
            };
            if let Err(error) = delivery_record::write_delivered_frontier(
                &record.provider,
                record.record_channel_id.get(),
                commit,
            ) {
                tracing::warn!(
                    provider = record.provider.as_str(),
                    channel_id = record.record_channel_id.get(),
                    error = %error,
                    "fresh-send durable frontier write failed"
                );
            }
        }
        Some(_) => {}
        None if generation_mtime_ns != 0 => {
            // F3: NoRange cannot claim frontier coverage. Its durable dedup is the
            // #4081 content fingerprint; the pseudo-range is process-local lease
            // mutual exclusion only.
            delivery_record::record_delivered_content_fingerprint_for_generation(
                &record.provider,
                record.record_channel_id.get(),
                body,
                generation_mtime_ns,
            );
        }
        None => {
            // A generation-less fingerprint would collapse unrelated wrapper
            // generations onto mtime=0. Refuse that false cross-generation claim.
        }
    }
}
