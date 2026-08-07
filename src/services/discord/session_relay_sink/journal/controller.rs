//! #5071 T1 S4 — the turn_bridge / controller cutover family.
//!
//! Three production sites in `turn_bridge/terminal_controller_cutover.rs` write
//! the durable delivered frontier: the A5 short-replace cutover
//! (`shadow_mirror_delivered_frontier`), the S1-d long-chunk cutover and the
//! pre-cutover long-chunk arm (both via `record_long_chunk_terminal_delivery`).
//! This module is the only way those sites reach the journal.
//!
//! Shadow only: nothing here is read back by live delivery, and a row exists
//! only when a PG pool, `DeliveryJournalMode::Shadow` and the cohort selection
//! all agree. If this observation ever changes what production delivers, that is
//! a bug — the facade reads process state, never mutates it (see
//! [`confirmed_end_generation_mtime_ns`]).
//!
//! ## The receipt ceiling: this family can never be `CandidateDelivered`
//!
//! The sink and the watcher settle with a real `DiscordTransportReceipt` read
//! off the Discord response, so their `T` event can be checked for a
//! requested/returned channel mismatch. The controller path has no such value.
//! The production `TurnGateway` hands `replace_long_message_raw_with_outcome`
//! its receipt slot as `&mut None` (`gateway.rs`) and returns only
//! `ReplaceLongMessageOutcome`; `send_long_message_with_rollback` returns bare
//! `Vec<MessageId>`; `toc::DeliveryOutcome` carries no receipt either. The only
//! transport evidence that survives to these sites is the anchor pair the
//! durable frontier already records.
//!
//! Synthesising a receipt from that anchor would make `requested == returned` by
//! construction — a `T` that can never trip the `channel_mismatch` branch, which
//! is exactly the forgery `journal.rs` refuses to commit for the in-process
//! gateway. So this family emits `O`+`A` before transport and `C` (or `U`)
//! after, and never `T`. `classify_shadow_observation` consequently reports
//! `Unknown` for every controller obligation — "a commit without transport
//! confirmation is not a candidate", the rule journal.rs already tests. That is
//! the honest ceiling and C5 pins it; lifting it means plumbing a receipt
//! through `TurnGateway`, which is not this slice.
//!
//! ## What `C` asserts, and what it does not
//!
//! `C` carries the site's OWN commit decision — the same boolean that gates the
//! durable-frontier write next to it (`outcome_is_shadow_delivered`, the
//! `Delivered { new_chunks: Some(..) }` arm, the legacy `commit_and_advance`
//! result). It does NOT assert that the durable mirror persisted: the public
//! `shadow_mirror_delivered_frontier` discards its inner result, so no caller at
//! these sites can observe it. Joining those funnels is #5071 T1 S7's work, not
//! this slice's.

use std::sync::atomic::Ordering;

use poise::serenity_prelude::{ChannelId, MessageId};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::services::discord::SharedData;
use crate::services::provider::ProviderKind;

use super::{
    AppendCommand, JOURNAL_NAMESPACE, JournalEvent, admit, event, process_observer, push_field,
};

/// Which of the three cutover-family sites opened the obligation. Closed, so a
/// fourth durable-frontier writer cannot appear in that file without naming
/// itself, and so the disposition participates in obligation identity (two sites
/// observing the same byte range are separate obligations, not a slot
/// collision).
#[rustfmt::skip]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum ControllerDisposition {
    /// #3089 A5 — `deliver_short_replace_via_controller`.
    ShortReplace,
    /// #3998 S1-d — `deliver_long_chunks_via_controller`.
    LongChunks,
    /// The pre-cutover arm — `apply_bridge_long_chunks_legacy`.
    LongChunksLegacy,
}

impl ControllerDisposition {
    #[rustfmt::skip]
    fn as_str(self) -> &'static str {
        match self {
            Self::ShortReplace => "controller_short_replace",
            Self::LongChunks => "controller_long_chunks",
            Self::LongChunksLegacy => "controller_long_chunks_legacy",
        }
    }
}

/// Read-only. Deliberately NOT `SharedData::tmux_relay_coord`, which inserts a
/// coord on first access: an observer must not mutate process state, and an
/// obligation that never opens must leave no trace at all. `0` — no coord, or an
/// unknown wrapper incarnation — is the same sentinel the durable writer refuses
/// to scope a frontier to (#5154).
fn confirmed_end_generation_mtime_ns(shared: &SharedData, channel: ChannelId) -> i64 {
    shared
        .tmux_relay_coords
        .get(&channel)
        .map(|coord| {
            coord
                .confirmed_end_generation_mtime_ns
                .load(Ordering::Acquire)
        })
        .unwrap_or(0)
}

