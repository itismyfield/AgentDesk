//! Relay verdict composition and authority — #5071 T4-B6 (4987 S3).
//!
//! Produces a `ReachabilityVerdict` from the durable materials earlier slices
//! land, folds in the external tier, and hands the product to a consumer.
//!
//! * [`classify_reachability`] is Tier A — 4987 §4.1's obligation↔receipt answer.
//! * [`compose_relay_verdict`] is 4987 §4.3-1's `worst(ReachabilityVerdict,
//!   ExternalRelayVerdict)`, restricted by §4.3-2 to only worsen.
//! * [`relay_verdict_source`] is the 4987 §5.1 switch; only `Composite` lets
//!   the composed verdict change the reported health polarity.
//!
//! [`RelayVerdict::authorizes_destructive_action`] is false on every composed
//! value (4987 §7.1 / I15) — destructive admission stays at the separate
//! `relay_recovery::destructive_warrant_bind` gate.

use std::path::Path;

use serde::Serialize;

use crate::config::RelayVerdictSource;
use crate::services::discord::outbound::delivery_record::delivery_record_path;
use crate::services::discord::outbound::receipt_index::{
    ReceiptIndex, ReceiptIndexRead, read_receipt_index_at,
};
use crate::services::discord::relay_health::RelayHealthSnapshot;
use crate::services::provider::ProviderKind;

use super::super::session_enrichment::ExecutorWitness;
use super::divergence::{CoordinateObservation, RowCoordinateDivergence, divergence};
use super::external_verdict::{
    ExternalRelayVerdict, classify_external_verdict_at, external_verdict_path,
};
use super::ledger::{
    LedgerObligation, ReachabilityLedger, ledger_file_exists, ledger_path, read_ledger_at,
};
use super::ledger_ttl::{EXPIRED_REASON, expired_without_a_producer, ledger_committed_at_epoch_ms};
use super::observation::REACHABILITY_OBSERVATION_INTERVAL_SECS;
use super::verdict::{
    NotAliveObligationState, ReachabilityUnknownReason, ReachabilityVerdict,
    TransportUnknownEvidence,
};

/// Obligation age at which composition stops reporting `Reachable`. Chosen,
/// not measured: four ticks of `health::STALL_WATCHDOG_INTERVAL_SECS`, the
/// shortest gap a missing receipt can't still be tick-alignment noise (4987
/// §-1.4 counterexample 6).
const OBLIGATION_WARN_BOUND_SECS: u64 = 120;

/// Obligation age at which composition reports `Unreachable`, unless a
/// transport trace demotes it to `TransportUnknown` (4987 §-1.3b). Chosen,
/// not measured: above the longest single provider turn tolerated before
/// calling a relay lost (4987 §7).
const OBLIGATION_FAIL_BOUND_SECS: u64 = 600;

/// Turn age past which a reconfirmed token-without-row stops passing health
/// (4987 §6.3 row 1): two observation ticks. Deliberately not the stall
/// classifier's `UNPAIRED_ACTIVE_TOKEN_GRACE_SECS` — that one decides when to
/// call the shape stuck, this one how long health may vouch for a turn with
/// no durable row.
pub(in crate::services::discord) const ROWLESS_REACHABILITY_GRACE_SECS: u64 =
    2 * REACHABILITY_OBSERVATION_INTERVAL_SECS;

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
/// Both operands are kept rather than collapsed into one rung: readers need
/// which tier is responsible, and the in-band variant carries §-1.3b's
/// manual-redelivery ban notice even when the external tier set the rung.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct RelayVerdict {
    in_band: ReachabilityVerdict,
    external: ExternalRelayVerdict,
    decided_by: RelayVerdictTier,
}

/// The shared ladder both tiers project onto. Spelled out per variant instead
/// of derived from declaration order, so a reordered enum can't silently
/// reorder the authority. `TransportUnknown` and `Unknown` share a rung —
/// there is no basis to order "a transport trace" above "an obligation set
/// we could not produce" without false precision.
fn in_band_rank(verdict: &ReachabilityVerdict) -> u8 {
    match verdict {
        // `Expired` shares rank 0 with `Reachable`: it claims no loss for the
        // external tier to outrank, not health — `permits_health` is false for
        // it, and a watchdog that DID see loss still degrades it (#5942).
        ReachabilityVerdict::Reachable | ReachabilityVerdict::Expired { .. } => 0,
        ReachabilityVerdict::Degraded { .. } => 1,
        ReachabilityVerdict::TransportUnknown { .. } | ReachabilityVerdict::Unknown { .. } => 2,
        ReachabilityVerdict::Unreachable { .. } => 3,
    }
}

