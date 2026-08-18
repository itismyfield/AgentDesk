//! Relay verdict composition and authority — #5071 T4-B6 (4987 S3).
//!
//! Everything above this file observes. This file is the first one that
//! produces a `ReachabilityVerdict` from the durable materials the earlier
//! slices landed, folds the external tier into it, and hands the product to a
//! consumer:
//!
//! * [`classify_reachability`] is Tier A — 4987 §4.1's obligation↔receipt
//!   answer, built from the T4-B2c ledger, the T4-B3 receipt projection, and
//!   the T4-B4 coordinate comparison.
//! * [`compose_relay_verdict`] is 4987 §4.3-1's
//!   `worst(ReachabilityVerdict, ExternalRelayVerdict)`, with §4.3-2's
//!   monotone-worsening restriction on the external tier.
//! * [`relay_verdict_source`] is the 4987 §5.1 switch. Both modes compute and
//!   publish the composed verdict; only `Composite` lets it change the reported
//!   health polarity.
//!
//! # Still non-destructive (4987 §7.1 / I15)
//!
//! Composition adds authority over the health POLARITY and nothing else. No
//! value produced here cancels a turn, kills a tmux session or a process,
//! removes a registry entry, or force-cleans a mailbox or an in-flight row.
//! [`RelayVerdict::authorizes_destructive_action`] answers false on every
//! composed value, and
//! `services::discord::relay_recovery::plan_relay_recovery_under_reachability`
//! is written so a non-`Reachable` verdict can only replace a recovery action
//! with a milder one.
//!
//! # What this composition does NOT establish
//!
//! * The bounds below are chosen, not measured. 4987 §3.4 makes the age
//!   histogram the OUTPUT of the observation period, so the numbers here are a
//!   starting position to be replaced by that histogram, not a finding from it.
//! * `TransportUnknownEvidence::UnreleasedDeliveryLease` has no producer here.
//!   [`RelayVerdictProbe`] wires the placeholder leg and this file derives the
//!   restart-boundary leg; a range whose only trace is an unreleased lease is
//!   therefore not demoted out of `Unreachable` by this slice.
//! * Nothing here reads the in-flight row. The row's own path reaches this file
//!   only as `RelayVerdictProbe::row_output_path`, which is handed straight to
//!   `super::divergence` as a comparison operand (I14) and is never used to
//!   resolve, tail, or frame anything.
//! * The composed verdict is produced on the DETAIL health path only, because
//!   the pane-idle operand 4987 §-1.4 requires for a `Reachable` is derived
//!   from the relay-health snapshot that only that path builds. The public
//!   `/api/health` aggregate is unchanged by this slice in both modes.

use std::path::Path;

use serde::Serialize;

use crate::config::RelayVerdictSource;
use crate::services::discord::outbound::delivery_record::delivery_record_path;
use crate::services::discord::outbound::receipt_index::{
    ReceiptIndex, ReceiptIndexRead, read_receipt_index_at,
};
use crate::services::provider::ProviderKind;

use super::divergence::{CoordinateObservation, RowCoordinateDivergence, divergence};
use super::external_verdict::{
    ExternalRelayVerdict, classify_external_verdict_at, external_verdict_path,
};
use super::ledger::{
    LedgerObligation, ReachabilityLedger, ledger_file_exists, ledger_path, read_ledger_at,
};
use super::verdict::{
    NotAliveObligationState, ReachabilityUnknownReason, ReachabilityVerdict,
    TransportUnknownEvidence,
};

/// Obligation age at which composition stops reporting `Reachable`.
///
/// Chosen, not measured: it is four ticks of the observation cadence
/// (`health::STALL_WATCHDOG_INTERVAL_SECS`), which is the shortest gap at which
/// a missing receipt cannot still be explained by tick alignment between the
/// observer and the delivery path. 4987 §-1.4 counterexample 6 is the case this
/// bound exists for — a placeholder whose terminal receipt has not landed yet
/// is `Reachable` under this bound and `Degraded` over it.
const OBLIGATION_WARN_BOUND_SECS: u64 = 120;

/// Obligation age at which composition reports `Unreachable`, unless a
/// transport trace demotes it to `TransportUnknown` (4987 §-1.3b).
///
/// Chosen, not measured, on the same basis as the warn bound: it sits above the
/// longest single provider turn this code path has to tolerate without calling
/// a relay lost. Raising it costs detection latency; lowering it turns slow
/// turns into false `Unreachable`s, which is the direction 4987 §7 rules out.
const OBLIGATION_FAIL_BOUND_SECS: u64 = 600;

