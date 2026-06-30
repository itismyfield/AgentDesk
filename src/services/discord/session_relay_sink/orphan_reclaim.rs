//! #3960 — idle-JSONL-relay gate that reclaims an orphaned `SessionBoundRelay`
//! TUI-direct row whose claim-time producer has died (the #3876 residual
//! deferred from PR #3953).
//!
//! The #3876 producer-gate stamps `SessionBoundRelay` only when a live
//! per-session relay producer exists AT CLAIM time. The producer can die in the
//! window before the terminal commit/ACK; the row then stays owned by
//! `SessionBoundRelay`, so the ownerless staleness reclaim never fires and the
//! TUI-direct answer black-holes. This gate RE-CHECKS producer liveness AND the
//! generation-aware committed-offset delivery authority at the idle-relay tick
//! (not just at claim) and, when the body was provably never delivered,
//! downgrades the orphaned owner to the bridge-adapter backstop (`None`) under
//! the inflight flock — re-joining the row to ownerless recovery, double-relay-safe.

use crate::services::cluster::relay_producer_registry::RelayProducerRegistry;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::inflight::{self, InflightTurnIdentity, InflightTurnState};
use crate::services::discord::outbound::delivery_record::effective_committed_offset;
use crate::services::provider::ProviderKind;
use serenity::model::id::ChannelId;

/// #3960 — pure reclaim decision for an orphaned `SessionBoundRelay` TUI-direct
/// row. ALL THREE conditions must hold before the owner is downgraded to the
/// bridge-adapter backstop:
///   1. `orphan_shape` — the row is a quiescent, stale, uncommitted
///      `SessionBoundRelay` TUI-direct claim (see
///      `inflight::orphan_relay_reclaim::session_bound_relay_external_input_orphan_shape`).
///   2. `producer_gone` — the claim-time relay producer is no longer registered
///      (the #3876 `global_relay_producer_registry` liveness, RE-CHECKED here at
///      commit time rather than only at claim time). A live producer that
///      survives keeps delivering normally → `producer_gone == false` → no
///      reclaim (no false reclaim).
///   3. `committed_offset <= turn_floor` — the generation-aware committed-offset
///      authority covers NOTHING of this turn's body, proving the terminal body
///      was never delivered. This is the double-relay guard: a
///      delivered-but-unmirrored row (#2415) has `committed > turn_floor` and is
///      never reclaimed.
pub(super) fn should_reclaim_orphaned_session_bound_relay(
    orphan_shape: bool,
    producer_gone: bool,
    committed_offset: u64,
    turn_floor: u64,
) -> bool {
    orphan_shape && producer_gone && committed_offset <= turn_floor
}

/// #3960 — reclaim an orphaned `SessionBoundRelay` TUI-direct row whose
/// claim-time producer has died. Returns `true` iff the orphaned owner was
/// downgraded to the bridge-adapter backstop (`None`).
///
/// The cheap row-shape gate runs first (the common live-row case returns
/// immediately without touching the producer registry or the delivery
/// authority). Only a stale orphan-shaped row consults the (re-checked) producer
/// liveness and the committed-offset authority, and only when both prove the
/// body was never delivered does it perform the flock-guarded owner downgrade —
/// whose in-lock re-check is the final claim→commit TOCTOU closure.
pub(super) async fn reclaim_orphaned_session_bound_relay_if_dead(
    health_registry: &HealthRegistry,
    producers: &RelayProducerRegistry,
    provider: &ProviderKind,
    channel_id: u64,
    session_name: &str,
    inflight: &InflightTurnState,
) -> bool {
    if !inflight::session_bound_relay_external_input_orphan_shape(inflight) {
        return false;
    }
    // #3876 producer-liveness, re-checked at THIS tick: a live producer (the
    // original or a replacement) still owns delivery → never reclaim.
    let producer_gone = producers.get_producer(session_name).is_none();
    if !producer_gone {
        return false;
    }
    let channel = ChannelId::new(channel_id);
    let Some(shared) = health_registry
        .shared_for_provider_on_channel(provider, channel)
        .await
        .or(health_registry.shared_for_provider(provider).await)
    else {
        return false;
    };
    let committed = effective_committed_offset(&shared, provider, channel, session_name);
    let turn_floor = inflight.turn_start_offset.unwrap_or(inflight.last_offset);
    if !should_reclaim_orphaned_session_bound_relay(true, producer_gone, committed, turn_floor) {
        return false;
    }
    matches!(
        inflight::downgrade_orphaned_session_bound_relay_owner_locked(
            provider,
            channel_id,
            &InflightTurnIdentity::from_state(inflight),
            session_name,
        ),
        inflight::OrphanRelayReclaimOutcome::Downgraded
    )
}

#[cfg(test)]
mod tests {
    use super::should_reclaim_orphaned_session_bound_relay as decide;

    #[test]
    fn producer_dies_before_commit_with_undelivered_body_is_reclaimed() {
        // Orphan shape + producer gone + committed authority covers nothing of
        // the body (committed <= turn_floor) → reclaim.
        assert!(decide(true, true, 0, 0));
        assert!(decide(true, true, 5, 10));
        assert!(decide(true, true, 10, 10)); // boundary: nothing PAST the start
    }

    #[test]
    fn delivered_body_is_never_reclaimed_no_double_relay() {
        // committed offset advanced PAST the turn floor → the body was (at least
        // partially) delivered → never reclaim, never double-relay. Covers the
        // #2415 delivered-but-unmirrored row.
        assert!(!decide(true, true, 11, 10));
        assert!(!decide(true, true, u64::MAX, 0));
    }

    #[test]
    fn live_producer_that_survives_is_not_falsely_reclaimed() {
        // A live producer still owns delivery → never reclaim regardless of the
        // (irrelevant) offsets.
        assert!(!decide(true, false, 0, 10));
        assert!(!decide(true, false, 0, 0));
    }

    #[test]
    fn non_orphan_shape_is_never_reclaimed() {
        assert!(!decide(false, true, 0, 10));
        assert!(!decide(false, false, 0, 10));
    }
}
