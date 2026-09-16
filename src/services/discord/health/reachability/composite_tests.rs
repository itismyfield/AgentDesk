//! Acceptance battery for #5071 T4-B6 (4987 S3).
//!
//! The seven named cases below are 4987 §-1.4's「오탐 반례 — 인수 테스트에 필수
//! 포함」table, one test each. Six of them are false positives: the table
//! forbids `Unreachable` for each, so every one of those asserts the exact
//! expected variant AND that it is not `Unreachable`. Counterexample 2 is the
//! table's single true positive and asserts `Unreachable` instead.

use tempfile::tempdir;

use super::*;
use crate::services::discord::health::STALL_WATCHDOG_INTERVAL_SECS;
use crate::services::discord::health::liveness_authority::CaptureCoordinateObservation;
use crate::services::discord::health::reachability::discovery::TranscriptFileId;
use crate::services::discord::health::reachability::ledger::{
    LedgerIncarnation, LedgerObligation, ReachabilityLedger,
};
use crate::services::discord::health::reachability::ledger_ttl::{
    LEDGER_OBSERVATION_TTL_SECS, LEDGER_OBSERVATION_TTL_TICKS,
    SHORTEST_PERIODIC_PRODUCER_PERIOD_SECS,
};
use crate::services::discord::health::reachability::observation::REACHABILITY_OBSERVATION_INTERVAL_SECS;
use crate::services::discord::health::session_enrichment::{ExecutorWitness, SessionEnrichment};
use crate::services::discord::health::snapshot::HealthStatus;
use crate::services::discord::outbound::delivery_record::{
    ConfirmedDeliveryReceipt, DeliveredCommit, DeliveryRecord, ExactJsonlSourceIdentity,
};
use crate::services::discord::outbound::receipt_index::ReceiptIndexUnknownReason;
use crate::services::discord::relay_health::{
    CoordFrontierObservation, DurableFrontierObservation, FrontierProvenance,
};

const NOW_MS: u64 = 10_000_000;
const PROCESS_STARTED_MS: u64 = 9_000_000;
const GENERATION: i64 = 1_700_491_601;
const SESSION: &str = "AgentDesk-claude-b6";

fn provider() -> ProviderKind {
    ProviderKind::Claude
}

/// An incarnation with a spawn nonce — the additional witness that makes
/// receipt coverage under the `(provider, session, generation)` key
/// attributable to THIS incarnation.
fn proven_incarnation() -> LedgerIncarnation {
    LedgerIncarnation::new(
        SESSION.to_string(),
        GENERATION,
        Some("nonce-b6".to_string()),
        TranscriptFileId { dev: 7, ino: 11 },
    )
}

fn ledger_with(
    obligations: Vec<LedgerObligation>,
    incarnation: LedgerIncarnation,
) -> ReachabilityLedger {
    ReachabilityLedger {
        schema_version: 1,
        incarnation,
        cursor_offset: 4_000,
        bootstrap_offset: 0,
        last_observed_len: 4_000,
        obligations,
        counters: Default::default(),
    }
}

fn obligation(start: u64, end: u64, age_secs: u64) -> LedgerObligation {
    LedgerObligation {
        start,
        end,
        first_observed_at_epoch_ms: NOW_MS - age_secs * 1_000,
    }
}

fn receipt(range: (u64, u64), generation: i64) -> ConfirmedDeliveryReceipt {
    ConfirmedDeliveryReceipt {
        source: ExactJsonlSourceIdentity {
            provider: "claude".to_string(),
            tmux_session_name: SESSION.to_string(),
            turn_nonce: "turn-b6".to_string(),
            range,
            generation_mtime_ns: generation,
            offset_authority_channel_id: 41,
            delivery_channel_id: 42,
        },
        delivery_channel_id: 42,
        message_id: 99,
    }
}

/// Project a delivery record through the SAME reader the runtime uses, so these
/// fixtures exercise the production coverage path rather than a hand-built
/// index. The temp dir is returned so the caller keeps it alive.
fn read_index(record: &DeliveryRecord) -> (ReceiptIndexRead, tempfile::TempDir) {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("record.json");
    std::fs::write(&path, serde_json::to_string(record).expect("serialize")).expect("write");
    (read_receipt_index_at(&path), dir)
}

/// One confirmed receipt covering `[start, end)` under `generation`.
fn receipts_covering(
    start: u64,
    end: u64,
    generation: i64,
) -> (ReceiptIndexRead, tempfile::TempDir) {
    read_index(&DeliveryRecord {
        confirmed_deliveries: vec![receipt((start, end), generation)],
        ..DeliveryRecord::default()
    })
}

struct Case {
    divergence: RowCoordinateDivergence,
    ledger: Option<ReachabilityLedger>,
    ledger_present: bool,
    ledger_observed_at_epoch_ms: Option<u64>,
    executor: ExecutorWitness,
    receipts: ReceiptIndexRead,
    transcript: TranscriptLiveness,
    read_truncated: bool,
    rowless_active_turn: bool,
    placeholder_present: bool,
}

impl Default for Case {
    fn default() -> Self {
        Self {
            divergence: RowCoordinateDivergence::SameFile,
            ledger: Some(ledger_with(Vec::new(), proven_incarnation())),
            ledger_present: true,
            // Observed on this very tick, with a live producer: the default
            // case is a channel nothing has expired and nothing may.
            ledger_observed_at_epoch_ms: Some(NOW_MS),
            executor: ExecutorWitness::Present,
            receipts: ReceiptIndexRead::Absent,
            transcript: TranscriptLiveness::Resolved {
                eof: 4_800,
                alive: true,
            },
            read_truncated: false,
            rowless_active_turn: false,
            placeholder_present: false,
        }
    }
}

impl Case {
    fn classify(&self) -> ReachabilityVerdict {
        self.classify_at(NOW_MS)
    }

    /// `classify` against a caller-supplied wall clock, so a test that reads a
    /// REAL file's timestamp can compare it against real `now` instead of the
    /// synthetic [`NOW_MS`] the rest of the battery runs on.
    fn classify_at(&self, now_epoch_ms: u64) -> ReachabilityVerdict {
        let provider = provider();
        classify_reachability(ReachabilityInputs {
            provider: &provider,
            divergence: self.divergence,
            ledger: self.ledger.as_ref(),
            ledger_present: self.ledger_present,
            ledger_observed_at_epoch_ms: self.ledger_observed_at_epoch_ms,
            executor: self.executor,
            receipts: &self.receipts,
            transcript: self.transcript,
            read_truncated: self.read_truncated,
            rowless_active_turn: self.rowless_active_turn,
            placeholder_present: self.placeholder_present,
            now_epoch_ms,
            process_started_at_epoch_ms: PROCESS_STARTED_MS,
        })
    }
}

/// The table's shared prohibition, asserted separately from each case's exact
/// expectation so a regression that lands on some OTHER wrong variant still
/// reports which half broke.
fn assert_not_unreachable(verdict: &ReachabilityVerdict, case: &str) {
    assert!(
        !matches!(verdict, ReachabilityVerdict::Unreachable { .. }),
        "4987 §-1.4 counterexample {case} forbids Unreachable, got {verdict:?}"
    );
}

// ---------------------------------------------------------------------------
// 4987 §-1.4 counterexample table
// ---------------------------------------------------------------------------

/// #1 — POST succeeded, the receipt write did not survive the crash window.
/// The obligation predates this process, so the restart boundary is the
/// transport trace §-1.3b demotes on. Expected `TransportUnknown`, not
/// `Unreachable`.
#[test]
fn counterexample_1_crash_window_is_transport_unknown_not_unreachable() {
    let stale_age = (NOW_MS - PROCESS_STARTED_MS) / 1_000 + 60;
    let case = Case {
        ledger: Some(ledger_with(
            vec![obligation(1_000, 2_000, stale_age)],
            proven_incarnation(),
        )),
        ..Case::default()
    };
    let verdict = case.classify();
    assert_not_unreachable(&verdict, "1");
    assert!(
        matches!(
            verdict,
            ReachabilityVerdict::TransportUnknown {
                evidence: TransportUnknownEvidence::RestartBoundaryCrossed,
                ..
            }
        ),
        "expected a restart-boundary TransportUnknown, got {verdict:?}"
    );
    assert!(
        verdict.requires_manual_redelivery_ban_notice(),
        "the crash window must carry the do-not-redeliver notice"
    );
}

