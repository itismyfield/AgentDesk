#[cfg(test)]
pub(in crate::services::discord) use crate::services::engine_runtime::relay_state::lease::payload_lock_entries_for_test;
pub(in crate::services::discord) use crate::services::engine_runtime::relay_state::lease::{
    DeliveryLeaseCell, LeaseHolder, LeaseOutcome, LeaseSnapshot, assert_payload_not_held,
    lease_now_ms,
};

use super::{ChannelId, DeliveryLeaseKey};
use std::sync::Arc;

impl super::SharedData {
    /// #3041 P1-1: the LIVE per-channel delivery-lease cell, created on first
    /// access alongside the relay coord. The watcher acquires/commits through
    /// this to make terminal delivery + offset advance a single-holder unit
    /// (§5.2). The returned `Arc` is shared across all watcher instances for the
    /// channel so a replacement watcher sees the live holder and skips the
    /// duplicate send (B2).
    pub(in crate::services::discord) fn delivery_lease(
        &self,
        channel_id: ChannelId,
    ) -> Arc<DeliveryLeaseCell> {
        self.tmux_relay_coord(channel_id).delivery_lease.clone()
    }
}

#[cfg(test)]
mod exact_lease;
#[cfg(unix)]
pub(super) mod source_epoch_observer;

// ===========================================================================
// #3041 §2-§3 — Delivery-lease `DeliveryLeaseCell` state machine.
//
// As of P1-1 the WATCHER terminal-delivery path wires this LIVE: the watcher
// acquires the cell before sending, heartbeat-renews it during the send, and
// commits+advances+releases INLINE. The `relay_slot` field above is LEFT
// UNTOUCHED for now (its guard migration is a later step). The SINK/BRIDGE
// committers (P1-2) and the 3-way ACK reconciliation (P1-3) are not wired yet,
// so the actor `CommitDelivery`/`ReleaseDelivery` messages and some helpers
// remain dormant — those still carry targeted `#[allow(dead_code)]` attributes
// tagged with this issue/phase, to be wired/removed by the follow-up phases.
//
// Design (faithful to #3041 §2-§3):
//   lease = (delivery_lease_key, byte_range [start,end))
//           → a "one-time terminal-delivery right".
//   The lease key is deliberately separate from the finalizer's `TurnKey`: the
//   finalizer keeps its id-0 channel-collapse semantics, while delivery leasing
//   needs id-0 turns disambiguated by their inflight start identity.
//   State machine:
//     Unleased --(CAS acquire)--> Leased{holder, deadline, range}
//               --(commit)-------> Committed{Delivered|NotDelivered|Unknown}
//               --(release)------> Unleased
//     deadline reclaim: Leased --(deadline elapsed)--> Unleased
// ===========================================================================

/// A non-empty half-open byte range that is allowed to carry frontier authority.
#[allow(dead_code)] // #5191 S2c PR-A: dormant until the producer/consumer cutover.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::services::discord) struct PositiveByteRange {
    start: u64,
    end: u64,
}

#[allow(dead_code)] // #5191 S2c PR-A: dormant until the producer/consumer cutover.
impl PositiveByteRange {
    pub(in crate::services::discord) fn new(start: u64, end: u64) -> Option<Self> {
        (end > start).then_some(Self { start, end })
    }

    pub(in crate::services::discord) fn start(self) -> u64 {
        self.start
    }

    pub(in crate::services::discord) fn end(self) -> u64 {
        self.end
    }
}

/// Opaque identity for one permission to publish without a byte frontier.
#[allow(dead_code)] // #5191 S2c PR-A: dormant until the producer/consumer cutover.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::services::discord) struct PermitId(u64);

impl PermitId {
    #[cfg(test)]
    fn for_test(value: u64) -> Self {
        Self(value)
    }
}

/// Publication permission that deliberately exposes no offset or end accessor.
#[allow(dead_code)] // #5191 S2c PR-A: dormant until the producer/consumer cutover.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::services::discord) struct ZeroWidthPermit {
    permit_id: PermitId,
}

