//! #5071 T1 S5 — the recovery / fresh-send / orphan family.
//!
//! One production site writes this family's durable delivered frontier:
//! `RecoveryDeliveryContext::record_durable_frontier` in
//! `recovery_engine/terminal_text_idempotency.rs`. This module is the only way
//! that site reaches the journal.
//!
//! #5071 T1 S7 REPLACED THE PARAGRAPH THAT WAS HERE. It described TWO raw
//! writers reached from that funnel — `delivery_record::write_delivered_frontier`
//! (the reuse-recorded-anchor arm) and
//! `delivery_record::write_proven_gone_equal_range_frontier` (the equal-range
//! re-anchor taken only after Discord proved the recorded anchor GONE, the
//! "orphan" in this family's name) — plus a completed-turn ledger append above
//! them as a third durable write on the same path. All three calls are gone. The
//! funnel now makes one call to `delivery_record::record_recovery_terminal_delivery`,
//! and the two arms above are the presence or absence of
//! `expected_gone_anchor` inside it; the ledger append is the funnel's, after
//! the frontier persists.
//!
//! Journal observation writes are admitted in `Shadow` and `Authority` when a
//! PG pool and the cohort selection agree. `Authority` prepares a journal read
//! path elsewhere; this module keeps the existing writer in place and the
//! facade reads process state (one extra generation-marker stat) without mutating it.
//!
//! ## This family's obligation opens AFTER transport — the sink/watcher/controller
//! families' does not
//!
//! The other three families open before the POST, so a delivery that vanishes
//! mid-transport leaves a dangling `O`+`A` instead of no trace. This family
//! cannot do that at two of its three entry points: the anchored-replace arm and
//! the recovery controller-cutover arm advance the frontier ONLY when the edit
//! transport already came back as `SentFallbackAfterEditFailure` /
//! `FreshFallbackAfterEditFailure`. Before that answer arrives there is no
//! frontier advance to hold an obligation for, and opening one anyway would emit
//! an `O`+`A`+`U` for every ordinary successful edit — an "unknown" that is in
//! fact the normal, expected non-advance.
//!
//! So the obligation opens where the family DECIDES to advance the frontier, on a
//! delivery Discord already confirmed. The consequence is stated plainly because
//! it is a real hole: **this family can never observe a recovery delivery lost
//! mid-POST.** What it does observe is the gap between "recovery delivery
//! confirmed" and "durable frontier persisted". S5 opened that gap as an
//! observation while `terminal_text_idempotency.rs` still bypassed
//! `shadow_mirror_delivered_frontier`; #5071 T1 S7 joined the funnel, so the gap
//! is now measured across the funnel rather than across a bypass. The hole in
//! the sentence above is unchanged by that join: a recovery delivery lost
//! mid-POST is still invisible to this family, because the obligation still
//! opens after transport.
//!
//! ## The receipt ceiling: this family can never be `CandidateDelivered` either
//!
//! `controller.rs` states the argument in full and it holds here for two
//! independent reasons. The transports on this path return no receipt at all
//! (`send_long_message_raw_with_reference_returning_message_ids` returns
//! `Vec<MessageId>`, `replace_long_message_raw_with_outcome` is handed its
//! receipt slot as `&mut None`, `toc::DeliveryOutcome` carries none); and the
//! obligation opens after transport, so even a receipt-bearing transport would
//! have no open attempt to attach one to. Synthesising a `T` from the anchor
//! message id would make `requested == returned` true by construction — the same
//! forgery both `journal.rs` and `controller.rs` refuse. So this family emits
//! `O`+`A` then `C` (or `U`), never `T`, and `classify_shadow_observation`
//! reports `Unknown` for every recovery obligation. R5 pins that ceiling.
//!
//! ## Where this family's admission deliberately differs from the controller's
//!
//! `controller.rs` refuses to open an obligation when the wrapper generation is
//! unknown (`0`), because that is where its durable writer declines to scope a
//! frontier. Here the generation marker is read INSIDE the write path, after the
//! delivery is already confirmed and the obligation already decided — so a
//! missing marker is not a reason to stay silent, it is the single most
//! interesting thing this family can report: a recovery delivery that landed in
//! Discord and then did NOT advance the durable frontier. `0` is therefore
//! admitted, carried in identity, and settled as
//! [`RecoverySettlement::NoGenerationMarker`].