/// #2 — the table's ONE true positive. Receipts exist, but only under a
/// different generation, so generation gating refuses them and the obligation
/// ages out uncovered. Expected `Unreachable`.
#[test]
fn counterexample_2_foreign_generation_receipts_stay_unreachable() {
    let (foreign, _foreign_dir) = receipts_covering(0, 9_000, GENERATION - 1);
    let case = Case {
        ledger: Some(ledger_with(
            vec![obligation(1_000, 2_000, OBLIGATION_FAIL_BOUND_SECS + 30)],
            proven_incarnation(),
        )),
        receipts: foreign,
        ..Case::default()
    };
    let verdict = case.classify();
    assert!(
        matches!(
            verdict,
            ReachabilityVerdict::Unreachable {
                uncovered_ranges: 1,
                ..
            }
        ),
        "generation gating must hold, got {verdict:?}"
    );

    // The control: the identical range under the CURRENT generation retires.
    // Without this the assertion above would also pass if `covers` ignored
    // receipts entirely.
    let (current, _current_dir) = receipts_covering(0, 9_000, GENERATION);
    let covered = Case {
        ledger: Some(ledger_with(
            vec![obligation(1_000, 2_000, OBLIGATION_FAIL_BOUND_SECS + 30)],
            proven_incarnation(),
        )),
        receipts: current,
        ..Case::default()
    };
    assert_eq!(covered.classify(), ReachabilityVerdict::Reachable);
}

/// #3 — only a previous turn's blocks exist and the current turn produced no
/// prose, so the obligation set is empty. Expected `Reachable` — but only
/// because the incarnation is positively alive; see the alive-gate test below.
#[test]
fn counterexample_3_zero_obligations_is_reachable() {
    let verdict = Case::default().classify();
    assert_not_unreachable(&verdict, "3");
    assert_eq!(verdict, ReachabilityVerdict::Reachable);
}

/// #4 — the row's coordinate and the independently resolved one name different
/// files. §-1.4's point is that equal SIZE cannot mask this, and here that holds
/// structurally rather than by fixture: `TranscriptFileId` carries `dev` and
/// `ino` and no length at all, so there is no size for these two to agree on and
/// none for the comparison to consult. Expected
/// `Unknown{TranscriptCoordinateDivergence}`.
#[test]
fn counterexample_4_same_size_different_inode_is_divergence() {
    let same_size_different_inode = divergence(
        CoordinateObservation::Resolved(TranscriptFileId { dev: 7, ino: 11 }),
        CoordinateObservation::Resolved(TranscriptFileId { dev: 7, ino: 12 }),
    );
    assert_eq!(same_size_different_inode, RowCoordinateDivergence::Diverged);

    let case = Case {
        divergence: same_size_different_inode,
        ..Case::default()
    };
    let verdict = case.classify();
    assert_not_unreachable(&verdict, "4");
    assert_eq!(
        verdict.unknown_reason(),
        Some(ReachabilityUnknownReason::TranscriptCoordinateDivergence)
    );
}

/// #5 — the watchdog's bounded read did not return the older messages, so it
/// publishes `unknown`. The dcserver's own verdict must be UNCHANGED: this is
/// the composition half of the table, so it is asserted across every in-band
/// verdict rather than one.
#[test]
fn counterexample_5_external_unknown_leaves_the_in_band_verdict_unchanged() {
    for in_band in in_band_ladder() {
        let composed = compose_relay_verdict(in_band.clone(), ExternalRelayVerdict::Unknown);
        assert_eq!(composed.in_band(), &in_band);
        assert_eq!(composed.decided_by(), RelayVerdictTier::InBand);
        assert_eq!(
            composed.permits_health(),
            in_band.permits_health(),
            "an unusable sidecar read changed the polarity of {in_band:?}"
        );
        if !matches!(in_band, ReachabilityVerdict::Unreachable { .. }) {
            assert_not_unreachable(composed.in_band(), "5");
        }
    }
}

/// #6 — a placeholder is up and its terminal receipt has not landed. Inside the
/// grace this is `Reachable`; past it, `Degraded`. `Unreachable` is forbidden at
/// every age, which the third leg checks past `fail_bound`.
#[test]
fn counterexample_6_placeholder_without_terminal_receipt_never_reaches_unreachable() {
    let placeholder_case = |age_secs: u64| Case {
        ledger: Some(ledger_with(
            vec![obligation(1_000, 2_000, age_secs)],
            proven_incarnation(),
        )),
        placeholder_present: true,
        ..Case::default()
    };

    let within_grace = placeholder_case(OBLIGATION_WARN_BOUND_SECS - 1).classify();
    assert_not_unreachable(&within_grace, "6 (within grace)");
    assert_eq!(within_grace, ReachabilityVerdict::Reachable);

    let past_grace = placeholder_case(OBLIGATION_WARN_BOUND_SECS + 1).classify();
    assert_not_unreachable(&past_grace, "6 (past grace)");
    assert!(
        matches!(past_grace, ReachabilityVerdict::Degraded { .. }),
        "past the grace a placeholder-only obligation is Degraded, got {past_grace:?}"
    );

    // Past `fail_bound` the placeholder is still a transport trace, so §-1.3b
    // demotes rather than declaring a loss.
    let past_fail = placeholder_case(OBLIGATION_FAIL_BOUND_SECS + 1).classify();
    assert_not_unreachable(&past_fail, "6 (past fail bound)");
    assert!(
        matches!(
            past_fail,
            ReachabilityVerdict::TransportUnknown {
                evidence: TransportUnknownEvidence::PlaceholderPresent,
                ..
            }
        ),
        "expected a placeholder TransportUnknown, got {past_fail:?}"
    );
}

/// #7 — the ledger exists and will not parse. Its coverage is unknown, not
/// absent. Expected `Unknown{ReceiptStoreUnreadable}`, not `Unreachable`.
#[test]
fn counterexample_7_malformed_store_is_unknown_not_unreachable() {
    let malformed_ledger = Case {
        ledger: None,
        ledger_present: true,
        ..Case::default()
    };
    let verdict = malformed_ledger.classify();
    assert_not_unreachable(&verdict, "7 (ledger)");
    assert_eq!(
        verdict.unknown_reason(),
        Some(ReachabilityUnknownReason::ReceiptStoreUnreadable)
    );

    // The receipt store half of the same rule.
    let malformed_receipts = Case {
        receipts: ReceiptIndexRead::Unknown(ReceiptIndexUnknownReason::ReceiptStoreUnreadable),
        ledger: Some(ledger_with(
            vec![obligation(1_000, 2_000, OBLIGATION_FAIL_BOUND_SECS + 60)],
            proven_incarnation(),
        )),
        ..Case::default()
    };
    let verdict = malformed_receipts.classify();
    assert_not_unreachable(&verdict, "7 (receipts)");
    assert_eq!(
        verdict.unknown_reason(),
        Some(ReachabilityUnknownReason::ReceiptStoreUnreadable)
    );

    // A genuinely absent ledger is a different fact and must not borrow the
    // malformed reason — nor, since #5071 relay-tail S1 (I-5), the resolution
    // ladder's.
    let never_written = Case {
        ledger: None,
        ledger_present: false,
        ..Case::default()
    };
    assert_eq!(
        never_written.classify().unknown_reason(),
        Some(ReachabilityUnknownReason::NeverObserved)
    );
}

// ---------------------------------------------------------------------------
// Mutation locks
// ---------------------------------------------------------------------------

