//! #3479 Phase-1 rank-1: tests for the session-bound terminal-ACK half. PURE MOVE
//! from `tmux_watcher.rs`'s `#[cfg(test)] mod tests` (zero logic change). Kept in
//! a sibling `*_tests.rs` so the production module stays within the
//! `src/services/discord/tmux_watcher/**` namespace LoC cap (test files are
//! excluded from the cap by the audit's `production_rust_files()` filter).

use super::*;

#[test]
fn relay_slot_guard_releases_on_drop() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    // Simulate a watcher acquiring the slot (CAS 0 -> non-zero token).
    let slot = Arc::new(AtomicU64::new(0));
    slot.store(42, Ordering::Release);
    {
        let _guard = RelaySlotGuard::new(slot.clone());
        assert_eq!(slot.load(Ordering::Acquire), 42, "slot held inside scope");
    }
    // #2840: dropping without an explicit release (panic / `?` / abort) must
    // still free the slot so a replacement watcher is not wedged.
    assert_eq!(slot.load(Ordering::Acquire), 0, "Drop released the slot");
}

#[test]
fn relay_slot_guard_release_is_idempotent_and_does_not_clobber_reacquire() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    let slot = Arc::new(AtomicU64::new(7));
    let mut guard = RelaySlotGuard::new(slot.clone());
    guard.release();
    assert_eq!(
        slot.load(Ordering::Acquire),
        0,
        "explicit release frees slot"
    );

    // After the explicit release, another watcher may legitimately acquire
    // the slot. The first guard's trailing Drop must NOT reset that token to
    // 0 — the idempotent `released` flag guarantees it.
    slot.store(99, Ordering::Release);
    drop(guard);
    assert_eq!(
        slot.load(Ordering::Acquire),
        99,
        "Drop after explicit release must not clobber a re-acquired slot"
    );
}

/// #3151: the deterministic decision seam for the in-flight sink-delivery
/// marker gate (`watcher_terminal_resend_action_gated`). Table-drives the gate
/// over every lease-snapshot variant and asserts the reclaim side-effect flag.
/// The decision fn is PURE (no cell mutation) so the side effect is testable in
/// isolation; the integration tests below exercise the actual `reclaim_if_expired`.
mod inflight_sink_marker_gate {
    use super::super::{
        WatcherTerminalResendAction, watcher_terminal_resend_action,
        watcher_terminal_resend_action_gated,
    };
    use crate::services::discord::turn_finalizer::TurnKey;
    use crate::services::discord::{
        DeliveryLeaseCell, LeaseHolder, LeaseOutcome, LeaseSnapshot, lease_now_ms,
    };
    use serenity::model::id::ChannelId;

    const START: u64 = 100;
    const END: u64 = 200;
    // `committed < end` so the underlying reconciliation would choose SendFull.
    const COMMITTED_BELOW_END: u64 = 100;
    const NOW: u64 = 50_000;

    fn turn() -> TurnKey {
        TurnKey::new(ChannelId::new(7201), 9, 0)
    }

    /// Unleased → behaves EXACTLY as the ungated reconciliation (SendFull when
    /// committed<end), no reclaim.
    #[test]
    fn unleased_defers_to_reconciliation() {
        let (action, reclaim) = watcher_terminal_resend_action_gated(
            &LeaseSnapshot::Unleased,
            COMMITTED_BELOW_END,
            START,
            END,
            NOW,
        );
        assert_eq!(action, WatcherTerminalResendAction::SendFull);
        assert!(!reclaim);
        // ... and committed>=end on an Unleased cell still Skips (unchanged).
        let (skip, reclaim2) =
            watcher_terminal_resend_action_gated(&LeaseSnapshot::Unleased, END, START, END, NOW);
        assert_eq!(skip, WatcherTerminalResendAction::SkipAlreadyCommitted);
        assert!(!reclaim2);
    }

    /// Leased{Sink, FRESH} (now < deadline) → WaitInFlight, no reclaim. This is
    /// the slow-sink-in-flight case: the watcher must NOT re-send this pass.
    #[test]
    fn leased_sink_fresh_waits_in_flight() {
        let snap = LeaseSnapshot::Leased {
            holder: LeaseHolder::Sink,
            turn: turn(),
            deadline_ms: NOW + 5_000, // fresh: deadline strictly in the future
            start: START,
            end: END,
        };
        let (action, reclaim) =
            watcher_terminal_resend_action_gated(&snap, COMMITTED_BELOW_END, START, END, NOW);
        assert_eq!(action, WatcherTerminalResendAction::WaitInFlight);
        assert!(!reclaim, "a fresh sink lease must NOT be reclaimed");
    }

