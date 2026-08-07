//! #5071 T1 S3a/S3b — the watcher terminal family and the no-transport
//! settlement sites.
//!
//! Two shapes, and the difference is the point. The **terminal delivery** path
//! really POSTs: `O`+`A` before transport, `T`+`C` (or `U`) after. The
//! **no-transport settlement** sites advance the watcher frontier with no POST,
//! no attempt and no receipt: they emit `O`+`S` only, and Q3 is explicit that
//! `SettledWithoutTransport` is NOT counted as Delivered.
//!
//! Shadow only: nothing here is read back by live delivery.

use std::sync::Arc;

use poise::serenity_prelude::ChannelId;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::services::discord::SharedData;
use crate::services::discord::outbound::DiscordTransportReceipt;
// The watcher lives under the `#[cfg(unix)] mod tmux`, so every item naming its
// result type is unix-only too.
#[cfg(unix)]
use crate::services::discord::tmux::tmux_watcher::terminal_long_chunks::GuardedWatcherDeliveryResult;
use crate::services::provider::ProviderKind;

use super::{
    AppendCommand, AttemptObservation, JOURNAL_NAMESPACE, JournalEvent, admit, event,
    process_observer, push_field,
};

pub(super) const TERMINAL_DISPOSITION: &str = "watcher_terminal";

/// The disposition class of a frontier advance that transported nothing. Closed,
/// so a new no-transport site cannot appear without naming itself, and so a
/// repeated settlement's canonical payload is byte-identical.
#[rustfmt::skip]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum SettlementReason {
    /// `terminal_preflight.rs` — `silent_turn` suppressed the terminal output.
    SilentTurnSuppressed,
    /// `terminal_preflight.rs` — a recent turn stop tombstoned the output.
    CancelTombstoneSuppressed,
    /// `no_result_exits.rs` — structural / pane-idle completion with no body.
    ReadyForInputFreshIdle,
    /// `loop_poll_prologue.rs` — post-terminal output with no inflight.
    PostTerminalNoInflightSuppressed,
    /// `tmux.rs` — dead-tmux tail drained to EOF before watcher shutdown.
    MissingInflightDeadTmuxDrain,
}

impl SettlementReason {
    #[rustfmt::skip]
    fn as_str(self) -> &'static str {
        match self {
            Self::SilentTurnSuppressed => "silent_turn_suppressed",
            Self::CancelTombstoneSuppressed => "cancel_tombstone_suppressed",
            Self::ReadyForInputFreshIdle => "ready_for_input_fresh_idle",
            Self::PostTerminalNoInflightSuppressed => "post_terminal_no_inflight_suppressed",
            Self::MissingInflightDeadTmuxDrain => "missing_inflight_dead_tmux_drain",
        }
    }
}

/// The coordinates a watcher observation is keyed on.
///
/// NOT the parent module's `TuiObligationKey`: the watcher has no
/// `SessionRelayDelivery` in scope, so `path_hash`, `file_id` and the turn
/// identity fields are absent and the generation is passed in by the caller from
/// the value it already read. What it carries is deterministic across nodes and
/// stable across a repeated advance of the same range.
#[derive(Clone, Copy)]
pub(in crate::services::discord) struct WatcherObligationCoordinates<'a> {
    pub(in crate::services::discord) provider: &'a ProviderKind,
    pub(in crate::services::discord) channel_id: ChannelId,
    pub(in crate::services::discord) tmux_session_name: &'a str,
    pub(in crate::services::discord) generation_mtime_ns: i64,
    pub(in crate::services::discord) range: (u64, u64),
}

/// `UUIDv5` over the canonical encoding of the coordinates plus the disposition
/// class. Q3: the disposition participates, so observations of the same byte
/// range under different dispositions are separate obligations, not a slot
/// collision.
pub(super) fn watcher_obligation_id(
    coordinates: WatcherObligationCoordinates<'_>,
    disposition_class: &str,
) -> Uuid {
    let mut bytes = Vec::new();
    for value in [
        coordinates.provider.as_str(),
        &coordinates.channel_id.get().to_string(),
        coordinates.tmux_session_name,
        disposition_class,
    ] {
        push_field(&mut bytes, value);
    }
    bytes.extend_from_slice(&coordinates.generation_mtime_ns.to_be_bytes());
    bytes.extend_from_slice(&coordinates.range.0.to_be_bytes());
    bytes.extend_from_slice(&coordinates.range.1.to_be_bytes());
    Uuid::new_v5(&JOURNAL_NAMESPACE, &bytes)
}

