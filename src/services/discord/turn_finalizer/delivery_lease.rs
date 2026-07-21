//! #3479 r9 — delivery-lease handler split out of `turn_finalizer.rs`.
//!
//! PURE MOVE (no logic change): the three dormant `DeliveryLeaseCell`
//! handler wrappers (`handle_acquire_delivery` / `handle_commit_delivery` /
//! `handle_release_delivery`) and their state-machine unit tests. The parent
//! re-imports the handlers (`use self::delivery_lease::{...}`) so the actor
//! call sites stay byte-identical.

use super::*;

// #3041 §2-§3 — delivery-lease handlers: thin wrappers over the
// `DeliveryLeaseCell` state machine (mod.rs), run in the actor task. DORMANT
// after the R2 revert (the watcher works the cell INLINE); kept + unit-tested
// for the sink/bridge wiring (P1-2..).

/// CAS-acquire for `(key, [start,end))` on behalf of `holder`. #3041, dormant
/// in the non-test build (the watcher acquires the cell directly, B4).
#[allow(dead_code)] // #3041: AcquireDelivery actor arm dormant until sink/bridge wiring.
pub(super) fn handle_acquire_delivery(
    lease: &DeliveryLeaseCell,
    key: DeliveryLeaseKey,
    holder: LeaseHolder,
    start: u64,
    end: u64,
    deadline_ms: u64,
) -> bool {
    lease.try_acquire(key, holder, start, end, deadline_ms)
}

/// Three-way commit; full `(holder, key, [start,end))` mismatch = no-op. #3041
/// P1-1: a successful `Delivered` commit advances the channel's
/// `confirmed_end_offset` watermark to `end` (§5.2), gated on the lease having
/// actually committed (so a rejected stale/duplicate commit never touches the
/// offset) and via `advance_watcher_confirmed_end`'s monotonic CAS (never a
/// double-advance). `NotDelivered`/`Unknown` never advance: an ambiguous
/// terminal must not claim bytes as delivered.
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_commit_delivery(
    lease: &DeliveryLeaseCell,
    key: DeliveryLeaseKey,
    holder: LeaseHolder,
    start: u64,
    end: u64,
    outcome: LeaseOutcome,
    provider: &ProviderKind,
    tmux_session_name: &str,
    shared: &SharedData,
) -> bool {
    let committed = lease.commit(holder, key.clone(), start, end, outcome);
    // `mod tmux` is `#[cfg(unix)]`; non-unix commits the lease without an
    // advance and consumes the otherwise-unused unix-only params.
    #[cfg(unix)]
    if committed && outcome == LeaseOutcome::Delivered {
        crate::services::discord::tmux::advance_watcher_confirmed_end(
            shared,
            provider,
            key.channel_id(),
            tmux_session_name,
            end,
            "src/services/discord/turn_finalizer.rs:commit_delivery_advance",
        );
    }
    #[cfg(not(unix))]
    let _ = (shared, provider, tmux_session_name);
    committed
}

/// Compare-and-release; full `(holder, key, [start,end))` match only. #3041.
#[allow(dead_code)] // #3041 P1-0: dormant, wired in P1-1..
pub(super) fn handle_release_delivery(
    lease: &DeliveryLeaseCell,
    key: DeliveryLeaseKey,
    holder: LeaseHolder,
    start: u64,
    end: u64,
) -> bool {
    lease.release(holder, key, start, end)
}

// =======================================================================
// #3041 §2-§3 §6 P1-0 — Dormant `DeliveryLeaseCell` state-machine tests.
//
// The cell is wired into no call path yet (P1-1..), but its transitions
// are proven correct now: single-winner CAS acquire, three-way commit,
// compare-and-release no-op on holder mismatch, and deadline reclaim. The
// tests drive the cell directly (and through the dormant handler wrappers)
// because that is the logic later phases depend on.
// =======================================================================
#[cfg(test)]
mod tests {
    use super::{handle_acquire_delivery, handle_commit_delivery, handle_release_delivery};
    use crate::services::discord::{
        DeliveryLeaseCell, DeliveryLeaseKey, LeaseHolder, LeaseOutcome, LeaseSnapshot,
    };
    use serenity::model::id::ChannelId;
    use std::sync::Arc;

