use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use super::{SessionRelayDelivery, SinkDeliveryLeaseGuard};
use crate::services::discord::tmux::WatcherDeliveryTarget;
use crate::services::discord::tmux::tmux_watcher::terminal_long_chunks::{
    WatcherDeliveryIdentity, WatcherDeliveryMutation, begin_watcher_delivery_mutation,
    watcher_delivery_identity,
};
use crate::services::discord::{DeliveryLeaseKey, LeaseOutcome, SharedData};
use crate::services::provider::ProviderKind;

#[derive(Clone, Copy)]
pub(super) struct SinkDeliveryAuthority {
    identity: WatcherDeliveryIdentity,
    range: (u64, u64),
}

/// Everything one sink delivery epilogue is scoped to: where it posts, which
/// frame it carries, and the immutable source authority captured before
/// transport. Bundled so the epilogue helpers stay within the argument-count
/// ratchet instead of carrying an `allow`.
#[derive(Clone, Copy)]
pub(super) struct SinkDeliveryCtx<'a> {
    pub(super) shared: &'a Arc<SharedData>,
    pub(super) provider: &'a ProviderKind,
    pub(super) channel: ChannelId,
    pub(super) delivery: &'a SessionRelayDelivery,
    pub(super) authority: SinkDeliveryAuthority,
}

impl<'a> SinkDeliveryCtx<'a> {
    fn target(&self) -> WatcherDeliveryTarget<'a> {
        WatcherDeliveryTarget {
            shared: self.shared,
            provider: self.provider,
            channel_id: self.channel,
            tmux_session_name: &self.delivery.session_name,
        }
    }

    fn inflight_matches(&self) -> Option<crate::services::discord::InflightTurnState> {
        current_inflight_matches(
            self.provider,
            self.channel.get(),
            &self.delivery.session_name,
            self.delivery,
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SinkDeliveryProofResult {
    Persisted,
    LandedStale,
    LandedUnrecorded,
}

pub(super) fn capture_sink_delivery_authority(
    shared: &SharedData,
    channel: ChannelId,
    delivery: &SessionRelayDelivery,
    lease_key: &DeliveryLeaseKey,
    range: (u64, u64),
) -> SinkDeliveryAuthority {
    SinkDeliveryAuthority {
        identity: watcher_delivery_identity(
            delivery.relay_generation_mtime_ns.unwrap_or(0),
            shared.relay_frontier_token(channel).reset_incarnation,
            Some(lease_key),
        ),
        range,
    }
}

fn current_inflight_matches(
    provider: &ProviderKind,
    channel_id: u64,
    session_name: &str,
    delivery: &SessionRelayDelivery,
) -> Option<crate::services::discord::InflightTurnState> {
    let inflight = crate::services::discord::inflight::load_inflight_state(provider, channel_id)?;
    (inflight.user_msg_id == delivery.frame_turn_user_msg_id
        && inflight.started_at == delivery.frame_turn_started_at
        && delivery.frame_turn_start_offset.is_some()
        && inflight.turn_start_offset == delivery.frame_turn_start_offset
        && inflight.tmux_session_name.as_deref() == Some(session_name))
    .then_some(inflight)
}

pub(super) fn begin_sink_delivery_mutation(
    ctx: SinkDeliveryCtx<'_>,
    context: &'static str,
) -> Option<WatcherDeliveryMutation> {
    if ctx.delivery.relay_range.is_none() {
        ctx.inflight_matches()?;
    }
    let mutation = begin_watcher_delivery_mutation(
        ctx.shared,
        ctx.channel,
        &ctx.delivery.session_name,
        ctx.authority.identity,
    )?;
    mutation
        .advance(ctx.target(), ctx.authority.range.1, context)
        .then_some(mutation)
}

pub(super) fn persist_sink_delivery(
    mutation: WatcherDeliveryMutation,
    ctx: SinkDeliveryCtx<'_>,
    terminal_anchor_msg_id: Option<u64>,
    raw_body: &str,
) -> SinkDeliveryProofResult {
    if !mutation.persist(
        ctx.target(),
        ctx.authority.range,
        terminal_anchor_msg_id,
        raw_body,
    ) {
        return SinkDeliveryProofResult::LandedUnrecorded;
    }
    if let Some(inflight) = ctx.inflight_matches() {
        crate::services::discord::inflight::mark_session_bound_relay_delivered_locked(
            ctx.provider,
            ctx.channel.get(),
            &crate::services::discord::inflight::InflightTurnIdentity::from_state(&inflight),
            &ctx.delivery.session_name,
        );
    }
    SinkDeliveryProofResult::Persisted
}

pub(super) fn finish_sink_delivery(
    ctx: SinkDeliveryCtx<'_>,
    terminal_anchor_msg_id: Option<u64>,
    raw_body: &str,
    lease_guard: Option<&SinkDeliveryLeaseGuard>,
    context: &'static str,
) -> SinkDeliveryProofResult {
    let result = begin_sink_delivery_mutation(ctx, context)
        .map_or(SinkDeliveryProofResult::LandedStale, |mutation| {
            persist_sink_delivery(mutation, ctx, terminal_anchor_msg_id, raw_body)
        });
    if let Some(guard) = lease_guard {
        // The transport landed even when its source authority went stale. Commit
        // the lease as delivered so reconciliation never duplicates that POST.
        guard.commit(LeaseOutcome::Delivered);
    }
    result
}