/// Which tier's claim the composition took.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::services::discord) enum RelayVerdictTier {
    /// Tier A (obligation ↔ receipt), including every case where the external
    /// tier said nothing or said something no worse.
    InBand,
    /// Tier B (the out-of-band watchdog sidecar), which reached this only by
    /// claiming strictly worse than Tier A.
    External,
}

/// 4987 §4.3-1's product: `worst(ReachabilityVerdict, ExternalRelayVerdict)`.
///
/// Both operands are kept rather than collapsed into one rung. The rung alone
/// would lose the two things later readers need: which tier is responsible
/// (an operator chases a different thing for each), and the in-band variant,
/// which is what carries §-1.3b's manual-redelivery ban notice even when the
/// external tier set the rung.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct RelayVerdict {
    in_band: ReachabilityVerdict,
    external: ExternalRelayVerdict,
    decided_by: RelayVerdictTier,
}

/// The shared ladder both tiers project onto.
///
/// Spelled out per variant instead of derived from declaration order, for the
/// same reason `ExternalRelayVerdict::severity` is: a reordered enum must not
/// silently reorder the authority.
///
/// `TransportUnknown` and `Unknown` share a rung. Neither permits health and
/// neither is `Unreachable`, and this file has no basis for ordering "a
/// transport we saw a trace of" against "an obligation set we could not
/// produce" — inventing one would put a false precision into the product.
fn in_band_rank(verdict: &ReachabilityVerdict) -> u8 {
    match verdict {
        ReachabilityVerdict::Reachable => 0,
        ReachabilityVerdict::Degraded { .. } => 1,
        ReachabilityVerdict::TransportUnknown { .. } | ReachabilityVerdict::Unknown { .. } => 2,
        ReachabilityVerdict::Unreachable { .. } => 3,
    }
}

/// The external tier's claim on the same ladder, or `None` when it made none.
///
/// `ExternalRelayVerdict::Unknown` is `None`, not rank 0: 4987 §-1.5 ① wants an
/// unusable sidecar read to leave the in-band verdict EXACTLY as it was, and a
/// rank of 0 would be a claim of "no loss" that the watchdog did not make.
fn external_rank(verdict: ExternalRelayVerdict) -> Option<u8> {
    match verdict {
        ExternalRelayVerdict::Unknown => None,
        ExternalRelayVerdict::NoLoss => Some(0),
        ExternalRelayVerdict::Lagging { .. } => Some(1),
        ExternalRelayVerdict::Unreachable { .. } => Some(3),
    }
}

/// 4987 §4.3-1 / §4.3-2.
///
/// The external tier displaces the in-band one only on a STRICTLY higher rank.
/// Equal ranks and lower ranks both keep Tier A, which is what makes §4.3-2's
/// "the external tier may only worsen" hold: a watchdog `ok` after an in-band
/// `Unreachable` cannot lift it, and an unusable sidecar read changes nothing
/// at all (4987 §-1.4 counterexample 5).
///
/// The structural signals (`RelayStallState`) are not operands here. 4987
/// §4.3-1 keeps them out of the product and gives them one job — choosing which
/// recovery action to consider — which is where they stay.
pub(in crate::services::discord) fn compose_relay_verdict(
    in_band: ReachabilityVerdict,
    external: ExternalRelayVerdict,
) -> RelayVerdict {
    let decided_by = match external_rank(external) {
        Some(rank) if rank > in_band_rank(&in_band) => RelayVerdictTier::External,
        _ => RelayVerdictTier::InBand,
    };
    RelayVerdict {
        in_band,
        external,
        decided_by,
    }
}

impl RelayVerdict {
    pub(in crate::services::discord) fn in_band(&self) -> &ReachabilityVerdict {
        &self.in_band
    }

    pub(in crate::services::discord) fn external(&self) -> ExternalRelayVerdict {
        self.external
    }

    pub(in crate::services::discord) fn decided_by(&self) -> RelayVerdictTier {
        self.decided_by
    }

    /// Whether this composed verdict permits a GREEN health polarity — 4987
    /// §4.1's rule carried through the composition.
    ///
    /// True only when Tier A spelled `Reachable` AND the external tier did not
    /// displace it. Every other composed value, including every `Unknown`, is
    /// false: §4.1 states directly that an unobservable relay is not a healthy
    /// one, and `Unknown` folding into GREEN is the exact regression this
    /// predicate exists to make impossible.
    pub(in crate::services::discord) fn permits_health(&self) -> bool {
        matches!(self.decided_by, RelayVerdictTier::InBand) && self.in_band.permits_health()
    }