    fn cell() -> DeliveryLeaseCell {
        DeliveryLeaseCell::new(ChannelId::new(42))
    }

    fn key(channel_id: ChannelId, user_msg_id: u64, generation: u64) -> DeliveryLeaseKey {
        DeliveryLeaseKey::new(
            channel_id,
            generation,
            user_msg_id,
            Some("2026-07-03 06:00:00"),
            Some(0),
        )
    }

    fn id0_key(channel_id: ChannelId, started_at: &str, start_offset: u64) -> DeliveryLeaseKey {
        DeliveryLeaseKey::new(channel_id, 0, 0, Some(started_at), Some(start_offset))
    }

    fn turn() -> DeliveryLeaseKey {
        key(ChannelId::new(42), 7, 0)
    }

    #[test]
    fn id0_turns_with_distinct_start_offsets_have_distinct_lease_identities() {
        let ch = ChannelId::new(42);
        let holder = LeaseHolder::Sink;
        let turn_a = id0_key(ch, "2026-07-03 06:00:00", 0);
        let turn_b = id0_key(ch, "2026-07-03 06:00:00", 128);
        assert_ne!(turn_a, turn_b);

        let c = cell();
        assert!(c.try_acquire(turn_a.clone(), holder, 0, 64, 10));
        assert!(c.reclaim_if_expired(10));
        assert!(c.try_acquire(turn_b.clone(), holder, 0, 64, 1_000));

        assert!(!c.commit(holder, turn_a.clone(), 0, 64, LeaseOutcome::Delivered));
        assert!(!c.release(holder, turn_a, 0, 64));
        assert!(matches!(
            c.read(),
            LeaseSnapshot::Leased { key, .. } if key == turn_b
        ));
        assert!(c.commit(holder, turn_b.clone(), 0, 64, LeaseOutcome::Delivered));
        assert!(c.release(holder, turn_b, 0, 64));
    }

    #[test]
    fn degenerate_id0_keys_match_each_other_but_not_disambiguated_id0() {
        let ch = ChannelId::new(42);
        let missing_offset =
            DeliveryLeaseKey::new_for_site(ch, 0, 0, Some("2026-07-03 06:00:00"), None, "test");
        let missing_started_at = DeliveryLeaseKey::new_for_site(ch, 0, 0, None, Some(10), "test");
        let disambiguated =
            DeliveryLeaseKey::new_for_site(ch, 0, 0, Some("2026-07-03 06:00:00"), Some(10), "test");

        assert_eq!(
            missing_offset, missing_started_at,
            "all residual id-0 keys without full disambiguators collapse to the legacy degenerate identity"
        );
        assert_ne!(
            missing_offset, disambiguated,
            "a fully disambiguated id-0 turn must not alias the degenerate residual class"
        );
    }

    #[test]
    fn nonzero_user_msg_id_lease_identity_ignores_disambiguators() {
        let ch = ChannelId::new(42);
        let with_a = DeliveryLeaseKey::new(ch, 9, 123, Some("started-a"), Some(1));
        let with_b = DeliveryLeaseKey::new(ch, 9, 123, Some("started-b"), Some(2));
        let from_turn = DeliveryLeaseKey::from_turn_key(
            crate::services::discord::turn_finalizer::TurnKey::new(ch, 123, 9),
        );

        assert_eq!(with_a, with_b);
        assert_eq!(with_a, from_turn);
    }

    #[test]
    fn fresh_cell_is_unleased() {
        let c = cell();
        assert!(matches!(c.read(), LeaseSnapshot::Unleased));
        assert_eq!(c.channel_id(), ChannelId::new(42));
    }

    #[test]
    fn acquire_records_holder_range_and_deadline() {
        let c = cell();
        let h = LeaseHolder::Watcher { instance_id: 1 };
        assert!(c.try_acquire(turn(), h, 10, 20, 1_000));
        match c.read() {
            LeaseSnapshot::Leased {
                holder,
                key,
                deadline_ms,
                start,
                end,
            } => {
                assert_eq!(holder, h);
                assert_eq!(key, self::turn());
                assert_eq!(deadline_ms, 1_000);
                assert_eq!((start, end), (10, 20));
            }
            other => panic!("expected Leased, got {other:?}"),
        }
    }