fn in_band_ladder() -> Vec<ReachabilityVerdict> {
    vec![
        ReachabilityVerdict::Reachable,
        ReachabilityVerdict::Degraded {
            oldest_unsatisfied_age_secs: 200,
            uncovered_ranges: 1,
        },
        ReachabilityVerdict::TransportUnknown {
            since_secs: 700,
            evidence: TransportUnknownEvidence::RestartBoundaryCrossed,
        },
        ReachabilityVerdict::unknown(ReachabilityUnknownReason::TranscriptUnresolved, 30),
        ReachabilityVerdict::unknown(
            ReachabilityUnknownReason::TranscriptCoordinateDivergence,
            30,
        ),
        ReachabilityVerdict::unknown(ReachabilityUnknownReason::RowlessActiveTurn, 30),
        ReachabilityVerdict::unknown(ReachabilityUnknownReason::ReadTruncated, 30),
        ReachabilityVerdict::unknown(ReachabilityUnknownReason::ReceiptStoreUnreadable, 30),
        ReachabilityVerdict::unknown(ReachabilityUnknownReason::NeverObserved, 30),
        ReachabilityVerdict::unknown(ReachabilityUnknownReason::ProviderUnresolved, 30),
        ReachabilityVerdict::unknown(
            ReachabilityUnknownReason::IncarnationNotAliveWitnessed(
                NotAliveObligationState::NoneOutstanding,
            ),
            30,
        ),
        ReachabilityVerdict::unknown(
            ReachabilityUnknownReason::IncarnationNotAliveWitnessed(
                NotAliveObligationState::WithinGrace,
            ),
            30,
        ),
        ReachabilityVerdict::Unreachable {
            oldest_unsatisfied_age_secs: 900,
            uncovered_ranges: 3,
        },
        // #5942: the ladder is what the composition tests sweep, so a variant
        // missing from it is a variant those tests never compose.
        ReachabilityVerdict::Expired {
            unobserved_for_secs: LEDGER_OBSERVATION_TTL_SECS + 1,
        },
    ]
}

/// No `_` arm: a seventh `ReachabilityVerdict` stops this file compiling until
/// someone decides whether the composition sweeps it.
fn ladder_verdict_index(verdict: &ReachabilityVerdict) -> usize {
    match verdict {
        ReachabilityVerdict::Reachable => 0,
        ReachabilityVerdict::Degraded { .. } => 1,
        ReachabilityVerdict::TransportUnknown { .. } => 2,
        ReachabilityVerdict::Unreachable { .. } => 3,
        ReachabilityVerdict::Unknown { .. } => 4,
        ReachabilityVerdict::Expired { .. } => 5,
    }
}

/// `in_band_ladder()` is what every composition test sweeps, so a variant
/// missing from it is a variant those tests never compose — which is precisely
/// how #5942 r1's `Expired` reached the composition untested. r3 (P2-4) gives
/// this fixture the same exhaustiveness guard the destructive-warrant table
/// got, because "add your variant to the list" is a convention and a convention
/// is not a gate.
#[test]
fn the_composition_ladder_covers_every_verdict_variant() {
    const VERDICT_COUNT: usize = 6;
    let mut seen = [false; VERDICT_COUNT];
    for verdict in in_band_ladder() {
        seen[ladder_verdict_index(&verdict)] = true;
    }
    for (index, seen) in seen.iter().enumerate() {
        assert!(
            *seen,
            "no rung of in_band_ladder() covers verdict index {index}"
        );
    }
}

/// The `Unknown → Healthy` mutation lock. 4987 §4.1: an unobservable relay is
/// not a healthy one. Every `Unknown` reason is checked against every external
/// verdict, including the watchdog's most optimistic one, so a composition that
/// folds `Unknown` into GREEN dies here whichever operand it folds through.
#[test]
fn unknown_never_composes_to_a_health_permitting_verdict() {
    let unknown_reasons = [
        ReachabilityUnknownReason::TranscriptUnresolved,
        ReachabilityUnknownReason::TranscriptCoordinateDivergence,
        ReachabilityUnknownReason::RowlessActiveTurn,
        ReachabilityUnknownReason::ReadTruncated,
        ReachabilityUnknownReason::ReceiptStoreUnreadable,
        // #5071 relay-tail S1 (I-5): the four reasons split out of
        // `TranscriptUnresolved` inherit the same prohibition. Splitting a
        // reason must not create one that composes to GREEN.
        ReachabilityUnknownReason::NeverObserved,
        ReachabilityUnknownReason::ProviderUnresolved,
        ReachabilityUnknownReason::IncarnationNotAliveWitnessed(
            NotAliveObligationState::NoneOutstanding,
        ),
        ReachabilityUnknownReason::IncarnationNotAliveWitnessed(
            NotAliveObligationState::WithinGrace,
        ),
    ];
    for reason in unknown_reasons {
        for external in [
            ExternalRelayVerdict::Unknown,
            ExternalRelayVerdict::NoLoss,
            ExternalRelayVerdict::Lagging { lost_blocks: 0 },
            ExternalRelayVerdict::Unreachable { lost_blocks: 9 },
        ] {
            let composed =
                compose_relay_verdict(ReachabilityVerdict::unknown(reason, 30), external);
            assert!(
                !composed.permits_health(),
                "Unknown{{{reason:?}}} + {external:?} produced a health-permitting verdict"
            );
            assert_ne!(composed.label(), "reachable");
        }
    }
}

/// 4987 §4.3-2: the external tier may only worsen. A watchdog that reports no
/// loss cannot lift ANY in-band claim, and an unusable read cannot either.
#[test]
fn the_external_tier_can_only_worsen() {
    for in_band in in_band_ladder() {
        for external in [ExternalRelayVerdict::Unknown, ExternalRelayVerdict::NoLoss] {
            let composed = compose_relay_verdict(in_band.clone(), external);
            assert_eq!(composed.decided_by(), RelayVerdictTier::InBand);
            assert_eq!(composed.permits_health(), in_band.permits_health());
        }
        // …while a worse external claim does displace a milder in-band one.
        let composed = compose_relay_verdict(
            in_band.clone(),
            ExternalRelayVerdict::Unreachable { lost_blocks: 4 },
        );
        let displaced = !matches!(in_band, ReachabilityVerdict::Unreachable { .. });
        assert_eq!(
            composed.decided_by() == RelayVerdictTier::External,
            displaced,
            "external Unreachable over {in_band:?} decided the wrong way"
        );
        assert!(!composed.permits_health());
    }
}

/// The §-1.3b ban notice survives an external override. A watchdog `gap` laid
/// over an in-band `TransportUnknown` does not turn the crash window into a
/// loss a human may redeliver.
#[test]
fn an_external_override_keeps_the_in_band_redelivery_ban() {
    let composed = compose_relay_verdict(
        ReachabilityVerdict::TransportUnknown {
            since_secs: 700,
            evidence: TransportUnknownEvidence::RestartBoundaryCrossed,
        },
        ExternalRelayVerdict::Unreachable { lost_blocks: 2 },
    );
    assert_eq!(composed.decided_by(), RelayVerdictTier::External);
    assert!(composed.requires_manual_redelivery_ban_notice());
}