#[rustfmt::skip]
pub(super) fn obligation_payload(coordinates: WatcherObligationCoordinates<'_>, disposition_class: &str) -> Value {
    json!({"source": "watcher", "disposition_class": disposition_class,
        "frontier_start": coordinates.range.0, "frontier_end": coordinates.range.1})
}

/// The `O`+`S` pair for one no-transport settlement.
///
/// Pure and total: the same coordinates and reason yield the same `event_id`,
/// `idempotency_key` and canonical payload every time, so a repeated advance of
/// the same range lands on the same two `(obligation_id, event_seq)` slots and
/// the 0103 `ON CONFLICT DO NOTHING` insert reports `DuplicateNoOp` instead of
/// appending. `S` takes slot 1 — the slot `A` would have taken — which is what
/// makes the two mutually exclusive.
#[rustfmt::skip]
pub(super) fn settlement_events(
    coordinates: WatcherObligationCoordinates<'_>,
    reason: SettlementReason,
) -> Vec<JournalEvent> {
    let disposition_class = reason.as_str();
    let obligation_id = watcher_obligation_id(coordinates, disposition_class);
    vec![
        event(obligation_id, None, "O", 0, obligation_payload(coordinates, disposition_class)),
        event(obligation_id, None, "S", 1, json!({"reason": disposition_class,
            "frontier_start": coordinates.range.0, "frontier_end": coordinates.range.1})),
    ]
}

/// Observe a frontier advance that transported nothing. Returns what it emitted
/// so a test can assert on it; an empty return means shadow mode was off, there
/// was no pool, or the cohort did not select this channel.
pub(in crate::services::discord) fn settle_without_transport(
    shared: &SharedData,
    coordinates: WatcherObligationCoordinates<'_>,
    reason: SettlementReason,
) -> Vec<JournalEvent> {
    let events = settlement_events(coordinates, reason);
    let Some(pool) = admit(shared, coordinates.channel_id, events[0].obligation_id) else {
        return Vec::new();
    };
    process_observer().submit(AppendCommand {
        pool,
        events: events.clone(),
    });
    events
}

/// `loop_poll_prologue` re-enters its suppression arm on every poll pass while
/// the same bytes stay suppressed, and already tracks the last suppressed range
/// to keep its warning one-shot. This is that same first-observation test, named
/// so the settlement can share it: without it a stuck suppression would submit an
/// `O`+`S` batch per pass and a mailbox of 256 would drop, which Q4 says
/// invalidates the whole observation window.
pub(in crate::services::discord) fn first_observation_of_suppressed_range(
    last_suppressed_range: Option<(u64, u64)>,
    suppressed_range: (u64, u64),
) -> bool {
    last_suppressed_range != Some(suppressed_range)
}

/// An open watcher terminal obligation, held across the POST by the watcher
/// loop. Wraps `AttemptObservation` so the watcher cannot reach the pool or the
/// raw event constructors.
pub(in crate::services::discord) struct WatcherTerminalObservation {
    inner: AttemptObservation,
}

