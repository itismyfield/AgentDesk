//! The `ReachabilityVerdict` type set — 4987 §-1.3b and §4.1 (#5071 T4-B1).
//! Vocabulary and polarity only: no composition rule, no threshold, no clock
//! read, no I/O — deferred to T4-B6's `worst(ReachabilityVerdict,
//! ExternalRelayVerdict)` (gated behind `G-T4`). Polarity: `!= Reachable` ⇒
//! not GREEN, but the converse doesn't hold — §-1.4 additionally requires
//! positive incarnation-alive evidence. `TransportUnknown` (§-1.3b, the
//! POST-succeeded/receipt-write-failed crash window, #4986) is neither health
//! nor a redelivery warrant — false for both
//! [`ReachabilityVerdict::permits_health`] and
//! [`ReachabilityVerdict::authorizes_redelivery`] — and the only variant that
//! sets [`ReachabilityVerdict::requires_manual_redelivery_ban_notice`]; 4987
//! §7.1/I15 denies destructive action to every variant regardless.

/// The reachability verdict, 4987 §-1.3b (which extends §4.1 with
/// `TransportUnknown`). Payload fields are the ones 4987 names; they are
/// carried, never interpreted, here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum ReachabilityVerdict {
    /// Every obligation in the incarnation range is covered by a confirmed
    /// and committed receipt. 4987 §4.1's zero-obligation case still needs
    /// §-1.4's positive incarnation-alive evidence — "nothing observed" is
    /// never GREEN.
    Reachable,
    /// Unsatisfied obligations passed `warn_bound` but not `fail_bound`.
    Degraded {
        oldest_unsatisfied_age_secs: u64,
        uncovered_ranges: u32,
    },
    /// No receipt, but positive empirical evidence the transport happened
    /// (an unreleased delivery lease, a restart boundary crossed mid-turn, a
    /// live placeholder). 4987 §-1.3b: **not** `Unreachable`; "do not
    /// redeliver by hand".
    TransportUnknown {
        since_secs: u64,
        evidence: TransportUnknownEvidence,
    },
    /// No receipt and no trace of a transport, past `fail_bound`.
    Unreachable {
        oldest_unsatisfied_age_secs: u64,
        uncovered_ranges: u32,
    },
    /// The obligation set could not be produced at all. 4987 §4.1: this is
    /// **not** `Reachable` — an unobservable relay is not a healthy one.
    Unknown {
        reason: ReachabilityUnknownReason,
        since_secs: u64,
    },
    /// The ledger outlived every producer that could ever resolve it (#5942):
    /// no commit for longer than the TTL, execution owner positively
    /// witnessed absent, nothing outstanding. **Not** a health claim — it
    /// withdraws instead of pinning non-GREEN forever; see
    /// [`ReachabilityVerdict::abstains_from_health_polarity`].
    Expired {
        /// Time since the last observation commit.
        unobserved_for_secs: u64,
    },
}

/// Why a `TransportUnknown` believes a transport occurred (4987 §-1.3b).
/// Every variant is an observation of a *trace*, never of a receipt: a
/// receipt would have made the range `Reachable` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum TransportUnknownEvidence {
    /// A delivery lease for this incarnation was taken and never released.
    UnreleasedDeliveryLease,
    /// The uncovered range spans a dcserver restart boundary (the crash window §-1.3b exists for).
    RestartBoundaryCrossed,
    /// A placeholder for the turn exists while its terminal receipt does not.
    PlaceholderPresent,
}

/// Which of the two obligation states accompanied a not-alive incarnation
/// (#5071 relay-tail S1, I-5): "nothing was ever owed" vs. "something is
/// owed and still inside its grace".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum NotAliveObligationState {
    /// Every obligation retired, or none was ever framed.
    NoneOutstanding,
    /// An obligation is outstanding and younger than the warn bound, so it is
    /// not yet evidence of anything.
    WithinGrace,
}

