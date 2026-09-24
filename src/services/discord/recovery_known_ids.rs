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

/// Which of the three sources answered for a known id.
///
/// #5996: the union below erases the provenance, and a consumer that only
/// SKIPS on membership does not miss it. One that also RETIRES state does:
/// `docs/relay-state-contract.md` I20 lets `catch_up`'s phase-2 checkpoint
/// advance past a message "only on evidence of dispatch or answer", and the
/// three arms do not agree on whether they carry that evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum RecoveryKnownIdArm {
    /// `intervention_queue`. Presence says the message was ACCEPTED for a
    /// turn, never that a turn took it — a queued entry that is dropped before
    /// it drains was never dispatched at all.
    Queued,
    /// The #3167 dequeue→claim reservation. The head left the queue and has
    /// not yet claimed the slot, so no turn has taken it either. An orphaned
    /// marker deliberately reads as NOT live (see
    /// [`live_pending_dispatch_message_ids`]) so recovery stays reachable;
    /// a checkpoint advanced during the live window forecloses the very rescan
    /// that fallback exists to reach.
    PendingDispatch,
    /// `active_user_message_id` — `try_start_turn` stamped THIS message onto
    /// the slot the current turn holds.
    ActiveTurn,
}

impl RecoveryKnownIdArm {
    /// I20: "the checkpoint may advance past a message only on evidence of
    /// dispatch or answer."
    pub(in crate::services::discord) fn is_dispatch_evidence(self) -> bool {
        match self {
            Self::ActiveTurn => true,
            Self::Queued | Self::PendingDispatch => false,
        }
    }
}

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

/// The known-id union, each id carrying the arm that answered for it.
///
/// [`recovery_known_message_ids`] is this map's key set, so the three sources
/// are walked once and the two views cannot drift apart.
fn recovery_known_id_arms(
    snapshot: &ChannelMailboxSnapshot,
) -> std::collections::HashMap<u64, RecoveryKnownIdArm> {
    // Weakest evidence first: a later insert overwrites an earlier one, so an
    // id claimed by two arms keeps the strongest claim.
    let mut arms: std::collections::HashMap<u64, RecoveryKnownIdArm> = queued_message_ids(snapshot)
        .into_iter()
        .map(|id| (id, RecoveryKnownIdArm::Queued))
        .collect();
    // #5191: the dequeue→claim window. Between the drain popping an
    // intervention and `try_start_turn` setting `active_user_message_id`, the
    // message id lives in NEITHER of the two other sets, so a catch-up scan
    // landing inside that window classified it `Recover` and enqueued a second
    // copy — one user message, two turns. The reservation marker is the only
    // in-mailbox evidence covering that gap, so recovery must consult it too.
    for reserved_id in live_pending_dispatch_message_ids(snapshot) {
        arms.insert(reserved_id.get(), RecoveryKnownIdArm::PendingDispatch);
    }
    if let Some(active_id) = snapshot.active_user_message_id {
        arms.insert(active_id.get(), RecoveryKnownIdArm::ActiveTurn);
    }
    arms
}

pub(in crate::services::discord) fn recovery_known_message_ids(
    snapshot: &ChannelMailboxSnapshot,
) -> std::collections::HashSet<u64> {
    recovery_known_id_arms(snapshot).into_keys().collect()
}

/// #5996: the provenance map and its key set from one walk, for scans that need
/// both membership and the arm behind it.
pub(in crate::services::discord) fn recovery_known_arms_and_ids(
    snapshot: &ChannelMailboxSnapshot,
) -> (
    std::collections::HashMap<u64, RecoveryKnownIdArm>,
    std::collections::HashSet<u64>,
) {
    let arms = recovery_known_id_arms(snapshot);
    let ids = arms.keys().copied().collect();
    (arms, ids)
}

#[cfg(test)]
mod recovery_known_message_ids_tests {
    use std::time::{Duration, Instant};