/// Open the watcher terminal obligation BEFORE transport. `journals` is the
/// caller's family predicate, taken as an argument so the decision is one
/// expression at the call site in the frozen anchor file.
#[rustfmt::skip]
pub(in crate::services::discord) fn begin_watcher_terminal(
    shared: &Arc<SharedData>,
    coordinates: WatcherObligationCoordinates<'_>,
    journals: bool,
) -> Option<WatcherTerminalObservation> {
    if !journals { return None; }
    let obligation_id = watcher_obligation_id(coordinates, TERMINAL_DISPOSITION);
    let pool = admit(shared, coordinates.channel_id, obligation_id)?;
    let attempt_id = Uuid::new_v5(&obligation_id, b"attempt:0");
    process_observer().submit(AppendCommand {
        pool: pool.clone(),
        events: vec![
            event(obligation_id, None, "O", 0, obligation_payload(coordinates, TERMINAL_DISPOSITION)),
            event(obligation_id, Some(attempt_id), "A", 1, json!({"attempt": 0,
                "frontier_start": coordinates.range.0, "frontier_end": coordinates.range.1})),
        ],
    });
    Some(WatcherTerminalObservation {
        inner: AttemptObservation { obligation_id, attempt_id, frontier: coordinates.range, pool },
    })
}

/// Only a `Persisted` guarded advance means the legacy durable record was
/// written. `AdvancedWithoutProof` moves the in-memory frontier with no record,
/// receipt or ledger entry behind it, and `LandedStale` / `LandedUnrecorded` name
/// their own failure, so all three non-Persisted results settle into `U`.
#[cfg(unix)]
pub(in crate::services::discord) fn watcher_terminal_committed(
    result: GuardedWatcherDeliveryResult,
) -> bool {
    matches!(result, GuardedWatcherDeliveryResult::Persisted)
}

/// Close the watcher terminal obligation. Mirrors the sink's `settle`: a missing
/// receipt emits nothing and leaves the observation open rather than fabricating
/// a confirmation, and the observation is consumed so a second settle cannot
/// append a second terminal event.
#[cfg(unix)]
pub(in crate::services::discord) fn settle_watcher_terminal(
    observation: &mut Option<WatcherTerminalObservation>,
    receipt: Option<DiscordTransportReceipt>,
    result: GuardedWatcherDeliveryResult,
) -> Vec<JournalEvent> {
    let Some(receipt) = receipt else {
        return Vec::new();
    };
    let Some(observation) = observation.take() else {
        return Vec::new();
    };
    process_observer().finish_fresh(
        observation.inner,
        receipt,
        watcher_terminal_committed(result),
    )
}

/// A pure predicate, so the family boundary is behavioural rather than
/// positional. The obligation exists only when the watcher itself holds the
/// delivery lease: `cutover_short_replace` hands it to the unified controller
/// (S4's family), and every other arm either transports nothing or belongs to the
/// sink direct family S2 instrumented.
pub(in crate::services::discord) fn journals_watcher_terminal(
    watcher_lease_acquired: bool,
    cutover_short_replace: bool,
) -> bool {
    watcher_lease_acquired && !cutover_short_replace
}

#[rustfmt::skip]
#[cfg(test)]
mod watcher_terminal_semantics_tests {
    //! #5071 T1 S3a W5-W9. RUNTIME semantic assertions: each test calls the
    //! production function and inspects its value, so a mutation that still
    //! compiles but changes what the code means fails here.
    use super::super::{ShadowClassification, classify_shadow_observation, transport_event};
    use super::*;

