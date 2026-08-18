//! Acceptance battery for #5071 T4-B6 (4987 S3).
//!
//! The seven named cases below are 4987 §-1.4's「오탐 반례 — 인수 테스트에 필수
//! 포함」table, one test each. Six of them are false positives: the table
//! forbids `Unreachable` for each, so every one of those asserts the exact
//! expected variant AND that it is not `Unreachable`. Counterexample 2 is the
//! table's single true positive and asserts `Unreachable` instead.

use tempfile::tempdir;

use super::*;
use crate::services::discord::health::liveness_authority::CaptureCoordinateObservation;
use crate::services::discord::health::reachability::discovery::TranscriptFileId;
use crate::services::discord::health::reachability::ledger::{
    LedgerIncarnation, LedgerObligation, ReachabilityLedger,
};
use crate::services::discord::health::session_enrichment::SessionEnrichment;
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
        let provider = provider();
        classify_reachability(ReachabilityInputs {
            provider: &provider,
            divergence: self.divergence,
            ledger: self.ledger.as_ref(),
            ledger_present: self.ledger_present,
            receipts: &self.receipts,
            transcript: self.transcript,
            read_truncated: self.read_truncated,
            rowless_active_turn: self.rowless_active_turn,
            placeholder_present: self.placeholder_present,
            now_epoch_ms: NOW_MS,
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
    ]
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