    #[test]
    fn acquire_cas_admits_a_single_winner() {
        // Two distinct holders race to acquire the SAME fresh cell; exactly
        // one wins the CAS and the loser is rejected without mutating state.
        let c = cell();
        let w1 = LeaseHolder::Watcher { instance_id: 1 };
        let w2 = LeaseHolder::Watcher { instance_id: 2 };
        assert!(c.try_acquire(turn(), w1, 0, 5, 1_000));
        // Second acquire on an already-Leased cell loses.
        assert!(!c.try_acquire(turn(), w2, 0, 5, 1_000));
        // The winner's payload is intact (loser did not overwrite it).
        match c.read() {
            LeaseSnapshot::Leased { holder, .. } => assert_eq!(holder, w1),
            other => panic!("expected Leased held by winner, got {other:?}"),
        }
    }

    #[test]
    fn concurrent_acquire_has_exactly_one_winner() {
        // Stronger single-winner proof: spawn N threads contending on one
        // shared cell; exactly one try_acquire returns true.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let c = Arc::new(cell());
        let wins = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(std::sync::Barrier::new(16));
        let mut handles = Vec::new();
        for i in 0..16u64 {
            let c = Arc::clone(&c);
            let wins = Arc::clone(&wins);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                if c.try_acquire(turn(), LeaseHolder::Watcher { instance_id: i }, 0, 1, 9_999) {
                    wins.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(wins.load(Ordering::Relaxed), 1, "CAS must admit one winner");
        assert!(matches!(c.read(), LeaseSnapshot::Leased { .. }));
    }

    #[test]
    fn commit_three_way_delivered_not_delivered_unknown() {
        for outcome in [
            LeaseOutcome::Delivered,
            LeaseOutcome::NotDelivered,
            LeaseOutcome::Unknown,
        ] {
            let c = cell();
            let h = LeaseHolder::Sink;
            assert!(c.try_acquire(turn(), h, 3, 9, 1_000));
            assert!(
                c.commit(h, turn(), 3, 9, outcome),
                "holder may commit {outcome:?}"
            );
            match c.read() {
                LeaseSnapshot::Committed {
                    holder,
                    start,
                    end,
                    outcome: got,
                    ..
                } => {
                    assert_eq!(holder, h);
                    assert_eq!((start, end), (3, 9));
                    assert_eq!(got, outcome);
                }
                other => panic!("expected Committed({outcome:?}), got {other:?}"),
            }
        }
    }

    #[test]
    fn commit_by_non_holder_is_noop() {
        let c = cell();
        let owner = LeaseHolder::Watcher { instance_id: 1 };
        let other = LeaseHolder::Watcher { instance_id: 2 };
        assert!(c.try_acquire(turn(), owner, 0, 4, 1_000));
        // Holder mismatch: commit refused, state stays Leased.
        assert!(!c.commit(other, turn(), 0, 4, LeaseOutcome::Delivered));
        assert!(matches!(c.read(), LeaseSnapshot::Leased { .. }));
    }

    #[test]
    fn commit_on_unleased_is_noop() {
        let c = cell();
        assert!(!c.commit(LeaseHolder::Bridge, turn(), 0, 1, LeaseOutcome::Delivered));
        assert!(matches!(c.read(), LeaseSnapshot::Unleased));
    }

    #[test]
    fn release_compare_and_release_noop_on_holder_mismatch() {
        let c = cell();
        let owner = LeaseHolder::Bridge;
        let stale = LeaseHolder::Watcher { instance_id: 99 };
        assert!(c.try_acquire(turn(), owner, 0, 8, 1_000));
        // A stale actor cannot release the live lease.
        assert!(!c.release(stale, turn(), 0, 8));
        assert!(matches!(c.read(), LeaseSnapshot::Leased { .. }));
        // The true holder releases successfully → back to Unleased.
        assert!(c.release(owner, turn(), 0, 8));
        assert!(matches!(c.read(), LeaseSnapshot::Unleased));
    }

    #[test]
    fn release_after_commit_returns_to_unleased() {
        let c = cell();
        let h = LeaseHolder::Sink;
        assert!(c.try_acquire(turn(), h, 0, 2, 1_000));
        assert!(c.commit(h, turn(), 0, 2, LeaseOutcome::Delivered));
        // Release is valid from Committed for the recorded holder.
        assert!(c.release(h, turn(), 0, 2));
        assert!(matches!(c.read(), LeaseSnapshot::Unleased));
        // Idempotent: a second release on the now-Unleased cell is a no-op.
        assert!(!c.release(h, turn(), 0, 2));
    }

    #[test]
    fn stale_turn_commit_and_release_are_noops_after_reacquire() {
        // #3041 §2 hazard, closed: turn A is acquired then reclaimed; turn B
        // reacquires the SAME channel with the SAME holder KIND. A stale
        // commit OR release carrying turn A's key must be a NO-OP and must
        // NOT touch turn B's live lease. (Holder kind alone would match —
        // only the stored turn identity distinguishes the two.)
        let c = cell();
        let holder = LeaseHolder::Sink; // same holder kind across both turns
        let turn_a = key(ChannelId::new(42), 100, 0);
        let turn_b = key(ChannelId::new(42), 200, 0);

        // Turn A acquires, then its deadline elapses and it is reclaimed.
        assert!(c.try_acquire(turn_a.clone(), holder, 0, 5, 10));
        assert!(c.reclaim_if_expired(10));
        assert!(matches!(c.read(), LeaseSnapshot::Unleased));

        // Turn B reacquires the freed cell (same channel, same holder kind).
        assert!(c.try_acquire(turn_b.clone(), holder, 5, 11, 1_000));

        // Stale commit from turn A: identity mismatch → no-op, B untouched.
        assert!(!c.commit(holder, turn_a.clone(), 5, 11, LeaseOutcome::Delivered));
        assert!(!c.commit(holder, turn_a.clone(), 0, 5, LeaseOutcome::Delivered));
        // Stale release from turn A: identity mismatch → no-op, B untouched.
        assert!(!c.release(holder, turn_a.clone(), 0, 5));
        match c.read() {
            LeaseSnapshot::Leased {
                key, start, end, ..
            } => {
                assert_eq!(key, turn_b, "B still holds");
                assert_eq!((start, end), (5, 11));
            }
            other => panic!("turn B lease must survive stale A ops, got {other:?}"),
        }

        // Turn B's own commit/release with its real key still work.
        assert!(c.commit(holder, turn_b.clone(), 5, 11, LeaseOutcome::Delivered));
        assert!(!c.release(holder, turn_a, 5, 11)); // stale release post-commit: no-op
        assert!(c.release(holder, turn_b, 5, 11));
        assert!(matches!(c.read(), LeaseSnapshot::Unleased));
    }

    #[test]
    fn same_turn_stale_range_release_is_noop_after_reacquire() {
        // #3041 codex R2: the SAME turn is reclaimed and reacquires a
        // DIFFERENT byte range (e.g. a continuation chunk). A stale release
        // carrying the OLD range — same holder AND same turn — must be a
        // NO-OP and must NOT release the live newer-range lease. Only the
        // correct range releases it (release is now range-scoped, symmetric
        // with commit).
        let c = cell();
        let holder = LeaseHolder::Sink;
        let t = key(ChannelId::new(7), 300, 0);

        // Acquire range [0,5), let the deadline elapse, reclaim.
        assert!(c.try_acquire(t.clone(), holder, 0, 5, 10));
        assert!(c.reclaim_if_expired(10));
        // Same turn reacquires a continuation range [5, 12).
        assert!(c.try_acquire(t.clone(), holder, 5, 12, 1_000));

        // Stale release with the OLD range [0,5): holder+turn match but the
        // range does not → NO-OP, live [5,12) lease survives.
        assert!(!c.release(holder, t.clone(), 0, 5));
        match c.read() {
            LeaseSnapshot::Leased { start, end, .. } => assert_eq!((start, end), (5, 12)),
            other => {
                panic!("newer-range lease must survive stale-range release, got {other:?}")
            }
        }
        // The correct range releases it.
        assert!(c.release(holder, t, 5, 12));
        assert!(matches!(c.read(), LeaseSnapshot::Unleased));
    }

    #[test]
    fn read_observes_payload_coherent_with_tag_under_race() {
        // #3041 codex coherence fix: a reader that observes a non-`Unleased`
        // state must observe the MATCHING payload — never a `Leased` tag
        // paired with an `Unleased`/empty payload. Because `try_acquire`
        // flips the tag AND writes the payload under one mutex (and `read`
        // also locks), this holds by construction. Hammer it: while one
        // thread repeatedly acquires/reclaims, readers must only ever see
        // `Unleased` or a fully-populated `Leased{turn,range}` — never a
        // torn intermediate.
        use std::sync::atomic::{AtomicBool, Ordering};
        let c = Arc::new(cell());
        let stop = Arc::new(AtomicBool::new(false));
        let t = turn();

        let writer = {
            let c = Arc::clone(&c);
            let stop = Arc::clone(&stop);
            let writer_t = t.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if c.try_acquire(writer_t.clone(), LeaseHolder::Sink, 7, 13, 1) {
                        // Immediately reclaim (deadline already in the past)
                        // so the cell churns Unleased↔Leased rapidly.
                        let _ = c.reclaim_if_expired(u64::MAX);
                    }
                }
            })
        };

