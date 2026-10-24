use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::TimeZone;

use super::observe::{FileSeen, Observation, Present};
use super::*;

const CHANNEL: u64 = 7;
const PANE: &str = "AgentDesk-claude-e2e";
/// Old sources `0..OLD`; source `OLD` is current.
const OLD: u64 = 59;
const LEGACY: [u64; 3] = [30, 31, 32];
const MISSING: u64 = 50;
const GC_ONLY_SUCCESSOR: u64 = 41;
const OPEN_FD: [u64; 2] = [57, 58];

fn at(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

/// Both probes and the witness age are judged against these, well past every mtime.
const FIRST_PROBE: u64 = 2_000_000;

fn source(n: u64) -> SourceId {
    SourceId {
        session_id: format!("session-{n}"),
        path: format!("/o/session-{n}.jsonl").into(),
        dev: 1,
        ino: 1000 + n,
    }
}

fn key(n: u64) -> String {
    source_key(&source(n))
}

fn len(n: u64) -> u64 {
    100 + n
}

fn cursor(n: u64) -> Cursor {
    Cursor {
        source: source(n),
        captured_through: len(n),
        prefix_hash: format!("{n:064x}"),
        retired: false,
        transition_cleanup: None,
    }
}

fn fingerprint(n: u64) -> Fingerprint {
    Fingerprint {
        len: len(n),
        dev: 1,
        ino: 1000 + n,
        modified: at(1_000_000 + n),
        change: (1_000_000 + n as i64, 0),
    }
}

fn present(n: u64) -> FileSeen {
    FileSeen::Present(Present {
        fingerprint: fingerprint(n),
        same_file: true,
        prefix_matches: true,
        whole_tail: true,
        steady: true,
    })
}

fn seen(cursor: &Cursor, secs: u64, file: FileSeen) -> Observation {
    Observation {
        source: cursor.source.clone(),
        through: cursor.captured_through,
        prefix_hash: cursor.prefix_hash.clone(),
        at: at(secs),
        file,
    }
}

fn witness(cursor: &Cursor, n: u64) -> CleanupWitness {
    CleanupWitness {
        source: cursor.source.clone(),
        captured_through: cursor.captured_through,
        prefix_hash: cursor.prefix_hash.clone(),
        fingerprint: fingerprint(n),
        observed_at: at(FIRST_PROBE - 3600),
    }
}

fn hop_seq(n: u64) -> u64 {
    n + 2
}

/// Fan-in where a resume skipped a source; every other hop moves to the next source.
fn successor_of(n: u64) -> u64 {
    match n {
        19 | 44 => n + 2,
        _ => n + 1,
    }
}

fn ended() -> ExecutionEnd {
    ExecutionEnd {
        host: "mac-book".into(),
        pane: PANE.into(),
        first_seq: 1,
        last_seq: hop_seq(OLD),
        basis:
            "tmux server for the pane was killed before the herdr switch; no claude process left"
                .into(),
    }
}

struct Fixture {
    snapshot: Snapshot,
    request: Request,
}

impl Fixture {
    /// 59 unproven old hops of one pane, each drained, quiet and covered by the termination.
    fn synthetic() -> Self {
        let mut rotation = Rotation::default();
        let mut sources = BTreeMap::new();
        let mut legacy = BTreeMap::new();
        for n in 0..OLD {
            let legacy_hop = LEGACY.contains(&n);
            let next = Successor {
                source: source(successor_of(n)),
                seq: (!legacy_hop).then_some(hop_seq(n)),
                tmux_session: (!legacy_hop).then(|| PANE.to_string()),
                drain_to: (n % 2 == 0 && !legacy_hop).then_some(len(n)),
                proof: None,
            };
            rotation.successors.insert(key(n), next);
            if legacy_hop {
                legacy.insert(key(n), vec![(hop_seq(n), PANE.to_string())]);
            }
        }
        for n in 0..=OLD {
            let cursor = cursor(n);
            let file = || {
                if n == MISSING {
                    FileSeen::Missing
                } else {
                    present(n)
                }
            };
            let open = OPEN_FD.contains(&n);
            let reader = ReaderView {
                capture: open.then_some((len(n), len(n))),
                pending_batch: false,
                drain_to: (n == 58).then_some(len(n)),
                captured_any: n != GC_ONLY_SUCCESSOR,
            };
            let facts = SourceFacts {
                reader: Some(reader),
                gc_logged: n == GC_ONLY_SUCCESSOR,
                witness: Some(witness(&cursor, n)),
                earlier: Some(seen(&cursor, FIRST_PROBE, file())),
                later: Some(seen(&cursor, FIRST_PROBE + 11, file())),
            };
            sources.insert(key(n), facts);
        }
        let snapshot = Snapshot {
            reading: 0,
            codex: None,
            channel: CHANNEL,
            provider: ShadowProvider::Claude,
            home: Home::Settled,
            rotation,
            cursors: (0..=OLD).map(cursor).collect(),
            pinned: Ok(Some(source(OLD))),
            execution: Ok(vec![source(OLD)]),
            sources,
            legacy_hops: Ok(legacy),
            obligations: Ok(Obligations::default()),
        };
        let request = Request {
            channel: CHANNEL,
            cutoff: hop_seq(OLD),
            job_id: "hrot-e2e-1".into(),
            operator: "operator".into(),
            at: chrono::Utc.timestamp_opt(2_000_100, 0).unwrap(),
            reason: "herdr transition of the e2e channel".into(),
            termination: Some(Termination::Confirmed(ended())),
        };
        Self { snapshot, request }
    }

    fn facts(&mut self, n: u64) -> &mut SourceFacts {
        self.snapshot.sources.get_mut(&key(n)).unwrap()
    }

    fn reader(&mut self, n: u64) -> &mut ReaderView {
        self.facts(n).reader.as_mut().unwrap()
    }

    fn later(&mut self, n: u64) -> &mut Observation {
        self.facts(n).later.as_mut().unwrap()
    }

    fn later_present(&mut self, n: u64) -> &mut Present {
        present_mut(self.later(n))
    }

    fn cursor(&mut self, n: u64) -> &mut Cursor {
        (self.snapshot.cursors.iter_mut())
            .find(|cursor| cursor.source == source(n))
            .unwrap()
    }

    fn judge(&self) -> Judgment {
        judge(&self.snapshot, &self.request)
    }
}

fn present_mut(seen: &mut Observation) -> &mut Present {
    match &mut seen.file {
        FileSeen::Present(present) => present,
        other => panic!("probed as {other:?}"),
    }
}

fn verdict(judgment: &Judgment, n: u64) -> &Verdict {
    let hop = judgment.hops.iter().find(|hop| hop.key == key(n));
    &hop.unwrap().verdict
}

/// The hop `n` gets `expected` while every other hop stays eligible.
fn only(fixture: Fixture, n: u64, expected: Verdict) {
    let judgment = fixture.judge();
    assert_eq!(judgment.hops.len(), OLD as usize);
    for hop in &judgment.hops {
        if hop.key == key(n) {
            assert_eq!(hop.verdict, expected, "hop {n}");
        } else {
            assert!(matches!(hop.verdict, Verdict::Eligible(_)), "{hop:?}");
        }
    }
}

/// Every hop gets `expected`.
fn all(fixture: Fixture, expected: Verdict) {
    let judgment = fixture.judge();
    assert_eq!(judgment.hops.len(), OLD as usize);
    for hop in &judgment.hops {
        assert_eq!(hop.verdict, expected, "{}", hop.key);
    }
    assert_eq!(judgment.summary.eligible, 0);
}

fn refused(refusal: Refusal) -> Verdict {
    Verdict::Refused(refusal)
}

fn unknown(reason: Unclear) -> Verdict {
    Verdict::Unknown(reason)
}

#[test]
fn transition_cleanup_synthetic_59_hops_are_eligible_with_drafts_bound_to_each_cursor() {
    let fixture = Fixture::synthetic();
    let judgment = fixture.judge();
    assert_eq!(judgment.hops.len(), 59);
    for n in 0..OLD {
        let cursor = cursor(n);
        let hop = judgment.hops.iter().find(|hop| hop.key == key(n)).unwrap();
        assert_eq!(hop.hop_seq, Some(hop_seq(n)));
        let Verdict::Eligible(draft) = &hop.verdict else {
            panic!("hop {n}: {:?}", hop.verdict);
        };
        let expected = TransitionCleanup {
            job_id: "hrot-e2e-1".into(),
            operator: "operator".into(),
            at: fixture.request.at,
            reason: "herdr transition of the e2e channel".into(),
            cutoff: hop_seq(OLD),
            hop_seq: hop_seq(n),
            verified: VerifiedEnd {
                offset: cursor.captured_through,
                prefix_hash: cursor.prefix_hash.clone(),
                fingerprint: fingerprint(n),
            },
            ended: ended(),
        };
        assert_eq!(**draft, expected, "hop {n}");
    }
    let summary = Summary {
        unsettled: 59,
        eligible: 59,
        already_done: 0,
        refused: 0,
        unknown: 0,
        not_target: 0,
        obligations: Some(Obligations::default()),
        proof_recovery: None,
    };
    assert_eq!(judgment.summary, summary);
}

#[test]
fn transition_cleanup_without_a_checked_termination_no_hop_is_eligible() {
    let mut fixture = Fixture::synthetic();
    fixture.request.termination = None;
    all(fixture, unknown(Unclear::NoTermination));
    let mut fixture = Fixture::synthetic();
    let claim = "herdr is configured, so tmux writers are gone".to_string();
    fixture.request.termination = Some(Termination::Implied(claim));
    all(fixture, unknown(Unclear::ImpliedTermination));
    let mut fixture = Fixture::synthetic();
    fixture.request.termination = Some(Termination::Unread("process list".into()));
    all(
        fixture,
        unknown(Unclear::TerminationUnread("process list".into())),
    );
    let mut fixture = Fixture::synthetic();
    let blank = ExecutionEnd {
        basis: " ".into(),
        ..ended()
    };
    fixture.request.termination = Some(Termination::Confirmed(blank));
    all(fixture, unknown(Unclear::TerminationIncomplete));
    let mut fixture = Fixture::synthetic();
    let other = ExecutionEnd {
        pane: "AgentDesk-claude-other".into(),
        ..ended()
    };
    fixture.request.termination = Some(Termination::Confirmed(other));
    all(fixture, unknown(Unclear::TerminationOtherPane));
    let mut fixture = Fixture::synthetic();
    let short = ExecutionEnd {
        last_seq: hop_seq(58) - 1,
        ..ended()
    };
    fixture.request.termination = Some(Termination::Confirmed(short));
    only(fixture, 58, unknown(Unclear::TerminationOutOfRange));
}

#[test]
fn transition_cleanup_spares_current_later_and_proven_hops() {
    let mut fixture = Fixture::synthetic();
    fixture.snapshot.pinned = Ok(Some(source(5)));
    only(fixture, 5, refused(Refusal::CurrentSource));
    let mut fixture = Fixture::synthetic();
    fixture.snapshot.execution = Ok(vec![source(OLD), source(7)]);
    only(fixture, 7, refused(Refusal::CurrentSource));
    let mut fixture = Fixture::synthetic();
    fixture.snapshot.pinned = Ok(None);
    all(fixture, unknown(Unclear::NoPinnedSource));
    let mut fixture = Fixture::synthetic();
    fixture.snapshot.execution = Err("binding projection".into());
    all(
        fixture,
        unknown(Unclear::ExecutionUnread("binding projection".into())),
    );
    let mut fixture = Fixture::synthetic();
    fixture.request.cutoff = hop_seq(58) - 1;
    only(fixture, 58, refused(Refusal::AfterCutoff));
    let mut fixture = Fixture::synthetic();
    let next = fixture.snapshot.rotation.successors.get_mut(&key(3));
    next.unwrap().proof = Some(70);
    only(fixture, 3, refused(Refusal::Proven));
}

#[test]
fn transition_cleanup_reader_obligations_hold_only_while_the_cursor_is_short_of_them() {
    let mut fixture = Fixture::synthetic();
    fixture.reader(11).pending_batch = true;
    only(fixture, 11, refused(Refusal::PendingBatch));
    let mut fixture = Fixture::synthetic();
    let next = fixture.snapshot.rotation.successors.get_mut(&key(12));
    next.unwrap().drain_to = Some(len(12) + 5);
    only(fixture, 12, refused(Refusal::DrainOwed));
    let mut fixture = Fixture::synthetic();
    fixture.reader(13).drain_to = Some(len(13) + 1);
    only(fixture, 13, refused(Refusal::DrainOwed));
    // A drain target the cursor has reached binds nothing, stored or held by the reader.
    let mut fixture = Fixture::synthetic();
    for n in [12, 13] {
        let next = fixture.snapshot.rotation.successors.get_mut(&key(n));
        next.unwrap().drain_to = Some(len(n));
        fixture.reader(n).drain_to = Some(len(n) - 1);
    }
    let judgment = fixture.judge();
    assert_eq!(judgment.summary.eligible, 59);
    let mut fixture = Fixture::synthetic();
    fixture.reader(57).capture = Some((len(57), len(57) + 3));
    only(fixture, 57, refused(Refusal::CaptureAhead));
    let mut fixture = Fixture::synthetic();
    fixture.snapshot.sources.remove(&key(9));
    let judgment = fixture.judge();
    assert_eq!(verdict(&judgment, 9), &unknown(Unclear::NoReaderState));
    // Source 9 is also hop 8's successor, which then has no capture shown.
    assert_eq!(
        verdict(&judgment, 8),
        &refused(Refusal::SuccessorUncaptured)
    );
    assert_eq!(judgment.summary.eligible, 57);
}

#[test]
fn transition_cleanup_needs_a_captured_successor_which_the_gc_ledger_may_show() {
    let mut fixture = Fixture::synthetic();
    fixture.facts(GC_ONLY_SUCCESSOR).gc_logged = false;
    only(fixture, 40, refused(Refusal::SuccessorUncaptured));
    let mut fixture = Fixture::synthetic();
    fixture.reader(21).captured_any = false;
    let judgment = fixture.judge();
    // Hops 19 and 20 both moved to source 21.
    for n in [19, 20] {
        assert_eq!(
            verdict(&judgment, n),
            &refused(Refusal::SuccessorUncaptured)
        );
    }
    assert_eq!(judgment.summary.eligible, 57);
}

#[test]
fn transition_cleanup_present_file_must_stay_at_its_verified_end() {
    // Grown before the first probe and quiet since: only the end check sees it.
    let mut fixture = Fixture::synthetic();
    fixture.later_present(8).fingerprint.len += 10;
    let earlier = fixture.facts(8).earlier.as_mut().unwrap();
    present_mut(earlier).fingerprint.len += 10;
    only(fixture, 8, refused(Refusal::Growing));
    let mut fixture = Fixture::synthetic();
    fixture.later_present(8).fingerprint.len += 10;
    only(fixture, 8, refused(Refusal::Growing));
    let mut fixture = Fixture::synthetic();
    fixture.later_present(9).fingerprint.modified = at(1_500_000);
    only(fixture, 9, refused(Refusal::NotQuiet));
    let mut fixture = Fixture::synthetic();
    fixture.later(10).at = at(FIRST_PROBE + 9);
    only(fixture, 10, refused(Refusal::NotQuiet));
    let mut fixture = Fixture::synthetic();
    fixture.later_present(14).whole_tail = false;
    only(fixture, 14, refused(Refusal::TornTail));
    let mut fixture = Fixture::synthetic();
    fixture.later_present(15).prefix_matches = false;
    only(fixture, 15, refused(Refusal::PrefixMismatch));
    let mut fixture = Fixture::synthetic();
    fixture.later_present(16).same_file = false;
    only(fixture, 16, refused(Refusal::Replaced));
    let mut fixture = Fixture::synthetic();
    fixture.later(17).file = FileSeen::Replaced;
    only(fixture, 17, refused(Refusal::Replaced));
    let mut fixture = Fixture::synthetic();
    fixture.later_present(18).fingerprint.len -= 1;
    only(fixture, 18, refused(Refusal::Shrunk));
    let mut fixture = Fixture::synthetic();
    fixture.later_present(22).steady = false;
    only(fixture, 22, refused(Refusal::NotQuiet));
    let mut fixture = Fixture::synthetic();
    fixture.later(23).file = FileSeen::Missing;
    only(fixture, 23, refused(Refusal::NotQuiet));
    let mut fixture = Fixture::synthetic();
    fixture.later(24).file = FileSeen::Unreadable("EACCES".into());
    only(
        fixture,
        24,
        unknown(Unclear::FileUnreadable("EACCES".into())),
    );
    let mut fixture = Fixture::synthetic();
    fixture.later(25).through -= 1;
    only(fixture, 25, unknown(Unclear::StaleObservation));
    let mut fixture = Fixture::synthetic();
    fixture.facts(26).earlier = None;
    only(fixture, 26, unknown(Unclear::NotObserved));
}

#[test]
fn transition_cleanup_missing_file_needs_a_fresh_witness_bound_to_the_cursor() {
    let mut fixture = Fixture::synthetic();
    fixture.facts(MISSING).witness = None;
    only(fixture, MISSING, refused(Refusal::NoWitness));
    let mut fixture = Fixture::synthetic();
    let moved = fixture.facts(MISSING).witness.as_mut().unwrap();
    moved.captured_through -= 1;
    moved.fingerprint.len -= 1;
    only(fixture, MISSING, refused(Refusal::WitnessUnbound));
    let mut fixture = Fixture::synthetic();
    let rehashed = fixture.facts(MISSING).witness.as_mut().unwrap();
    rehashed.prefix_hash = "f".repeat(64);
    only(fixture, MISSING, refused(Refusal::WitnessUnbound));
    let mut fixture = Fixture::synthetic();
    let fresh = fixture.facts(MISSING).witness.as_mut().unwrap();
    fresh.fingerprint.modified = at(FIRST_PROBE - 60);
    only(fixture, MISSING, refused(Refusal::WitnessNotEligible));
    let mut fixture = Fixture::synthetic();
    fixture.reader(MISSING).capture = Some((len(MISSING), len(MISSING)));
    only(fixture, MISSING, unknown(Unclear::MissingWithOpenFd));
}

#[test]
fn transition_cleanup_owed_output_holds_every_hop() {
    let owed = [
        (
            Obligations {
                owed: 1,
                ..Default::default()
            },
            Owed::Queue,
        ),
        (
            Obligations {
                prepared: 1,
                ..Default::default()
            },
            Owed::Prepared,
        ),
        (
            Obligations {
                unsealed: 1,
                ..Default::default()
            },
            Owed::Unsealed,
        ),
        (
            Obligations {
                posting: 1,
                ..Default::default()
            },
            Owed::Posting,
        ),
        (
            Obligations {
                verified_work: 2,
                ..Default::default()
            },
            Owed::VerifiedWork,
        ),
    ];
    for (obligations, what) in owed {
        let mut fixture = Fixture::synthetic();
        fixture.snapshot.obligations = Ok(obligations.clone());
        let judgment = fixture.judge();
        assert_eq!(judgment.summary.obligations, Some(obligations));
        all(fixture, refused(Refusal::OutputOwed(what)));
    }
    let mut fixture = Fixture::synthetic();
    let protected = Obligations {
        protected: BTreeSet::from([key(20)]),
        ..Default::default()
    };
    fixture.snapshot.obligations = Ok(protected);
    only(
        fixture,
        20,
        refused(Refusal::OutputOwed(Owed::VerifiedWork)),
    );
    let mut fixture = Fixture::synthetic();
    fixture.snapshot.obligations = Err("actor gone".into());
    all(
        fixture,
        unknown(Unclear::ObligationsUnread("actor gone".into())),
    );
}

#[test]
fn transition_cleanup_first_version_refuses_codex_moving_homes_and_bad_requests() {
    let mut fixture = Fixture::synthetic();
    fixture.snapshot.provider = ShadowProvider::Codex;
    all(fixture, refused(Refusal::CodexDeferred));
    let mut fixture = Fixture::synthetic();
    fixture.snapshot.home = Home::Moving;
    all(fixture, refused(Refusal::HomeMoving));
    let mut fixture = Fixture::synthetic();
    fixture.snapshot.home = Home::Unread("permit".into());
    all(fixture, unknown(Unclear::HomeUnread("permit".into())));
    let mut fixture = Fixture::synthetic();
    fixture.request.channel = CHANNEL + 1;
    all(fixture, refused(Refusal::OtherChannel));
    let mut fixture = Fixture::synthetic();
    fixture.request.operator = String::new();
    all(fixture, refused(Refusal::IncompleteRequest));
    let mut fixture = Fixture::synthetic();
    fixture
        .snapshot
        .cursors
        .retain(|cursor| cursor.source != source(4));
    only(fixture, 4, unknown(Unclear::CursorMissing));
}

#[test]
fn transition_cleanup_seqless_hop_takes_its_seq_only_from_one_validated_history_match() {
    // Without a match the hop is unknown even where any seq and its stored pane would pass.
    let mut fixture = Fixture::synthetic();
    let history = fixture.snapshot.legacy_hops.as_mut().unwrap();
    history.remove(&key(31));
    let next = fixture.snapshot.rotation.successors.get_mut(&key(31));
    next.unwrap().tmux_session = Some(PANE.into());
    let from_zero = ExecutionEnd {
        first_seq: 0,
        ..ended()
    };
    fixture.request.termination = Some(Termination::Confirmed(from_zero));
    only(fixture, 31, unknown(Unclear::NoHistory));
    let pair = (hop_seq(32), PANE.to_string());
    let other_pane = (hop_seq(32), "AgentDesk-claude-other".to_string());
    for second in [pair.clone(), (hop_seq(32) + 1, PANE.into()), other_pane] {
        let mut fixture = Fixture::synthetic();
        let history = fixture.snapshot.legacy_hops.as_mut().unwrap();
        history.insert(key(32), vec![pair.clone(), second]);
        only(fixture, 32, unknown(Unclear::AmbiguousHistory));
    }
    let mut fixture = Fixture::synthetic();
    fixture.snapshot.legacy_hops = Err("binding log gap".into());
    let judgment = fixture.judge();
    for n in LEGACY {
        let gap = unknown(Unclear::HistoryUnread("binding log gap".into()));
        assert_eq!(verdict(&judgment, n), &gap);
    }
    assert_eq!(judgment.summary.eligible, 56);
}

#[test]
fn transition_cleanup_mapped_seq_and_pane_meet_cutoff_range_and_pane_each() {
    let mapped = |seq: u64, pane: &str| {
        let mut fixture = Fixture::synthetic();
        let history = fixture.snapshot.legacy_hops.as_mut().unwrap();
        history.insert(key(30), vec![(seq, pane.to_string())]);
        fixture
    };
    only(
        mapped(hop_seq(OLD) + 1, PANE),
        30,
        refused(Refusal::AfterCutoff),
    );
    // Seq 0 is within the cutoff but before the termination range, which starts at 1.
    only(mapped(0, PANE), 30, unknown(Unclear::TerminationOutOfRange));
    let other = mapped(hop_seq(30), "AgentDesk-claude-other");
    only(other, 30, unknown(Unclear::TerminationOtherPane));
}

#[test]
fn transition_cleanup_past_audit_settles_only_its_own_job_on_an_unchanged_snapshot() {
    let mut fixture = Fixture::synthetic();
    fixture.cursor(6).retired = true;
    let judgment = fixture.judge();
    assert_eq!(verdict(&judgment, 6), &Verdict::NotTarget);
    assert_eq!(
        (judgment.summary.unsettled, judgment.summary.not_target),
        (58, 1)
    );
    let base = Fixture::synthetic().judge();
    let Verdict::Eligible(draft) = verdict(&base, 6).clone() else {
        panic!("hop 6 is eligible in the base fixture");
    };
    let done = |fixture: &mut Fixture| {
        let cursor = fixture.cursor(6);
        cursor.retired = true;
        cursor.transition_cleanup = Some(*draft.clone());
    };
    let mut fixture = Fixture::synthetic();
    done(&mut fixture);
    only(fixture, 6, Verdict::AlreadyDone);
    // The same job after the file was touched is judged again and records the new end.
    let mut fixture = Fixture::synthetic();
    done(&mut fixture);
    fixture.later_present(6).fingerprint.modified = at(1_900_000);
    let earlier = fixture.facts(6).earlier.as_mut().unwrap();
    present_mut(earlier).fingerprint.modified = at(1_900_000);
    let judgment = fixture.judge();
    let Verdict::Eligible(again) = verdict(&judgment, 6) else {
        panic!("{:?}", verdict(&judgment, 6));
    };
    assert_eq!(again.verified.fingerprint.modified, at(1_900_000));
    // Another job on the same snapshot is judged again as well.
    let mut fixture = Fixture::synthetic();
    done(&mut fixture);
    fixture.request.job_id = "hrot-e2e-2".into();
    let judgment = fixture.judge();
    let Verdict::Eligible(again) = verdict(&judgment, 6) else {
        panic!("{:?}", verdict(&judgment, 6));
    };
    assert_eq!(again.job_id, "hrot-e2e-2");
    // A retired hop that cannot be judged again stays refused, whatever its record says.
    let mut fixture = Fixture::synthetic();
    done(&mut fixture);
    fixture.later_present(6).fingerprint.len += 1;
    only(fixture, 6, refused(Refusal::Growing));
}

#[test]
fn transition_cleanup_missing_file_retry_compares_the_retired_cursor_with_its_witness() {
    let base = Fixture::synthetic().judge();
    let Verdict::Eligible(draft) = verdict(&base, MISSING).clone() else {
        panic!("the missing hop is eligible in the base fixture");
    };
    let done = |fixture: &mut Fixture| {
        let cursor = fixture.cursor(MISSING);
        cursor.retired = true;
        cursor.transition_cleanup = Some(*draft.clone());
    };
    let mut fixture = Fixture::synthetic();
    done(&mut fixture);
    only(fixture, MISSING, Verdict::AlreadyDone);
    // Another job is judged on the current conditions, not waved through by the record.
    let mut fixture = Fixture::synthetic();
    done(&mut fixture);
    fixture.request.job_id = "hrot-e2e-2".into();
    let judgment = fixture.judge();
    let Verdict::Eligible(again) = verdict(&judgment, MISSING) else {
        panic!("{:?}", verdict(&judgment, MISSING));
    };
    assert_eq!(
        (again.job_id.as_str(), &again.verified),
        ("hrot-e2e-2", &draft.verified)
    );
    let mut fixture = Fixture::synthetic();
    done(&mut fixture);
    fixture.request.job_id = "hrot-e2e-2".into();
    fixture.snapshot.obligations = Ok(Obligations {
        owed: 1,
        ..Default::default()
    });
    let judgment = fixture.judge();
    let held = refused(Refusal::OutputOwed(Owed::Queue));
    assert_eq!(verdict(&judgment, MISSING), &held);
    // A witness of another offset or hash neither settles the job nor passes again.
    for moved_offset in [true, false] {
        let mut fixture = Fixture::synthetic();
        done(&mut fixture);
        let witness = fixture.facts(MISSING).witness.as_mut().unwrap();
        if moved_offset {
            witness.captured_through -= 1;
        } else {
            witness.prefix_hash = "f".repeat(64);
        }
        only(fixture, MISSING, refused(Refusal::WitnessUnbound));
    }
}

#[cfg(unix)]
mod operations_shape {
    //! The measured e2e channel: 59 unproven hops of one pane, all files at their end, no reader FD,
    //! a witness each, and the oldest an init source without a cursor file.

    use sha2::{Digest, Sha256};

    use super::*;
    use crate::services::tui_o::shadow::binding_reader::source_id_for;
    use crate::services::tui_o::shadow::capture::SourceCapture;
    use crate::services::tui_o::shadow::{CaptureOutcome, CaptureSource};
    use crate::services::tui_o::store::rotation::{Boundary, SourceLink};
    use crate::services::tui_o::store::{
        CURSOR_DIR, ChannelStore, InitSource, Initialized, OStore, STORE_DIR_NAME, StoreConfig,
    };

    fn hop_seqs() -> Vec<u64> {
        let (unknown, resume) = ([2, 4], [20, 46]);
        let startup = (5..=18).chain(21..=45).chain(47..=62);
        let mut seqs: Vec<u64> = unknown.into_iter().chain(resume).chain(startup).collect();
        seqs.sort_unstable();
        seqs
    }

    #[test]
    fn transition_cleanup_operations_shaped_store_judges_all_59_hops_eligible() {
        let seqs = hop_seqs();
        assert_eq!(seqs.len(), 59);
        let runtime = tempfile::tempdir().unwrap();
        let sources: Vec<SourceId> = (0..=OLD)
            .map(|n| {
                let path = runtime.path().join(format!("t{n}.jsonl"));
                let body: String = (0..=n % 5)
                    .map(|line| format!("{{\"n\":{line}}}\n"))
                    .collect();
                std::fs::write(&path, body).unwrap();
                source_id_for(&format!("session-{n}"), &path).unwrap()
            })
            .collect();
        let init_len = std::fs::metadata(&sources[0].path).unwrap().len();
        let init = InitSource {
            source_id: sources[0].clone(),
            delivery_start: init_len,
            prefix_hash: hex::encode(Sha256::digest(std::fs::read(&sources[0].path).unwrap())),
        };
        let config = StoreConfig { enabled: true };
        let store = OStore::open_if_enabled(&config, runtime.path())
            .unwrap()
            .unwrap();
        let now = chrono::Utc::now();
        let initialized = Initialized {
            channel: CHANNEL,
            sources: vec![init],
            initial_anchor: 0,
            build_digest: "test".into(),
            at: now,
        };
        let era = store
            .begin_era(&[CHANNEL], now, |_| Ok(initialized.clone()))
            .unwrap();
        let mut channel = store.open_channel(&era, CHANNEL).unwrap().unwrap();
        for source in &sources[1..] {
            channel.attach_source(source).unwrap();
            let mut capture = SourceCapture::open(source.clone(), 0).unwrap();
            let CaptureOutcome::Batch(batch) = capture.poll(1 << 20) else {
                panic!("capture failed");
            };
            channel
                .append_spool(&batch, &capture.prefix_hash())
                .unwrap();
        }
        let mut rotation = Rotation::default();
        for (n, seq) in seqs.iter().enumerate() {
            let next = Successor {
                source: sources[n + 1].clone(),
                seq: Some(*seq),
                tmux_session: Some(PANE.into()),
                drain_to: Some(std::fs::metadata(&sources[n].path).unwrap().len()),
                proof: None,
            };
            rotation.successors.insert(source_key(&sources[n]), next);
        }
        // 58 links: the source bound at seq 4 was only restored from history.
        for (n, seq) in seqs.iter().enumerate().filter(|(_, seq)| **seq != 4) {
            let link = SourceLink {
                source: sources[n + 1].clone(),
                seq: *seq,
                parent: None,
                committed_at: now,
                boundary: Boundary::Owed { from: 0 },
                hot_candidate: None,
            };
            rotation.links.insert(source_key(&sources[n + 1]), link);
        }
        assert_eq!((rotation.successors.len(), rotation.links.len()), (59, 58));
        channel.write_rotation(&rotation).unwrap();
        channel.prepare_cleanup_witnesses().unwrap();
        let wall = SystemTime::now();
        for source in &sources[..OLD as usize] {
            let cursor = channel.cursor(source).unwrap().clone();
            let witness = CleanupWitness {
                source: source.clone(),
                captured_through: cursor.captured_through,
                prefix_hash: cursor.prefix_hash.clone(),
                fingerprint: Fingerprint::path(source).unwrap(),
                observed_at: wall,
            };
            channel.write_cleanup_witness(&witness).unwrap();
        }
        let cursor_files = runtime
            .path()
            .join(STORE_DIR_NAME)
            .join(CHANNEL.to_string())
            .join(CURSOR_DIR);
        let file_of = |source: &SourceId| cursor_files.join(format!("{}.json", source_key(source)));
        assert!(!file_of(&sources[0]).exists());
        assert!(sources[1..].iter().all(|source| file_of(source).exists()));
        let cursors: Vec<Cursor> = channel.cursors().cloned().collect();
        assert_eq!(cursors.len(), 60);
        let mut facts = BTreeMap::new();
        for cursor in &cursors {
            let source = &cursor.source;
            let reader = ReaderView {
                capture: None,
                pending_batch: false,
                drain_to: None,
                captured_any: *source != sources[0],
            };
            let source_facts = SourceFacts {
                reader: Some(reader),
                gc_logged: !channel.ledger().gc_segments(source).is_empty(),
                witness: channel.cleanup_witness(source).unwrap(),
                earlier: Some(observe::observe(cursor, wall)),
                later: Some(observe::observe(cursor, wall + RETIRE_QUIET)),
            };
            facts.insert(source_key(source), source_facts);
        }
        let current = sources[OLD as usize].clone();
        let snapshot = Snapshot {
            reading: 0,
            codex: None,
            channel: CHANNEL,
            provider: ShadowProvider::Claude,
            home: Home::Settled,
            rotation: channel.rotation().unwrap(),
            cursors,
            pinned: Ok(Some(current.clone())),
            execution: Ok(vec![current]),
            sources: facts,
            legacy_hops: Ok(BTreeMap::new()),
            obligations: Ok(Obligations::default()),
        };
        let mut request = Fixture::synthetic().request;
        request.cutoff = 62;
        request.termination = Some(Termination::Confirmed(ExecutionEnd {
            last_seq: 62,
            ..ended()
        }));
        let judgment = judge(&snapshot, &request);
        assert_eq!(judgment.summary.unsettled, 59);
        assert_eq!(judgment.summary.eligible, 59, "{:?}", judgment.hops);
        for (n, seq) in seqs.iter().enumerate() {
            let source = &sources[n];
            let hop = (judgment.hops.iter())
                .find(|hop| hop.key == source_key(source))
                .unwrap();
            assert_eq!(hop.hop_seq, Some(*seq));
            let Verdict::Eligible(draft) = &hop.verdict else {
                panic!("seq {seq}: {:?}", hop.verdict);
            };
            let cursor = channel.cursor(source).unwrap();
            let verified = VerifiedEnd {
                offset: cursor.captured_through,
                prefix_hash: cursor.prefix_hash.clone(),
                fingerprint: Fingerprint::path(source).unwrap(),
            };
            assert_eq!(draft.verified, verified, "seq {seq}");
            assert_eq!(
                verified.offset,
                std::fs::metadata(&source.path).unwrap().len()
            );
        }
        request.termination = None;
        let judgment = judge(&snapshot, &request);
        assert_eq!(
            (judgment.summary.eligible, judgment.summary.unknown),
            (0, 59)
        );
    }

    #[test]
    fn transition_cleanup_retired_missing_source_retry_reads_its_witness_back_from_the_store() {
        let runtime = tempfile::tempdir().unwrap();
        let sources: Vec<SourceId> = (0..2)
            .map(|n| {
                let path = runtime.path().join(format!("t{n}.jsonl"));
                std::fs::write(&path, b"{\"n\":0}\n").unwrap();
                source_id_for(&format!("session-{n}"), &path).unwrap()
            })
            .collect();
        let config = StoreConfig { enabled: true };
        let store = OStore::open_if_enabled(&config, runtime.path())
            .unwrap()
            .unwrap();
        let now = chrono::Utc::now();
        let initialized = Initialized {
            channel: CHANNEL,
            sources: Vec::new(),
            initial_anchor: 0,
            build_digest: "test".into(),
            at: now,
        };
        let era = store
            .begin_era(&[CHANNEL], now, |_| Ok(initialized.clone()))
            .unwrap();
        let mut channel = store.open_channel(&era, CHANNEL).unwrap().unwrap();
        for source in &sources {
            channel.attach_source(source).unwrap();
            let mut capture = SourceCapture::open(source.clone(), 0).unwrap();
            let CaptureOutcome::Batch(batch) = capture.poll(1 << 20) else {
                panic!("capture failed");
            };
            channel
                .append_spool(&batch, &capture.prefix_hash())
                .unwrap();
        }
        let (old, current) = (&sources[0], &sources[1]);
        let mut rotation = Rotation::default();
        let next = Successor {
            source: current.clone(),
            seq: Some(2),
            tmux_session: Some(PANE.into()),
            drain_to: None,
            proof: None,
        };
        rotation.successors.insert(source_key(old), next);
        channel.write_rotation(&rotation).unwrap();
        channel.prepare_cleanup_witnesses().unwrap();
        // Probe stamps are the caller's: the file's mtime is a day old by them, the witness fresh.
        let wall = SystemTime::now();
        let probed = wall + Duration::from_secs(25 * 3600);
        let cursor = channel.cursor(old).unwrap().clone();
        let witness = CleanupWitness {
            source: old.clone(),
            captured_through: cursor.captured_through,
            prefix_hash: cursor.prefix_hash.clone(),
            fingerprint: Fingerprint::path(old).unwrap(),
            observed_at: wall + Duration::from_secs(3 * 3600),
        };
        channel.write_cleanup_witness(&witness).unwrap();
        std::fs::remove_file(&old.path).unwrap();
        let snapshot_of = |channel: &ChannelStore, witness: Option<CleanupWitness>| {
            let cursors: Vec<Cursor> = channel.cursors().cloned().collect();
            let mut facts = BTreeMap::new();
            for cursor in &cursors {
                let is_old = cursor.source == *old;
                let reader = ReaderView {
                    capture: None,
                    pending_batch: false,
                    drain_to: None,
                    captured_any: true,
                };
                let source_facts = SourceFacts {
                    reader: Some(reader),
                    gc_logged: false,
                    witness: if is_old { witness.clone() } else { None },
                    earlier: Some(observe::observe(cursor, probed)),
                    later: Some(observe::observe(cursor, probed + RETIRE_QUIET)),
                };
                facts.insert(source_key(&cursor.source), source_facts);
            }
            Snapshot {
                reading: 0,
                codex: None,
                channel: CHANNEL,
                provider: ShadowProvider::Claude,
                home: Home::Settled,
                rotation: channel.rotation().unwrap(),
                cursors,
                pinned: Ok(Some(current.clone())),
                execution: Ok(vec![current.clone()]),
                sources: facts,
                legacy_hops: Ok(BTreeMap::new()),
                obligations: Ok(Obligations::default()),
            }
        };
        let request = Fixture::synthetic().request;
        let read = channel.cleanup_witness(old).unwrap();
        let judgment = judge(&snapshot_of(&channel, read), &request);
        let Verdict::Eligible(draft) = judgment.hops[0].verdict.clone() else {
            panic!("{:?}", judgment.hops);
        };
        assert_eq!(draft.verified.fingerprint, witness.fingerprint);
        // Stand in for the retirement: the audited, retired cursor as the store would hold it.
        channel.set_retired(old, true).unwrap();
        let retired = Cursor {
            transition_cleanup: Some(*draft),
            ..channel.cursor(old).unwrap().clone()
        };
        let cursor_file = (runtime.path().join(STORE_DIR_NAME))
            .join(CHANNEL.to_string())
            .join(CURSOR_DIR)
            .join(format!("{}.json", source_key(old)));
        std::fs::write(cursor_file, serde_json::to_vec(&retired).unwrap()).unwrap();
        drop(channel);
        let reopened = store.open_channel(&era, CHANNEL).unwrap().unwrap();
        assert_eq!(reopened.cursor(old), Some(&retired));
        assert_eq!(reopened.cleanup_witness(old).unwrap(), None);
        let recorded = reopened.recorded_witness(old).unwrap();
        assert_eq!(recorded.as_ref(), Some(&witness));
        let snapshot = snapshot_of(&reopened, recorded);
        let judgment = judge(&snapshot, &request);
        assert_eq!(judgment.hops[0].verdict, Verdict::AlreadyDone);
        let other = Request {
            job_id: "hrot-e2e-2".into(),
            ..request
        };
        let judgment = judge(&snapshot, &other);
        let Verdict::Eligible(again) = &judgment.hops[0].verdict else {
            panic!("{:?}", judgment.hops);
        };
        assert_eq!(
            (again.job_id.as_str(), again.verified.fingerprint.clone()),
            ("hrot-e2e-2", witness.fingerprint)
        );
    }
}

#[path = "transition_scope_tests.rs"]
mod scope_tests;