    /// Whether an alarm for this composed verdict must carry §-1.3b's explicit
    /// "do not redeliver by hand" notice.
    ///
    /// Read off the IN-BAND operand even when the external tier set the rung. A
    /// watchdog `gap` over an in-band `TransportUnknown` does not make the
    /// crash window a loss; the range still has a transport trace, and a human
    /// who redelivers it still creates the duplicate #4986 refused to create.
    pub(in crate::services::discord) fn requires_manual_redelivery_ban_notice(&self) -> bool {
        self.in_band.requires_manual_redelivery_ban_notice()
    }

    /// Whether this composed verdict authorizes a destructive action — turn
    /// cancel, tmux/process kill, registry removal, mailbox/in-flight
    /// force-clean.
    ///
    /// **No composed value does** (4987 §7.1 / I15). Composition changes what
    /// health polarity may be declared; it grants no capability the operands
    /// did not have, and neither operand has this one. Spelled as an exhaustive
    /// match over the deciding tier plus the in-band delegate so a future tier
    /// has to choose here before it compiles.
    pub(in crate::services::discord) fn authorizes_destructive_action(&self) -> bool {
        match self.decided_by {
            RelayVerdictTier::InBand => self.in_band.authorizes_destructive_action(),
            // The external tier is a bounded read of somebody else's channel
            // history (4987 §5.2). It is even further from a destruction
            // warrant than Tier A, which already has none.
            RelayVerdictTier::External => false,
        }
    }

    /// The wire spelling of the composed rung, for the health detail surface.
    pub(in crate::services::discord) fn label(&self) -> &'static str {
        match self.decided_by {
            RelayVerdictTier::InBand => match self.in_band {
                ReachabilityVerdict::Reachable => "reachable",
                ReachabilityVerdict::Degraded { .. } => "degraded",
                ReachabilityVerdict::TransportUnknown { .. } => "transport_unknown",
                ReachabilityVerdict::Unknown { .. } => "unknown",
                ReachabilityVerdict::Unreachable { .. } => "unreachable",
            },
            RelayVerdictTier::External => match self.external {
                ExternalRelayVerdict::Unknown => "unknown",
                ExternalRelayVerdict::NoLoss => "reachable",
                ExternalRelayVerdict::Lagging { .. } => "degraded",
                ExternalRelayVerdict::Unreachable { .. } => "unreachable",
            },
        }
    }
}

/// The 4987 §4.4 `reachability { verdict, oldest_unsatisfied_age_secs,
/// uncovered_ranges, reason }` object, published on the health detail surface
/// in BOTH switch modes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(in crate::services::discord) struct RelayVerdictReport {
    pub verdict: &'static str,
    pub decided_by: RelayVerdictTier,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_unsatisfied_age_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uncovered_ranges: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_lost_blocks: Option<u32>,
    /// Whether this value was allowed to change the health polarity of the
    /// entry it sits on. False under `RelayVerdictSource::Structural`, where
    /// the same object is published purely as a shadow.
    pub governs_health_polarity: bool,
    /// 4987 §-1.3b's ban notice, carried onto the surface so an operator
    /// reading a non-GREEN entry is told not to redeliver by hand.
    pub manual_redelivery_banned: bool,
}

impl RelayVerdictReport {
    pub(in crate::services::discord) fn of(
        verdict: &RelayVerdict,
        governs_health_polarity: bool,
    ) -> Self {
        let (oldest_unsatisfied_age_secs, uncovered_ranges, reason) = match verdict.in_band() {
            ReachabilityVerdict::Reachable => (None, None, None),
            ReachabilityVerdict::Degraded {
                oldest_unsatisfied_age_secs,
                uncovered_ranges,
            }
            | ReachabilityVerdict::Unreachable {
                oldest_unsatisfied_age_secs,
                uncovered_ranges,
            } => (
                Some(*oldest_unsatisfied_age_secs),
                Some(*uncovered_ranges),
                None,
            ),
            ReachabilityVerdict::TransportUnknown {
                since_secs,
                evidence,
            } => (
                Some(*since_secs),
                None,
                Some(transport_evidence_str(*evidence)),
            ),
            ReachabilityVerdict::Unknown {
                reason,
                since_secs: _,
            } => (None, None, Some(unknown_reason_str(*reason))),
        };
        let external_lost_blocks = match verdict.external() {
            ExternalRelayVerdict::Lagging { lost_blocks }
            | ExternalRelayVerdict::Unreachable { lost_blocks } => Some(lost_blocks),
            ExternalRelayVerdict::Unknown | ExternalRelayVerdict::NoLoss => None,
        };
        Self {
            verdict: verdict.label(),
            decided_by: verdict.decided_by(),
            oldest_unsatisfied_age_secs,
            uncovered_ranges,
            reason,
            external_lost_blocks,
            governs_health_polarity,
            manual_redelivery_banned: verdict.requires_manual_redelivery_ban_notice(),
        }
    }
}

