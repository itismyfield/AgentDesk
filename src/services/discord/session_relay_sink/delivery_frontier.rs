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
        .or_else(|| cancellation_episode(self.shared, self.delivery))
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

pub(super) fn current_inflight_matches(
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
    if let Some(original) = cancellation_episode(ctx.shared, ctx.delivery) {
        // `mutation` continues holding the existing reset-incarnation guard
        // while the confirmed receipt is persisted; never recreate the row.
        return persist_cancelled_episode(ctx, &original, terminal_anchor_msg_id, raw_body);
    }
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

/// Reuse a cancelled reader's original episode only for the exact source/fenced
/// frame. The existing sink lease still serializes every actual transport.
fn cancellation_episode(
    shared: &SharedData,
    delivery: &SessionRelayDelivery,
) -> Option<crate::services::discord::InflightTurnState> {
    let captured = crate::services::discord::tmux::tmux_watcher::cancel_handoff::recorded_episode(
        shared,
        &delivery.provider,
        ChannelId::new(delivery.channel_id),
        &delivery.session_name,
    )?;
    let source_matches = captured.matches_source(
        delivery.relay_generation_mtime_ns,
        delivery.relay_source_stamp,
    );
    let row = captured.original;
    let end = delivery.terminal_consumed_end?;
    (delivery.frame_turn_user_msg_id == row.user_msg_id
        && delivery.frame_turn_started_at == row.started_at
        && delivery.frame_turn_start_offset == row.turn_start_offset
        && row.turn_start_offset.is_some_and(|start| end > start)
        && source_matches
        && row
            .output_path
            .as_ref()
            .is_some_and(|path| std::fs::metadata(path).is_ok_and(|meta| meta.len() >= end)))
    .then_some(row)
}

impl super::SessionBoundDiscordRelaySink {
    pub(super) async fn cancelled_episode_is_retained(
        &self,
        delivery: &SessionRelayDelivery,
    ) -> bool {
        self.health_registry
            .shared_for_provider(&delivery.provider)
            .await
            .is_some_and(|shared| cancellation_episode(&shared, delivery).is_some())
    }
}

fn persist_cancelled_episode(
    ctx: SinkDeliveryCtx<'_>,
    original: &crate::services::discord::InflightTurnState,
    anchor: Option<u64>,
    body: &str,
) -> SinkDeliveryProofResult {
    use crate::services::discord::outbound::delivery_record as records;
    let Some(anchor) = anchor.filter(|id| *id != 0) else {
        return SinkDeliveryProofResult::LandedUnrecorded;
    };
    let source = records::ExactJsonlSourceIdentity {
        provider: ctx.provider.as_str().into(),
        tmux_session_name: ctx.delivery.session_name.clone(),
        turn_nonce: original.turn_nonce.clone().unwrap_or_default(),
        range: ctx.authority.range,
        generation_mtime_ns: ctx.authority.identity.generation_mtime_ns,
        offset_authority_channel_id: ctx.channel.get(),
        delivery_channel_id: ctx.channel.get(),
    };
    if records::record_current_pinned_delivery(&source, anchor).is_err() {
        return SinkDeliveryProofResult::LandedUnrecorded;
    }
    records::record_pinned_delivery_metadata(&source, body, original.effective_finalizer_turn_id());
    SinkDeliveryProofResult::Persisted
}