/// Why the obligation set could not be produced (4987 §4.1). #5071
/// relay-tail S1 (I-5): `TranscriptUnresolved` means the resolution ladder
/// and nothing else.
///
/// Equality now includes `RowlessActiveTurn`'s counts, so two rowless verdicts
/// whose coverage differs are no longer `==`. Nothing dedupes on this today; a
/// future alarm that does must compare the discriminant, not the whole reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum ReachabilityUnknownReason {
    /// Every rank of the 4987 §-1.3 resolution ladder failed to resolve an
    /// existing coordinate.
    TranscriptUnresolved,
    /// No ledger was ever written for this channel — 4987 §-1.4: "not
    /// observed" is not `Reachable`, and no coordinate was ever framed.
    NeverObserved,
    /// No provider owns this channel; upstream of every rank of the ladder.
    ProviderUnresolved,
    /// The ladder resolved and the incarnation is not witnessed alive (the producer is the unknown here, not the transcript).
    IncarnationNotAliveWitnessed(NotAliveObligationState),
    /// Two independently resolved coordinates name different files, or the
    /// file under an established cursor changed identity.
    TranscriptCoordinateDivergence,
    /// The mailbox reports an active turn with no inflight row. 4987 §-1.4
    /// demotes this to an explanatory attribute — no verdict of its own.
    ///
    /// Carries what the coverage sweep saw (#5946 O1).
    ///
    /// **The scope is the INCARNATION, not the current turn, and these numbers
    /// cannot isolate one turn from another.** `ObligationExtinction::ReceiptCovered`
    /// has no producer, so a covered obligation is never subtracted from
    /// [`super::ledger::ReachabilityLedger::live_obligations`] — the count falls
    /// only when the incarnation is replaced, and `LedgerIncarnation` carries no
    /// turn identifier. The receipt side cannot supply one either: the projection
    /// key in [`crate::services::discord::outbound::receipt_index`] deliberately
    /// omits `turn_nonce`.
    ///
    /// The consequence a consumer must not walk into: from the SECOND turn of an
    /// incarnation onward, a live turn that has framed nothing yet publishes
    /// `uncovered_ranges: 0` beside a non-zero `incarnation_live_obligations` —
    /// byte-for-byte what a turn whose obligations are all covered publishes.
    /// **Reading that as "this turn's answer landed" retires a turn that has not
    /// answered**, which is the (b) failure this signal was added to expose. A
    /// turn-scoped discriminator is NOT implemented; until one exists these are
    /// telemetry and the reader fails closed.
    ///
    /// `permits_health` and the two authorization predicates are unchanged:
    /// `Unknown` grants nothing whatever the payload says.
    RowlessActiveTurn {
        /// Obligations the ledger holds for this INCARNATION. The ledger's own
        /// words are "observed and not yet subtracted", not "undelivered":
        /// covered ones stay in the set.
        incarnation_live_obligations: u32,
        /// Of those, the ones no receipt and no frontier covers.
        uncovered_ranges: u32,
        /// Of those, the ones covered under a generation key with no additional
        /// witness. An incarnation-wide switch, not a per-range property:
        /// `sweep_coverage` is handed `ledger.incarnation.spawn_nonce.is_some()`,
        /// so a nonce-less incarnation sends EVERY covered obligation here.
        unproven_ranges: u32,
    },
    /// The bounded per-tick read hit its cap; see
    /// [`super::tail::TAIL_READ_CAP_BYTES`].
    ReadTruncated,
    /// The receipt store could not be read — a malformed ledger is
    /// `Unknown`, never `Unreachable` (4987 §-1.4 counterexample 7).
    ReceiptStoreUnreadable,
}

impl ReachabilityVerdict {
    /// Whether this verdict permits a GREEN final health verdict — 4987 §4.1.
    /// True for `Reachable` only; §-1.4 still requires positive
    /// incarnation-alive evidence. T4-B6's composed
    /// `RelayVerdict::permits_health` delegates to this for its in-band arm.
    pub(in crate::services::discord) fn permits_health(&self) -> bool {
        match self {
            Self::Reachable => true,
            Self::Degraded { .. }
            | Self::TransportUnknown { .. }
            | Self::Unreachable { .. }
            | Self::Unknown { .. }
            // #5942: expiry withdraws the entry; it never promotes it.
            | Self::Expired { .. } => false,
        }
    }

    /// Whether this verdict authorizes redelivering an uncovered range.
    /// **No variant does**: 4987 keeps automatic range redelivery (S7) at
    /// NO-GO. Arms are spelled out rather than collapsed to `false` so a new
    /// variant is a compile error here.
    pub(in crate::services::discord) fn authorizes_redelivery(&self) -> bool {
        match self {
            Self::Reachable
            | Self::Degraded { .. }
            | Self::TransportUnknown { .. }
            | Self::Unreachable { .. }
            | Self::Unknown { .. }
            | Self::Expired { .. } => false,
        }
    }