    /// Leased{Sink, EXPIRED} (now >= deadline) → reclaim flag set AND SendFull
    /// (committed<end). This is the dead-sink no-black-hole arm.
    #[test]
    fn leased_sink_expired_reclaims_and_sends_full() {
        let snap = LeaseSnapshot::Leased {
            holder: LeaseHolder::Sink,
            turn: turn(),
            deadline_ms: NOW, // expired: now >= deadline
            start: START,
            end: END,
        };
        let (action, reclaim) =
            watcher_terminal_resend_action_gated(&snap, COMMITTED_BELOW_END, START, END, NOW);
        assert_eq!(action, WatcherTerminalResendAction::SendFull);
        assert!(
            reclaim,
            "an expired sink lease MUST be reclaimed (no black-hole)"
        );
    }

    /// #3159 BUG 1: Committed{Sink, Delivered} with committed >= end (a real
    /// delivered commit advances the offset BEFORE committing) → Skip, no reclaim.
    /// This is the no-duplicate invariant: a genuinely-delivered range is never
    /// re-sent.
    #[test]
    fn committed_sink_delivered_covered_skips() {
        let snap = LeaseSnapshot::Committed {
            holder: LeaseHolder::Sink,
            turn: turn(),
            start: START,
            end: END,
            outcome: LeaseOutcome::Delivered,
        };
        // committed >= end (END): the advance ran before commit.
        let (action, reclaim) = watcher_terminal_resend_action_gated(&snap, END, START, END, NOW);
        assert_eq!(action, WatcherTerminalResendAction::SkipAlreadyCommitted);
        assert!(!reclaim);
    }

    /// #3159 BUG 1 (no black-hole): Committed{Sink, NotDelivered} — the identity
    /// gate REFUSED the advance, so committed stayed < end. The gate now routes
    /// through committed-offset reconciliation → SendFull (re-send, not Skip).
    /// Previously this blind-Skipped → under-delivery / black-hole.
    #[test]
    fn committed_sink_not_delivered_sends_full() {
        let snap = LeaseSnapshot::Committed {
            holder: LeaseHolder::Sink,
            turn: turn(),
            start: START,
            end: END,
            outcome: LeaseOutcome::NotDelivered,
        };
        let (action, reclaim) =
            watcher_terminal_resend_action_gated(&snap, COMMITTED_BELOW_END, START, END, NOW);
        assert_eq!(
            action,
            WatcherTerminalResendAction::SendFull,
            "a NotDelivered commit (committed<end) must re-send, not Skip"
        );
        assert!(!reclaim);
    }

    /// #3159 BUG 1 belt-and-suspenders: even a Delivered-labelled commit with
    /// committed < end (which the fixed producer no longer emits, since Delivered
    /// is committed only after a real advance) re-sends — the committed offset is
    /// the SOLE delivered-test, not the outcome label. No black-hole regardless.
    #[test]
    fn committed_sink_below_end_sends_full_regardless_of_label() {
        let snap = LeaseSnapshot::Committed {
            holder: LeaseHolder::Sink,
            turn: turn(),
            start: START,
            end: END,
            outcome: LeaseOutcome::Delivered,
        };
        let (action, reclaim) =
            watcher_terminal_resend_action_gated(&snap, COMMITTED_BELOW_END, START, END, NOW);
        assert_eq!(action, WatcherTerminalResendAction::SendFull);
        assert!(!reclaim);
    }

    /// Leased by a WATCHER (non-Sink) holder → the #3151 gate does NOT interpose;
    /// it defers to the existing reconciliation (the B2 path is untouched).
    #[test]
    fn leased_by_watcher_defers_to_reconciliation() {
        let snap = LeaseSnapshot::Leased {
            holder: LeaseHolder::Watcher { instance_id: 1 },
            turn: turn(),
            deadline_ms: NOW + 5_000,
            start: START,
            end: END,
        };
        // committed<end → SendFull (NOT WaitInFlight: only a Sink lease waits).
        let (action, reclaim) =
            watcher_terminal_resend_action_gated(&snap, COMMITTED_BELOW_END, START, END, NOW);
        assert_eq!(action, WatcherTerminalResendAction::SendFull);
        assert!(!reclaim);
        // committed>=end on a watcher-held lease still Skips.
        let (skip, _) = watcher_terminal_resend_action_gated(&snap, END, START, END, NOW);
        assert_eq!(skip, WatcherTerminalResendAction::SkipAlreadyCommitted);
    }