/// #5071 relay-tail S1 (I-5): one string per branch.
///
/// Five producers used to spell `"transcript_unresolved"`, which made the
/// published reason unable to say which of them answered — the exact question
/// #adk-cc's `unknown{transcript_unresolved}` could not be asked. Every arm
/// below is now reached by exactly one REACHABLE branch of
/// [`classify_reachability`] or [`observe_relay_verdict`]; r2 review (legB
/// P2): `receipt_store_unreadable` is spelled by two branches of
/// `classify_reachability`, the second of them unreachable behind the guard
/// that already answered it and kept so a reordering of those guards costs a
/// conservative verdict rather than the polling task.
fn unknown_reason_str(reason: ReachabilityUnknownReason) -> &'static str {
    match reason {
        ReachabilityUnknownReason::TranscriptUnresolved => "transcript_unresolved",
        ReachabilityUnknownReason::NeverObserved => "never_observed",
        ReachabilityUnknownReason::ProviderUnresolved => "provider_unresolved",
        ReachabilityUnknownReason::IncarnationNotAliveWitnessed(
            NotAliveObligationState::NoneOutstanding,
        ) => "incarnation_not_alive_no_obligations",
        ReachabilityUnknownReason::IncarnationNotAliveWitnessed(
            NotAliveObligationState::WithinGrace,
        ) => "incarnation_not_alive_within_grace",
        ReachabilityUnknownReason::TranscriptCoordinateDivergence => {
            "transcript_coordinate_divergence"
        }
        ReachabilityUnknownReason::RowlessActiveTurn => "rowless_active_turn",
        ReachabilityUnknownReason::ReadTruncated => "read_truncated",
        ReachabilityUnknownReason::ReceiptStoreUnreadable => "receipt_store_unreadable",
    }
}

fn transport_evidence_str(evidence: TransportUnknownEvidence) -> &'static str {
    match evidence {
        TransportUnknownEvidence::UnreleasedDeliveryLease => "unreleased_delivery_lease",
        TransportUnknownEvidence::RestartBoundaryCrossed => "restart_boundary_crossed",
        TransportUnknownEvidence::PlaceholderPresent => "placeholder_present",
    }
}

/// Whether the ledger's incarnation could be read at all, and the §-1.4
/// positive incarnation-alive evidence for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum TranscriptLiveness {
    /// Every rank of the 4987 §-1.3 resolution ladder failed, or no observation
    /// has ever run for this channel so there is nothing to resolve against.
    Unresolved,
    /// Resolved to a file of this length. `alive` is 4987 §-1.4's positive
    /// evidence — file length advanced since the last observation, OR the pane
    /// was confirmed idle. Both false means "we cannot see it", which §-1.4
    /// states is never GREEN.
    Resolved { eof: u64, alive: bool },
}

/// The Tier A materials one composition consumes. Every field is a fact some
/// earlier slice already produces; nothing here opens a file.
pub(in crate::services::discord) struct ReachabilityInputs<'a> {
    pub provider: &'a ProviderKind,
    /// T4-B4's row ↔ independently-resolved coordinate comparison.
    pub divergence: RowCoordinateDivergence,
    /// The durable ledger for this channel, or `None` when it could not be
    /// read. `ledger_present` distinguishes "never written" from "will not
    /// parse", which 4987 §-1.4 counterexample 7 requires.
    pub ledger: Option<&'a ReachabilityLedger>,
    pub ledger_present: bool,
    /// T4-B3's receipt projection read.
    pub receipts: &'a ReceiptIndexRead,
    pub transcript: TranscriptLiveness,
    /// The bounded per-tick read did not see the whole tail.
    pub read_truncated: bool,
    /// The mailbox reports an active turn with no in-flight row.
    pub rowless_active_turn: bool,
    /// A placeholder exists for the turn while its terminal receipt does not.
    pub placeholder_present: bool,
    pub now_epoch_ms: u64,
    /// This dcserver process's start time. An obligation first observed before
    /// it spans a restart boundary — 4987 §-1.3b's POST-succeeded /
    /// receipt-write-failed crash window.
    pub process_started_at_epoch_ms: u64,
}