        for _ in 0..200_000 {
            match c.read() {
                LeaseSnapshot::Unleased => {}
                LeaseSnapshot::Leased {
                    key, start, end, ..
                } => {
                    // The payload paired with the Leased state is always the
                    // exact one the writer published — never torn/empty.
                    assert_eq!(key, t);
                    assert_eq!((start, end), (7, 13));
                }
                LeaseSnapshot::Committed { .. } => {
                    panic!("writer never commits; tag/payload incoherent")
                }
            }
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
    }

    #[test]
    fn deadline_reclaim_forces_unleased_when_expired() {
        let c = cell();
        let h = LeaseHolder::Watcher { instance_id: 1 };
        assert!(c.try_acquire(turn(), h, 0, 3, 100));
        // Not yet expired: no reclaim.
        assert!(!c.reclaim_if_expired(50));
        assert!(matches!(c.read(), LeaseSnapshot::Leased { .. }));
        // At/after the deadline: reclaimed regardless of holder identity.
        assert!(c.reclaim_if_expired(100));
        assert!(matches!(c.read(), LeaseSnapshot::Unleased));
        // After a reclaim a fresh acquire can win again.
        assert!(c.try_acquire(turn(), h, 0, 3, 200));
    }

    #[test]
    fn deadline_reclaim_never_touches_committed() {
        let c = cell();
        let h = LeaseHolder::Bridge;
        assert!(c.try_acquire(turn(), h, 0, 3, 10));
        assert!(c.commit(h, turn(), 0, 3, LeaseOutcome::Delivered));
        // A Committed lease awaits an explicit release; deadline reclaim is a
        // no-op even far past the (now meaningless) deadline.
        assert!(!c.reclaim_if_expired(10_000));
        assert!(matches!(c.read(), LeaseSnapshot::Committed { .. }));
    }