    /// Whether this verdict authorizes a destructive action — turn cancel,
    /// tmux/process kill, registry removal, mailbox/inflight force-clean.
    /// **No variant does** (4987 §7.1 / I15) — convention plus a source
    /// lint, not a sealed capability (§-1.5).
    pub(in crate::services::discord) fn authorizes_destructive_action(&self) -> bool {
        match self {
            Self::Reachable
            | Self::Degraded { .. }
            | Self::TransportUnknown { .. }
            | Self::Unreachable { .. }
            | Self::Unknown { .. }
            | Self::Expired { .. } => false,
        }
    }

    /// Whether an alarm for this verdict must carry the explicit "do not
    /// redeliver by hand" notice (4987 §-1.3b): `TransportUnknown` only, since
    /// the crash window looks like a loss and is not one.
    pub(in crate::services::discord) fn requires_manual_redelivery_ban_notice(&self) -> bool {
        matches!(self, Self::TransportUnknown { .. })
    }

    /// Whether this verdict withdraws from the health polarity instead of
    /// deciding it (#5942). `Expired` only — a THIRD answer, not a loosening
    /// of [`ReachabilityVerdict::permits_health`]: it loses its vote, not
    /// its non-GREEN status. Spelled as an exhaustive match so a new variant
    /// must claim an answer here before it compiles.
    pub(in crate::services::discord) fn abstains_from_health_polarity(&self) -> bool {
        match self {
            Self::Expired { .. } => true,
            Self::Reachable
            | Self::Degraded { .. }
            | Self::TransportUnknown { .. }
            | Self::Unreachable { .. }
            | Self::Unknown { .. } => false,
        }
    }

    /// The `Unknown` reason, when this is an `Unknown`.
    pub(in crate::services::discord) fn unknown_reason(&self) -> Option<ReachabilityUnknownReason> {
        match self {
            Self::Unknown { reason, .. } => Some(*reason),
            _ => None,
        }
    }