/// The external tier's claim on the same ladder, or `None` when it made none.
/// `Unknown` is `None`, not rank 0: 4987 §-1.5 ① wants an unusable sidecar
/// read to leave the in-band verdict unchanged, not claim "no loss".
fn external_rank(verdict: ExternalRelayVerdict) -> Option<u8> {
    match verdict {
        ExternalRelayVerdict::Unknown => None,
        ExternalRelayVerdict::NoLoss => Some(0),
        ExternalRelayVerdict::Lagging { .. } => Some(1),
        ExternalRelayVerdict::Unreachable { .. } => Some(3),
    }
}

/// 4987 §4.3-1 / §4.3-2: the external tier displaces the in-band one only on
/// a STRICTLY higher rank; equal and lower ranks keep Tier A, which is what
/// makes §4.3-2's "external tier may only worsen" hold. `RelayStallState`
/// structural signals are not operands here.
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

    /// Whether this composed verdict permits a GREEN health polarity (4987
    /// §4.1): true only when Tier A spelled `Reachable` AND the external tier
    /// did not displace it — every `Unknown` is false.
    pub(in crate::services::discord) fn permits_health(&self) -> bool {
        matches!(self.decided_by, RelayVerdictTier::InBand) && self.in_band.permits_health()
    }

    /// Whether this composed verdict withdraws from the health polarity
    /// rather than deciding it (#5942): true only when Tier A itself expired
    /// AND the external tier did not displace it.
    pub(in crate::services::discord) fn abstains_from_health_polarity(&self) -> bool {
        matches!(self.decided_by, RelayVerdictTier::InBand)
            && self.in_band.abstains_from_health_polarity()
    }

    /// Whether an alarm for this composed verdict must carry §-1.3b's
    /// "do not redeliver by hand" notice. Read off the IN-BAND operand even
    /// when the external tier set the rung — manual redelivery still creates
    /// the duplicate #4986 refused to create.
    pub(in crate::services::discord) fn requires_manual_redelivery_ban_notice(&self) -> bool {
        self.in_band.requires_manual_redelivery_ban_notice()
    }

    /// Whether this composed verdict authorizes a destructive action — turn
    /// cancel, tmux/process kill, registry removal, mailbox/in-flight
    /// force-clean. **No composed value does** (4987 §7.1 / I15).
    pub(in crate::services::discord) fn authorizes_destructive_action(&self) -> bool {
        match self.decided_by {
            RelayVerdictTier::InBand => self.in_band.authorizes_destructive_action(),
            // A bounded read of somebody else's channel history (4987 §5.2) —
            // further from a destruction warrant than Tier A, which has none.
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
                ReachabilityVerdict::Expired { .. } => "expired",
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
/// uncovered_ranges, reason }` object, published in BOTH switch modes —
/// and, through `MailboxHealthSnapshot`, on `GET /api/health/detail`. Every
/// field here is an external wire surface whatever its Rust visibility says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(in crate::services::discord) struct RelayVerdictReport {
    pub verdict: &'static str,
    pub decided_by: RelayVerdictTier,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_unsatisfied_age_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uncovered_ranges: Option<u32>,
    /// #5946 O1: obligations the ledger holds for the INCARNATION, not for the
    /// current turn — covered ones are never subtracted, so this only falls
    /// when the incarnation is replaced. It does NOT separate "this turn's
    /// obligations are all covered" from "this turn has framed nothing yet";
    /// see [`ReachabilityUnknownReason::RowlessActiveTurn`] for why no operand
    /// available here can, and read this as telemetry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incarnation_live_obligations: Option<u32>,
    /// #5946 O1: covered under a generation key with no additional witness.
    /// Incarnation-wide, not per-range: a nonce-less incarnation reports every
    /// covered obligation here and none as covered-and-proven.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unproven_ranges: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_lost_blocks: Option<u32>,
    /// #5942: how long an `expired` entry's ledger went without an observation
    /// commit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unobserved_for_secs: Option<u64>,
    /// #5942: withdrew from the health polarity instead of deciding it —
    /// separate from `governs_health_polarity` (the §5.1 switch).
    pub health_polarity_abstained: bool,
    /// Whether this value was allowed to change the health polarity of the
    /// entry it sits on. False under `RelayVerdictSource::Structural`.
    pub governs_health_polarity: bool,
    /// 4987 §-1.3b's ban notice: don't redeliver a non-GREEN entry by hand.
    pub manual_redelivery_banned: bool,
}

impl RelayVerdictReport {
    pub(in crate::services::discord) fn of(
        verdict: &RelayVerdict,
        governs_health_polarity: bool,
    ) -> Self {
        let mut unobserved_for_secs = None;
        let mut incarnation_live_obligations = None;
        let mut unproven_ranges = None;
        let (oldest_unsatisfied_age_secs, uncovered_ranges, reason) = match verdict.in_band() {
            ReachabilityVerdict::Reachable => (None, None, None),
            ReachabilityVerdict::Expired {
                unobserved_for_secs: unobserved,
            } => {
                unobserved_for_secs = Some(*unobserved);
                (None, None, Some(EXPIRED_REASON))
            }
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
                reason:
                    reason @ ReachabilityUnknownReason::RowlessActiveTurn {
                        incarnation_live_obligations: live,
                        uncovered_ranges: uncovered,
                        unproven_ranges: unproven,
                    },
                since_secs,
            } => {
                incarnation_live_obligations = Some(*live);
                unproven_ranges = Some(*unproven);
                // An age only exists when something is actually held. With
                // nothing held there is no oldest unsatisfied obligation, and
                // publishing `0` would read as one a second old.
                let held = *uncovered + *unproven;
                (
                    (held > 0).then_some(*since_secs),
                    Some(*uncovered),
                    Some(unknown_reason_str(*reason)),
                )
            }
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
            incarnation_live_obligations,
            unproven_ranges,
            reason,
            external_lost_blocks,
            unobserved_for_secs,
            health_polarity_abstained: verdict.abstains_from_health_polarity(),
            governs_health_polarity,
            manual_redelivery_banned: verdict.requires_manual_redelivery_ban_notice(),
        }
    }
}