use poise::serenity_prelude::{ChannelId, MessageId};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::services::discord::SharedData;
use crate::services::discord::outbound::delivery_record;
use crate::services::provider::ProviderKind;

use super::{
    AppendCommand, JOURNAL_NAMESPACE, JournalEvent, admit, event, process_observer, push_field,
};

/// Which of the family's three entry points confirmed the delivery. Closed, so a
/// fourth durable-frontier writer cannot join the funnel without naming itself,
/// and so the entry point participates in obligation identity: an anchored-edit
/// fallback and a no-anchor fresh send over the same byte range are two separate
/// obligations, not a payload conflict on one slot.
#[rustfmt::skip]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum RecoveryDisposition {
    /// `relay_no_anchor_terminal_text` — the leased no-anchor fresh send.
    NoAnchorFreshSend,
    /// `replace_anchored_terminal_text` — the POST that replaced a failed edit.
    AnchoredEditFallback,
    /// `recovery_paths/controller_cutover.rs` — the same fallback through the
    /// turn-output controller.
    ControllerEditFallback,
}

impl RecoveryDisposition {
    #[rustfmt::skip]
    fn as_str(self) -> &'static str {
        match self {
            Self::NoAnchorFreshSend => "recovery_no_anchor_fresh_send",
            Self::AnchoredEditFallback => "recovery_anchored_edit_fallback",
            Self::ControllerEditFallback => "recovery_controller_edit_fallback",
        }
    }
}

/// How the confirmed delivery left the durable frontier. Exactly one variant
/// commits; every other one names the reason the frontier did NOT advance, and
/// each is a branch that exists in `record_durable_frontier` today. Closed, so a
/// new refusal branch cannot be added to that funnel without naming itself here.
#[rustfmt::skip]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum RecoverySettlement {
    /// The durable write returned `Ok(())`. The only committing variant.
    FrontierPersisted,
    /// The inflight anchor bind did not persist, so the funnel was never entered.
    AnchorBindNotPersisted,
    /// The context carries no tmux session name, so no generation is resolvable.
    NoTmuxSessionName,
    /// `current_generation_mtime_ns` returned `0` — the durable writer's own
    /// "distrust" sentinel (#5154).
    NoGenerationMarker,
    /// The durable write itself returned `Err`.
    DurableWriteFailed,
    /// #5071 T1 S7: the relay frontier's reset incarnation moved between the
    /// recovery decision snapshot and the durable write, so
    /// `acquire_relay_frontier_mutation_for_incarnation` refused admission. The
    /// watcher family's equivalent verdict is `LandedStale`. Distinct from
    /// `DurableWriteFailed`: nothing was attempted against the record.
    FrontierResetDuringDelivery,
    /// Backstop for the trailing settle: the obligation reached the end of
    /// `record_successful_fresh_send` without any branch above having closed it.
    /// The exits enumerated above are expected to cover every path today, so this
    /// variant exists to keep a future early return from leaving an obligation
    /// dangling rather than to describe a path that runs now.
    DeliveryNotRecorded,
}

impl RecoverySettlement {
    fn committed(self) -> bool {
        matches!(self, Self::FrontierPersisted)
    }

    #[rustfmt::skip]
    fn as_str(self) -> &'static str {
        match self {
            Self::FrontierPersisted => "recovery_frontier_persisted",
            Self::AnchorBindNotPersisted => "recovery_anchor_bind_not_persisted",
            Self::NoTmuxSessionName => "recovery_no_tmux_session_name",
            Self::NoGenerationMarker => "recovery_no_generation_marker",
            Self::DurableWriteFailed => "recovery_durable_write_failed",
            Self::FrontierResetDuringDelivery => "recovery_frontier_reset_during_delivery",
            Self::DeliveryNotRecorded => "recovery_delivery_not_recorded",
        }
    }
}

/// Pure admission of the coordinates, so the refusals stay testable without a PG
/// pool (which alone makes every `begin` return `None`).
///
/// Only the range is a refusal here. It mirrors `record_durable_frontier`'s own
/// two guards — no `durable_range`, or an empty/inverted one — so the journal's
/// obligation set cannot drift from the writer's: those are precisely the cases
/// in which the writer returns before touching the frontier, and an obligation
/// for a write that provably never happens is noise, not observation.
pub(super) fn recovery_obligation_range(range: Option<(u64, u64)>) -> Option<(u64, u64)> {
    range.filter(|range| range.1 > range.0)
}