/// What the coverage sweep could and could not retire.
struct CoverageSweep {
    /// Ages of obligations no receipt and no frontier covers.
    uncovered_ages_secs: Vec<u64>,
    /// Ages of obligations that ARE covered, but under a generation key with no
    /// additional witness. See [`classify_reachability`].
    unproven_ages_secs: Vec<u64>,
    /// The oldest obligation that could not be retired, whatever the reason.
    oldest_first_observed_at_epoch_ms: Option<u64>,
}

fn age_secs(now_epoch_ms: u64, first_observed_at_epoch_ms: u64) -> u64 {
    now_epoch_ms.saturating_sub(first_observed_at_epoch_ms) / 1_000
}

/// Sweep the live obligations against the receipt projection.
///
/// The index is clamped to the transcript EOF first: `ReceiptIndex::covers` has
/// no EOF input of its own and a stale-high same-generation frontier would
/// otherwise retire byte ranges that no longer exist. That clamp is the
/// consumer obligation the receipt-index module docs name.
///
/// `generation_proven` splits the covered obligations in two. The receipt
/// projection key is `(provider, tmux_session, generation_mtime_ns)`, and the
/// receipt-index module docs record that the bump-failure path can let a new
/// incarnation publish its predecessor's `generation_mtime_ns` — so under that
/// key alone, coverage may belong to the predecessor. The spawn nonce is the
/// additional witness: when the ledger carries one, the incarnation this
/// coverage is being read for is distinguishable from a same-generation
/// predecessor, and the obligation may be retired. When it does not, the
/// obligation is held instead of retired, and [`classify_reachability`] caps
/// what a held obligation can produce.
fn sweep_coverage(
    obligations: &[LedgerObligation],
    index: Option<ReceiptIndex>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    generation_mtime_ns: i64,
    generation_proven: bool,
    now_epoch_ms: u64,
) -> CoverageSweep {
    let mut sweep = CoverageSweep {
        uncovered_ages_secs: Vec::new(),
        unproven_ages_secs: Vec::new(),
        oldest_first_observed_at_epoch_ms: None,
    };
    for obligation in obligations {
        let covered = index.as_ref().is_some_and(|index| {
            index.covers(
                provider,
                tmux_session_name,
                generation_mtime_ns,
                (obligation.start, obligation.end),
            )
        });
        if covered && generation_proven {
            continue;
        }
        let age = age_secs(now_epoch_ms, obligation.first_observed_at_epoch_ms);
        if covered {
            sweep.unproven_ages_secs.push(age);
        } else {
            sweep.uncovered_ages_secs.push(age);
        }
        sweep.oldest_first_observed_at_epoch_ms = Some(
            sweep
                .oldest_first_observed_at_epoch_ms
                .map_or(obligation.first_observed_at_epoch_ms, |held| {
                    held.min(obligation.first_observed_at_epoch_ms)
                }),
        );
    }
    sweep
}