/// #5071 relay-tail S1 (I-5): one string per branch, so the published reason
/// says which branch answered. `receipt_store_unreadable` is spelled by two
/// branches of `classify_reachability`; the second is unreachable behind an
/// already-answered guard, kept so a reorder costs a conservative verdict.
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
        ReachabilityUnknownReason::RowlessActiveTurn { .. } => "rowless_active_turn",
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
    /// has ever run for this channel.
    Unresolved,
    /// Resolved to a file of this length. `alive` is 4987 §-1.4's positive
    /// evidence (file grew since last observation, or the pane was confirmed
    /// idle); both false is never GREEN.
    Resolved { eof: u64, alive: bool },
}

/// A mailbox turn with no inflight row, split at [`ROWLESS_REACHABILITY_GRACE_SECS`].
/// Inside it the rowless shape is the normal turn-boundary window of 4987 §6.3
/// row 1 and takes the obligation ladder; past it the turn is `Unknown` unless
/// the ladder has something stronger to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum RowlessTurn {
    None,
    WithinGrace,
    OutlivedGrace,
}

impl RowlessTurn {
    pub(in crate::services::discord) fn of(health: &RelayHealthSnapshot) -> Self {
        if !(health.unpaired_active_token_reconfirmed
            && health.mailbox_has_cancel_token
            && !health.bridge_inflight_present)
        {
            return Self::None;
        }
        match health.mailbox_turn_age_secs {
            Some(age) if age < ROWLESS_REACHABILITY_GRACE_SECS => Self::WithinGrace,
            // An unreadable turn age cannot place the turn inside the grace.
            _ => Self::OutlivedGrace,
        }
    }
}