/// `UUIDv5` over the canonical coordinates. The leading `recovery` field is what
/// keeps this family's obligations disjoint from the watcher's and the
/// controller's over the same channel and range, independently of how the
/// disposition strings evolve.
///
/// `current_msg_id` is the turn's live Discord message id, carried by
/// `RecoveryDeliveryContext`. It is what separates two different turns that
/// happen to recover the same byte range; the generation separates two wrapper
/// incarnations of the same turn.
pub(super) struct RecoveryObligationCoordinates<'a> {
    pub(super) provider: &'a ProviderKind,
    pub(super) frontier_channel: ChannelId,
    pub(super) tmux_session_name: Option<&'a str>,
    pub(super) generation_mtime_ns: i64,
    pub(super) current_msg_id: u64,
    pub(super) range: (u64, u64),
    pub(super) disposition: RecoveryDisposition,
}

pub(super) fn recovery_obligation_id(coordinates: &RecoveryObligationCoordinates<'_>) -> Uuid {
    let mut bytes = Vec::new();
    for value in [
        "recovery",
        coordinates.provider.as_str(),
        &coordinates.frontier_channel.get().to_string(),
        coordinates.tmux_session_name.unwrap_or(""),
        coordinates.disposition.as_str(),
    ] {
        push_field(&mut bytes, value);
    }
    bytes.extend_from_slice(&coordinates.generation_mtime_ns.to_be_bytes());
    bytes.extend_from_slice(&coordinates.current_msg_id.to_be_bytes());
    bytes.extend_from_slice(&coordinates.range.0.to_be_bytes());
    bytes.extend_from_slice(&coordinates.range.1.to_be_bytes());
    Uuid::new_v5(&JOURNAL_NAMESPACE, &bytes)
}

#[rustfmt::skip]
fn obligation_payload(disposition: RecoveryDisposition, delivery_channel: ChannelId, generation_mtime_ns: i64, range: (u64, u64)) -> Value {
    json!({"source": "recovery", "disposition_class": disposition.as_str(),
        "delivery_channel_id": delivery_channel.get(),
        "generation_mtime_ns": generation_mtime_ns,
        "frontier_start": range.0, "frontier_end": range.1})
}

/// An open recovery obligation, held from the confirmed delivery to the durable
/// write's answer. It keeps the pool and the raw event constructors out of the
/// anchor file's reach.
pub(in crate::services::discord) struct RecoveryTerminalObservation {
    obligation_id: Uuid,
    attempt_id: Uuid,
    delivery_channel: ChannelId,
    range: (u64, u64),
    pool: sqlx::PgPool,
}

/// Open the obligation on a delivery Discord has already confirmed, immediately
/// before the funnel that decides whether the durable frontier advances.
///
/// `channels` is `(frontier, delivery)`: the frontier channel is the OFFSET
/// AUTHORITY the durable write is keyed by (`record_channel_id`), the delivery
/// channel is where the anchor message and the inbound turn actually live
/// (`channel_id`). A recovered/reused-watcher turn resolves these differently, so
/// the pair is recorded exactly as `record_durable_frontier` uses it.
///
/// `range` is `Option` because the context may hold no durable range at all; no
/// range, no frontier advance, no obligation.
///
/// The generation marker is read here rather than taken from the caller so the
/// anchor file gains no production read of its own. It is a stat, not a mutation,
/// and `0` is admitted on purpose — see the module docs.
#[rustfmt::skip]
pub(in crate::services::discord) fn begin_recovery_terminal(
    shared: &SharedData,
    provider: &ProviderKind,
    disposition: RecoveryDisposition,
    channels: (ChannelId, ChannelId),
    tmux_session_name: Option<&str>,
    current_msg_id: u64,
    range: Option<(u64, u64)>,
) -> Option<RecoveryTerminalObservation> {
    let (frontier_channel, delivery_channel) = channels;
    let range = recovery_obligation_range(range)?;
    let generation_mtime_ns = tmux_session_name
        .map_or(0, delivery_record::current_generation_mtime_ns);
    let obligation_id = recovery_obligation_id(&RecoveryObligationCoordinates {
        provider, frontier_channel, tmux_session_name, generation_mtime_ns,
        current_msg_id, range, disposition,
    });
    let pool = admit(shared, frontier_channel, obligation_id)?;
    let attempt_id = Uuid::new_v5(&obligation_id, b"attempt:0");
    process_observer().submit(AppendCommand {
        pool: pool.clone(),
        events: vec![
            event(obligation_id, None, "O", 0, obligation_payload(disposition, delivery_channel, generation_mtime_ns, range)),
            event(obligation_id, Some(attempt_id), "A", 1, json!({"attempt": 0,
                "frontier_start": range.0, "frontier_end": range.1})),
        ],
    });
    Some(RecoveryTerminalObservation { obligation_id, attempt_id, delivery_channel, range, pool })
}

