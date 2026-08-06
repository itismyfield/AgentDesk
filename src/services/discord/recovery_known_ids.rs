//! #5191 — the identity set the Discord catch-up recovery scan treats as
//! "already known", i.e. messages it must NOT re-enqueue.
//!
//! Three sources make up that set, and they must be read together or the
//! union develops a hole:
//!
//! 1. `intervention_queue` — entries still waiting their turn.
//! 2. `active_user_message_id` — the message whose turn currently holds the slot.
//! 3. `pending_user_dispatch` — the #3167 dequeue→claim reservation, covering
//!    the window where a head has left the queue but has not yet claimed the
//!    slot. Source 1 and 2 are both blind there.
//!
//! Extracted from `discord/mod.rs` so the recovery-dedup contract and its tests
//! live in a non-giant module (`giant_file_ratchet`).

use poise::serenity_prelude as serenity;
use serenity::MessageId;

use super::ChannelMailboxSnapshot;
use crate::services::turn_orchestrator::PENDING_USER_DISPATCH_LEASE_ORPHAN_AFTER;

pub(in crate::services::discord) fn queued_message_ids(
    snapshot: &ChannelMailboxSnapshot,
) -> std::collections::HashSet<u64> {
    let mut ids = std::collections::HashSet::new();
    for item in &snapshot.intervention_queue {
        ids.insert(item.message_id.get());
        ids.extend(
            item.source_message_ids
                .iter()
                .map(|message_id| message_id.get()),
        );
    }
    ids
}

/// True when the mailbox still holds a LIVE dequeue→claim reservation, i.e. a
/// message that has already left `intervention_queue` but has not yet been
/// stamped onto `active_user_message_id` by `try_start_turn`.
///
/// #5191: tracks the orphan predicate in `dispatch_reservation.rs` — a
/// reservation counts as live while its lease is still held OR it is younger
/// than [`PENDING_USER_DISPATCH_LEASE_ORPHAN_AFTER`]. An abandoned (orphaned)
/// reservation deliberately reads as NOT live so a leaked marker can never
/// suppress recovery of a genuinely unanswered message.
///
/// Two deliberate divergences from `pending_dispatch_lease_is_orphaned`, both
/// resolving toward RECOVERY (a false `Recover` costs a duplicate; a false
/// suppression costs a lost message, so the asymmetry is intentional):
///
/// - the canonical predicate also requires `cancel_token.is_none()`. Omitted
///   here because a live `cancel_token` implies `active_user_message_id` is
///   set, and [`recovery_known_message_ids`] already covers that id.
/// - a reservation with no `since` timestamp reads as NOT live here, while the
///   canonical predicate treats a missing timestamp as age 0. The setter always
///   writes both, so this only fires on a state we believe unreachable.
///
/// Returns the primary id AND every id the reserved head absorbed by merging,
/// mirroring the union [`queued_message_ids`] applies while the entry is still
/// queued.
fn live_pending_dispatch_message_ids(snapshot: &ChannelMailboxSnapshot) -> Vec<MessageId> {
    let Some(reserved_id) = snapshot.pending_user_dispatch else {
        return Vec::new();
    };
    let Some(reserved_at) = snapshot.pending_user_dispatch_since else {
        return Vec::new();
    };
    let live = snapshot.pending_user_dispatch_lease_held_by_caller
        || reserved_at.elapsed() < PENDING_USER_DISPATCH_LEASE_ORPHAN_AFTER;
    if !live {
        return Vec::new();
    }
    let mut ids = Vec::with_capacity(snapshot.pending_user_dispatch_source_ids.len() + 1);
    ids.push(reserved_id);
    ids.extend(snapshot.pending_user_dispatch_source_ids.iter().copied());
    ids
}

pub(in crate::services::discord) fn recovery_known_message_ids(
    snapshot: &ChannelMailboxSnapshot,
) -> std::collections::HashSet<u64> {
    let mut ids = queued_message_ids(snapshot);
    if let Some(active_id) = snapshot.active_user_message_id {
        ids.insert(active_id.get());
    }
    // #5191: the dequeue→claim window. Between the drain popping an
    // intervention and `try_start_turn` setting `active_user_message_id`, the
    // message id lives in NEITHER of the two sets above, so a catch-up scan
    // landing inside that window classified it `Recover` and enqueued a second
    // copy — one user message, two turns. The reservation marker is the only
    // in-mailbox evidence covering that gap, so recovery must consult it too.
    for reserved_id in live_pending_dispatch_message_ids(snapshot) {
        ids.insert(reserved_id.get());
    }
    ids
}