impl ZeroWidthPermit {
    #[cfg(test)]
    fn for_test(permit_id: PermitId) -> Self {
        Self { permit_id }
    }

    #[cfg(test)]
    fn permit_id(self) -> PermitId {
        self.permit_id
    }
}

/// Typed publication coordinate: only `Positive` may advance a byte frontier.
#[allow(dead_code)] // #5191 S2c PR-A: dormant until the producer/consumer cutover.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::services::discord) enum PublicationCoordinate {
    Positive(PositiveByteRange),
    ZeroWidth(ZeroWidthPermit),
    NoRange,
}

#[allow(dead_code)] // #5191 S2c PR-A: dormant until the producer/consumer cutover.
impl PublicationCoordinate {
    pub(in crate::services::discord) fn positive_range(self) -> Option<PositiveByteRange> {
        match self {
            Self::Positive(range) => Some(range),
            Self::ZeroWidth(_) | Self::NoRange => None,
        }
    }
}

/// Sink holder generation reserved for the PR-B consumer cutover.
#[allow(dead_code)] // #5191 S2c PR-A: dormant until the producer/consumer cutover.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::services::discord) struct SinkEpoch(u64);

#[cfg(test)]
mod publication_coordinate_tests {
    use super::*;

    #[test]
    fn positive_byte_range_requires_strictly_positive_width() {
        assert_eq!(PositiveByteRange::new(7, 7), None);
        assert_eq!(PositiveByteRange::new(8, 7), None);
        let range = PositiveByteRange::new(7, 8).expect("positive range");
        assert_eq!((range.start(), range.end()), (7, 8));
    }

    #[test]
    fn only_positive_coordinate_exposes_frontier_range() {
        let positive =
            PublicationCoordinate::Positive(PositiveByteRange::new(3, 9).expect("positive range"));
        let permit = ZeroWidthPermit::for_test(PermitId::for_test(41));
        assert_eq!(permit.permit_id(), PermitId::for_test(41));
        assert_eq!(
            positive.positive_range().map(PositiveByteRange::end),
            Some(9)
        );
        assert_eq!(
            PublicationCoordinate::ZeroWidth(permit).positive_range(),
            None
        );
        assert_eq!(PublicationCoordinate::NoRange.positive_range(), None);
    }

    #[cfg(unix)]
    #[test]
    #[rustfmt::skip]
    fn source_epoch_adopts_once_and_bumps_only_on_witness_change() {
        use crate::services::cluster::stream_relay::{GenerationSourceIdentity, SourceFileIdentity, SourceWitness};
        let session = format!("epoch-observer-{}", uuid::Uuid::new_v4().simple());
        let stamp = |generation_ino, nonce, file_dev, file_ino| super::source_epoch_observer::source_stamp(
            &session,
            SourceWitness { generation: Some(GenerationSourceIdentity::Unix { mtime_ns: 10, dev: 1, ino: generation_ino }), spawn_nonce_hash: Some([nonce; 32]) },
            SourceFileIdentity::Unix { dev: file_dev, ino: file_ino },
        ).expect("complete Unix source identity is stamp-eligible");
        let empty = SourceWitness { generation: None, spawn_nonce_hash: None };
        assert!(super::source_epoch_observer::source_stamp(&session, empty, SourceFileIdentity::Unix { dev: 2, ino: 99 }).is_none());
        assert!(super::source_epoch_observer::source_stamp(&session, SourceWitness { spawn_nonce_hash: Some([1; 32]), ..empty }, SourceFileIdentity::Unavailable).is_none());
        let adopted = stamp(11, 1, 2, 99); assert_eq!(stamp(11, 1, 2, 99).epoch, adopted.epoch);
        let payload_file_changed = stamp(11, 1, 3, 100); assert!(payload_file_changed.epoch > adopted.epoch);
        let generation_changed = stamp(12, 1, 3, 100); assert!(generation_changed.epoch > payload_file_changed.epoch);
        let nonce_changed = stamp(12, 2, 3, 100); assert!(nonce_changed.epoch > generation_changed.epoch);
        super::source_epoch_observer::assert_bounded_cache_eviction();
    }
}

