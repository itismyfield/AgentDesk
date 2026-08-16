//! Observation-task tests (#5071 T4-B2).
//!
//! These pin what a tick concludes and records, including the last one, which
//! pins what it may never conclude.
//!
//! The OTHER half of "this slice has no judgment authority" is not here, and
//! deliberately: "nothing outside the tree reads it" and "no destructive verb
//! is reachable from it" are claims about absence, which no example can
//! witness. Those are source gates, and this repo already has the right lexer
//! for them — `scripts/check_reachability_canonical_equivalence.py` carries the
//! consumer-set and no-bounds checks over the shared Rust neutralizer, and the
//! destructive half is already covered per file by
//! `scripts/check_destructive_call_site_ratchet.py`, whose four categories are
//! exactly 4987's destructive surfaces. Writing a second, worse Rust lexer here
//! would be the same two-oracle mistake this slice exists to close.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use tempfile::TempDir;

use super::*;
use crate::services::discord::health::reachability::ledger::read_ledger_at;

const NOW_MS: u64 = 1_786_000_000_000;

/// A tick's world: a transcript, an overridden runtime root, and the guard that
/// keeps the override from leaking into another test.
struct Harness {
    dir: TempDir,
    _root: crate::config::TestEnvVarGuard,
}

impl Harness {
    fn new() -> Self {
        let dir = TempDir::new().expect("tempdir");
        let root = crate::config::set_agentdesk_root_for_test(dir.path());
        Self { dir, _root: root }
    }

    fn transcript(&self) -> PathBuf {
        self.dir.path().join("transcript.jsonl")
    }

    fn write(&self, body: &[u8]) {
        let mut file = fs::File::create(self.transcript()).expect("create transcript");
        file.write_all(body).expect("write transcript");
    }

    fn append(&self, body: &[u8]) {
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(self.transcript())
            .expect("open transcript");
        file.write_all(body).expect("append transcript");
    }

    fn input(&self) -> ObservationInput {
        ObservationInput {
            channel_id: 4242,
            // A tmux session that does not exist, so `.generation` reads 0 and
            // `.spawn_nonce` reads `None`. Both are the honest values for an
            // absent marker (T3-R2 forbids widening a `None` into a match), and
            // neither is fabricated for the test's convenience.
            tmux_session_name: "adk-t4b2-observation-test".to_string(),
            registry_output_path: Some(self.transcript().to_string_lossy().into_owned()),
        }
    }

    fn ledger_path(&self) -> PathBuf {
        ledger_path(&ProviderKind::Claude, 4242).expect("ledger path")
    }

    fn observe(&self, state: &mut ObservationState, now_ms: u64) -> ObservationReport {
        observe_channel(&ProviderKind::Claude, &self.input(), state, now_ms)
    }
}

fn assistant_line(text: &str) -> String {
    format!(
        "{{\"type\":\"assistant\",\"timestamp\":\"2026-08-17T01:02:03\",\
         \"message\":{{\"model\":\"m\",\"content\":[{{\"type\":\"text\",\"text\":\"{text}\"}}]}}}}\n"
    )
}

/// The first tick never claims anything. It writes the bootstrap watermark and
/// says so: the tail before that offset was never read, so its absence from the
/// ledger is evidence of nothing.
#[test]
fn the_first_tick_bootstraps_at_end_of_file_and_concludes_nothing() {
    let harness = Harness::new();
    harness.write(assistant_line("delivered long ago").as_bytes());
    let mut state = ObservationState::default();

    let report = harness.observe(&mut state, NOW_MS);

    assert_eq!(
        report.outcome,
        ObservationOutcome::Withheld(VerdictWithheldReason::IncarnationBootstrapped)
    );
    assert_eq!(report.new_obligations, 0);
    let ledger = read_ledger_at(&harness.ledger_path()).expect("ledger written");
    let len = fs::metadata(harness.transcript()).expect("stat").len();
    assert_eq!(ledger.bootstrap_offset, len);
    assert_eq!(ledger.cursor_offset, len);
    assert!(
        ledger.live_obligations().is_empty(),
        "the pre-bootstrap tail is not claimed as observed"
    );
}