/// Pure admission of the coordinates themselves, so the three refusals are
/// testable without a PG pool (which alone makes every `begin` return `None`).
///
/// The family observes a range only when the site holds one, when it is a real
/// `[start, end)`, and when the wrapper generation is known. Those are the same
/// three cases in which the durable frontier is or is not generation-scoped, so
/// the journal's obligation set cannot drift from the writer's. Keeping the
/// generation in identity is what makes a repeat of the same range under a NEW
/// incarnation a NEW obligation rather than a payload conflict on an old slot.
pub(super) fn controller_obligation_range(
    range: Option<(u64, u64)>,
    generation_mtime_ns: i64,
) -> Option<(u64, u64)> {
    range.filter(|range| range.1 > range.0 && generation_mtime_ns != 0)
}

/// `UUIDv5` over the canonical coordinates. The leading `controller` field is
/// what keeps this family's obligations disjoint from the watcher's over the
/// same channel/range, independently of how the disposition strings evolve.
pub(super) fn controller_obligation_id(
    provider: &ProviderKind,
    frontier_channel: ChannelId,
    generation_mtime_ns: i64,
    range: (u64, u64),
    disposition: ControllerDisposition,
) -> Uuid {
    let mut bytes = Vec::new();
    for value in [
        "controller",
        provider.as_str(),
        &frontier_channel.get().to_string(),
        disposition.as_str(),
    ] {
        push_field(&mut bytes, value);
    }
    bytes.extend_from_slice(&generation_mtime_ns.to_be_bytes());
    bytes.extend_from_slice(&range.0.to_be_bytes());
    bytes.extend_from_slice(&range.1.to_be_bytes());
    Uuid::new_v5(&JOURNAL_NAMESPACE, &bytes)
}

#[rustfmt::skip]
fn obligation_payload(disposition: ControllerDisposition, anchor_channel: ChannelId, range: (u64, u64)) -> Value {
    json!({"source": "controller", "disposition_class": disposition.as_str(),
        "anchor_channel_id": anchor_channel.get(),
        "frontier_start": range.0, "frontier_end": range.1})
}

/// An open controller obligation, held across the transport. It keeps the pool
/// and the raw event constructors out of the anchor file's reach.
pub(in crate::services::discord) struct ControllerTerminalObservation {
    obligation_id: Uuid,
    attempt_id: Uuid,
    anchor_channel: ChannelId,
    range: (u64, u64),
    pool: sqlx::PgPool,
}

/// Open the obligation BEFORE transport, so a delivery that vanishes mid-POST
/// leaves a dangling `O`+`A` instead of no trace at all.
///
/// `channels` is `(frontier, anchor)`: the frontier channel is the OFFSET
/// AUTHORITY the durable write is keyed by (`watcher_owner_channel_id`), the
/// anchor channel is the EDIT TARGET the anchor message lives in (`channel_id`).
/// A recovered/reused-watcher bridge resolves these differently, so the pair is
/// recorded exactly as #3610 PR-1b records it durably.
///
/// `range` is `Option` because the legacy long-chunk arm only has one when it
/// actually holds a lease: no lease, no obligation. That keeps the family
/// boundary behavioural rather than positional.
#[rustfmt::skip]
pub(in crate::services::discord) fn begin_controller_terminal(
    shared: &SharedData,
    provider: &ProviderKind,
    disposition: ControllerDisposition,
    channels: (ChannelId, ChannelId),
    range: Option<(u64, u64)>,
) -> Option<ControllerTerminalObservation> {
    let (frontier_channel, anchor_channel) = channels;
    let generation_mtime_ns = confirmed_end_generation_mtime_ns(shared, frontier_channel);
    let range = controller_obligation_range(range, generation_mtime_ns)?;
    let obligation_id =
        controller_obligation_id(provider, frontier_channel, generation_mtime_ns, range, disposition);
    let pool = admit(shared, frontier_channel, obligation_id)?;
    let attempt_id = Uuid::new_v5(&obligation_id, b"attempt:0");
    process_observer().submit(AppendCommand {
        pool: pool.clone(),
        events: vec![
            event(obligation_id, None, "O", 0, obligation_payload(disposition, anchor_channel, range)),
            event(obligation_id, Some(attempt_id), "A", 1, json!({"attempt": 0,
                "frontier_start": range.0, "frontier_end": range.1})),
        ],
    });
    Some(ControllerTerminalObservation { obligation_id, attempt_id, anchor_channel, range, pool })
}

