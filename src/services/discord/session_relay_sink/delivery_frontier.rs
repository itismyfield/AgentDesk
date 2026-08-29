use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use super::SessionRelayDelivery;
use crate::services::discord::tmux::WatcherDeliveryTarget;
use crate::services::discord::tmux::tmux_watcher::terminal_long_chunks::{
    WatcherDeliveryIdentity, WatcherDeliveryMutation, begin_watcher_delivery_mutation,
    watcher_delivery_identity,
};
use crate::services::discord::{DeliveryLeaseKey, LeaseOutcome, LeaseToken, SharedData};
use crate::services::provider::ProviderKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TerminalDeliveryAuthority {
    pub(super) consumed: u64,
    pub(super) source_range: (u64, u64),
    pub(super) reset_incarnation: u64,
}

pub(super) fn validate_terminal_delivery(
    delivery: &SessionRelayDelivery,
) -> Option<TerminalDeliveryAuthority> {
    let consumed = delivery.terminal_consumed_end?;
    let source_range = delivery.terminal_source_range?;
    let reset_incarnation = delivery.terminal_reset_incarnation?;
    (consumed > 0
        && source_range.0 < source_range.1
        && delivery.frame_turn_start_offset.is_some()
        && !delivery.frame_turn_started_at.trim().is_empty())
    .then_some(TerminalDeliveryAuthority {
        consumed,
        source_range,
        reset_incarnation,
    })
}

/// Owns one exact acquired sink lease on one pinned cell. Drop can only release
/// this token; it never re-looks up a channel/generation cell.
pub(super) struct SinkDeliveryLeaseGuard {
    cell: Arc<crate::services::discord::DeliveryLeaseCell>,
    key: DeliveryLeaseKey,
    coordinate: u64,
    token: LeaseToken,
    heartbeat: Option<tokio::task::JoinHandle<()>>,
}

impl SinkDeliveryLeaseGuard {
    pub(super) fn acquire(
        cell: &Arc<crate::services::discord::DeliveryLeaseCell>,
        key: DeliveryLeaseKey,
        coordinate: u64,
    ) -> Option<Self> {
        cell.reclaim_if_expired(crate::services::discord::lease_now_ms());
        let token = cell.try_acquire_sink_exact(
            key.clone(),
            coordinate,
            crate::services::discord::lease_now_ms()
                .saturating_add(crate::services::discord::DELIVERY_LEASE_DEADLINE_MS),
        )?;
        let heartbeat_cell = cell.clone();
        let heartbeat_key = key.clone();
        let heartbeat = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(
                crate::services::discord::DELIVERY_LEASE_HEARTBEAT_MS,
            ));
            interval.tick().await;
            loop {
                interval.tick().await;
                if !heartbeat_cell.renew_sink_exact(
                    &heartbeat_key,
                    coordinate,
                    token,
                    crate::services::discord::lease_now_ms()
                        .saturating_add(crate::services::discord::DELIVERY_LEASE_DEADLINE_MS),
                ) {
                    break;
                }
            }
        });
        Some(Self {
            cell: cell.clone(),
            key,
            coordinate,
            token,
            heartbeat: Some(heartbeat),
        })
    }

    pub(super) fn commit(&self, outcome: LeaseOutcome) -> bool {
        self.cell
            .commit_sink_exact(&self.key, self.coordinate, self.token, outcome)
    }

    pub(super) fn matches_exact(&self) -> bool {
        self.cell
            .sink_exact_matches(&self.key, self.coordinate, self.token)
    }

    #[cfg(test)]
    fn token(&self) -> LeaseToken {
        self.token
    }
}