    fn coordinates(range: (u64, u64)) -> WatcherObligationCoordinates<'static> {
        WatcherObligationCoordinates { provider: &ProviderKind::Claude, channel_id: ChannelId::new(4_242),
            tmux_session_name: "adk-claude-s3", generation_mtime_ns: 77, range }
    }
    fn receipt(requested: u64, returned: u64, message_id: u64) -> DiscordTransportReceipt {
        DiscordTransportReceipt { requested_channel_id: requested.to_string(),
            returned_channel_id: returned.to_string(), message_id: message_id.to_string() }
    }
    #[cfg(unix)]
    fn observation() -> Option<WatcherTerminalObservation> {
        Some(WatcherTerminalObservation { inner: AttemptObservation {
            obligation_id: Uuid::from_u128(11), attempt_id: Uuid::from_u128(12), frontier: (10, 20),
            pool: sqlx::Pool::<sqlx::Postgres>::connect_lazy("postgres://localhost/agentdesk_test")
                .expect("lazy test pool URL is valid") } })
    }

    const POST_TERMINAL: SettlementReason = SettlementReason::PostTerminalNoInflightSuppressed;

    fn obligation_of(coordinates: WatcherObligationCoordinates<'_>, reason: SettlementReason) -> Uuid {
        settlement_events(coordinates, reason)[0].obligation_id
    }

    /// W1 (kills M-W1). Q3 is explicit that `O`+`S` is not a delivery. A
    /// settlement that grew an A/T/C would be counted as Delivered by the
    /// verifier — the exact forgery this family can commit.
    #[test]
    fn w1_settlement_is_o_plus_s_only_and_is_not_delivered() {
        let events = settlement_events(coordinates((10, 20)), SettlementReason::SilentTurnSuppressed);
        assert_eq!(events.iter().map(|event| event.kind).collect::<Vec<_>>(), vec!["O", "S"],
            "a no-transport settlement emits exactly O and S");
        assert_eq!(events[1].seq, 1, "S takes the slot A would have taken");
        assert!(events.iter().all(|event| event.attempt_id.is_none()),
            "no attempt was started, so no event may carry an attempt id");
        assert!(events.iter().all(|event| event.receipt.is_none()),
            "nothing was transported, so no event may carry a receipt");
        assert_eq!(classify_shadow_observation(&events, false), ShadowClassification::SettledWithoutTransport);
        assert_ne!(classify_shadow_observation(&events, true), ShadowClassification::CandidateDelivered,
            "an elapsed grace must not promote a settlement into a delivery");
    }

    /// W3 (kills M-W3). Settlement identity must move when the observation is
    /// genuinely different, or every settlement in the process would collapse
    /// onto one row.
    #[test]
    fn w3_settlement_identity_separates_range_reason_and_source() {
        let base = obligation_of(coordinates((10, 20)), POST_TERMINAL);
        let mut moved = coordinates((10, 21));
        assert_ne!(base, obligation_of(moved, POST_TERMINAL), "a different end offset is a different observation");
        moved.range = (11, 20);
        assert_ne!(base, obligation_of(moved, POST_TERMINAL), "a different start offset is a different observation");
        moved = coordinates((10, 20)); moved.generation_mtime_ns = 78;
        assert_ne!(base, obligation_of(moved, POST_TERMINAL), "the same bytes under a new wrapper generation are new");
        moved = coordinates((10, 20)); moved.channel_id = ChannelId::new(4_243);
        assert_ne!(base, obligation_of(moved, POST_TERMINAL), "channel participates in obligation identity");
        for reason in [SettlementReason::SilentTurnSuppressed, SettlementReason::CancelTombstoneSuppressed,
                       SettlementReason::ReadyForInputFreshIdle, SettlementReason::MissingInflightDeadTmuxDrain] {
            assert_ne!(base, obligation_of(coordinates((10, 20)), reason),
                "{reason:?} is a different disposition class over the same range");
        }
    }

    /// W4 (kills M-W4). A settlement and a real delivery over the SAME range must
    /// not share an obligation, or the delivery's `A` and the settlement's `S`
    /// would compete for slot 1 and one would be an `InvariantConflict`.
    #[test]
    fn w4_settlement_and_terminal_delivery_are_separate_obligations() {
        assert_ne!(obligation_of(coordinates((10, 20)), POST_TERMINAL),
            watcher_obligation_id(coordinates((10, 20)), TERMINAL_DISPOSITION),
            "the delivery and suppression dispositions are distinct obligations");
    }

    /// W2 (kills M-W2). Repeated-settlement idempotency, the core S3c claim.
    /// `loop_poll_prologue` re-enters with the same range, so the second
    /// observation must land on the SAME logical slots — identical event ids,
    /// idempotency keys and canonical payloads — for the 0103 `ON CONFLICT DO
    /// NOTHING` insert to report `DuplicateNoOp` instead of appending a row.
    #[test]
    fn w2_repeated_settlement_is_an_exact_no_op() {
        let (first, second) = (settlement_events(coordinates((10, 20)), POST_TERMINAL),
                               settlement_events(coordinates((10, 20)), POST_TERMINAL));
        assert_eq!(first.len(), second.len(), "a repeat emits the same number of events");
        for (left, right) in first.iter().zip(second.iter()) {
            assert_eq!(left.obligation_id, right.obligation_id, "same obligation");
            assert_eq!(left.event_id, right.event_id, "same event id");
            assert_eq!(left.seq, right.seq, "same logical slot");
            assert_eq!(left.idempotency_key, right.idempotency_key, "same idempotency key");
            assert_eq!(left.canonical_payload, right.canonical_payload, "same canonical payload");
        }
    }

    /// W2b. The repeat is stable across MANY re-entries, not just two — a stuck
    /// suppression re-enters for as long as the bytes stay suppressed, and every
    /// pass must reuse the same two slots.
    #[test]
    fn w2b_many_re_entries_reuse_exactly_two_logical_slots() {
        let slots: std::collections::HashSet<_> = (0..32)
            .flat_map(|_| settlement_events(coordinates((10, 20)), POST_TERMINAL))
            .map(|event| (event.obligation_id, event.seq))
            .collect();
        assert_eq!(slots.len(), 2, "32 re-entries must occupy exactly the O and S slots");
        let keys: std::collections::HashSet<_> = (0..32)
            .flat_map(|_| settlement_events(coordinates((10, 20)), POST_TERMINAL))
            .map(|event| event.idempotency_key)
            .collect();
        assert_eq!(keys.len(), 2, "32 re-entries must produce exactly two idempotency keys");
    }

    /// W7 (kills M-W7). The re-entry guard that keeps those repeats off the
    /// mailbox entirely: only the first pass over a suppressed range observes.
    #[test]
    fn w7_only_the_first_pass_over_a_suppressed_range_observes() {
        assert!(first_observation_of_suppressed_range(None, (10, 20)), "the first pass observes");
        assert!(!first_observation_of_suppressed_range(Some((10, 20)), (10, 20)),
            "re-entering with the SAME range must not observe again");
        assert!(first_observation_of_suppressed_range(Some((10, 20)), (10, 21)),
            "the suppressed range growing is a new observation");
        assert!(first_observation_of_suppressed_range(Some((10, 20)), (20, 30)),
            "a disjoint suppressed range is a new observation");
    }

    /// W5 (kills M-W5). Journalling `AdvancedWithoutProof` as delivered is the
    /// worst forgery available here: the frontier moved, but no receipt or ledger
    /// entry exists behind it.
    #[cfg(unix)]
    #[test]
    fn w5_only_a_persisted_guarded_advance_is_a_commit() {
        assert!(watcher_terminal_committed(GuardedWatcherDeliveryResult::Persisted));
        for not_committed in [GuardedWatcherDeliveryResult::AdvancedWithoutProof,
                              GuardedWatcherDeliveryResult::LandedStale,
                              GuardedWatcherDeliveryResult::LandedUnrecorded] {
            assert!(!watcher_terminal_committed(not_committed),
                "{not_committed:?} has no durable record and must settle into U");
        }
    }

    /// W6 (kills M-W6). The terminal settle inherits the sink's two invariants:
    /// the proof decides `T`+`C` vs `U`, and the observation is single-use.
    #[cfg(unix)]
    #[tokio::test]
    async fn w6_terminal_settle_derives_commit_from_the_proof_and_is_single_use() {
        let mut settled = observation();
        assert_eq!(settle_watcher_terminal(&mut settled, Some(receipt(10, 10, 30)),
            GuardedWatcherDeliveryResult::Persisted).iter().map(|e| e.kind).collect::<Vec<_>>(),
            vec!["T", "C"], "a persisted watcher terminal delivery emits the transport and commit pair");
        assert!(settled.is_none(), "a settled observation is consumed");
        assert!(settle_watcher_terminal(&mut settled, Some(receipt(10, 10, 30)),
            GuardedWatcherDeliveryResult::Persisted).is_empty(),
            "a second settle on the same observation must emit nothing");

        let mut unproven = observation();
        let events = settle_watcher_terminal(&mut unproven, Some(receipt(10, 10, 30)),
            GuardedWatcherDeliveryResult::AdvancedWithoutProof);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "U", "a proof-less advance is Unknown, never T+C");

        let mut receiptless = observation();
        assert!(settle_watcher_terminal(&mut receiptless, None,
            GuardedWatcherDeliveryResult::Persisted).is_empty(), "no receipt, no terminal event");
        assert!(receiptless.is_some(), "a missing receipt must not consume the observation");
    }

    /// W8. Positive control: a watcher terminal obligation that DID transport
    /// classifies as delivered, so the negative assertions elsewhere are not
    /// passing merely because the classifier rejects everything.
    #[test]
    fn w8_watcher_terminal_delivery_classifies_as_candidate_delivered() {
        let obligation_id = watcher_obligation_id(coordinates((10, 20)), TERMINAL_DISPOSITION);
        let attempt_id = Uuid::new_v5(&obligation_id, b"attempt:0");
        let events = vec![
            event(obligation_id, None, "O", 0, obligation_payload(coordinates((10, 20)), TERMINAL_DISPOSITION)),
            event(obligation_id, Some(attempt_id), "A", 1, json!({"attempt": 0, "frontier_start": 10, "frontier_end": 20})),
            transport_event(obligation_id, attempt_id, receipt(10, 10, 30)),
            event(obligation_id, Some(attempt_id), "C", 3, json!({"frontier_start": 10, "frontier_end": 20})),
        ];
        assert_eq!(classify_shadow_observation(&events, false), ShadowClassification::CandidateDelivered);
    }

    /// W9 (kills M-W9). The family boundary, and the `journals` gate inside
    /// `begin_watcher_terminal`: a cutover short-replace is the controller's
    /// obligation (S4), and journalling it here would double-count it.
    #[test]
    fn w9_only_a_watcher_held_lease_is_a_watcher_terminal_obligation() {
        assert!(journals_watcher_terminal(true, false), "the watcher's own leased direct send is this family");
        assert!(!journals_watcher_terminal(true, true), "cutover short-replace belongs to the controller family");
        assert!(!journals_watcher_terminal(false, false), "no lease, no watcher terminal obligation");
        assert!(!journals_watcher_terminal(false, true));
        // The gate is inside the facade, not only at the call site.
        let shared = Arc::new(crate::services::discord::make_shared_data_for_tests_with_storage(None));
        assert!(begin_watcher_terminal(&shared, coordinates((10, 20)), false).is_none(),
            "a non-family delivery must not open an obligation");
    }

    /// W10 (kills M-W10). Terminal obligation identity must move with every
    /// coordinate, or two different deliveries would collapse onto one row.
    #[test]
    fn w10_terminal_obligation_identity_covers_every_coordinate() {
        let base = watcher_obligation_id(coordinates((10, 20)), TERMINAL_DISPOSITION);
        assert_eq!(base, watcher_obligation_id(coordinates((10, 20)), TERMINAL_DISPOSITION), "deterministic");
        let mut moved = coordinates((10, 21));
        assert_ne!(base, watcher_obligation_id(moved, TERMINAL_DISPOSITION), "end offset participates");
        moved.range = (11, 20);
        assert_ne!(base, watcher_obligation_id(moved, TERMINAL_DISPOSITION), "start offset participates");
        moved = coordinates((10, 20)); moved.generation_mtime_ns = 78;
        assert_ne!(base, watcher_obligation_id(moved, TERMINAL_DISPOSITION), "wrapper generation participates");
        moved = coordinates((10, 20)); moved.channel_id = ChannelId::new(4_243);
        assert_ne!(base, watcher_obligation_id(moved, TERMINAL_DISPOSITION), "channel participates");
        moved = coordinates((10, 20)); moved.tmux_session_name = "adk-claude-other";
        assert_ne!(base, watcher_obligation_id(moved, TERMINAL_DISPOSITION), "tmux session participates");
        assert_ne!(base, watcher_obligation_id(coordinates((10, 20)), "other_disposition"),
            "disposition class participates, so S3b's settlements cannot collide with a delivery");
    }
}
