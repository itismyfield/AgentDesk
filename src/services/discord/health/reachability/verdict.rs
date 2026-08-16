//! The `ReachabilityVerdict` type set — 4987 §-1.3b and §4.1 (#5071 T4-B1).
//!
//! This file is vocabulary and polarity. It deliberately holds no composition
//! rule, no threshold, no clock read, and no I/O:
//!
//! * choosing `Degraded` vs `Unreachable` is the `warn_bound`/`fail_bound`
//!   subtraction T4-B2 adds on top of the obligation ledger;
//! * the final product `worst(ReachabilityVerdict, ExternalRelayVerdict)` is
//!   T4-B6, and turning it on is gated behind `G-T4`.
//!
//! Landing the names first is what lets B2..B6 be reviewed against one fixed
//! set instead of each slice inventing its own spelling.
//!
//! # Polarity (4987 §4.1)
//!
//! > `ReachabilityVerdict != Reachable` ⇒ the final health verdict is not
//! > GREEN, whatever the structural signals say.
//!
//! The converse does NOT hold: `Reachable` does not *declare* health, it only
//! fails to deny it — §-1.4 additionally requires positive incarnation-alive
//! evidence before a producer may spell it, and producing verdicts is B2's job.
//!
//! # `TransportUnknown` is neither health nor a redelivery warrant
//!
//! §-1.3b introduced it for the POST-succeeded/receipt-write-failed crash
//! window, because round 1 sent that window straight to `Unreachable`, a human
//! then redelivered by hand, and that produced the duplicate #4986 was refusing
//! to create. So it is false for both [`ReachabilityVerdict::permits_health`]
//! (§-1.3b puts it on the degraded side) and
//! [`ReachabilityVerdict::authorizes_redelivery`], and it is the only variant
//! that sets [`ReachabilityVerdict::requires_manual_redelivery_ban_notice`].
//! Encoding "neither" rather than "one of the two" is the point: a non-GREEN
//! variant is exactly what a later reader is tempted to read as permission to
//! act, and 4987 §7.1/I15 denies that to every variant.

/// The reachability verdict, 4987 §-1.3b (which extends §4.1 with
/// `TransportUnknown`).
///
/// The payload fields are the ones 4987 names; they are carried, never
/// interpreted, here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum ReachabilityVerdict {
    /// Every obligation in the incarnation range is covered by a confirmed and
    /// committed receipt. 4987 §4.1 includes the zero-obligation case, but
    /// §-1.4 then requires positive incarnation-alive evidence before a
    /// producer may spell it — "nothing observed" is never GREEN.
    Reachable,
    /// Unsatisfied obligations passed `warn_bound` but not `fail_bound`.
    Degraded {
        oldest_unsatisfied_age_secs: u64,
        uncovered_ranges: u32,
    },
    /// No receipt, but positive empirical evidence that the transport actually
    /// happened (an unreleased delivery lease, a restart boundary crossed
    /// mid-turn, a live placeholder). 4987 §-1.3b: this is **not**
    /// `Unreachable`, its alarm wording differs, and it states "do not
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
}

/// Why a `TransportUnknown` believes a transport occurred (4987 §-1.3b).
///
/// Every variant is an observation of a *trace*, never of a receipt: a receipt
/// would have made the range `Reachable` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum TransportUnknownEvidence {
    /// A delivery lease for this incarnation was taken and never released.
    UnreleasedDeliveryLease,
    /// The uncovered range spans a dcserver restart boundary, i.e. the exact
    /// success→commit crash window §-1.3b was created for.
    RestartBoundaryCrossed,
    /// A placeholder for the turn exists while its terminal receipt does not.
    PlaceholderPresent,
}

/// Why the obligation set could not be produced (4987 §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum ReachabilityUnknownReason {
    /// Every rank of the 4987 §-1.3 resolution ladder failed.
    TranscriptUnresolved,
    /// Two independently resolved coordinates name different files, or the
    /// file under an established cursor stopped being that file.
    TranscriptCoordinateDivergence,
    /// The mailbox reports an active turn with no inflight row. 4987 §-1.4
    /// demotes this to an explanatory attribute; it produces no verdict of its
    /// own beyond this `Unknown`.
    RowlessActiveTurn,
    /// The bounded per-tick read hit its cap, so the tick did not see the whole
    /// tail. See [`super::tail::TAIL_READ_CAP_BYTES`].
    ReadTruncated,
    /// The receipt store could not be read (4987 §-1.4 counterexample 7: a
    /// malformed ledger is `Unknown`, never `Unreachable`).
    ReceiptStoreUnreadable,
}

impl ReachabilityVerdict {
    /// Whether this verdict permits a GREEN final health verdict — 4987 §4.1.
    /// True for `Reachable` only; `TransportUnknown` is false here by the same
    /// rule as `Unreachable`. Permission is not a declaration: §-1.4 still
    /// requires positive incarnation-alive evidence before a producer may spell
    /// `Reachable` at all, B6 owns the product this feeds, and nothing calls
    /// this yet.
    pub(in crate::services::discord) fn permits_health(&self) -> bool {
        match self {
            Self::Reachable => true,
            Self::Degraded { .. }
            | Self::TransportUnknown { .. }
            | Self::Unreachable { .. }
            | Self::Unknown { .. } => false,
        }
    }