    #[test]
    fn dormant_handlers_drive_the_same_transitions() {
        // The actor-task handler wrappers must produce identical results to
        // the direct cell methods (they are wired in P1-1.. and exercised
        // through these wrappers).
        let c = cell();
        let h = LeaseHolder::Watcher { instance_id: 3 };
        assert!(handle_acquire_delivery(&c, turn(), h, 0, 6, 1_000));
        assert!(!handle_acquire_delivery(
            &c,
            turn(),
            LeaseHolder::Sink,
            0,
            6,
            1_000
        ));
        // #3041 P1-1: the commit handler now takes provider/session/shared so
        // a `Delivered` commit can advance the channel watermark. Supply a
        // throwaway `SharedData`; the advance targets the cell's channel (42).
        let shared = super::super::super::make_shared_data_for_tests_with_storage(None);
        assert!(handle_commit_delivery(
            &c,
            turn(),
            h,
            0,
            6,
            LeaseOutcome::Delivered,
            &crate::services::provider::ProviderKind::Claude,
            "dormant-handler-test-session",
            &shared,
        ));
        assert!(!handle_release_delivery(
            &c,
            turn(),
            LeaseHolder::Watcher { instance_id: 4 },
            0,
            6
        ));
        assert!(handle_release_delivery(&c, turn(), h, 0, 6));
        assert!(matches!(c.read(), LeaseSnapshot::Unleased));
    }
}