/// Who currently holds (or is attempting to hold) the delivery lease.
///
/// #3041 P1-0: dormant, wired in P1-1.. — the holder is matched on
/// compare-and-release so an actor can only release a lease it actually owns.

/// The three-way commit outcome (#3041 §3). `Unknown` is the safety value for
/// any ambiguous terminal (drop / panic / partial write) and MUST NOT advance
/// the confirmed-delivery offset — only `Delivered` does.
///
/// #3041 P1-0: dormant, wired in P1-1...

/// The lease state machine value, owned behind the cell's mutex. The `AtomicU8`
/// tag below is the single-winner CAS gate for acquire; this payload is only
/// ever mutated by that winner (or by a deadline reclaim), and every mutation
/// flips the tag AND writes the payload under the SAME mutex, so the tag and
/// payload are always observed coherently (#3041 codex). `read()` also takes
/// the mutex — there is no lock-free read fast path.
///
/// #3041 P1-0: dormant, wired in P1-1...

/// #3041 P1-1: process-monotonic millisecond clock for delivery-lease
/// deadlines. The acquire deadline and the reconciler's `reclaim_if_expired`
/// MUST read the SAME clock; a wall clock would jump on NTP steps and could
/// reclaim a live holder or strand a dead one. Anchored to a process-start
/// `Instant` so it is purely monotonic (never goes backwards). NOTE: this is a
/// real wall-monotonic clock, not the Tokio test clock; gated-clock tests drive
/// `reclaim_if_expired` with explicit `now_ms` arguments rather than this fn.

/// Internal CAS gate tag for the [`DeliveryLeaseCell`]. The CAS that flips
/// `UNLEASED → LEASED` is the single-winner acquire primitive — exactly one
/// acquirer wins; concurrent losers serialize on the payload mutex and observe
/// a non-`UNLEASED` tag under the lock. The tag is taken/flipped under the
/// payload mutex (never on its own); it is NOT a lock-free read fast path —
/// `read()` always takes the mutex (#3041 R1 coherence fix).

/// One-time terminal-delivery right for a single `(channel, turn, byte_range)`
/// (#3041 §2-§3). DORMANT in P1-0 — added alongside, NOT replacing,
/// `TmuxRelayCoord::relay_slot`. The `state_tag` is the single-winner CAS
/// acquire primitive; the `payload` mutex carries the rich lease state (holder
/// / deadline / range / outcome). The tag flip, payload write, and `read()` all
/// happen under the one mutex, so they are always mutually coherent.
///
/// #3041 P1-0: dormant, wired in P1-1...

/// Watcher-incarnation metadata paired with a live registry row. This is
/// deliberately independent of `LeaseState`: reserving a watcher never
/// consumes terminal delivery authority.

/// A point-in-time snapshot of a [`DeliveryLeaseCell`], returned by `read()`
/// (which materializes it under the payload mutex).
///
/// #3041 P1-0: dormant, wired in P1-1...

/// What a [`LeaseSnapshot`] holds for ONE expected [`DeliveryLeaseKey`], as
/// returned by [`LeaseSnapshot::identity_matched`].

/// #3041 P1-1/P1-2: delivery-lease acquire deadline shared by BOTH the watcher
/// and the bridge terminal-delivery paths. The deadline is a HOLDER-LIVENESS
/// signal, NOT a hard cap on delivery duration — while a send future is in
/// flight the holder keeps the lease alive with a background HEARTBEAT that
/// `renew()`s the deadline every [`DELIVERY_LEASE_HEARTBEAT_MS`]. Because a LIVE
/// holder always re-extends within one interval, a long multi-chunk send (which
/// can exceed any FIXED deadline) is NEVER reclaimed mid-flight; a genuinely
/// DEAD holder stops renewing, so the lease expires and a replacement reclaims
/// it within ~one deadline. Picked as 3× the heartbeat (15s = 3 × 5s): one tick
/// can be skipped entirely and the lease still survives to the next, while
/// dead-holder recovery is ~15s. P1-2 reuses this so the WATCHER and the BRIDGE
/// share one deadline against the one per-channel cell — whoever holds it blocks
/// the other's acquire (cross-actor duplicate prevention).
pub(in crate::services::discord) const DELIVERY_LEASE_DEADLINE_MS: u64 = 15_000;