/// Produce the Tier A verdict — 4987 §4.1 / §-1.3b / §-1.4.
///
/// The `Unknown` arms are tried before the obligation ladder, because each of
/// them means the obligation set on hand is not the whole one, and grading an
/// incomplete set produces a confident answer about nothing. Within them the
/// order is: coordinate divergence, then store readability, then transcript
/// resolution, then read truncation, then the rowless-active-turn attribute.
/// Only the divergence-first step is load bearing (a diverged pair makes every
/// later operand ambiguous); the rest are mutually exclusive in practice and
/// their order is a convention this comment records rather than a claim.
///
/// This function reads no clock and opens no file: `now_epoch_ms` and every
/// material arrive from the caller.
///
/// #5071 relay-tail S1 (I-5): the `Unknown` arms name what they observed rather
/// than sharing one reason. The ORDER above is unchanged and so is every
/// verdict variant — only the label a branch hands to `Unknown` moved, and
/// `Unknown` permits no health whichever label it carries.
pub(in crate::services::discord) fn classify_reachability(
    inputs: ReachabilityInputs<'_>,
) -> ReachabilityVerdict {
    if let Some(reason) = inputs.divergence.unknown_reason() {
        return ReachabilityVerdict::unknown(reason, 0);
    }
    if matches!(inputs.receipts, ReceiptIndexRead::Unknown(_))
        || (inputs.ledger.is_none() && inputs.ledger_present)
    {
        // 4987 §-1.4 counterexample 7: a store that exists and will not parse is
        // `Unknown`, never `Unreachable`. Its coverage is unknown, not absent.
        return ReachabilityVerdict::unknown(ReachabilityUnknownReason::ReceiptStoreUnreadable, 0);
    }
    let Some(ledger) = inputs.ledger else {
        // No observation has ever recorded this channel. 4987 §-1.4: "not
        // observed" is not `Reachable` — and #5071 relay-tail S1 (I-5): it is
        // not an unresolved transcript either. Nothing here failed to resolve a
        // coordinate; no coordinate was ever framed.
        return ReachabilityVerdict::unknown(ReachabilityUnknownReason::NeverObserved, 0);
    };
    let TranscriptLiveness::Resolved { eof, alive } = inputs.transcript else {
        return ReachabilityVerdict::unknown(ReachabilityUnknownReason::TranscriptUnresolved, 0);
    };
    if inputs.read_truncated {
        return ReachabilityVerdict::unknown(ReachabilityUnknownReason::ReadTruncated, 0);
    }
    if inputs.rowless_active_turn {
        return ReachabilityVerdict::unknown(ReachabilityUnknownReason::RowlessActiveTurn, 0);
    }

    let index = match inputs.receipts {
        ReceiptIndexRead::Ready(index) => Some(index.clone().with_frontier_clamped_to_eof(eof)),
        // A genuinely absent store covers nothing. That is an answer, not a
        // fault: it is how a channel that has never delivered reads.
        ReceiptIndexRead::Absent => None,
        // Already answered above. Re-answered rather than `unreachable!`,
        // because this runs on the health poll and a reordering of the guards
        // should cost a conservative verdict, not the polling task.
        ReceiptIndexRead::Unknown(_) => {
            return ReachabilityVerdict::unknown(
                ReachabilityUnknownReason::ReceiptStoreUnreadable,
                0,
            );
        }
    };
    let sweep = sweep_coverage(
        ledger.live_obligations(),
        index,
        inputs.provider,
        &ledger.incarnation.tmux_session_name,
        ledger.incarnation.generation_mtime_ns,
        ledger.incarnation.spawn_nonce.is_some(),
        inputs.now_epoch_ms,
    );

    let oldest_uncovered = sweep.uncovered_ages_secs.iter().copied().max();
    let oldest_unproven = sweep.unproven_ages_secs.iter().copied().max();
    let held_ranges = (sweep.uncovered_ages_secs.len() + sweep.unproven_ages_secs.len()) as u32;
    let oldest_held = oldest_uncovered.max(oldest_unproven);

    match oldest_held {
        // Every obligation retired, or none was ever framed. 4987 §4.1 includes
        // the zero-obligation case, and §-1.4 gates it on the positive
        // incarnation-alive evidence checked here rather than on its absence.
        None => {
            if alive {
                ReachabilityVerdict::Reachable
            } else {
                // `since_secs` is 0: this reader knows the incarnation is not
                // visibly alive right now, and holds no record of when it
                // stopped being so.
                //
                // #5071 relay-tail S1 (I-5): the transcript resolved. What is
                // unknown is the producer, with nothing owed to it.
                ReachabilityVerdict::unknown(
                    ReachabilityUnknownReason::IncarnationNotAliveWitnessed(
                        NotAliveObligationState::NoneOutstanding,
                    ),
                    0,
                )
            }
        }
        Some(oldest) if oldest < OBLIGATION_WARN_BOUND_SECS => {
            // 4987 §-1.4 counterexample 6: inside the grace an unsatisfied
            // obligation is not yet evidence of anything, so the same alive
            // gate as the zero-obligation case decides.
            if alive {
                ReachabilityVerdict::Reachable
            } else {
                // #5071 relay-tail S1 (I-5): same not-alive producer as the
                // zero-obligation arm above, with an obligation outstanding —
                // `oldest` is how long it has been, and the grace it is inside
                // is why that is not yet evidence.
                ReachabilityVerdict::unknown(
                    ReachabilityUnknownReason::IncarnationNotAliveWitnessed(
                        NotAliveObligationState::WithinGrace,
                    ),
                    oldest,
                )
            }
        }
        Some(oldest) => {
            let past_fail = oldest_uncovered.is_some_and(|age| age >= OBLIGATION_FAIL_BOUND_SECS);
            if !past_fail {
                return ReachabilityVerdict::Degraded {
                    oldest_unsatisfied_age_secs: oldest,
                    uncovered_ranges: held_ranges,
                };
            }
            // Past `fail_bound` with at least one genuinely uncovered range.
            // A trace of the transport demotes this to `TransportUnknown`
            // (4987 §-1.3b); without one it is the §-1.4 counterexample 2
            // true positive.
            match transport_evidence(
                &sweep,
                inputs.placeholder_present,
                inputs.process_started_at_epoch_ms,
            ) {
                Some(evidence) => ReachabilityVerdict::TransportUnknown {
                    since_secs: oldest,
                    evidence,
                },
                None => ReachabilityVerdict::Unreachable {
                    oldest_unsatisfied_age_secs: oldest,
                    uncovered_ranges: held_ranges,
                },
            }
        }
    }
}