    /// Whether this verdict authorizes redelivering an uncovered range.
    ///
    /// **No variant does**: 4987 keeps automatic range redelivery (S7) at
    /// NO-GO, and §-1.3b singles out `TransportUnknown` as the variant most
    /// likely to be misread as "we probably lost it, resend" — which is exactly
    /// how the duplicate gets created. The arms are spelled out rather than
    /// collapsed to `false` so a new variant is a compile error here and a
    /// flipped arm dies in a named test instead of vanishing into a constant.
    pub(in crate::services::discord) fn authorizes_redelivery(&self) -> bool {
        match self {
            Self::Reachable
            | Self::Degraded { .. }
            | Self::TransportUnknown { .. }
            | Self::Unreachable { .. }
            | Self::Unknown { .. } => false,
        }
    }

    /// Whether this verdict authorizes a destructive action — turn cancel,
    /// tmux/process kill, registry removal, mailbox/inflight force-clean.
    ///
    /// **No variant does** (4987 §7.1 / I15). Convention plus a source lint,
    /// not a sealed capability: §-1.5 records the decision not to put the
    /// destructive `RelayRecoveryActionKind` variants behind a private
    /// constructor, so a future caller CAN ignore this. It exists so that
    /// ignoring it is a visible choice.
    pub(in crate::services::discord) fn authorizes_destructive_action(&self) -> bool {
        match self {
            Self::Reachable
            | Self::Degraded { .. }
            | Self::TransportUnknown { .. }
            | Self::Unreachable { .. }
            | Self::Unknown { .. } => false,
        }
    }

    /// Whether an alarm for this verdict must carry the explicit "do not
    /// redeliver by hand" notice (4987 §-1.3b). `TransportUnknown` only:
    /// `Unreachable` gets the ordinary wording, and the ban notice exists
    /// because the crash window looks like a loss and is not one.
    pub(in crate::services::discord) fn requires_manual_redelivery_ban_notice(&self) -> bool {
        matches!(self, Self::TransportUnknown { .. })
    }

    /// The `Unknown` reason, when this is an `Unknown`.
    pub(in crate::services::discord) fn unknown_reason(&self) -> Option<ReachabilityUnknownReason> {
        match self {
            Self::Unknown { reason, .. } => Some(*reason),
            _ => None,
        }
    }

    /// Build an `Unknown` from a reason produced by the resolution ladder or
    /// the tail reader. `since_secs` is supplied by the caller because this
    /// file reads no clock.
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
    /// construction rather than by reviewer attention.
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
                reason: ReachabilityUnknownReason::RowlessActiveTurn,
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
        ]
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

    /// 4987 §-1.3b: `TransportUnknown` is not health. The named test exists
    /// separately from the table above because this is the exact polarity the
    /// design row calls out, and a table can be weakened without anyone
    /// noticing which row it lost.
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

    /// The manual-redelivery ban notice is the one thing that distinguishes
    /// `TransportUnknown`'s alarm from `Unreachable`'s, so it must be exactly
    /// that variant — not "everything non-GREEN".
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
    const UNKNOWN_REASON_COUNT: usize = 5;

    /// Give each `Unknown` reason its own index.
    ///
    /// This `match` is the mechanism, not the table it feeds: it has no `_` arm
    /// and no or-pattern, so a sixth `ReachabilityUnknownReason` variant makes
    /// this test module stop compiling until someone names it here — the same
    /// spelled-out-arms device [`ReachabilityVerdict::authorizes_redelivery`]
    /// uses in production. A hand-written list of reasons could not do that: a
    /// new variant would simply not appear in it, and every assertion over it
    /// would keep passing.
    fn unknown_reason_index(reason: ReachabilityUnknownReason) -> usize {
        match reason {
            ReachabilityUnknownReason::TranscriptUnresolved => 0,
            ReachabilityUnknownReason::TranscriptCoordinateDivergence => 1,
            ReachabilityUnknownReason::RowlessActiveTurn => 2,
            ReachabilityUnknownReason::ReadTruncated => 3,
            ReachabilityUnknownReason::ReceiptStoreUnreadable => 4,
        }
    }

    /// The table below enumerates every `ReachabilityUnknownReason` exactly
    /// once — none listed twice, none left out.
    ///
    /// That is all the body proves, and it proves it indirectly: it checks that
    /// the table's [`unknown_reason_index`] values cover every index below
    /// [`UNKNOWN_REASON_COUNT`] without collision, which they can only do if
    /// the table is a permutation of the `match`'s arms. What forces a *future*
    /// reason through this file is the exhaustiveness of that `match` — a
    /// compiler obligation, which holds whether or not anyone reads this test.
    /// The guarantee stops at this module: nothing here constrains how B2..B6
    /// later choose to produce or consume the reasons.
    #[test]
    fn every_unknown_reason_is_named_exactly_once() {
        let every_reason = [
            ReachabilityUnknownReason::TranscriptUnresolved,
            ReachabilityUnknownReason::TranscriptCoordinateDivergence,
            ReachabilityUnknownReason::RowlessActiveTurn,
            ReachabilityUnknownReason::ReadTruncated,
            ReachabilityUnknownReason::ReceiptStoreUnreadable,
        ];

        // Deliberately no `every_reason.len() == UNKNOWN_REASON_COUNT` assert:
        // it would make the coverage loop below unreachable, and a check that
        // cannot fail is what this test was rewritten to stop shipping.
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