    /// Build an `Unknown` from a reason produced by the resolution ladder or
    /// tail reader. `since_secs` comes from the caller — this file reads no
    /// clock.
    pub(in crate::services::discord) fn unknown(
        reason: ReachabilityUnknownReason,
        since_secs: u64,
    ) -> Self {
        Self::Unknown { reason, since_secs }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant, once, so the polarity tables below are exhaustive by
    /// construction.
    fn every_verdict() -> Vec<ReachabilityVerdict> {
        vec![
            ReachabilityVerdict::Reachable,
            ReachabilityVerdict::Degraded {
                oldest_unsatisfied_age_secs: 61,
                uncovered_ranges: 1,
            },
            ReachabilityVerdict::TransportUnknown {
                since_secs: 12,
                evidence: TransportUnknownEvidence::UnreleasedDeliveryLease,
            },
            ReachabilityVerdict::TransportUnknown {
                since_secs: 12,
                evidence: TransportUnknownEvidence::RestartBoundaryCrossed,
            },
            ReachabilityVerdict::TransportUnknown {
                since_secs: 12,
                evidence: TransportUnknownEvidence::PlaceholderPresent,
            },
            ReachabilityVerdict::Unreachable {
                oldest_unsatisfied_age_secs: 601,
                uncovered_ranges: 3,
            },
            ReachabilityVerdict::Unknown {
                reason: ReachabilityUnknownReason::TranscriptUnresolved,
                since_secs: 5,
            },
            ReachabilityVerdict::Unknown {
                reason: ReachabilityUnknownReason::TranscriptCoordinateDivergence,
                since_secs: 5,
            },
            ReachabilityVerdict::Unknown {
                reason: ReachabilityUnknownReason::RowlessActiveTurn {
                    incarnation_live_obligations: 2,
                    uncovered_ranges: 1,
                    unproven_ranges: 0,
                },
                since_secs: 5,
            },
            ReachabilityVerdict::Unknown {
                reason: ReachabilityUnknownReason::ReadTruncated,
                since_secs: 5,
            },
            ReachabilityVerdict::Unknown {
                reason: ReachabilityUnknownReason::ReceiptStoreUnreadable,
                since_secs: 5,
            },
            ReachabilityVerdict::Unknown {
                reason: ReachabilityUnknownReason::NeverObserved,
                since_secs: 5,
            },
            ReachabilityVerdict::Unknown {
                reason: ReachabilityUnknownReason::ProviderUnresolved,
                since_secs: 5,
            },
            ReachabilityVerdict::Unknown {
                reason: ReachabilityUnknownReason::IncarnationNotAliveWitnessed(
                    NotAliveObligationState::NoneOutstanding,
                ),
                since_secs: 5,
            },
            ReachabilityVerdict::Unknown {
                reason: ReachabilityUnknownReason::IncarnationNotAliveWitnessed(
                    NotAliveObligationState::WithinGrace,
                ),
                since_secs: 5,
            },
            ReachabilityVerdict::Expired {
                unobserved_for_secs: 3_600,
            },
        ]
    }

    /// No `_` arm, so a seventh variant stops this module compiling until
    /// someone names it (#5942).
    fn verdict_index(verdict: &ReachabilityVerdict) -> usize {
        match verdict {
            ReachabilityVerdict::Reachable => 0,
            ReachabilityVerdict::Degraded { .. } => 1,
            ReachabilityVerdict::TransportUnknown { .. } => 2,
            ReachabilityVerdict::Unreachable { .. } => 3,
            ReachabilityVerdict::Unknown { .. } => 4,
            ReachabilityVerdict::Expired { .. } => 5,
        }
    }

    /// Every variant is represented at least once — the fixture deliberately
    /// carries several `TransportUnknown` evidences and every `Unknown` reason.
    #[test]
    fn the_polarity_fixture_covers_every_verdict_variant() {
        const VERDICT_COUNT: usize = 6;
        let mut seen = [false; VERDICT_COUNT];
        for verdict in every_verdict() {
            seen[verdict_index(&verdict)] = true;
        }
        for (index, seen) in seen.iter().enumerate() {
            assert!(
                *seen,
                "no row of every_verdict() covers verdict index {index}"
            );
        }
    }

    #[test]
    fn only_reachable_permits_green_health() {
        for verdict in every_verdict() {
            let expected = matches!(verdict, ReachabilityVerdict::Reachable);
            assert_eq!(
                verdict.permits_health(),
                expected,
                "4987 §4.1 polarity broken for {verdict:?}"
            );
        }
    }

    /// 4987 §-1.3b: `TransportUnknown` is not health — the exact polarity
    /// the design row calls out.
    #[test]
    fn transport_unknown_is_not_health() {
        let verdict = ReachabilityVerdict::TransportUnknown {
            since_secs: 30,
            evidence: TransportUnknownEvidence::RestartBoundaryCrossed,
        };
        assert!(!verdict.permits_health());
    }

    /// 4987 §-1.3b + S7 NO-GO: `TransportUnknown` is not a redelivery warrant
    /// either. Being non-GREEN is not permission to act.
    #[test]
    fn transport_unknown_is_not_a_redelivery_warrant() {
        for evidence in [
            TransportUnknownEvidence::UnreleasedDeliveryLease,
            TransportUnknownEvidence::RestartBoundaryCrossed,
            TransportUnknownEvidence::PlaceholderPresent,
        ] {
            let verdict = ReachabilityVerdict::TransportUnknown {
                since_secs: 30,
                evidence,
            };
            assert!(!verdict.authorizes_redelivery());
            assert!(!verdict.authorizes_destructive_action());
        }
    }

    #[test]
    fn no_verdict_authorizes_redelivery_or_destruction() {
        for verdict in every_verdict() {
            assert!(
                !verdict.authorizes_redelivery(),
                "4987 S7 stays NO-GO; {verdict:?} must not authorize redelivery"
            );
            assert!(
                !verdict.authorizes_destructive_action(),
                "4987 §7.1/I15; {verdict:?} must not authorize destruction"
            );
        }
    }

    /// The manual-redelivery ban notice distinguishes `TransportUnknown`'s
    /// alarm from `Unreachable`'s, so it must be exactly that variant.
    #[test]
    fn only_transport_unknown_carries_the_manual_redelivery_ban_notice() {
        for verdict in every_verdict() {
            let expected = matches!(verdict, ReachabilityVerdict::TransportUnknown { .. });
            assert_eq!(
                verdict.requires_manual_redelivery_ban_notice(),
                expected,
                "wrong ban-notice polarity for {verdict:?}"
            );
        }
    }

    /// #5942: expiry is the ONLY verdict that withdraws from the polarity —
    /// `permits_health` is re-asserted beside it: abstaining AND not health.
    #[test]
    fn only_expired_abstains_from_health_polarity() {
        for verdict in every_verdict() {
            let expected = matches!(verdict, ReachabilityVerdict::Expired { .. });
            assert_eq!(
                verdict.abstains_from_health_polarity(),
                expected,
                "wrong abstention polarity for {verdict:?}"
            );
            if expected {
                assert!(
                    !verdict.permits_health(),
                    "abstaining is not permission: {verdict:?}"
                );
            }
        }
    }

    #[test]
    fn unknown_reason_is_readable_only_from_unknown() {
        assert_eq!(
            ReachabilityVerdict::unknown(ReachabilityUnknownReason::ReadTruncated, 7)
                .unknown_reason(),
            Some(ReachabilityUnknownReason::ReadTruncated)
        );
        assert_eq!(ReachabilityVerdict::Reachable.unknown_reason(), None);
        assert_eq!(
            ReachabilityVerdict::TransportUnknown {
                since_secs: 1,
                evidence: TransportUnknownEvidence::PlaceholderPresent,
            }
            .unknown_reason(),
            None,
            "TransportUnknown is its own variant, not an Unknown reason"
        );
    }

    /// How many reasons 4987 §4.1 defines, and therefore how many distinct
    /// indices [`unknown_reason_index`] may hand out.
    const UNKNOWN_REASON_COUNT: usize = 9;

    /// Give each `Unknown` reason its own index. No `_` arm and no
    /// or-pattern, so a new `ReachabilityUnknownReason` variant stops this
    /// module compiling until someone names it here.
    fn unknown_reason_index(reason: ReachabilityUnknownReason) -> usize {
        match reason {
            ReachabilityUnknownReason::TranscriptUnresolved => 0,
            ReachabilityUnknownReason::TranscriptCoordinateDivergence => 1,
            // The coverage payload is an observation, not an identity: every
            // rowless verdict claims this one index whatever the sweep saw.
            ReachabilityUnknownReason::RowlessActiveTurn { .. } => 2,
            ReachabilityUnknownReason::ReadTruncated => 3,
            ReachabilityUnknownReason::ReceiptStoreUnreadable => 4,
            ReachabilityUnknownReason::NeverObserved => 5,
            ReachabilityUnknownReason::ProviderUnresolved => 6,
            // The payload is matched out, not wildcarded: a third not-alive
            // state has to claim its own index here too.
            ReachabilityUnknownReason::IncarnationNotAliveWitnessed(
                NotAliveObligationState::NoneOutstanding,
            ) => 7,
            ReachabilityUnknownReason::IncarnationNotAliveWitnessed(
                NotAliveObligationState::WithinGrace,
            ) => 8,
        }
    }

    /// The table below enumerates every `ReachabilityUnknownReason` exactly
    /// once. Proved indirectly: [`unknown_reason_index`] values must cover
    /// every index below [`UNKNOWN_REASON_COUNT`] without collision, which
    /// only holds for a permutation of the `match`'s arms.
    #[test]
    fn every_unknown_reason_is_named_exactly_once() {
        let every_reason = [
            ReachabilityUnknownReason::TranscriptUnresolved,
            ReachabilityUnknownReason::TranscriptCoordinateDivergence,
            ReachabilityUnknownReason::RowlessActiveTurn {
                incarnation_live_obligations: 2,
                uncovered_ranges: 1,
                unproven_ranges: 0,
            },
            ReachabilityUnknownReason::ReadTruncated,
            ReachabilityUnknownReason::ReceiptStoreUnreadable,
            ReachabilityUnknownReason::NeverObserved,
            ReachabilityUnknownReason::ProviderUnresolved,
            ReachabilityUnknownReason::IncarnationNotAliveWitnessed(
                NotAliveObligationState::NoneOutstanding,
            ),
            ReachabilityUnknownReason::IncarnationNotAliveWitnessed(
                NotAliveObligationState::WithinGrace,
            ),
        ];

        // Deliberately no `every_reason.len() == UNKNOWN_REASON_COUNT` assert:
        // it would make the coverage loop below unreachable.
        let mut claimed: [Option<ReachabilityUnknownReason>; UNKNOWN_REASON_COUNT] =
            [None; UNKNOWN_REASON_COUNT];
        for reason in every_reason {
            let slot = &mut claimed[unknown_reason_index(reason)];
            assert_eq!(
                *slot, None,
                "{reason:?} wants an index {slot:?} already claimed"
            );
            *slot = Some(reason);
        }
        for (index, slot) in claimed.iter().enumerate() {
            assert!(
                slot.is_some(),
                "index {index} is unclaimed: the table above is missing a reason"
            );
        }
    }
}