/// Which trace, if any, says the transport happened without a receipt.
///
/// Restart-boundary first: it is the window §-1.3b was created for (a POST that
/// succeeded and a receipt write that did not survive the crash), and it is the
/// one whose alarm wording differs. A placeholder is the weaker trace — it says
/// a turn started, not that its bytes reached Discord — so it only answers when
/// the restart boundary does not.
fn transport_evidence(
    sweep: &CoverageSweep,
    placeholder_present: bool,
    process_started_at_epoch_ms: u64,
) -> Option<TransportUnknownEvidence> {
    if sweep
        .oldest_first_observed_at_epoch_ms
        .is_some_and(|first_observed| first_observed < process_started_at_epoch_ms)
    {
        return Some(TransportUnknownEvidence::RestartBoundaryCrossed);
    }
    if placeholder_present {
        return Some(TransportUnknownEvidence::PlaceholderPresent);
    }
    None
}

/// What the live health path knows about one channel when it asks for a
/// composed verdict.
pub(in crate::services::discord) struct RelayVerdictProbe<'a> {
    /// `None` when the health registry could not resolve the provider name to a
    /// known kind. Every durable store this composition reads is addressed by
    /// provider, so without one there is nothing to read — which composes to
    /// `Unknown`, not to a pass.
    pub provider: Option<&'a ProviderKind>,
    pub channel_id: u64,
    /// The in-flight row's transcript path. Handed to `super::divergence` as a
    /// comparison operand and to nothing else (I14).
    pub row_output_path: Option<&'a str>,
    /// The registry's independently resolved transcript path.
    pub registry_output_path: Option<&'a str>,
    /// 4987 §-1.4's second alive witness: the pane is up and has nothing
    /// pending, so a transcript that is not growing is idle rather than dead.
    pub pane_idle_confirmed: bool,
    pub rowless_active_turn: bool,
    /// A placeholder message is outstanding for this channel.
    pub placeholder_present: bool,
    pub now_epoch_ms: u64,
    pub process_started_at_epoch_ms: u64,
}