/// The single terminal event. `C` takes slot 3 and `U` slot 2, exactly as the
/// sink's `finish_fresh` and the controller's settle place them; slot 2's `T`
/// stays empty because no transport receipt is observable here, and because the
/// obligation opened after the transport anyway (see the module docs).
///
/// A retry of the same range under the same turn and generation is deliberately
/// the SAME attempt: `begin` hardcodes `attempt:0` and no attempt counter reaches
/// the payload, so a repeat produces byte-identical events and lands as a
/// duplicate no-op rather than a second `A` that would push the window to
/// `Unknown` for a reason that has nothing to do with the delivery.
#[rustfmt::skip]
fn terminal_events(observation: &RecoveryTerminalObservation, anchor_msg_id: Option<MessageId>, settlement: RecoverySettlement) -> Vec<JournalEvent> {
    let (kind, seq) = if settlement.committed() { ("C", 3) } else { ("U", 2) };
    let mut payload = json!({"frontier_start": observation.range.0, "frontier_end": observation.range.1,
        "delivery_channel_id": observation.delivery_channel.get(),
        "anchor_msg_id": anchor_msg_id.map(|id| id.get())});
    if !settlement.committed() { payload["reason"] = json!(settlement.as_str()); }
    vec![event(observation.obligation_id, Some(observation.attempt_id), kind, seq, payload)]
}

/// Close the obligation. `settlement` is the funnel's own answer about the
/// durable frontier — the same branch that returned, or the `Ok`/`Err` the
/// durable writer handed back.
///
/// Single-use, like the sink's `settle` and the controller's
/// `settle_controller_terminal`: the observation is consumed, so a second call
/// emits nothing. `record_successful_fresh_send` relies on that directly — every
/// branch settles its own reason and the trailing
/// [`RecoverySettlement::DeliveryNotRecorded`] settle on the way out therefore
/// closes nothing unless a future edit introduces a path that closes nothing
/// else.
pub(in crate::services::discord) fn settle_recovery_terminal(
    observation: &mut Option<RecoveryTerminalObservation>,
    anchor_msg_id: Option<MessageId>,
    settlement: RecoverySettlement,
) -> Vec<JournalEvent> {
    let Some(observation) = observation.take() else {
        return Vec::new();
    };
    let events = terminal_events(&observation, anchor_msg_id, settlement);
    process_observer().submit(AppendCommand {
        pool: observation.pool,
        events: events.clone(),
    });
    events
}

#[rustfmt::skip]
#[cfg(test)]
mod recovery_terminal_semantics_tests {
    //! #5071 T1 S5 R1-R5. RUNTIME semantic assertions: each test calls the
    //! production function and inspects its value, so a mutation that still
    //! compiles but changes what the code means fails here.
    use super::super::controller::{ControllerDisposition, controller_obligation_id};
    use super::super::{ShadowClassification, classify_shadow_observation};
    use super::*;

    const NO_ANCHOR: RecoveryDisposition = RecoveryDisposition::NoAnchorFreshSend;

    fn observation(range: (u64, u64)) -> Option<RecoveryTerminalObservation> {
        Some(RecoveryTerminalObservation {
            obligation_id: Uuid::from_u128(31), attempt_id: Uuid::from_u128(32),
            delivery_channel: ChannelId::new(5_071), range,
            pool: sqlx::Pool::<sqlx::Postgres>::connect_lazy("postgres://localhost/agentdesk_test")
                .expect("lazy test pool URL is valid"),
        })
    }

    fn obligation(channel: u64, session: Option<&str>, generation: i64, current_msg_id: u64,
        range: (u64, u64), disposition: RecoveryDisposition) -> Uuid {
        recovery_obligation_id(&RecoveryObligationCoordinates {
            provider: &ProviderKind::Claude, frontier_channel: ChannelId::new(channel),
            tmux_session_name: session, generation_mtime_ns: generation,
            current_msg_id, range, disposition,
        })
    }