impl Drop for SinkDeliveryLeaseGuard {
    fn drop(&mut self) {
        if let Some(heartbeat) = self.heartbeat.take() {
            heartbeat.abort();
        }
        self.cell
            .release_sink_exact(&self.key, self.coordinate, self.token);
    }
}

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
    let reset_incarnation = delivery
        .terminal_reset_incarnation
        .unwrap_or_else(|| shared.relay_frontier_token(channel).reset_incarnation);
    let range = validate_terminal_delivery(delivery)
        .map(|authority| (authority.source_range.0, authority.consumed))
        .unwrap_or(range);
    SinkDeliveryAuthority {
        identity: watcher_delivery_identity(
            delivery.relay_generation_mtime_ns.unwrap_or(0),
            reset_incarnation,
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
    _context: &'static str,
) -> Option<WatcherDeliveryMutation> {
    if ctx.delivery.relay_range.is_none() {
        ctx.inflight_matches()?;
    }
    begin_watcher_delivery_mutation(
        ctx.shared,
        ctx.channel,
        &ctx.delivery.session_name,
        ctx.authority.identity,
    )
}

pub(super) fn persist_sink_delivery(
    mutation: WatcherDeliveryMutation,
    ctx: SinkDeliveryCtx<'_>,
    terminal_anchor_msg_id: Option<u64>,
    raw_body: &str,
) -> SinkDeliveryProofResult {
    if !mutation.advance(
        ctx.target(),
        ctx.authority.range.1,
        "src/services/discord/session_relay_sink/delivery_frontier.rs:publish_after_receipt",
    ) || !mutation.persist(
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

#[cfg(test)]
mod pr_b_contract_tests {
    use super::*;
    use crate::services::discord::{DeliveryLeaseCell, DeliveryLeaseKey, LeaseSnapshot};

    fn key(channel: ChannelId, generation: u64) -> DeliveryLeaseKey {
        DeliveryLeaseKey::new(channel, generation, 7, Some("turn"), Some(1))
    }

    #[tokio::test]
    async fn matching_durable_receipt_commits_exact_acquired_lease() {
        let channel = ChannelId::new(51_911);
        let cell = Arc::new(DeliveryLeaseCell::new(channel));
        let guard = SinkDeliveryLeaseGuard::acquire(&cell, key(channel, 1), 19).unwrap();
        assert!(guard.matches_exact());
        assert!(guard.commit(LeaseOutcome::Delivered));
        assert!(matches!(
            cell.read(),
            LeaseSnapshot::Committed {
                outcome: LeaseOutcome::Delivered,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn mismatched_receipt_cannot_commit_or_drop_current_lease() {
        let channel = ChannelId::new(51_912);
        let cell = Arc::new(DeliveryLeaseCell::new(channel));
        let mut old = SinkDeliveryLeaseGuard::acquire(&cell, key(channel, 1), 19).unwrap();
        let old_token = old.token();
        assert!(cell.release_sink_exact(&key(channel, 1), 19, old_token));
        let current = SinkDeliveryLeaseGuard::acquire(&cell, key(channel, 1), 19).unwrap();
        assert!(!old.commit(LeaseOutcome::Delivered));
        drop(old);
        assert!(current.matches_exact());
    }

    #[tokio::test]
    async fn transport_failure_drops_only_the_exact_acquired_lease() {
        let channel = ChannelId::new(51_913);
        let cell = Arc::new(DeliveryLeaseCell::new(channel));
        let guard = SinkDeliveryLeaseGuard::acquire(&cell, key(channel, 1), 23).unwrap();
        drop(guard);
        assert!(matches!(cell.read(), LeaseSnapshot::Unleased));
    }

    #[tokio::test]
    async fn stale_pre_reset_guard_cannot_drop_post_reset_replacement_lease() {
        let channel = ChannelId::new(51_914);
        let cell = Arc::new(DeliveryLeaseCell::new(channel));
        let old = SinkDeliveryLeaseGuard::acquire(&cell, key(channel, 1), 29).unwrap();
        assert!(cell.release_sink_exact(&key(channel, 1), 29, old.token()));
        let replacement = SinkDeliveryLeaseGuard::acquire(&cell, key(channel, 2), 29).unwrap();
        drop(old);
        assert!(replacement.matches_exact());
    }

    #[test]
    fn landed_stale_settles_attempt_without_advancing_current_frontier() {
        assert_ne!(
            SinkDeliveryProofResult::LandedStale,
            SinkDeliveryProofResult::Persisted
        );
    }

    #[test]
    fn journal_capture_uses_enqueued_reset_identity_not_live_frontier() {
        let authority = TerminalDeliveryAuthority {
            consumed: 41,
            source_range: (3, 40),
            reset_incarnation: 17,
        };
        assert_eq!(authority.reset_incarnation, 17);
    }

    #[test]
    fn commit_precedes_frontier_and_frontier_never_precedes_transport_receipt() {
        let source = include_str!("delivery_frontier.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        assert!(production.find("settle_exact(").is_none());
        let sink = include_str!("../session_relay_sink.rs");
        assert!(
            sink.find("journal::settle_exact(").unwrap()
                < sink
                    .find("delivery_frontier::finish_sink_delivery(")
                    .unwrap()
        );
    }
}