/// 4987 §7.1 / I15 at the composed layer: composition adds polarity authority
/// and no capability.
#[test]
fn no_composed_verdict_authorizes_a_destructive_action() {
    for in_band in in_band_ladder() {
        for external in [
            ExternalRelayVerdict::Unknown,
            ExternalRelayVerdict::NoLoss,
            ExternalRelayVerdict::Lagging { lost_blocks: 1 },
            ExternalRelayVerdict::Unreachable { lost_blocks: 1 },
        ] {
            assert!(
                !compose_relay_verdict(in_band.clone(), external).authorizes_destructive_action()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Consumer obligations carried over from the T4-B3 v2 / T4-B5 audit
// ---------------------------------------------------------------------------

/// The frontier EOF clamp, at the consumer. A stale-high same-generation
/// frontier claims bytes past the end of the transcript it was stamped against
/// (#4188). `ReceiptIndex::covers` has no EOF input of its own, so if this
/// classifier stops clamping, the frontier retires an obligation over bytes
/// that no longer exist and the aged range reads GREEN.
#[test]
fn a_stale_high_frontier_does_not_retire_an_obligation_past_the_transcript_eof() {
    let (receipts, _dir) = read_index(&DeliveryRecord {
        delivered_frontier: Some(DeliveredCommit {
            range: (0, 9_000),
            generation_mtime_ns: GENERATION,
            attempts: 1,
            panel_msg_id: None,
            panel_channel_id: None,
        }),
        ..DeliveryRecord::default()
    });
    let aged_beyond_eof = vec![obligation(5_000, 6_000, OBLIGATION_FAIL_BOUND_SECS + 60)];

    // A transcript still long enough for the frontier: nothing to clamp, the
    // obligation retires.
    let long_enough = Case {
        ledger: Some(ledger_with(aged_beyond_eof.clone(), proven_incarnation())),
        receipts: receipts.clone(),
        transcript: TranscriptLiveness::Resolved {
            eof: 9_000,
            alive: true,
        },
        ..Case::default()
    };
    assert_eq!(long_enough.classify(), ReachabilityVerdict::Reachable);

    // The same frontier over a transcript that is now 4000 bytes long. Clamped,
    // it cannot reach 5000..6000, so the obligation stays unsatisfied.
    let shortened = Case {
        ledger: Some(ledger_with(aged_beyond_eof, proven_incarnation())),
        receipts,
        transcript: TranscriptLiveness::Resolved {
            eof: 4_000,
            alive: true,
        },
        ..Case::default()
    };
    let verdict = shortened.classify();
    assert!(
        !verdict.permits_health(),
        "an unclamped stale-high frontier reported GREEN, got {verdict:?}"
    );
}

/// The unproven-generation rule. Without a spawn nonce the projection key
/// cannot separate this incarnation from a same-generation predecessor on the
/// bump-failure path, so coverage under it may not retire an aged obligation
/// into GREEN — and equally may not push it to `Unreachable`, because a receipt
/// does exist.
#[test]
fn coverage_under_an_unproven_generation_is_not_promoted_to_green() {
    let unproven = LedgerIncarnation::new(
        SESSION.to_string(),
        GENERATION,
        None,
        TranscriptFileId { dev: 7, ino: 11 },
    );
    let (receipts, _dir) = receipts_covering(0, 9_000, GENERATION);
    let aged = |incarnation: LedgerIncarnation| Case {
        ledger: Some(ledger_with(
            vec![obligation(1_000, 2_000, OBLIGATION_FAIL_BOUND_SECS + 60)],
            incarnation,
        )),
        receipts: receipts.clone(),
        ..Case::default()
    };

    // With the witness, the same coverage retires the obligation.
    assert_eq!(
        aged(proven_incarnation()).classify(),
        ReachabilityVerdict::Reachable
    );

    let verdict = aged(unproven).classify();
    assert!(
        !verdict.permits_health(),
        "unproven coverage must not reach GREEN, got {verdict:?}"
    );
    assert_not_unreachable(&verdict, "unproven generation");
    assert!(
        matches!(verdict, ReachabilityVerdict::Degraded { .. }),
        "a held-but-covered obligation caps at Degraded, got {verdict:?}"
    );
}

// ---------------------------------------------------------------------------
// 4987 §6.2's two residual assertion clauses
// ---------------------------------------------------------------------------

/// The §6.2 enrichment shape, as [`SessionEnrichment::desynced`] sees it.
///
/// That predicate reads exactly four fields — `capture_lagged`,
/// `inflight_state_present`, `relay_stale` and `tmux_session_mismatch` — plus
/// its two arguments; the rest of this literal is inert for it and is set to
/// what `SessionEnrichment::load` would carry in this shape.
///
/// The shape is §6.2's: an in-flight row exists and names an `output_path` that
/// no longer stats, while the registry's transcript resolves. `load` derives
/// `last_capture_offset` from that failed stat, so it is `None`, and its
/// `capture_lagged` term is `unwrap_or(false)` over exactly that `Option` —
/// which is why `capture_lagged` is a parameter here rather than a constant:
/// `false` is the §6.2 value, and `true` is passed only to show the predicate
/// can still answer. That derivation belongs to `load` and is not re-asserted
/// here; this builds the record `load` would hand `desynced`.
///
/// `relay_stale` is set even though §6.2 does not pin it, because it is the one
/// inert-looking field that could make `false` a lucky answer: it is the last
/// conjunct of the orphan term, so setting it leaves the `attached` ARGUMENT as
/// the only thing holding that term down. `health/snapshot.rs` passes
/// `watcher_attached` for that argument, which is what the caller below passes
/// too — the same-named `attached` field is not read by `desynced` at all.
///
/// `inflight` stays `None` while `inflight_state_present` is `true`: 4987 I14
/// forbids this tree from naming the in-flight row type at all, and `desynced`
/// reads the flag, never the row.
fn section_6_2_enrichment(capture_lagged: bool) -> SessionEnrichment {
    let row_output_path = "/nonexistent/agentdesk/b6-row-transcript.jsonl";
    SessionEnrichment {
        inflight: None,
        attached: true,
        watcher_attached: true,
        watcher_attached_stale: false,
        has_relay_coord: true,
        watcher_owner_channel_id: None,
        watcher_output_path: Some("/nonexistent/agentdesk/b6-registry.jsonl".to_string()),
        tmux_session: Some(SESSION.to_string()),
        inflight_state_present: true,
        tmux_session_mismatch: false,
        last_relay_offset: 0,
        last_relay_ts_ms: 0,
        reconnect_count: 0,
        last_capture_offset: None,
        capture_coordinate: CaptureCoordinateObservation::missing(Some(row_output_path)),
        unread_bytes: None,
        relay_stale: true,
        capture_lagged,
        // #5071 relay-tail S1 (I-4): the §6.2 shape has a coordinate entry that
        // never advanced and no durable row — `has_relay_coord` above already
        // says the first half. `desynced` reads neither.
        frontier_provenance: FrontierProvenance::observe(
            CoordFrontierObservation::PresentZero,
            DurableFrontierObservation::RowAbsent,
        ),
    }
}

/// 4987 §6.2's mutation test carries four clauses. Its first two — the shape of
/// the divergence input and the `Unknown{TranscriptCoordinateDivergence}` it
/// produces — landed with T4-B4. These are the remaining two: the pre-existing
/// structural signal is STILL blind (`desynced == false`), and the composed
/// verdict is nevertheless not healthy.
///
/// Clause 3 calls [`SessionEnrichment::desynced`] on the §6.2 shape
/// [`section_6_2_enrichment`] builds, so a change that made that predicate fire
/// on this shape fails here. This composition does not change that value; it
/// adds a second authority beside it.
#[test]
fn section_6_2_divergence_leaves_desynced_false_and_the_relay_verdict_not_healthy() {
    let row_path_missing_while_registry_live = divergence(
        CoordinateObservation::Unresolvable,
        CoordinateObservation::Resolved(TranscriptFileId { dev: 7, ino: 11 }),
    );
    assert_eq!(
        row_path_missing_while_registry_live,
        RowCoordinateDivergence::RowPathUnresolvableWhileRegistryLive
    );

    let case = Case {
        divergence: row_path_missing_while_registry_live,
        ..Case::default()
    };
    let in_band = case.classify();
    assert_eq!(
        in_band.unknown_reason(),
        Some(ReachabilityUnknownReason::TranscriptCoordinateDivergence)
    );

    // Clause 3: the structural signal this design says is blind here, read out
    // of the production predicate for this shape rather than restated.
    let tmux_present = true;
    let session = section_6_2_enrichment(false);
    assert!(
        !session.desynced(tmux_present, session.watcher_attached),
        "§6.2 asserts the pre-existing desynced term stays false in this shape"
    );
    // The same call with the capture-lag term set answers `true`, so the line
    // above is this predicate's answer to THIS shape and not a predicate that
    // cannot say anything else.
    let lagging = section_6_2_enrichment(true);
    assert!(
        lagging.desynced(tmux_present, lagging.watcher_attached),
        "the desynced predicate must still fire when its capture-lag term is set"
    );

    // Clause 4: the composed verdict is not healthy anyway — and stays that way
    // whatever the external tier says, including its most optimistic answer.
    for external in [
        ExternalRelayVerdict::Unknown,
        ExternalRelayVerdict::NoLoss,
        ExternalRelayVerdict::Lagging { lost_blocks: 0 },
        ExternalRelayVerdict::Unreachable { lost_blocks: 3 },
    ] {
        let composed = compose_relay_verdict(in_band.clone(), external);
        assert!(
            !composed.permits_health(),
            "§6.2 clause 4: RelayVerdict must not be Healthy under {external:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// The §5.1 switch
// ---------------------------------------------------------------------------

/// 4987 §5.1: the composed verdict is produced identically in both modes, and
/// only `Composite` lets it decide. The report published on the detail surface
/// records which of the two applied.
#[test]
fn the_switch_changes_authority_and_not_the_composed_value() {
    let composed = compose_relay_verdict(
        ReachabilityVerdict::unknown(ReachabilityUnknownReason::ReceiptStoreUnreadable, 42),
        ExternalRelayVerdict::NoLoss,
    );

    let structural = RelayVerdictReport::of(&composed, false);
    let composite = RelayVerdictReport::of(&composed, true);
    assert_eq!(structural.verdict, composite.verdict);
    assert_eq!(structural.reason, Some("receipt_store_unreadable"));
    assert!(!structural.governs_health_polarity);
    assert!(composite.governs_health_polarity);

    assert_eq!(relay_verdict_source(), RelayVerdictSource::Structural);
    assert!(!relay_verdict_source().governs_health_polarity());
    let _guard = set_relay_verdict_source_for_tests(RelayVerdictSource::Composite);
    assert!(relay_verdict_source().governs_health_polarity());
}

/// 4987 §-1.4's blind spot fix, stated as its own lock because it is the one
/// rule a future change is most likely to "simplify" away: with no obligations
/// AND no positive incarnation-alive evidence, the answer is `Unknown`, not
/// `Reachable`. "Nothing observed" is not GREEN.
#[test]
fn nothing_observed_is_not_green() {
    let not_alive = Case {
        transcript: TranscriptLiveness::Resolved {
            eof: 4_000,
            alive: false,
        },
        ..Case::default()
    };
    assert_eq!(
        not_alive.classify().unknown_reason(),
        Some(ReachabilityUnknownReason::IncarnationNotAliveWitnessed(
            NotAliveObligationState::NoneOutstanding
        ))
    );

    let unresolved = Case {
        transcript: TranscriptLiveness::Unresolved,
        ..Case::default()
    };
    assert_eq!(
        unresolved.classify().unknown_reason(),
        Some(ReachabilityUnknownReason::TranscriptUnresolved)
    );
}

// ---------------------------------------------------------------------------
// #5071 relay-tail S1 (I-5): the reason-splitting table
// ---------------------------------------------------------------------------

/// The string the health detail publishes for a branch, taken from the
/// PRODUCTION formatter rather than restated here — a split that renames a
/// reason without telling the surface would otherwise pass.
fn published_reason(verdict: &ReachabilityVerdict) -> &'static str {
    unknown_reason_str(
        verdict
            .unknown_reason()
            .expect("every branch in this table produces an Unknown"),
    )
}

/// The provider-absent branch lives in `observe_relay_verdict`, upstream of
/// `classify_reachability`: with no provider there is no ledger, receipt
/// projection or sidecar path to build. Every coordinate is `None`, so the
/// probe returns before touching the filesystem.
fn provider_absent_reason() -> &'static str {
    let verdict = observe_relay_verdict(RelayVerdictProbe {
        provider: None,
        channel_id: 42,
        row_output_path: None,
        registry_output_path: None,
        pane_idle_confirmed: false,
        rowless_active_turn: false,
        placeholder_present: false,
        executor: ExecutorWitness::Unwitnessed,
        now_epoch_ms: NOW_MS,
        process_started_at_epoch_ms: PROCESS_STARTED_MS,
    });
    RelayVerdictReport::of(&verdict, false)
        .reason
        .expect("a provider-absent probe is Unknown")
}

/// One fixture per branch that used to spell `transcript_unresolved`, each
/// built from the shape that actually reaches it.
fn split_branch_reasons() -> [(&'static str, &'static str); 5] {
    let ladder_failed = Case {
        transcript: TranscriptLiveness::Unresolved,
        ..Case::default()
    };
    let never_written = Case {
        ledger: None,
        ledger_present: false,
        ..Case::default()
    };
    let not_alive_nothing_owed = Case {
        transcript: TranscriptLiveness::Resolved {
            eof: 4_000,
            alive: false,
        },
        ..Case::default()
    };
    let not_alive_within_grace = Case {
        ledger: Some(ledger_with(
            vec![obligation(0, 512, OBLIGATION_WARN_BOUND_SECS / 2)],
            proven_incarnation(),
        )),
        transcript: TranscriptLiveness::Resolved {
            eof: 4_000,
            alive: false,
        },
        ..Case::default()
    };

    [
        (
            "resolution ladder failed",
            published_reason(&ladder_failed.classify()),
        ),
        (
            "no ledger was ever written",
            published_reason(&never_written.classify()),
        ),
        (
            "incarnation not alive, nothing owed",
            published_reason(&not_alive_nothing_owed.classify()),
        ),
        (
            "incarnation not alive, obligation inside the grace",
            published_reason(&not_alive_within_grace.classify()),
        ),
        ("no provider owns the channel", provider_absent_reason()),
    ]
}

/// I-5's acceptance: each of the five branches names itself.
#[test]
fn each_branch_that_shared_transcript_unresolved_now_names_itself() {
    let expected = [
        "transcript_unresolved",
        "never_observed",
        "incarnation_not_alive_no_obligations",
        "incarnation_not_alive_within_grace",
        "provider_unresolved",
    ];
    for ((branch, actual), expected) in split_branch_reasons().into_iter().zip(expected) {
        assert_eq!(actual, expected, "branch: {branch}");
    }
}

/// The other half of the same claim, and the one a future "simplification"
/// would break silently: no two branches may share a string. Asserted over the
/// produced values rather than the expected ones, so collapsing two arms of
/// `unknown_reason_str` back together fails here even if the fixtures still
/// look distinct.
#[test]
fn the_five_branches_are_pairwise_distinguishable() {
    let produced = split_branch_reasons();
    for (index, (branch, reason)) in produced.iter().enumerate() {
        for (other_branch, other_reason) in produced.iter().skip(index + 1) {
            assert_ne!(
                reason, other_reason,
                "'{branch}' and '{other_branch}' are indistinguishable on the detail surface"
            );
        }
    }
}

/// And the overloaded string itself: `transcript_unresolved` answers for
/// exactly one of the five now, so reading it off #adk-cc's detail entry
/// identifies a branch instead of naming a set of five.
#[test]
fn transcript_unresolved_no_longer_answers_for_five_branches() {
    assert_eq!(
        split_branch_reasons()
            .iter()
            .filter(|(_, reason)| *reason == "transcript_unresolved")
            .count(),
        1
    );
}

// ---------------------------------------------------------------------------
// #5942: the ledger TTL
//
// A routine thread's producer lives for a few minutes and then exits. The
// ledger it leaves behind has no tmux session to resolve its transcript
// against, so every later tick reads `unknown{transcript_unresolved}`, which
// `permits_health()` refuses — and with no expiry path the node is pinned
// `ok=false` for as long as the file exists. These cases pin the expiry that
// ends that, and — the larger half of the battery — the five separate
// conditions that must hold before anything may expire at all.
// ---------------------------------------------------------------------------

/// The shape a dead routine thread leaves behind: a ledger owing nothing, with
/// no producer left, last written well outside the TTL.
fn abandoned_ledger_case() -> Case {
    Case {
        ledger: Some(ledger_with(Vec::new(), proven_incarnation())),
        ledger_observed_at_epoch_ms: Some(NOW_MS - (LEDGER_OBSERVATION_TTL_SECS + 60) * 1_000),
        executor: ExecutorWitness::Absent,
        // The production shape: with the session gone the registry resolves no
        // transcript, which is what makes the entry permanently unknown today.
        transcript: TranscriptLiveness::Unresolved,
        ..Case::default()
    }
}

/// The verdict an unexpired version of the same channel produces. Named once so
/// every guard below asserts "still THIS", not merely "not expired" — a guard
/// that let the classifier fall through to some other wrong answer would
/// otherwise pass.
fn unexpired_verdict() -> ReachabilityVerdict {
    ReachabilityVerdict::unknown(ReachabilityUnknownReason::TranscriptUnresolved, 0)
}

/// The fix: a ledger past its TTL with no producer and nothing owed leaves the
/// health judgement — as an `Expired` that is still published, never as a
/// `Reachable` and never as silence.
#[test]
fn an_abandoned_ledger_past_its_ttl_expires_out_of_the_health_judgment() {
    let verdict = abandoned_ledger_case().classify();
    assert!(
        matches!(verdict, ReachabilityVerdict::Expired { .. }),
        "an abandoned ledger past its TTL must expire, got {verdict:?}"
    );
    assert!(
        !verdict.permits_health(),
        "expiry must not be dressed up as health"
    );
    assert!(
        verdict.abstains_from_health_polarity(),
        "an expired ledger must withdraw from the polarity, not decide it"
    );

    let composed = compose_relay_verdict(verdict, ExternalRelayVerdict::Unknown);
    let mut degraded_reasons = Vec::new();
    let mut expired = Vec::new();
    let mut status = HealthStatus::Healthy;
    apply_relay_verdict_polarity(
        true,
        &composed,
        "claude",
        1_519_100_943_889_727_641,
        &mut degraded_reasons,
        &mut expired,
        &mut status,
    );
    assert!(
        degraded_reasons.is_empty(),
        "#5942: an expired ledger must stop producing degraded reasons, got {degraded_reasons:?}"
    );
    assert_eq!(
        status,
        HealthStatus::Healthy,
        "an expired ledger must not pin the node non-GREEN"
    );
    assert_eq!(
        expired,
        vec!["relay_verdict_expired_claude_1519100943889727641".to_string()],
        "expiry must stay observable under its own name"
    );
}

/// Over-expiry guard 1 — the TTL edge. At the bound and one second under it the
/// ledger is still in the judgement; the first expiry is strictly past it.
#[test]
fn a_ledger_inside_its_ttl_is_not_expired() {
    for age_secs in [
        0,
        1,
        LEDGER_OBSERVATION_TTL_SECS - 1,
        LEDGER_OBSERVATION_TTL_SECS,
    ] {
        let case = Case {
            ledger_observed_at_epoch_ms: Some(NOW_MS - age_secs * 1_000),
            ..abandoned_ledger_case()
        };
        assert_eq!(
            case.classify(),
            unexpired_verdict(),
            "a ledger observed {age_secs}s ago is inside the TTL and must still be judged"
        );
    }

    let just_past = Case {
        ledger_observed_at_epoch_ms: Some(NOW_MS - (LEDGER_OBSERVATION_TTL_SECS + 1) * 1_000),
        ..abandoned_ledger_case()
    };
    assert_eq!(
        just_past.classify(),
        ReachabilityVerdict::Expired {
            unobserved_for_secs: LEDGER_OBSERVATION_TTL_SECS + 1,
        },
        "the first second past the bound expires, and reports its own age"
    );
}

/// Over-expiry guard 2 — a live producer. A stale ledger beside a session that
/// is still up is a DIFFERENT defect (the observation task is not committing),
/// and expiring it would hide exactly that.
#[test]
fn a_live_executor_blocks_expiry_of_a_stale_ledger() {
    let case = Case {
        executor: ExecutorWitness::Present,
        ..abandoned_ledger_case()
    };
    assert_eq!(
        case.classify(),
        unexpired_verdict(),
        "a stale ledger with a live producer must stay in the judgement"
    );
}

/// Over-expiry guard 3 — an unwitnessed producer. A spent probe budget or a
/// probe that did not return is not evidence the session is gone, so it may not
/// expire anything.
#[test]
fn an_unwitnessed_executor_blocks_expiry_of_a_stale_ledger() {
    let case = Case {
        executor: ExecutorWitness::Unwitnessed,
        ..abandoned_ledger_case()
    };
    assert_eq!(
        case.classify(),
        unexpired_verdict(),
        "an unwitnessed probe must never be read as an absent producer"
    );
}

/// Over-expiry guard 4 — an outstanding obligation. This is the one that keeps
/// the TTL from eating a real loss: bytes were owed when the producer died, and
/// that is a delivery failure, not an abandoned ledger.
#[test]
fn an_outstanding_obligation_blocks_expiry_of_a_stale_ledger() {
    let case = Case {
        ledger: Some(ledger_with(
            vec![obligation(1_000, 2_000, OBLIGATION_FAIL_BOUND_SECS + 30)],
            proven_incarnation(),
        )),
        ..abandoned_ledger_case()
    };
    let verdict = case.classify();
    assert!(
        !matches!(verdict, ReachabilityVerdict::Expired { .. }),
        "an unsatisfied obligation must survive the TTL, got {verdict:?}"
    );
    assert_eq!(verdict, unexpired_verdict());
}

/// Over-expiry guard 5 — a live turn. A placeholder outstanding in Discord, or
/// a mailbox reporting an active turn, both say something is still going on for
/// this channel whatever the tmux probe answered.
#[test]
fn a_live_placeholder_or_rowless_active_turn_blocks_expiry() {
    let with_placeholder = Case {
        placeholder_present: true,
        ..abandoned_ledger_case()
    };
    assert_eq!(
        with_placeholder.classify(),
        unexpired_verdict(),
        "an outstanding placeholder must not be expired away"
    );

    let with_active_turn = Case {
        rowless_active_turn: true,
        ..abandoned_ledger_case()
    };
    assert_eq!(
        with_active_turn.classify(),
        unexpired_verdict(),
        "a mailbox-reported active turn must not be expired away"
    );
}

/// Over-expiry guard 6 — an undated ledger. A clock that could not be read is
/// not an old ledger, so `None` expires nothing.
#[test]
fn an_undated_ledger_is_never_expired() {
    let case = Case {
        ledger_observed_at_epoch_ms: None,
        ..abandoned_ledger_case()
    };
    assert_eq!(
        case.classify(),
        unexpired_verdict(),
        "an unreadable observation clock must not expire a ledger"
    );
}

/// P2-3, the gate-position re-adjudication: expiry may never preempt a verdict
/// the transcript ladder would have called `Reachable`.
///
/// The gate runs BEFORE the ladder, so this is a real hazard rather than a
/// theoretical one, and r2 defended the position with the wrong argument — that
/// a ledger with no producer "can never pass the ladder". It can: a transcript
/// resolved with `alive: true` is 4987 §-1.4's positive evidence, and the
/// reviewer demonstrated it composing to `Reachable` one tick before the gate
/// expired it. The position is kept and the ARGUMENT replaced by a condition:
/// conjunct 5 refuses to expire over positive alive evidence, which is the
/// complete set of inputs that can produce `Reachable`, so the gate is now
/// provably unable to preempt a GREEN answer.
#[test]
fn a_live_transcript_is_never_expired_even_with_no_executor_and_a_cold_ledger() {
    let case = Case {
        transcript: TranscriptLiveness::Resolved {
            eof: 4_800,
            alive: true,
        },
        ..abandoned_ledger_case()
    };
    assert_eq!(
        case.classify(),
        ReachabilityVerdict::Reachable,
        "positive §-1.4 alive evidence outranks the TTL; expiring it would have hidden a \
         channel that was answering"
    );
}

/// The other half of conjunct 5, so the guard is a condition and not a blanket
/// "any resolved transcript is safe".
///
/// `Resolved { alive: false }` still expires, and that is the correct trade:
/// the ladder's answer for it is `unknown{incarnation_not_alive_witnessed}`,
/// which `permits_health()` refuses exactly as `transcript_unresolved` does and
/// which no later tick can retire on its own. Expiring it is the #5942 fix
/// doing its job; refusing to would have made conjunct 5 a second, much wider
/// rule than the one that was argued for.
#[test]
fn a_resolved_but_not_alive_transcript_still_expires_with_no_executor() {
    let case = Case {
        transcript: TranscriptLiveness::Resolved {
            eof: 4_800,
            alive: false,
        },
        ..abandoned_ledger_case()
    };
    assert!(
        matches!(case.classify(), ReachabilityVerdict::Expired { .. }),
        "a ledger with no producer and no alive witness must still expire, got {:?}",
        case.classify()
    );

    // And the verdict it displaces is itself non-GREEN and permanent — the
    // thing being retired is a stuck answer, not a finding.
    let with_a_producer = Case {
        executor: ExecutorWitness::Present,
        ..case
    };
    let ladder_answer = with_a_producer.classify();
    assert!(
        !ladder_answer.permits_health(),
        "if the ladder's own answer here were health, conjunct 5 would be too narrow; got \
         {ladder_answer:?}"
    );
}

/// Expiry must be READABLE, not merely quiet: the detail surface names the
/// verdict, its reason, its age, and the fact that it abstained.
#[test]
fn the_expired_report_publishes_its_age_and_its_abstention() {
    let composed = compose_relay_verdict(
        abandoned_ledger_case().classify(),
        ExternalRelayVerdict::Unknown,
    );
    let report = RelayVerdictReport::of(&composed, true);
    assert_eq!(report.verdict, "expired");
    assert_eq!(report.reason, Some("ledger_unobserved_past_ttl"));
    assert_eq!(
        report.unobserved_for_secs,
        Some(LEDGER_OBSERVATION_TTL_SECS + 60)
    );
    assert!(report.health_polarity_abstained);
    assert!(
        report.governs_health_polarity,
        "the §5.1 switch was live; abstention is this entry's own answer, not the switch's"
    );
    assert!(!report.manual_redelivery_banned);

    // And the pair is not published on anything else.
    let unexpired = compose_relay_verdict(unexpired_verdict(), ExternalRelayVerdict::Unknown);
    let unexpired_report = RelayVerdictReport::of(&unexpired, true);
    assert_eq!(unexpired_report.unobserved_for_secs, None);
    assert!(!unexpired_report.health_polarity_abstained);
}

/// An abstention is Tier A's, and only while Tier A decides. An out-of-band
/// watchdog that actually observed lost blocks on this channel outranks an
/// expired ledger and degrades on its own evidence.
#[test]
fn the_external_tier_still_degrades_an_expired_ledger() {
    let expired = abandoned_ledger_case().classify();
    for external in [
        ExternalRelayVerdict::Lagging { lost_blocks: 2 },
        ExternalRelayVerdict::Unreachable { lost_blocks: 5 },
    ] {
        let composed = compose_relay_verdict(expired.clone(), external);
        assert_eq!(composed.decided_by(), RelayVerdictTier::External);
        assert!(!composed.permits_health());
        assert!(
            !composed.abstains_from_health_polarity(),
            "a watchdog finding must not be swallowed by an expired ledger: {external:?}"
        );

        let mut degraded_reasons = Vec::new();
        let mut expired_ledgers = Vec::new();
        let mut status = HealthStatus::Healthy;
        apply_relay_verdict_polarity(
            true,
            &composed,
            "claude",
            7,
            &mut degraded_reasons,
            &mut expired_ledgers,
            &mut status,
        );
        assert_eq!(degraded_reasons.len(), 1, "{external:?} must still degrade");
        assert!(expired_ledgers.is_empty());
        assert_eq!(status, HealthStatus::Degraded);
    }

    // The control: the same expired verdict with a watchdog that claims no loss
    // keeps Tier A and keeps abstaining, so the assertions above are testing the
    // external rank and not the expiry.
    let quiet = compose_relay_verdict(expired, ExternalRelayVerdict::NoLoss);
    assert_eq!(quiet.decided_by(), RelayVerdictTier::InBand);
    assert!(quiet.abstains_from_health_polarity());
}

/// The TTL is derived from the observation cadence and then CHECKED against the
/// two independent quantities it has to sit between.
///
/// r1 wrote the lower bound as `TTL >= FAIL_BOUND` while defining
/// `TTL = FAIL_BOUND`, so it could not fail — a tautology wearing an
/// assertion's clothes. The TTL is now `ticks × cadence`, which is what
/// actually stamps the file, so both comparisons below are between numbers that
/// move for different reasons and either can go red:
///
/// * under [`OBLIGATION_FAIL_BOUND_SECS`] an obligation could be expired out
///   from under the ladder still grading it;
/// * at or above [`SHORTEST_PERIODIC_PRODUCER_PERIOD_SECS`] a routine re-stamps
///   its ledger every cycle, renews the TTL forever, and never expires — #5942
///   with extra steps.
///
/// What this still cannot check is whether
/// `SHORTEST_PERIODIC_PRODUCER_PERIOD_SECS` matches the routines actually
/// installed, because those are runtime rows behind `/api/routines`. The
/// constant's own doc records that, and records why the failure is a return to
/// today's behaviour rather than a new one.
#[test]
fn the_ledger_ttl_holds_against_the_delivery_bound_and_the_producer_period() {
    // The derivation itself: ticks of the real observation cadence.
    assert_eq!(
        LEDGER_OBSERVATION_TTL_SECS,
        LEDGER_OBSERVATION_TTL_TICKS * REACHABILITY_OBSERVATION_INTERVAL_SECS,
        "the TTL must stay counted in observation ticks, not aliased to another bound"
    );
    assert!(
        LEDGER_OBSERVATION_TTL_SECS >= OBLIGATION_FAIL_BOUND_SECS,
        "a TTL under the fail bound ({OBLIGATION_FAIL_BOUND_SECS}s) can expire an obligation \
         the ladder is still grading"
    );
    assert!(
        LEDGER_OBSERVATION_TTL_SECS < SHORTEST_PERIODIC_PRODUCER_PERIOD_SECS,
        "a TTL at or above the shortest producer period can be renewed forever and never fires"
    );
}

/// The cadence the TTL counts is the OBSERVATION task's, and the observation
/// task really reads that constant.
///
/// r2 derived the TTL from `STALL_WATCHDOG_INTERVAL_SECS`, which is the stall
/// watchdog's number: retuning the watchdog would have silently moved a
/// reachability TTL that has nothing to do with it. r3 gave the reachability
/// tree its own [`REACHABILITY_OBSERVATION_INTERVAL_SECS`], which
/// `runtime_bootstrap::spawns::run_bot_spawn_reachability_observation` now
/// sleeps on.
///
/// The two are EQUAL today, and that is a coincidence worth pinning rather than
/// collapsing: if someone retunes the watchdog, this assertion goes red and a
/// human decides whether the observation cadence should follow, instead of the
/// TTL moving on its own.
#[test]
fn the_ledger_ttl_is_counted_in_the_observation_tasks_own_cadence() {
    assert_eq!(
        REACHABILITY_OBSERVATION_INTERVAL_SECS, STALL_WATCHDOG_INTERVAL_SECS,
        "the observation task is driven off the stall watchdog's tick today; if that changed, \
         decide deliberately whether the reachability TTL follows"
    );
    assert_eq!(
        LEDGER_OBSERVATION_TTL_SECS % REACHABILITY_OBSERVATION_INTERVAL_SECS,
        0,
        "a TTL that is not a whole number of observation ticks cannot be read as a tick count"
    );
}

/// The TTL's clock is the ledger file's MODIFICATION time — not its access
/// time, not its inode-change time, and not a constant.
///
/// r2's version only checked that a just-written file reads back "within 60
/// seconds of now", which every one of those three clocks satisfies: swapping
/// `.modified()` for `.accessed()` kept it green. That mutant is not academic.
/// On Linux `relatime` — the default on the deployment host's filesystems —
/// each health tick's `read_ledger_at` would touch atime forward, so an
/// atime-based TTL would be renewed by the very act of reading it and NOTHING
/// would ever expire; #5942 would ship as a no-op that passes its own tests.
///
/// So the file here is given a modification time far in the past and an access
/// time of NOW, which separates the three clocks: `.modified()` returns the
/// backdated value, while `.accessed()` and the inode-change time both land on
/// now. Then the value is fed to the classifier against a real wall clock, so
/// the file's clock and the verdict are connected by the test rather than by
/// assumption.
#[test]
fn the_ttl_clock_is_the_ledger_files_modification_time_not_its_access_time() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("ledger.json");
    std::fs::write(&path, "{}").expect("write");

    let now = std::time::SystemTime::now();
    let now_ms = now
        .duration_since(std::time::UNIX_EPOCH)
        .expect("epoch")
        .as_millis() as u64;
    let backdated_secs = LEDGER_OBSERVATION_TTL_SECS + 300;
    let backdated = now - std::time::Duration::from_secs(backdated_secs);
    let times = std::fs::FileTimes::new()
        .set_accessed(now)
        .set_modified(backdated);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .expect("open for set_times")
        .set_times(times)
        .expect("set_times");

    let observed = ledger_committed_at_epoch_ms(&path).expect("a written file has a commit time");
    let age_secs = (now_ms.saturating_sub(observed)) / 1_000;
    assert!(
        age_secs > LEDGER_OBSERVATION_TTL_SECS,
        "the commit time must be the backdated mtime ({backdated_secs}s old), but it read as \
         {age_secs}s old — an atime- or ctime-based clock would read ~0 here, and under Linux \
         relatime every health tick would renew it"
    );
    assert!(
        age_secs < backdated_secs + 120,
        "the commit time must be THIS file's mtime, got an age of {age_secs}s against the \
         {backdated_secs}s that was set"
    );

    // And the classifier really runs on that number: same file, real clock.
    let case = Case {
        ledger_observed_at_epoch_ms: Some(observed),
        ..abandoned_ledger_case()
    };
    assert!(
        matches!(
            case.classify_at(now_ms),
            ReachabilityVerdict::Expired { .. }
        ),
        "a ledger whose FILE says it has not been stamped in {backdated_secs}s must expire"
    );

    assert_eq!(
        ledger_committed_at_epoch_ms(&dir.path().join("absent.json")),
        None,
        "a ledger with no file has no commit time, and an unknown clock expires nothing"
    );
}

/// #5942 r4 (P1-3): the TTL runs on the ledger `observe_relay_verdict` LOCATES,
/// not on a number a test handed it.
///
/// Every other TTL test in this file builds [`ReachabilityInputs`] by hand
/// through [`Case`], and the closest one to production —
/// `the_ttl_clock_is_the_ledger_files_modification_time_not_its_access_time` —
/// still calls `ledger_committed_at_epoch_ms` ITSELF and passes the result in.
/// So the production wiring in `observe_relay_verdict` — resolving the path,
/// stamping the commit time onto the inputs, forwarding the caller's executor
/// witness — had no test at all, and an adversarial review killed it twice with
/// the whole 623-test suite green:
///
/// * `ledger_path…and_then(ledger_committed_at_epoch_ms)` → `.and(None)`, which
///   makes conjunct (6) unreachable and #5942 a permanent no-op;
/// * `executor: probe.executor` → `ExecutorWitness::Absent`, which discards
///   conjunct (1) and expires a stale ledger sitting next to a LIVE tmux.
///
/// This drives the real entry point against a real file at the canonical path
/// and asserts both directions, so neither mutant survives: the first dies on
/// the expiry, the second on the two witnesses that must refuse it.
#[test]
fn observe_relay_verdict_expires_a_backdated_ledger_it_located_itself() {
    let root = tempdir().expect("temp runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());

    let provider = provider();
    // Distinct from every other channel id in this binary: the test runtime
    // root is process-wide, so a shared id would let two tests read each
    // other's ledgers.
    let channel_id = 5_942_000_000_000_000_041_u64;
    let path = ledger_path(&provider, channel_id).expect("canonical ledger path");
    std::fs::create_dir_all(path.parent().expect("ledger parent"))
        .expect("create canonical ledger directory");
    std::fs::write(
        &path,
        serde_json::to_string(&ledger_with(Vec::new(), proven_incarnation()))
            .expect("serialize ledger"),
    )
    .expect("write ledger");

    let now = std::time::SystemTime::now();
    let now_ms = now
        .duration_since(std::time::UNIX_EPOCH)
        .expect("epoch")
        .as_millis() as u64;
    let stamp = |at: std::time::SystemTime| {
        std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open ledger for set_times")
            .set_times(std::fs::FileTimes::new().set_accessed(now).set_modified(at))
            .expect("set_times");
    };
    let observe = |executor| {
        observe_relay_verdict(RelayVerdictProbe {
            provider: Some(&provider),
            channel_id,
            // No coordinates and no transcript: the shape a routine leaves
            // behind once its tmux session is gone, which is the #5942
            // population.
            row_output_path: None,
            registry_output_path: None,
            pane_idle_confirmed: false,
            rowless_active_turn: false,
            placeholder_present: false,
            executor,
            now_epoch_ms: now_ms,
            process_started_at_epoch_ms: now_ms.saturating_sub(60_000),
        })
    };

    stamp(now - std::time::Duration::from_secs(LEDGER_OBSERVATION_TTL_SECS + 300));
    let expired = observe(ExecutorWitness::Absent);
    assert!(
        matches!(expired.in_band(), ReachabilityVerdict::Expired { .. }),
        "a witnessed-absent owner over a ledger whose FILE is {}s stale must expire; got {:?} — \
         the probe is not reading the ledger's commit time off disk",
        LEDGER_OBSERVATION_TTL_SECS + 300,
        expired.in_band()
    );
    assert!(
        expired.abstains_from_health_polarity(),
        "the composed verdict must withdraw from the polarity, not decide it"
    );

    for witness in [ExecutorWitness::Present, ExecutorWitness::Unwitnessed] {
        let verdict = observe(witness);
        assert!(
            !matches!(verdict.in_band(), ReachabilityVerdict::Expired { .. }),
            "{witness:?} must never expire the same backdated ledger; got {:?} — the probe's \
             executor witness is not reaching the gate",
            verdict.in_band()
        );
    }

    // The control that makes the expiry above an age rather than a constant:
    // the same file, the same call, stamped NOW.
    stamp(now);
    let fresh = observe(ExecutorWitness::Absent);
    assert!(
        !matches!(fresh.in_band(), ReachabilityVerdict::Expired { .. }),
        "a ledger stamped on this tick must not expire; got {:?}",
        fresh.in_band()
    );
}

/// #5942 r4 (P2-1): the FAULT arms really do run before the timer.
///
/// `classify_reachability` carries a comment claiming every fault arm precedes
/// the TTL gate, and r3 moved `read_truncated` above the gate to make the claim
/// true. Nothing asserted it: an adversarial review moved that arm back under
/// the gate and the suite stayed green, because `read_truncated` is hardcoded
/// `false` at the one production call site so no other test can reach the
/// ordering.
///
/// The claim is worth holding even so. A truncated read is a thing that went
/// WRONG, and the whole point of the ordering is that a clock must not retire
/// one — the moment a caller starts passing a real truncation flag, an arm
/// under the gate would answer `Expired` for a channel whose coverage is
/// unknown, which is exactly the "expire a real loss" failure conjunct (2)
/// exists to prevent.
#[test]
fn a_truncated_read_outranks_the_ttl_gate_even_when_every_expiry_conjunct_holds() {
    let expires = abandoned_ledger_case();
    assert!(
        matches!(expires.classify(), ReachabilityVerdict::Expired { .. }),
        "fixture must expire without the truncation, or the assertion below is vacuous"
    );

    let truncated = Case {
        read_truncated: true,
        ..abandoned_ledger_case()
    };
    let verdict = truncated.classify();
    assert_eq!(
        verdict.unknown_reason(),
        Some(ReachabilityUnknownReason::ReadTruncated),
        "a truncated read must outrank the TTL gate; got {verdict:?} — a fault was retired by a \
         clock"
    );
}