/// The single terminal event. `C` takes slot 3 and `U` slot 2, exactly as the
/// sink's `finish_fresh` places them; slot 2's `T` stays empty because no
/// transport receipt is observable here (see the module docs).
#[rustfmt::skip]
fn terminal_events(observation: &ControllerTerminalObservation, anchor_msg_id: Option<MessageId>, committed: bool) -> Vec<JournalEvent> {
    let (kind, seq) = if committed { ("C", 3) } else { ("U", 2) };
    let mut payload = json!({"frontier_start": observation.range.0, "frontier_end": observation.range.1,
        "anchor_channel_id": observation.anchor_channel.get(),
        "anchor_msg_id": anchor_msg_id.map(|id| id.get())});
    if !committed { payload["reason"] = json!("controller_delivery_not_committed"); }
    vec![event(observation.obligation_id, Some(observation.attempt_id), kind, seq, payload)]
}

/// Close the obligation. `committed` is the site's own commit decision — the
/// boolean that gates the durable-frontier write beside it.
///
/// Single-use, like the sink's `settle` and the watcher's
/// `settle_watcher_terminal`: the observation is consumed, so a second call
/// emits nothing. The long-chunk sites rely on that directly — they settle
/// `true` inside their commit arm and then call this once more with
/// `(None, false)` on the way out, which closes the obligation as `U` only when
/// the commit arm did not run.
pub(in crate::services::discord) fn settle_controller_terminal(
    observation: &mut Option<ControllerTerminalObservation>,
    anchor_msg_id: Option<MessageId>,
    committed: bool,
) -> Vec<JournalEvent> {
    let Some(observation) = observation.take() else {
        return Vec::new();
    };
    let events = terminal_events(&observation, anchor_msg_id, committed);
    process_observer().submit(AppendCommand {
        pool: observation.pool,
        events: events.clone(),
    });
    events
}

#[rustfmt::skip]
#[cfg(test)]
mod controller_terminal_semantics_tests {
    //! #5071 T1 S4 C1-C5. RUNTIME semantic assertions: each test calls the
    //! production function and inspects its value, so a mutation that still
    //! compiles but changes what the code means fails here.
    use super::super::watcher::{WatcherObligationCoordinates, watcher_obligation_id};
    use super::super::{ShadowClassification, classify_shadow_observation};
    use super::*;

    const SHORT: ControllerDisposition = ControllerDisposition::ShortReplace;

    fn observation(range: (u64, u64)) -> Option<ControllerTerminalObservation> {
        Some(ControllerTerminalObservation {
            obligation_id: Uuid::from_u128(21), attempt_id: Uuid::from_u128(22),
            anchor_channel: ChannelId::new(4_243), range,
            pool: sqlx::Pool::<sqlx::Postgres>::connect_lazy("postgres://localhost/agentdesk_test")
                .expect("lazy test pool URL is valid"),
        })
    }

    fn obligation(channel: u64, generation: i64, range: (u64, u64), disposition: ControllerDisposition) -> Uuid {
        controller_obligation_id(&ProviderKind::Claude, ChannelId::new(channel), generation, range, disposition)
    }

    /// C1 (kills M-C1). Obligation identity must move with every coordinate, or
    /// two different deliveries collapse onto one row and the second one's
    /// payload becomes an `InvariantConflict` instead of an observation.
    #[test]
    fn c1_controller_obligation_identity_covers_every_coordinate() {
        let base = obligation(4_242, 77, (10, 20), SHORT);
        assert_eq!(base, obligation(4_242, 77, (10, 20), SHORT), "deterministic");
        assert_ne!(base, obligation(4_243, 77, (10, 20), SHORT), "frontier channel participates");
        assert_ne!(base, obligation(4_242, 78, (10, 20), SHORT), "wrapper generation participates");
        assert_ne!(base, obligation(4_242, 77, (11, 20), SHORT), "start offset participates");
        assert_ne!(base, obligation(4_242, 77, (10, 21), SHORT), "end offset participates");
        for other in [ControllerDisposition::LongChunks, ControllerDisposition::LongChunksLegacy] {
            assert_ne!(base, obligation(4_242, 77, (10, 20), other), "{other:?} is its own disposition class");
        }
        assert_ne!(base, controller_obligation_id(&ProviderKind::Codex, ChannelId::new(4_242), 77, (10, 20), SHORT),
            "provider participates");
    }

    /// C2 (kills M-C2). The controller and the watcher both observe terminal
    /// deliveries keyed by channel + generation + range. If their obligation ids
    /// could coincide, one family's `A` and the other's would compete for slot 1
    /// over the same delivery.
    #[test]
    fn c2_controller_and_watcher_obligations_never_coincide() {
        let watcher = watcher_obligation_id(
            WatcherObligationCoordinates { provider: &ProviderKind::Claude, channel_id: ChannelId::new(4_242),
                tmux_session_name: "adk-claude-s4", generation_mtime_ns: 77, range: (10, 20) },
            "controller_short_replace",
        );
        assert_ne!(obligation(4_242, 77, (10, 20), SHORT), watcher,
            "the leading `controller` field keeps the two families disjoint even under an identical disposition string");
    }