/// Read this channel's durable materials and compose one verdict.
///
/// Three small reads: the T4-B2c ledger, the T4-B3 receipt projection, and the
/// T4-B5 sidecar. Each is a whole-file read of a record the writers publish by
/// atomic rename, so a concurrent writer shows this reader the old bytes or the
/// new bytes and never a torn record. It takes no lock and mutates nothing.
///
/// The sidecar is gated on the ledger's own incarnation, so a sidecar written
/// for a previous incarnation classifies as `WrongIncarnation` and contributes
/// `ExternalRelayVerdict::Unknown` — which by [`compose_relay_verdict`] leaves
/// the in-band verdict untouched. `accepted_epoch` is `None` on every call:
/// this reader keeps no cross-tick state, so it accepts whatever epoch the
/// sidecar currently carries and cannot detect a watchdog epoch regression
/// between two of its own reads.
pub(in crate::services::discord) fn observe_relay_verdict(
    probe: RelayVerdictProbe<'_>,
) -> RelayVerdict {
    let divergence_outcome = divergence(
        CoordinateObservation::observe(probe.row_output_path),
        CoordinateObservation::observe(probe.registry_output_path),
    );

    let Some(provider) = probe.provider else {
        // #5071 relay-tail S1 (I-5): no provider owns this channel, so no
        // ledger, receipt projection or sidecar can even be located. That is
        // upstream of every rank of the resolution ladder, not a failure of it.
        return compose_relay_verdict(
            ReachabilityVerdict::unknown(ReachabilityUnknownReason::ProviderUnresolved, 0),
            ExternalRelayVerdict::Unknown,
        );
    };

    let ledger_path = ledger_path(provider, probe.channel_id);
    let ledger = ledger_path.as_deref().and_then(read_ledger_at);
    let ledger_present = ledger_path.as_deref().is_some_and(ledger_file_exists);

    let receipts = delivery_record_path(provider, probe.channel_id)
        .as_deref()
        .map_or(ReceiptIndexRead::Absent, read_receipt_index_at);

    let transcript = ledger
        .as_ref()
        .map_or(TranscriptLiveness::Unresolved, |ledger| {
            transcript_liveness(
                probe.registry_output_path,
                ledger,
                probe.pane_idle_confirmed,
            )
        });

    let in_band = classify_reachability(ReachabilityInputs {
        provider,
        divergence: divergence_outcome,
        ledger: ledger.as_ref(),
        ledger_present,
        receipts: &receipts,
        transcript,
        // The bounded read happens inside the observation task, which records
        // its truncation in the ledger it writes. This reader does not tail,
        // so it has no truncation of its own to report.
        read_truncated: false,
        rowless_active_turn: probe.rowless_active_turn,
        placeholder_present: probe.placeholder_present,
        now_epoch_ms: probe.now_epoch_ms,
        process_started_at_epoch_ms: probe.process_started_at_epoch_ms,
    });

    let external = ledger
        .as_ref()
        .map_or(ExternalRelayVerdict::Unknown, |ledger| {
            external_verdict_path(provider, probe.channel_id)
                .as_deref()
                .map_or(ExternalRelayVerdict::Unknown, |path| {
                    classify_external_verdict_at(path, &ledger.incarnation, None).verdict()
                })
        });

    compose_relay_verdict(in_band, external)
}

/// Resolve the registry's transcript and decide 4987 §-1.4's alive question.
///
/// Growth is measured against the ledger's `last_observed_len`, which the
/// observation task stamps each tick. A file longer than that stamp advanced
/// between the two reads. Equal lengths are not a claim that it died — that is
/// what `pane_idle_confirmed` answers instead.
fn transcript_liveness(
    registry_output_path: Option<&str>,
    ledger: &ReachabilityLedger,
    pane_idle_confirmed: bool,
) -> TranscriptLiveness {
    let Some(path) = registry_output_path
        .map(str::trim)
        .filter(|path| !path.is_empty())
    else {
        return TranscriptLiveness::Unresolved;
    };
    let Ok(metadata) = std::fs::metadata(Path::new(path)) else {
        return TranscriptLiveness::Unresolved;
    };
    if !metadata.is_file() {
        return TranscriptLiveness::Unresolved;
    }
    let eof = metadata.len();
    TranscriptLiveness::Resolved {
        eof,
        alive: eof > ledger.last_observed_len || pane_idle_confirmed,
    }
}

#[cfg(test)]
static RELAY_VERDICT_SOURCE_OVERRIDE: std::sync::Mutex<Option<RelayVerdictSource>> =
    std::sync::Mutex::new(None);

/// The live 4987 §5.1 switch. Same shape as `execution_identity_mode`: a live
/// `agentdesk.yaml` edit applies on the next read without a restart, and an
/// unreadable config falls back to the compiled default (`Structural`).
pub(in crate::services::discord) fn relay_verdict_source() -> RelayVerdictSource {
    #[cfg(test)]
    if let Some(source) = *RELAY_VERDICT_SOURCE_OVERRIDE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
    {
        return source;
    }
    crate::config_live_reload::current()
        .map(|config| config.runtime.relay_verdict_source)
        .unwrap_or_default()
}

#[cfg(test)]
pub(in crate::services::discord) struct RelayVerdictSourceGuard {
    previous: Option<RelayVerdictSource>,
}

#[cfg(test)]
impl Drop for RelayVerdictSourceGuard {
    fn drop(&mut self) {
        *RELAY_VERDICT_SOURCE_OVERRIDE
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = self.previous.take();
    }
}

#[cfg(test)]
pub(in crate::services::discord) fn set_relay_verdict_source_for_tests(
    source: RelayVerdictSource,
) -> RelayVerdictSourceGuard {
    let previous = RELAY_VERDICT_SOURCE_OVERRIDE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .replace(source);
    RelayVerdictSourceGuard { previous }
}

#[cfg(test)]
#[path = "composite_tests.rs"]
mod composite_tests;