/// The Tier A materials one composition consumes. Every field is a fact some
/// earlier slice already produces; nothing here opens a file.
pub(in crate::services::discord) struct ReachabilityInputs<'a> {
    pub provider: &'a ProviderKind,
    /// T4-B4's row ↔ independently-resolved coordinate comparison.
    pub divergence: RowCoordinateDivergence,
    /// The durable ledger for this channel, or `None` when unreadable.
    /// `ledger_present` distinguishes "never written" from "will not parse"
    /// (4987 §-1.4 counterexample 7).
    pub ledger: Option<&'a ReachabilityLedger>,
    pub ledger_present: bool,
    /// #5942: when the ledger file was last committed, or `None` if that
    /// could not be established. `None` never expires a ledger.
    pub ledger_observed_at_epoch_ms: Option<u64>,
    /// #5942: what the caller could establish about this channel's execution
    /// owner. Only [`ExecutorWitness::Absent`] can expire a ledger.
    pub executor: ExecutorWitness,
    /// T4-B3's receipt projection read.
    pub receipts: &'a ReceiptIndexRead,
    pub transcript: TranscriptLiveness,
    /// The bounded per-tick read did not see the whole tail.
    pub read_truncated: bool,
    /// The mailbox reports an active turn with no in-flight row.
    pub rowless_turn: RowlessTurn,
    /// A placeholder exists for the turn while its terminal receipt does not.
    pub placeholder_present: bool,
    pub now_epoch_ms: u64,
    /// This dcserver process's start time — an obligation first observed
    /// before it spans a restart boundary (4987 §-1.3b's crash window).
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

pub(super) fn age_secs(now_epoch_ms: u64, first_observed_at_epoch_ms: u64) -> u64 {
    now_epoch_ms.saturating_sub(first_observed_at_epoch_ms) / 1_000
}

/// Sweep the live obligations against the receipt projection. The index is
/// clamped to the transcript EOF first (a stale-high frontier would otherwise
/// retire byte ranges that no longer exist). `generation_proven` splits
/// covered obligations in two: a bump failure can let a new incarnation
/// publish its predecessor's `generation_mtime_ns`, so the spawn-nonce
/// witness decides retirable vs. held; [`classify_reachability`] caps what a
/// held obligation produces.
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