    /// C3 (kills M-C3). The three refusals, tested where they are decidable: a
    /// pool-less test process makes every `begin_controller_terminal` return
    /// `None`, so asserting on the facade would prove nothing.
    #[test]
    fn c3_only_a_real_range_under_a_known_generation_is_an_obligation() {
        assert_eq!(controller_obligation_range(Some((10, 20)), 77), Some((10, 20)), "the ordinary case observes");
        assert_eq!(controller_obligation_range(None, 77), None, "no lease range, no obligation");
        assert_eq!(controller_obligation_range(Some((10, 10)), 77), None, "an empty range advances nothing");
        assert_eq!(controller_obligation_range(Some((20, 10)), 77), None, "an inverted range is not a frontier");
        assert_eq!(controller_obligation_range(Some((10, 20)), 0), None,
            "an unknown wrapper incarnation is exactly where the durable writer declines to scope a frontier (#5154)");
    }

    /// C4 (kills M-C4). The commit decision selects the terminal event, and the
    /// observation is single-use — which is what makes the long-chunk sites'
    /// trailing `(None, false)` settle a no-op after their commit arm ran.
    #[tokio::test]
    async fn c4_settle_selects_the_terminal_event_and_is_single_use() {
        let mut committed = observation((10, 20));
        let events = settle_controller_terminal(&mut committed, Some(MessageId::new(901)), true);
        assert_eq!(events.iter().map(|event| event.kind).collect::<Vec<_>>(), vec!["C"],
            "a committed controller delivery emits exactly one C");
        assert_eq!(events[0].seq, 3, "C takes the sink's commit slot");
        assert_eq!(events[0].canonical_payload["anchor_msg_id"], 901);
        assert_eq!(events[0].canonical_payload["frontier_end"], 20);
        assert!(events[0].canonical_payload.get("reason").is_none(), "a commit needs no failure reason");
        assert!(committed.is_none(), "a settled observation is consumed");
        assert!(settle_controller_terminal(&mut committed, Some(MessageId::new(901)), true).is_empty(),
            "a second settle on the same observation must emit nothing");
        assert!(settle_controller_terminal(&mut committed, None, false).is_empty(),
            "the sites' trailing unconfirmed settle must not append after a commit");

        let mut uncommitted = observation((10, 20));
        let events = settle_controller_terminal(&mut uncommitted, None, false);
        assert_eq!(events.iter().map(|event| event.kind).collect::<Vec<_>>(), vec!["U"],
            "a delivery the site did not commit is Unknown, never C");
        assert_eq!(events[0].seq, 2, "U takes the slot T would have taken");
        assert_eq!(events[0].canonical_payload["reason"], "controller_delivery_not_committed");
        assert!(settle_controller_terminal(&mut None, Some(MessageId::new(901)), true).is_empty(),
            "an unobserved delivery settles into nothing");
    }

    /// C5 (kills M-C5). The declared ceiling. No controller event may carry a
    /// transport receipt, because none is observable on this path, and the
    /// classifier must therefore refuse to call the family delivered. A future
    /// slice that synthesises a receipt from the anchor pair — `requested ==
    /// returned` by construction, so the mismatch branch could never fire —
    /// fails here rather than silently promoting forgeries to Delivered.
    #[tokio::test]
    async fn c5_controller_family_never_classifies_as_delivered() {
        let mut open = observation((10, 20));
        let obligation_id = open.as_ref().expect("observation").obligation_id;
        let attempt_id = open.as_ref().expect("observation").attempt_id;
        let opened = vec![
            event(obligation_id, None, "O", 0, obligation_payload(SHORT, ChannelId::new(4_243), (10, 20))),
            event(obligation_id, Some(attempt_id), "A", 1, json!({"attempt": 0, "frontier_start": 10, "frontier_end": 20})),
        ];
        let settled = settle_controller_terminal(&mut open, Some(MessageId::new(901)), true);
        let window = [opened.clone(), settled.clone()].concat();
        assert!(window.iter().all(|event| event.receipt.is_none()),
            "no controller event may carry a transport receipt: none is observable on this path");
        assert_eq!(classify_shadow_observation(&window, false), ShadowClassification::Unknown,
            "a commit without transport confirmation is not a candidate");
        assert_ne!(classify_shadow_observation(&window, true), ShadowClassification::CandidateDelivered,
            "an elapsed grace must not promote the ceiling into a delivery");
        assert_ne!(classify_shadow_observation(&opened, true), ShadowClassification::CandidateDelivered,
            "a delivery that vanished after the open is Unknown, not delivered");
    }
}