#[cfg(test)]
mod recovery_known_message_ids_tests {
    use std::time::{Duration, Instant};

    use super::*;

    const RESERVED: u64 = 1_534_895_957_961_867_314;
    /// An id the reserved head absorbed by merging — it is NOT the primary.
    const MERGED_SOURCE: u64 = 1_534_895_957_961_867_300;

    fn snapshot_with_reservation(
        since: Option<Instant>,
        lease_held: bool,
    ) -> ChannelMailboxSnapshot {
        snapshot_with_reservation_sources(since, lease_held, Vec::new())
    }

    fn snapshot_with_reservation_sources(
        since: Option<Instant>,
        lease_held: bool,
        source_ids: Vec<MessageId>,
    ) -> ChannelMailboxSnapshot {
        ChannelMailboxSnapshot {
            pending_user_dispatch: Some(MessageId::new(RESERVED)),
            pending_user_dispatch_source_ids: source_ids,
            pending_user_dispatch_since: since,
            pending_user_dispatch_lease_held_by_caller: lease_held,
            ..ChannelMailboxSnapshot::default()
        }
    }

    /// #5191 regression: a message popped from the queue but not yet stamped
    /// onto `active_user_message_id` must still read as known, or the catch-up
    /// scan recovers it a second time and one user message runs two turns.
    #[test]
    fn live_dequeue_to_claim_reservation_is_known() {
        let snapshot = snapshot_with_reservation(Some(Instant::now()), false);
        assert!(recovery_known_message_ids(&snapshot).contains(&RESERVED));
    }

    #[test]
    fn held_lease_keeps_reservation_known_past_the_orphan_window() {
        let since = Instant::now()
            .checked_sub(PENDING_USER_DISPATCH_LEASE_ORPHAN_AFTER * 3)
            .expect("instant in range");
        let snapshot = snapshot_with_reservation(Some(since), true);
        assert!(recovery_known_message_ids(&snapshot).contains(&RESERVED));
    }

    /// The suppression must not outlive the reservation: an orphaned marker
    /// falls back to `Recover` so a genuinely unanswered message is never lost.
    #[test]
    fn orphaned_reservation_does_not_suppress_recovery() {
        let since = Instant::now()
            .checked_sub(PENDING_USER_DISPATCH_LEASE_ORPHAN_AFTER + Duration::from_secs(1))
            .expect("instant in range");
        let snapshot = snapshot_with_reservation(Some(since), false);
        assert!(!recovery_known_message_ids(&snapshot).contains(&RESERVED));
    }

    #[test]
    fn reservation_without_timestamp_does_not_suppress_recovery() {
        let snapshot = snapshot_with_reservation(None, true);
        assert!(!recovery_known_message_ids(&snapshot).contains(&RESERVED));
    }

    /// #5191 codex review P1: a merged head keeps the NEWEST message as the
    /// primary while still answering for the ids it absorbed. Reserving only
    /// the primary left those absorbed ids visible to the catch-up scan for
    /// the whole dequeue→claim window, so their content ran a second time.
    #[test]
    fn live_reservation_covers_merged_source_ids_not_just_the_primary() {
        let snapshot = snapshot_with_reservation_sources(
            Some(Instant::now()),
            false,
            vec![MessageId::new(MERGED_SOURCE), MessageId::new(RESERVED)],
        );
        let known = recovery_known_message_ids(&snapshot);
        assert!(known.contains(&RESERVED), "primary must stay known");
        assert!(
            known.contains(&MERGED_SOURCE),
            "an absorbed source id must not be re-exposed to recovery"
        );
    }

    /// The merged ids inherit the reservation's liveness — an orphaned marker
    /// must not suppress them either.
    #[test]
    fn orphaned_reservation_does_not_suppress_merged_source_ids() {
        let since = Instant::now()
            .checked_sub(PENDING_USER_DISPATCH_LEASE_ORPHAN_AFTER + Duration::from_secs(1))
            .expect("instant in range");
        let snapshot = snapshot_with_reservation_sources(
            since.into(),
            false,
            vec![MessageId::new(MERGED_SOURCE)],
        );
        let known = recovery_known_message_ids(&snapshot);
        assert!(!known.contains(&RESERVED));
        assert!(!known.contains(&MERGED_SOURCE));
    }
}