    /// R1 (kills M-R1). Obligation identity must move with every coordinate, or
    /// two different recovery deliveries collapse onto one row and the second
    /// one's payload becomes an `InvariantConflict` instead of an observation.
    #[test]
    fn r1_recovery_obligation_identity_covers_every_coordinate() {
        let base = obligation(5_071, Some("adk-claude-s5"), 77, 900, (10, 20), NO_ANCHOR);
        assert_eq!(base, obligation(5_071, Some("adk-claude-s5"), 77, 900, (10, 20), NO_ANCHOR), "deterministic");
        assert_ne!(base, obligation(5_072, Some("adk-claude-s5"), 77, 900, (10, 20), NO_ANCHOR), "frontier channel participates");
        assert_ne!(base, obligation(5_071, Some("adk-claude-s6"), 77, 900, (10, 20), NO_ANCHOR), "tmux session participates");
        assert_ne!(base, obligation(5_071, None, 77, 900, (10, 20), NO_ANCHOR), "an absent session is its own coordinate");
        assert_ne!(base, obligation(5_071, Some("adk-claude-s5"), 78, 900, (10, 20), NO_ANCHOR), "wrapper generation participates");
        assert_ne!(base, obligation(5_071, Some("adk-claude-s5"), 77, 901, (10, 20), NO_ANCHOR), "the turn's message id participates");
        assert_ne!(base, obligation(5_071, Some("adk-claude-s5"), 77, 900, (11, 20), NO_ANCHOR), "start offset participates");
        assert_ne!(base, obligation(5_071, Some("adk-claude-s5"), 77, 900, (10, 21), NO_ANCHOR), "end offset participates");
        for other in [RecoveryDisposition::AnchoredEditFallback, RecoveryDisposition::ControllerEditFallback] {
            assert_ne!(base, obligation(5_071, Some("adk-claude-s5"), 77, 900, (10, 20), other), "{other:?} is its own disposition class");
        }
        assert_ne!(base, recovery_obligation_id(&RecoveryObligationCoordinates {
            provider: &ProviderKind::Codex, frontier_channel: ChannelId::new(5_071),
            tmux_session_name: Some("adk-claude-s5"), generation_mtime_ns: 77,
            current_msg_id: 900, range: (10, 20), disposition: NO_ANCHOR,
        }), "provider participates");
    }

    /// R2 (kills M-R2). The recovery and the controller families both observe
    /// terminal deliveries keyed by channel + generation + range. If their
    /// obligation ids could coincide, one family's `A` and the other's would
    /// compete for slot 1 over the same delivery.
    #[test]
    fn r2_recovery_and_controller_obligations_never_coincide() {
        let controller = controller_obligation_id(
            &ProviderKind::Claude, ChannelId::new(5_071), 77, (10, 20),
            ControllerDisposition::ShortReplace,
        );
        assert_ne!(obligation(5_071, Some("adk-claude-s5"), 77, 900, (10, 20), NO_ANCHOR), controller,
            "the leading `recovery` field keeps the two families disjoint");
    }

    /// R3 (kills M-R3). The range refusal, tested where it is decidable: a
    /// pool-less test process makes every `begin_recovery_terminal` return
    /// `None`, so asserting on the facade would prove nothing.
    ///
    /// The absence of a generation refusal is asserted too. That is the one place
    /// this family deliberately parts company with `controller_obligation_range`,
    /// and re-adding the `generation != 0` filter here would silently delete the
    /// family's most interesting observation.
    #[test]
    fn r3_only_a_real_range_is_an_obligation_and_generation_zero_still_opens() {
        assert_eq!(recovery_obligation_range(Some((10, 20))), Some((10, 20)), "the ordinary case observes");
        assert_eq!(recovery_obligation_range(None), None, "no durable range, no obligation");
        assert_eq!(recovery_obligation_range(Some((10, 10))), None, "an empty range advances nothing");
        assert_eq!(recovery_obligation_range(Some((20, 10))), None, "an inverted range is not a frontier");
        assert_ne!(obligation(5_071, Some("adk-claude-s5"), 0, 900, (10, 20), NO_ANCHOR),
            obligation(5_071, Some("adk-claude-s5"), 77, 900, (10, 20), NO_ANCHOR),
            "an unknown generation is admitted as its own obligation, not refused");
    }