    use super::*;

    const RESERVED: u64 = 1_534_895_957_961_867_314;
    /// An id the reserved head absorbed by merging — it is NOT the primary.
    const MERGED_SOURCE: u64 = 1_534_895_957_961_867_300;

    fn queued_intervention(message_id: u64) -> crate::services::turn_orchestrator::Intervention {
        crate::services::turn_orchestrator::Intervention {
            author_id: poise::serenity_prelude::UserId::new(4_162_001),
            author_is_bot: false,
            message_id: MessageId::new(message_id),
            queued_generation: 0,
            source_message_ids: vec![MessageId::new(message_id)],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: "queued".to_string(),
            mode: crate::services::turn_orchestrator::InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        }
    }

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

    /// #5996: only the active-turn arm names a message a turn actually took,
    /// so only it may move a checkpoint past one.
    #[test]
    fn only_the_active_turn_arm_is_dispatch_evidence() {
        assert!(RecoveryKnownIdArm::ActiveTurn.is_dispatch_evidence());
        assert!(!RecoveryKnownIdArm::Queued.is_dispatch_evidence());
        assert!(!RecoveryKnownIdArm::PendingDispatch.is_dispatch_evidence());
    }

    /// #5996: the union erased which source answered. Each arm must be
    /// recoverable, or `catch_up` cannot tell a dispatched message from a
    /// merely queued one.
    #[test]
    fn each_source_reports_its_own_arm() {
        const QUEUED: u64 = 1_534_895_957_961_867_001;
        const ACTIVE: u64 = 1_534_895_957_961_867_002;
        let snapshot = ChannelMailboxSnapshot {
            intervention_queue: vec![queued_intervention(QUEUED)],
            active_user_message_id: Some(MessageId::new(ACTIVE)),
            pending_user_dispatch: Some(MessageId::new(RESERVED)),
            pending_user_dispatch_since: Some(Instant::now()),
            ..ChannelMailboxSnapshot::default()
        };
        let arms = recovery_known_id_arms(&snapshot);
        assert_eq!(arms.get(&QUEUED), Some(&RecoveryKnownIdArm::Queued));
        assert_eq!(arms.get(&ACTIVE), Some(&RecoveryKnownIdArm::ActiveTurn));
        assert_eq!(
            arms.get(&RESERVED),
            Some(&RecoveryKnownIdArm::PendingDispatch)
        );
    }

    /// The two views are one walk: whatever the map keys, the set contains.
    #[test]
    fn known_ids_are_exactly_the_arm_map_keys() {
        const QUEUED: u64 = 1_534_895_957_961_867_003;
        let snapshot = ChannelMailboxSnapshot {
            intervention_queue: vec![queued_intervention(QUEUED)],
            active_user_message_id: Some(MessageId::new(RESERVED)),
            ..ChannelMailboxSnapshot::default()
        };
        let (arms, ids) = recovery_known_arms_and_ids(&snapshot);
        assert_eq!(arms, recovery_known_id_arms(&snapshot));
        assert_eq!(ids, recovery_known_message_ids(&snapshot));
        assert_eq!(
            arms.keys()
                .copied()
                .collect::<std::collections::HashSet<u64>>(),
            ids
        );
    }

    /// An id both queued and stamped active keeps the arm that can move a
    /// checkpoint; resolving it the other way would re-open #5996 for it.
    #[test]
    fn active_turn_outranks_a_stale_queue_entry_for_the_same_id() {
        const BOTH: u64 = 1_534_895_957_961_867_004;
        let snapshot = ChannelMailboxSnapshot {
            intervention_queue: vec![queued_intervention(BOTH)],
            active_user_message_id: Some(MessageId::new(BOTH)),
            ..ChannelMailboxSnapshot::default()
        };
        assert_eq!(
            recovery_known_id_arms(&snapshot).get(&BOTH),
            Some(&RecoveryKnownIdArm::ActiveTurn)
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