    /// committed>=end with a Bridge holder → Skip (the range is delivered),
    /// matching the ungated path.
    #[test]
    fn committed_covered_skips_for_non_sink() {
        let snap = LeaseSnapshot::Leased {
            holder: LeaseHolder::Bridge,
            turn: turn(),
            deadline_ms: NOW + 1,
            start: START,
            end: END,
        };
        let (action, _) = watcher_terminal_resend_action_gated(&snap, END, START, END, NOW);
        assert_eq!(action, WatcherTerminalResendAction::SkipAlreadyCommitted);
        // Sanity: the gated decision equals the ungated reconciliation here.
        assert_eq!(action, watcher_terminal_resend_action(END, START, END));
    }

    /// (b) Integration: a DEAD/STALE sink marker on a real cell is reclaimed by
    /// the gate's `reclaim_if_expired` side effect, then the watcher re-acquires
    /// and SendFulls — NO black-hole. Drives the actual cell, not just the flag.
    #[test]
    fn dead_sink_marker_reclaimed_then_resent_no_blackhole() {
        let ch = ChannelId::new(7202);
        let cell = DeliveryLeaseCell::new(ch);
        let sink_turn = TurnKey::new(ch, 9, 0);
        let now = lease_now_ms();
        let deadline = now.saturating_add(10);
        // Sink set the marker then "died" (no heartbeat renews it).
        assert!(cell.try_acquire(sink_turn, LeaseHolder::Sink, START, END, deadline));

        // The gate, observed at a time PAST the deadline, decides reclaim+SendFull.
        let past = deadline.saturating_add(1);
        let snap = cell.read();
        let (action, reclaim) =
            watcher_terminal_resend_action_gated(&snap, COMMITTED_BELOW_END, START, END, past);
        assert_eq!(action, WatcherTerminalResendAction::SendFull);
        assert!(reclaim);

        // The caller performs the reclaim → the dead marker clears → a
        // replacement watcher re-acquires and re-delivers (no black-hole).
        assert!(cell.reclaim_if_expired(past));
        assert!(matches!(cell.read(), LeaseSnapshot::Unleased));
        let watcher_turn = TurnKey::new(ch, 9, 0);
        assert!(
            cell.try_acquire(
                watcher_turn,
                LeaseHolder::Watcher { instance_id: 1 },
                START,
                END,
                past.saturating_add(10_000),
            ),
            "the reclaimed cell is re-acquirable by the watcher (no black-hole)"
        );
    }

    /// (c) reclaim-races-with-late-sink-success cannot corrupt the lease: after a
    /// dead sink's marker is reclaimed and the watcher re-acquires, the zombie
    /// sink's late `commit`/`release` (full-identity-gated) NO-OP against the
    /// watcher's lease — no wrong-holder advance, no stolen release.
    #[test]
    fn reclaim_then_late_sink_commit_cannot_corrupt_lease() {
        let ch = ChannelId::new(7203);
        let cell = DeliveryLeaseCell::new(ch);
        let sink_turn = TurnKey::new(ch, 9, 0);
        let now = lease_now_ms();
        let deadline = now.saturating_add(10);
        assert!(cell.try_acquire(sink_turn, LeaseHolder::Sink, START, END, deadline));

        // Watcher reclaims the expired sink marker and re-acquires the SAME range.
        let past = deadline.saturating_add(1);
        assert!(cell.reclaim_if_expired(past));
        let watcher_holder = LeaseHolder::Watcher { instance_id: 1 };
        assert!(cell.try_acquire(
            sink_turn,
            watcher_holder,
            START,
            END,
            past.saturating_add(10_000),
        ));

        // The zombie sink's LATE commit/release target Sink+sink_turn; the cell
        // is now held by the Watcher → both no-op (false). No corruption.
        assert!(
            !cell.commit(
                LeaseHolder::Sink,
                sink_turn,
                START,
                END,
                LeaseOutcome::Delivered
            ),
            "a late sink commit must NOT act on the watcher's lease"
        );
        assert!(
            !cell.release(LeaseHolder::Sink, sink_turn, START, END),
            "a late sink release must NOT free the watcher's lease"
        );
        // The watcher's lease is intact and committable by its true holder.
        assert!(cell.commit(
            watcher_holder,
            sink_turn,
            START,
            END,
            LeaseOutcome::Delivered
        ));
    }
}