/// Produce the Tier A verdict — 4987 §4.1 / §-1.3b / §-1.4. The `Unknown` arms
/// run before the obligation ladder, since grading an incomplete obligation
/// set answers nothing. Actual order: coordinate divergence, store
/// readability, never-observed, read truncation, ledger expiry, transcript
/// resolution — only divergence-first is load bearing (it makes every later
/// operand ambiguous). Rowless-active-turn is the exception: it is graded
/// against the ladder's own verdict and yields to a strictly stronger one.
/// #5071 relay-tail S1 (I-5): the `Unknown` arms name what they observed;
/// `Unknown` permits no health regardless.
pub(in crate::services::discord) fn classify_reachability(
    inputs: ReachabilityInputs<'_>,
) -> ReachabilityVerdict {
    if let Some(reason) = inputs.divergence.unknown_reason() {
        return ReachabilityVerdict::unknown(reason, 0);
    }
    if matches!(inputs.receipts, ReceiptIndexRead::Unknown(_))
        || (inputs.ledger.is_none() && inputs.ledger_present)
    {
        // 4987 §-1.4 counterexample 7: a store that exists and won't parse is
        // `Unknown`, never `Unreachable` — coverage is unknown, not absent.
        return ReachabilityVerdict::unknown(ReachabilityUnknownReason::ReceiptStoreUnreadable, 0);
    }
    let Some(ledger) = inputs.ledger else {
        // Never observed. 4987 §-1.4: not `Reachable`; #5071 relay-tail S1
        // (I-5): not an unresolved transcript either — no coordinate was framed.
        return ReachabilityVerdict::unknown(ReachabilityUnknownReason::NeverObserved, 0);
    };
    // Every FAULT arm runs before the timer (#5942): a thing that went WRONG
    // must not be retired by a clock. `read_truncated` is hardcoded `false` at
    // the production call site today, so this pins an ordering only.
    if inputs.read_truncated {
        return ReachabilityVerdict::unknown(ReachabilityUnknownReason::ReadTruncated, 0);
    }
    // Checked before the transcript arm (a producerless ledger can never pass
    // it), but must not preempt a `Reachable` verdict — `expired_without_a_producer`
    // refuses to expire over §-1.4's positive alive evidence.
    if let Some(unobserved_for_secs) = expired_without_a_producer(&inputs, ledger) {
        return ReachabilityVerdict::Expired {
            unobserved_for_secs,
        };
    }
    let TranscriptLiveness::Resolved { eof, alive } = inputs.transcript else {
        return ReachabilityVerdict::unknown(ReachabilityUnknownReason::TranscriptUnresolved, 0);
    };

    let index = match inputs.receipts {
        ReceiptIndexRead::Ready(index) => Some(index.clone().with_frontier_clamped_to_eof(eof)),
        // A genuinely absent store covers nothing — not a fault.
        ReceiptIndexRead::Absent => None,
        // Already answered above; re-answered rather than `unreachable!` so a
        // guard reorder costs a conservative verdict, not the polling task.
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

    let ladder = match oldest_held {
        // Every obligation retired, or none was ever framed (4987 §4.1); §-1.4
        // gates this on positive alive evidence, not its absence.
        None => {
            if alive {
                ReachabilityVerdict::Reachable
            } else {
                ReachabilityVerdict::unknown(
                    ReachabilityUnknownReason::IncarnationNotAliveWitnessed(
                        NotAliveObligationState::NoneOutstanding,
                    ),
                    0,
                )
            }
        }
        Some(oldest) if oldest < OBLIGATION_WARN_BOUND_SECS => {
            // 4987 §-1.4 counterexample 6: inside the grace, not yet evidence.
            if alive {
                ReachabilityVerdict::Reachable
            } else {
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
                ReachabilityVerdict::Degraded {
                    oldest_unsatisfied_age_secs: oldest,
                    uncovered_ranges: held_ranges,
                }
            } else {
                // Past `fail_bound` with a genuinely uncovered range: a transport
                // trace demotes to `TransportUnknown` (4987 §-1.3b); without one
                // it's the §-1.4 counterexample 2 true positive.
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
    };

    // Graded after the sweep so it publishes the coverage it saw. The counts
    // are the INCARNATION's (covered obligations are never subtracted); see
    // `ReachabilityUnknownReason::RowlessActiveTurn`.
    if inputs.rowless_turn != RowlessTurn::OutlivedGrace {
        return ladder;
    }
    let rowless = ReachabilityVerdict::unknown(
        ReachabilityUnknownReason::RowlessActiveTurn {
            incarnation_live_obligations: ledger.live_obligations().len() as u32,
            uncovered_ranges: sweep.uncovered_ages_secs.len() as u32,
            unproven_ranges: sweep.unproven_ages_secs.len() as u32,
        },
        oldest_held.unwrap_or(0),
    );
    // A stuck turn's own prose can age past `fail_bound` only after the turn
    // outlived the grace, so a strictly stronger ladder verdict must win here.
    // An equal-rank verdict carrying the manual redelivery ban also wins, so the
    // rowless reason never strips the duplicate-send warning.
    if in_band_rank(&ladder) > in_band_rank(&rowless)
        || ladder.requires_manual_redelivery_ban_notice()
    {
        ladder
    } else {
        rowless
    }
}

/// Which trace, if any, says the transport happened without a receipt.
///
/// Restart-boundary first: the window §-1.3b was created for (a POST that
/// succeeded, a receipt write that didn't survive the crash), with different
/// alarm wording. A placeholder is the weaker trace — a turn started, not that
/// its bytes reached Discord — so it only answers when the boundary doesn't.
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
    /// `None` when the health registry could not resolve the provider name —
    /// nothing here is readable without one, so this composes to `Unknown`.
    pub provider: Option<&'a ProviderKind>,
    pub channel_id: u64,
    /// The in-flight row's transcript path, handed to `super::divergence` as a
    /// comparison operand and nothing else (I14).
    pub row_output_path: Option<&'a str>,
    /// The registry's independently resolved transcript path.
    pub registry_output_path: Option<&'a str>,
    /// 4987 §-1.4's second alive witness: pane up with nothing pending, so a
    /// non-growing transcript is idle rather than dead.
    pub pane_idle_confirmed: bool,
    pub rowless_turn: RowlessTurn,
    /// A placeholder message is outstanding for this channel.
    pub placeholder_present: bool,
    /// #5942: caller-owned since establishing it costs a tmux round trip
    /// against a shared budget this file must not spend.
    pub executor: ExecutorWitness,
    pub now_epoch_ms: u64,
    pub process_started_at_epoch_ms: u64,
}

/// Read this channel's durable materials and compose one verdict. Three small
/// reads (T4-B2c ledger, T4-B3 receipt projection, T4-B5 sidecar), each a
/// whole-file read published by atomic rename so a concurrent writer shows
/// old or new bytes, never torn. Takes no lock, mutates nothing. The sidecar
/// is gated on the ledger's own incarnation: one written for a previous
/// incarnation classifies `WrongIncarnation` and contributes
/// `ExternalRelayVerdict::Unknown`, which [`compose_relay_verdict`] leaves the
/// in-band verdict untouched by.
pub(in crate::services::discord) fn observe_relay_verdict(
    probe: RelayVerdictProbe<'_>,
) -> RelayVerdict {
    let divergence_outcome = divergence(
        CoordinateObservation::observe(probe.row_output_path),
        CoordinateObservation::observe(probe.registry_output_path),
    );

    let Some(provider) = probe.provider else {
        // #5071 relay-tail S1 (I-5): no provider owns this channel, so nothing
        // can even be located — upstream of the resolution ladder, not a
        // failure of it.
        return compose_relay_verdict(
            ReachabilityVerdict::unknown(ReachabilityUnknownReason::ProviderUnresolved, 0),
            ExternalRelayVerdict::Unknown,
        );
    };

    let ledger_path = ledger_path(provider, probe.channel_id);
    let ledger = ledger_path.as_deref().and_then(read_ledger_at);
    let ledger_present = ledger_path.as_deref().is_some_and(ledger_file_exists);
    let ledger_observed_at_epoch_ms = ledger_path
        .as_deref()
        .and_then(ledger_committed_at_epoch_ms);

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
        ledger_observed_at_epoch_ms,
        executor: probe.executor,
        receipts: &receipts,
        transcript,
        // The observation task records its own truncation in the ledger it
        // writes; this reader doesn't tail, so it has none of its own.
        read_truncated: false,
        rowless_turn: probe.rowless_turn,
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
/// Growth is measured against the ledger's `last_observed_len`, stamped each
/// tick by the observation task. Equal lengths aren't a claim it died — that's
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

/// The only place 4987 §5.1's switch changes a snapshot's polarity (#5071
/// T4-B6). Under `Structural` this touches neither output — that's what makes
/// the shadow mode a shadow. One reason per non-green CHANNEL, not per
/// provider. `Degraded`, never `Unhealthy`: 4987 §4.4 asks a non-`Reachable`
/// relay to set the degraded flag; taking the process out of HTTP readiness
/// is authority this switch was not given.
pub(in crate::services::discord) fn apply_relay_verdict_polarity(
    composite_governs_polarity: bool,
    relay_verdict: &RelayVerdict,
    provider: &str,
    channel_id: u64,
    degraded_reasons: &mut Vec<String>,
    expired_relay_ledgers: &mut Vec<String>,
    status: &mut super::super::snapshot::HealthStatus,
) {
    if !composite_governs_polarity || relay_verdict.permits_health() {
        return;
    }
    let entry = format!(
        "relay_verdict_{}_{provider}_{channel_id}",
        relay_verdict.label(),
    );
    // #5942: an expired entry is recorded, not counted — its own vector, not
    // `degraded_reasons`, so the channel stays visible without pinning the
    // node non-GREEN forever.
    if relay_verdict.abstains_from_health_polarity() {
        expired_relay_ledgers.push(entry);
        return;
    }
    degraded_reasons.push(entry);
    *status = status.worsen(super::super::snapshot::HealthStatus::Degraded);
}

#[cfg(test)]
static RELAY_VERDICT_SOURCE_OVERRIDE: std::sync::Mutex<Option<RelayVerdictSource>> =
    std::sync::Mutex::new(None);

/// The live 4987 §5.1 switch. Same shape as `execution_identity_mode`: a live
/// `agentdesk.yaml` edit applies on next read without a restart, and an
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