#[test]
fn a_second_tick_records_the_growth_as_obligations() {
    let harness = Harness::new();
    harness.write(b"");
    let mut state = ObservationState::default();
    harness.observe(&mut state, NOW_MS);

    harness.append(assistant_line("first").as_bytes());
    harness.append(assistant_line("second").as_bytes());
    let report = harness.observe(&mut state, NOW_MS + 30_000);

    assert_eq!(report.new_obligations, 2);
    let ledger = read_ledger_at(&harness.ledger_path()).expect("ledger");
    assert_eq!(ledger.live_obligations().len(), 2);
    assert_eq!(ledger.counters.total_obligations, 2);
    assert_eq!(ledger.counters.ticks, 1);
}

/// A live obligation in THIS slice means "not yet subtracted", not
/// "undelivered": the receipt index is T4-B3 and the bounds are the output of
/// this observation. So the tick must withhold, by name, rather than reach for
/// `Degraded` or `Unreachable`.
#[test]
fn live_obligations_withhold_a_verdict_instead_of_producing_unreachable() {
    let harness = Harness::new();
    harness.write(b"");
    let mut state = ObservationState::default();
    harness.observe(&mut state, NOW_MS);

    harness.append(assistant_line("unsubtracted").as_bytes());
    let report = harness.observe(&mut state, NOW_MS + 30_000);

    assert_eq!(
        report.outcome,
        ObservationOutcome::Withheld(VerdictWithheldReason::CoverageAndBoundsUnavailable)
    );
}

/// 4987 §-1.4: `Reachable` needs zero obligations AND positive
/// incarnation-alive evidence. Growth that produces no obligation is that
/// evidence; a file that did not move is not.
#[test]
fn reachable_requires_zero_obligations_and_a_file_that_actually_grew() {
    let harness = Harness::new();
    harness.write(b"");
    let mut state = ObservationState::default();
    harness.observe(&mut state, NOW_MS);

    harness.append(b"{\"type\":\"user\",\"timestamp\":\"2026-08-17T01:02:03\"}\n");
    let grew = harness.observe(&mut state, NOW_MS + 30_000);
    assert_eq!(
        grew.outcome,
        ObservationOutcome::Verdict(ReachabilityVerdict::Reachable)
    );

    let quiet = harness.observe(&mut state, NOW_MS + 60_000);
    assert_eq!(
        quiet.outcome,
        ObservationOutcome::Withheld(VerdictWithheldReason::NoIncarnationAliveEvidence),
        "\"nothing observed\" is never GREEN (4987 §-1.4)"
    );
}

/// A partial line is re-read whole rather than framed twice: the cursor stops
/// at its first byte, and when the record completes it becomes exactly one
/// obligation.
#[test]
fn a_record_split_across_two_ticks_produces_exactly_one_obligation() {
    let harness = Harness::new();
    harness.write(b"");
    let mut state = ObservationState::default();
    harness.observe(&mut state, NOW_MS);

    let line = assistant_line("split across ticks");
    let (head, tail) = line.split_at(40);
    harness.append(head.as_bytes());
    let first = harness.observe(&mut state, NOW_MS + 30_000);
    assert_eq!(first.new_obligations, 0);
    let ledger = read_ledger_at(&harness.ledger_path()).expect("ledger");
    assert_eq!(
        ledger.cursor_offset, 0,
        "the cursor must hold at the partial record's first byte"
    );

    harness.append(tail.as_bytes());
    let second = harness.observe(&mut state, NOW_MS + 60_000);
    assert_eq!(second.new_obligations, 1);
    let ledger = read_ledger_at(&harness.ledger_path()).expect("ledger");
    assert_eq!(ledger.live_obligations().len(), 1);
    assert_eq!(
        (
            ledger.live_obligations()[0].start,
            ledger.live_obligations()[0].end
        ),
        (0, line.len() as u64),
        "the obligation covers the whole record, not the half seen first"
    );
}