    /// R4 (kills M-R4). The settlement selects the terminal event and names its
    /// own reason, and the observation is single-use — which is what makes the
    /// trailing `DeliveryNotRecorded` settle a no-op after a real branch ran.
    #[tokio::test]
    async fn r4_settle_selects_the_terminal_event_and_is_single_use() {
        let mut persisted = observation((10, 20));
        let events = settle_recovery_terminal(&mut persisted, Some(MessageId::new(901)), RecoverySettlement::FrontierPersisted);
        assert_eq!(events.iter().map(|event| event.kind).collect::<Vec<_>>(), vec!["C"],
            "a persisted recovery frontier emits exactly one C");
        assert_eq!(events[0].seq, 3, "C takes the sink's commit slot");
        assert_eq!(events[0].canonical_payload["anchor_msg_id"], 901);
        assert_eq!(events[0].canonical_payload["frontier_end"], 20);
        assert!(events[0].canonical_payload.get("reason").is_none(), "a commit needs no failure reason");
        assert!(persisted.is_none(), "a settled observation is consumed");
        assert!(settle_recovery_terminal(&mut persisted, None, RecoverySettlement::DeliveryNotRecorded).is_empty(),
            "the trailing settle must not append after a real branch closed the obligation");

        for (settlement, reason) in [
            (RecoverySettlement::AnchorBindNotPersisted, "recovery_anchor_bind_not_persisted"),
            (RecoverySettlement::NoTmuxSessionName, "recovery_no_tmux_session_name"),
            (RecoverySettlement::NoGenerationMarker, "recovery_no_generation_marker"),
            (RecoverySettlement::DurableWriteFailed, "recovery_durable_write_failed"),
            (RecoverySettlement::DeliveryNotRecorded, "recovery_delivery_not_recorded"),
        ] {
            let mut open = observation((10, 20));
            let events = settle_recovery_terminal(&mut open, None, settlement);
            assert_eq!(events.iter().map(|event| event.kind).collect::<Vec<_>>(), vec!["U"],
                "{settlement:?} did not advance the frontier, so it is Unknown, never C");
            assert_eq!(events[0].seq, 2, "U takes the slot T would have taken");
            assert_eq!(events[0].canonical_payload["reason"], reason,
                "every non-advance names the branch it came from");
        }
        assert!(settle_recovery_terminal(&mut None, Some(MessageId::new(901)), RecoverySettlement::FrontierPersisted).is_empty(),
            "an unobserved delivery settles into nothing");
    }

    /// R5 (kills M-R5). The declared ceiling. No recovery event may carry a
    /// transport receipt: none is observable on this path, and the obligation
    /// opens after the transport in any case. A future slice that synthesises one
    /// from the anchor message id — `requested == returned` by construction, so
    /// the mismatch branch could never fire — fails here rather than silently
    /// promoting a forgery to Delivered.
    #[tokio::test]
    async fn r5_recovery_family_never_classifies_as_delivered() {
        let mut open = observation((10, 20));
        let obligation_id = open.as_ref().expect("observation").obligation_id;
        let attempt_id = open.as_ref().expect("observation").attempt_id;
        let opened = vec![
            event(obligation_id, None, "O", 0, obligation_payload(NO_ANCHOR, ChannelId::new(5_071), 77, (10, 20))),
            event(obligation_id, Some(attempt_id), "A", 1, json!({"attempt": 0, "frontier_start": 10, "frontier_end": 20})),
        ];
        let settled = settle_recovery_terminal(&mut open, Some(MessageId::new(901)), RecoverySettlement::FrontierPersisted);
        let window = [opened.clone(), settled.clone()].concat();
        assert!(window.iter().all(|event| event.receipt.is_none()),
            "no recovery event may carry a transport receipt: none is observable on this path");
        assert_eq!(classify_shadow_observation(&window, false), ShadowClassification::Unknown,
            "a persisted frontier without transport confirmation is not a candidate");
        assert_ne!(classify_shadow_observation(&window, true), ShadowClassification::CandidateDelivered,
            "an elapsed grace must not promote the ceiling into a delivery");
        assert_ne!(classify_shadow_observation(&opened, true), ShadowClassification::CandidateDelivered,
            "a recovery delivery whose funnel never answered is Unknown, not delivered");
    }
}