/// #3041 P1-1/P1-2: how often an in-flight holder renews its delivery lease.
/// Must be strictly less than (and a small fraction of)
/// [`DELIVERY_LEASE_DEADLINE_MS`] so a live holder always re-extends before
/// expiry even if one tick is delayed (the deadline is 3× this).
pub(in crate::services::discord) const DELIVERY_LEASE_HEARTBEAT_MS: u64 = 5_000;

/// #3041 P1-1 (§3, codex R2 Issue-1) / P1-2: RAII handle for the in-flight
/// delivery-lease heartbeat task, shared by the watcher and the bridge. The
/// holder spawns the heartbeat right after a successful `try_acquire` and
/// `stop()`s it BEFORE the inline commit (and the `Drop` impl aborts it on any
/// early return / panic), so the renew loop can NEVER outlive the send and race
/// the commit. While the holder task lives the heartbeat keeps the lease alive
/// (`renew`); if the holder TASK dies the spawned heartbeat is dropped/aborted
/// with it → the lease stops being renewed → it expires → a replacement reclaims
/// it. A heartbeat tick can only ever `renew` THIS holder's OWN still-`Leased`
/// lease (matched on holder+key), so a last tick that races `stop()`+commit
/// merely extends our own deadline, which the immediately-following commit then
/// flips to `Committed` — harmless.
pub(in crate::services::discord) struct DeliveryLeaseHeartbeat {
    handle: tokio::task::JoinHandle<()>,
}

impl DeliveryLeaseHeartbeat {
    /// Spawn a background task that renews `(holder, key)`'s lease on `cell`
    /// every [`DELIVERY_LEASE_HEARTBEAT_MS`], each time pushing the deadline to
    /// `lease_now_ms() + DELIVERY_LEASE_DEADLINE_MS`. The first tick fires AFTER
    /// one interval (the acquire already set a fresh deadline). The loop exits on
    /// its own as soon as a `renew` returns false (the lease is no longer ours —
    /// committed, released, or reclaimed), so it self-terminates even before an
    /// explicit `stop()`.
    pub(in crate::services::discord) fn spawn(
        cell: std::sync::Arc<DeliveryLeaseCell>,
        holder: LeaseHolder,
        key: DeliveryLeaseKey,
    ) -> Self {
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(
                DELIVERY_LEASE_HEARTBEAT_MS,
            ));
            // Skip the immediate tick `interval` emits at t=0; the acquire just
            // set a fresh deadline, so the first renew is one interval later.
            interval.tick().await;
            loop {
                interval.tick().await;
                let renewed = cell.renew(
                    holder,
                    key.clone(),
                    lease_now_ms().saturating_add(DELIVERY_LEASE_DEADLINE_MS),
                );
                if !renewed {
                    // Lease is no longer ours (committed/released/reclaimed):
                    // nothing left to keep alive.
                    break;
                }
            }
        });
        Self { handle }
    }

    /// Stop the heartbeat. Idempotent. Called BEFORE the inline commit so the
    /// renew loop is guaranteed not to race the commit.
    pub(in crate::services::discord) fn stop(self) {
        self.handle.abort();
    }
}

impl Drop for DeliveryLeaseHeartbeat {
    fn drop(&mut self) {
        // Safety net: if the send path returns early / panics before an explicit
        // `stop()`, aborting on drop guarantees the heartbeat cannot outlive the
        // owning holder frame.
        self.handle.abort();
    }
}