/// Rotation: same path, different file. The old byte offsets name nothing in
/// the new file, so the obligations are retired by their typed reason and the
/// ledger re-bootstraps — it never resumes at a meaningless offset.
#[test]
fn a_rotated_transcript_retires_the_old_incarnation_instead_of_resuming() {
    let harness = Harness::new();
    harness.write(b"");
    let mut state = ObservationState::default();
    harness.observe(&mut state, NOW_MS);
    harness.append(assistant_line("before rotation").as_bytes());
    harness.observe(&mut state, NOW_MS + 30_000);
    let before = read_ledger_at(&harness.ledger_path()).expect("ledger");
    assert_eq!(before.live_obligations().len(), 1);

    let replacement = harness.dir.path().join("replacement.jsonl");
    fs::write(&replacement, b"rotated\n").expect("write replacement");
    fs::rename(&replacement, harness.transcript()).expect("rotate");

    let report = harness.observe(&mut state, NOW_MS + 60_000);

    assert_eq!(
        report.outcome,
        ObservationOutcome::Withheld(VerdictWithheldReason::IncarnationBootstrapped)
    );
    let after = read_ledger_at(&harness.ledger_path()).expect("ledger");
    assert!(after.live_obligations().is_empty());
    assert_eq!(after.counters.retired_incarnation, 1);
    assert_ne!(
        after.incarnation.transcript_ino, before.incarnation.transcript_ino,
        "the fixture must actually change the inode"
    );
    assert_eq!(
        after.counters.retired_receipt_covered, 0,
        "a rotation is not evidence that anything was delivered"
    );
}

/// In-place truncation keeps the inode but breaks the coordinate. Same answer:
/// non-GREEN this tick, and a re-bootstrap so the next one means something.
#[test]
fn an_in_place_truncation_is_coordinate_divergence_not_a_quiet_relay() {
    let harness = Harness::new();
    harness.write(b"");
    let mut state = ObservationState::default();
    harness.observe(&mut state, NOW_MS);
    harness.append(assistant_line("before truncation").as_bytes());
    harness.observe(&mut state, NOW_MS + 30_000);

    fs::OpenOptions::new()
        .write(true)
        .open(harness.transcript())
        .expect("open")
        .set_len(3)
        .expect("truncate");

    let report = harness.observe(&mut state, NOW_MS + 60_000);

    let ObservationOutcome::Verdict(ReachabilityVerdict::Unknown { reason, .. }) = report.outcome
    else {
        panic!("expected an Unknown, got {:?}", report.outcome);
    };
    assert_eq!(
        reason,
        ReachabilityUnknownReason::TranscriptCoordinateDivergence
    );
    let after = read_ledger_at(&harness.ledger_path()).expect("ledger");
    assert!(after.live_obligations().is_empty());
    assert_eq!(after.counters.retired_incarnation, 1);
}

/// A registry entry with no resolvable transcript fails closed to `Unknown`,
/// which is NOT `Reachable`. 4987 §-1.4 turns the resolution failure itself
/// into a detection, which is what stops a wrong coordinate from becoming a
/// silent GREEN.
#[test]
fn an_unresolvable_transcript_is_unknown_and_writes_no_ledger() {
    let harness = Harness::new();
    let mut state = ObservationState::default();
    let mut input = harness.input();
    input.registry_output_path = Some(
        harness
            .dir
            .path()
            .join("does-not-exist.jsonl")
            .to_string_lossy()
            .into_owned(),
    );

    let report = observe_channel(&ProviderKind::Claude, &input, &mut state, NOW_MS);

    assert_eq!(
        report.outcome,
        ObservationOutcome::Verdict(ReachabilityVerdict::unknown(
            ReachabilityUnknownReason::TranscriptUnresolved,
            0
        ))
    );
    assert!(!harness.ledger_path().is_file());
}

