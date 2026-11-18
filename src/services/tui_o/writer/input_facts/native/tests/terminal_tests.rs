//! Strict interrupt rows, and the barrier an earlier reading stored for one.
use super::*;
use crate::services::tui_input::human_frame::history::v2::native::Outcome;

const MARK: &str = "[Request interrupted by user]";

fn stop(content: Value) -> Value {
    json!({"type":"user","uuid":"stop","sessionId":"parent","message":{"content":content}})
}

fn owned(f: &Fixture, closer: Value) -> Ticket {
    let ticket = f.register("ours", "our request");
    f.append(&[user("own", "our request"), closer]);
    f.receipt(&ticket, "own", "our request");
    f.history.note_native_any_key("native-b1", &ticket).unwrap();
    ticket
}

/// The prefix analysed with its last `live` rows appended as captured records.
fn projected(f: &Fixture, live: usize) -> Analysis {
    let bytes = std::fs::read(&f.binding.source.path).unwrap();
    let ends = bytes.iter().enumerate().filter(|(_, b)| **b == b'\n');
    let ends: Vec<_> = std::iter::once(0).chain(ends.map(|(i, _)| i + 1)).collect();
    let rows = &ends[ends.len() - 1 - live..];
    let hash = hex::encode(sha2::Sha256::digest(&bytes[..rows[0]]));
    let (_, seed) = Projection::reopen(f.binding.clone(), rows[0] as u64, &hash).unwrap();
    let mut seed = seed.unwrap();
    for row in rows.windows(2) {
        let line = bytes[row[0]..row[1] - 1].to_vec();
        let (start, end) = (row[0] as u64, row[1] as u64);
        let record = crate::services::tui_o::shadow::CapturedRecord { start, end, line };
        seed.append(&record).unwrap();
    }
    let registrations = f.history.native_classification_registrations().unwrap();
    seed.analyze(&registrations).unwrap()
}

/// The barrier the earlier Prompt reading stored for the strict interrupt row, after `edit`.
fn stored(f: &Fixture, ticket: &Ticket, edit: fn(&mut Barrier)) -> (Barrier, String) {
    let bytes = std::fs::read(&f.binding.source.path).unwrap();
    let (own, end) = (
        bytes.iter().position(|b| *b == b'\n').unwrap() + 1,
        bytes.len(),
    );
    let registrations = f.history.native_classification_registrations().unwrap();
    let stamp = registrations_fingerprint(&registrations).unwrap();
    let mut barrier: Barrier = serde_json::from_value(json!({"namespace":"native-b1","ticket":ticket,"membership_range":[0,own],"membership_row":"own","foreign_range":[own,end],"foreign_row":"stop","foreign_hashes":frame_hashes(MARK),"through":end,"registrations_fingerprint":stamp})).unwrap();
    edit(&mut barrier);
    assert!(f.history.record_native_barrier(&barrier).unwrap());
    (barrier, stamp)
}

fn outcome(e: &Evidence) -> Outcome {
    serde_json::from_value(
        json!({"disposition":e.disposition,"range":[e.range.0,e.range.1],"native_proof":e}),
    )
    .unwrap()
}

#[test]
fn native_8148_terminal_strict_interrupt_cancels_without_a_foreign_barrier() {
    let cases = [
        (json!([{"type":"text","text":MARK}]), true),
        (json!(format!("it printed {MARK} again")), false),
        (json!([{"type":"text","text":MARK},{"type":"image"}]), false),
    ];
    for (content, strict) in cases {
        for live in [0, 1] {
            let f = Fixture::new();
            owned(&f, stop(content.clone()));
            let seen = projected(&f, live);
            let want = (usize::from(strict), usize::from(!strict));
            assert_eq!(
                (seen.evidence.len(), seen.barriers.len()),
                want,
                "{content} {live}"
            );
            let cancelled = |e: &Evidence| e.disposition == Disposition::Cancelled;
            assert!(seen.evidence.iter().all(cancelled));
        }
    }
}

#[test]
fn native_8148_terminal_stored_interrupt_barrier_is_superseded_only_exactly() {
    let strict = || stop(json!([{"type":"text","text":MARK}]));
    let others: [fn(&mut Barrier); 4] = [
        |b| b.foreign_hashes = frame_hashes("human direct input"),
        |b| b.foreign_row = "another".into(),
        |b| b.foreign_range[0] += 1,
        |b| b.membership_range[0] += 1,
    ];
    for other in others {
        let f = Fixture::new();
        let ticket = owned(&f, strict());
        stored(&f, &ticket, other);
        assert_eq!(projected(&f, 0), Analysis::default());
    }
    for control in [false, true] {
        let f = Fixture::new();
        let ticket = owned(&f, strict());
        let (barrier, stamp) = stored(&f, &ticket, |_| {});
        let evidence = projected(&f, 1).evidence.remove(0);
        assert_eq!(evidence.disposition, Disposition::Cancelled);
        let record = |evidence: &Evidence, stamp: &str| {
            let history = &f.history;
            history.record_native_outcome_at_stamp("native-b1", &ticket, &outcome(evidence), stamp)
        };
        let forgeries: [fn(&mut Evidence); 4] = [
            |e| e.supersedes = None,
            |e| e.supersedes.as_mut().unwrap().barrier_digest.push('0'),
            |e| e.disposition = Disposition::Executed,
            |e| e.closer_range = e.membership_range,
        ];
        for forge in forgeries {
            let mut forged = evidence.clone();
            forge(&mut forged);
            let refused = record(&forged, &stamp).unwrap_err();
            assert_eq!(refused, "native execution crosses foreign prompt barrier");
        }
        let stale = record(&evidence, "stale").unwrap_err();
        assert_eq!(stale, "native registrations superseded");
        let mut expected = outcome(&evidence);
        if control {
            expected =
                serde_json::from_value(json!({"disposition":"operator_released","range":null}))
                    .unwrap();
            f.history
                .record_native_outcome("native-b1", &ticket, &expected)
                .unwrap();
            let settled = record(&evidence, &stamp).unwrap_err();
            assert_eq!(settled, "native result already settled");
        } else {
            assert_eq!(record(&evidence, &stamp), Ok(true));
        }
        let saved = f.history.native_settlement("native-b1", &ticket).unwrap();
        assert_eq!(saved.outcome, Some(expected));
        assert_eq!(saved.barrier("native-b1").unwrap(), Some(barrier));
        assert_eq!(projected(&f, 0), Analysis::default());
    }
}

#[test]
fn native_8148_terminal_supersession_refuses_an_unknown_or_moved_barrier() {
    let f = Fixture::new();
    let ticket = owned(&f, stop(json!([{"type":"text","text":MARK}])));
    stored(&f, &ticket, |_| {});
    let proof = outcome(&projected(&f, 1).evidence.remove(0));
    let saved = f.history.native_settlement("native-b1", &ticket).unwrap();
    assert!(saved.known_barrier("native-b1").unwrap().is_some());
    assert_eq!(saved.admits_native("native-b1", &proof), Ok(()));
    let edits: [fn(&mut serde_json::Map<String, Value>); 3] = [
        |extra| extra["foreign_prompt_barrier"]["unknown"] = json!(1),
        |extra| extra["foreign_prompt_barrier"]["through"] = json!(1 << 20),
        |extra| drop(extra.remove("foreign_prompt_barrier")),
    ];
    for edit in edits {
        let mut moved = saved.clone();
        edit(&mut moved.extra);
        let refused = moved.admits_native("native-b1", &proof).unwrap_err();
        assert_eq!(refused, "native execution crosses foreign prompt barrier");
    }
}