/// 4987 §-1.4 counterexample 7: a store that will not parse is
/// `Unknown{ReceiptStoreUnreadable}`, never an empty obligation set that would
/// read as healthy.
#[test]
fn a_corrupt_ledger_is_unknown_rather_than_an_empty_obligation_set() {
    let harness = Harness::new();
    harness.write(assistant_line("present").as_bytes());
    let path = harness.ledger_path();
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(&path, "{ not a ledger").expect("write garbage");
    let mut state = ObservationState::default();

    let report = harness.observe(&mut state, NOW_MS);

    assert_eq!(
        report.outcome,
        ObservationOutcome::Verdict(ReachabilityVerdict::unknown(
            ReachabilityUnknownReason::ReceiptStoreUnreadable,
            0
        ))
    );
}

/// `since_secs` runs from the first tick that saw the reason and restarts when
/// the reason changes, so a long-standing `Unknown` is distinguishable from a
/// fresh one. It is process-local by design; nothing consumes it.
#[test]
fn unknown_since_secs_accumulates_and_resets_when_the_reason_changes() {
    let mut state = ObservationState::default();
    let first = state.since_secs(7, ReachabilityUnknownReason::TranscriptUnresolved, NOW_MS);
    let later = state.since_secs(
        7,
        ReachabilityUnknownReason::TranscriptUnresolved,
        NOW_MS + 90_000,
    );
    assert_eq!((first, later), (0, 90));

    let changed = state.since_secs(7, ReachabilityUnknownReason::ReadTruncated, NOW_MS + 90_000);
    assert_eq!(changed, 0, "a different reason starts its own clock");

    let other_channel =
        state.since_secs(8, ReachabilityUnknownReason::ReadTruncated, NOW_MS + 90_000);
    assert_eq!(other_channel, 0, "clocks are per channel");
}

/// The standing statement of what this slice may never conclude.
///
/// Every scenario above runs again here through one shared assertion, because
/// the claim is about the OUTCOME SET and not about any one path: no tick may
/// produce `Degraded`, `Unreachable` or `TransportUnknown`. The first two need
/// bounds that 4987 §10 forbids at S1 and a subtrahend that arrives with T4-B3;
/// the third needs evidence sources this task never reads. A future edit that
/// reached for any of them — the exact way an observation slice turns into a
/// judgment one — fails here regardless of which branch it took.
#[test]
fn no_tick_can_produce_a_bounded_or_transport_verdict() {
    let harness = Harness::new();
    let mut state = ObservationState::default();
    let mut outcomes = Vec::new();

    // Bootstrap, growth with obligations, growth without, a quiet tick.
    harness.write(b"");
    outcomes.push(harness.observe(&mut state, NOW_MS).outcome);
    harness.append(assistant_line("obligated").as_bytes());
    outcomes.push(harness.observe(&mut state, NOW_MS + 30_000).outcome);
    harness.append(b"{\"type\":\"user\",\"timestamp\":\"2026-08-17T01:02:03\"}\n");
    outcomes.push(harness.observe(&mut state, NOW_MS + 60_000).outcome);
    outcomes.push(harness.observe(&mut state, NOW_MS + 90_000).outcome);

    // Coordinate divergence, then an unresolvable transcript.
    fs::OpenOptions::new()
        .write(true)
        .open(harness.transcript())
        .expect("open")
        .set_len(1)
        .expect("truncate");
    outcomes.push(harness.observe(&mut state, NOW_MS + 120_000).outcome);
    fs::remove_file(harness.transcript()).expect("remove transcript");
    outcomes.push(harness.observe(&mut state, NOW_MS + 150_000).outcome);

    assert_eq!(
        outcomes.len(),
        6,
        "every scenario above must be represented"
    );
    for outcome in outcomes {
        let ObservationOutcome::Verdict(verdict) = &outcome else {
            continue;
        };
        assert!(
            matches!(
                verdict,
                ReachabilityVerdict::Reachable | ReachabilityVerdict::Unknown { .. }
            ),
            "a tick produced {verdict:?}. T4-B2 observes: Degraded/Unreachable need \
             bounds (4987 §10 NO-GO at S1) and the receipt index (T4-B3), and \
             TransportUnknown needs evidence sources this task never reads"
        );
    }
}
