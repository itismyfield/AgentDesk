//! G1b P2-3/F3/F4/P2-4: a returning row proves its receipt identity before any effect, and a
//! collected tombstone or a conflicting snapshot never reactivates or reads as a receipt.
use std::fs;
use std::io;
use std::path::Path;

use serde_json::{Value, json};

use super::super::attempt::{
    AttemptMeta, Effect, Retire, TOMBSTONE_HORIZON_MS, Tracking, fresh_token,
};
use super::super::ledger::{Ledger, Record};
use super::super::rows::receipt_identity::{ReceiptIdentity, Responsibility};
use super::super::rows::{
    AbandonReason, AttemptEvidence, DoneReason, Entry, Owner, RowState, Rows,
};
use crate::services::tui_o::shadow::{ShadowProvider, SourceBinding, SourceId};

const CHANNEL: u64 = 9;
const DAY_MS: u64 = 24 * 60 * 60 * 1000;
const BACK: RowState = RowState::Abandoned(AbandonReason::Handback);

fn sandbox() -> tempfile::TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/i01-tmp");
    fs::create_dir_all(&root).unwrap();
    tempfile::Builder::new()
        .prefix("g1b-rows-")
        .tempdir_in(root)
        .unwrap()
}

fn identity(key: u64, sources: &[u64]) -> ReceiptIdentity {
    ReceiptIdentity::new(key, sources.to_vec(), 7, 8, CHANNEL).unwrap()
}

pub(in crate::services::tui_input) fn input(key: u64, sources: &[u64]) -> Value {
    json!({"text":format!("input {key}"),"receipt_identity":identity(key, sources)})
}

fn append(ledger: &mut Ledger, entry: Entry) -> io::Result<u64> {
    ledger.append_entry(&entry, &[])
}

fn set(ledger: &mut Ledger, key: u64, state: RowState) {
    let entry = Entry::Transition {
        key,
        state,
        attempt: None,
    };
    append(ledger, entry).unwrap();
}

fn received(key: u64, input: Value) -> Entry {
    Entry::Received { key, input }
}

fn refused(result: io::Result<impl std::fmt::Debug>) {
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
}

fn record(seq: u64, entry: &Entry) -> Record {
    let tagged = serde_json::to_value(entry).unwrap();
    Record {
        seq,
        prev_crc: 0,
        crc: 0,
        kind: tagged["kind"].as_str().unwrap().into(),
        payload: tagged["payload"].clone(),
    }
}

pub(in crate::services::tui_input) fn tracked(ledger: &mut Ledger, key: u64) {
    let source = SourceId {
        session_id: "session".into(),
        path: "/nonexistent/session.jsonl".into(),
        dev: 1,
        ino: 1,
    };
    let meta = AttemptMeta {
        generation: 1,
        token: fresh_token(),
        frame_digest: "ab".repeat(32),
        frame_profile: None,
        execution_nonce: "n1".into(),
        source: source.clone(),
        anchor: 0,
        effect: Effect::Intent,
        incarnation: None,
        queue_end: None,
    };
    let evidence = AttemptEvidence {
        binding: SourceBinding {
            channel_id: CHANNEL,
            provider: ShadowProvider::Claude,
            source,
        },
        execution_nonce: "n1".into(),
        eof: 0,
        rendered_prompt: format!("frame {key}"),
        source_ids: vec![key],
        record_end: None,
        native_turn_id: None,
    };
    let tracking = Tracking {
        attempt: Some(meta),
        ..Tracking::default()
    };
    (ledger.append_tracked(key, RowState::Injecting, Some(evidence), &tracking)).unwrap();
}

struct Facts(u64);

impl Retire for Facts {
    fn now_ms(&self) -> Option<u64> {
        Some(self.0)
    }
    fn retired(&self, _: &AttemptMeta) -> bool {
        true
    }
    fn obligated(&self, _: u64) -> bool {
        false
    }
}

// Two checkpoints past the horizon collect a settled row's attempt detail.
pub(in crate::services::tui_input) fn collect(ledger: &mut Ledger) {
    let now = 1_000 * DAY_MS;
    ledger.checkpoint_rows_with(&Facts(now)).unwrap();
    let later = Facts(now + TOMBSTONE_HORIZON_MS);
    ledger.checkpoint_rows_with(&later).unwrap();
    ledger.checkpoint_rows_with(&later).unwrap();
}

#[test]
fn g1b_received_reactivation_preflight() {
    let root = sandbox();
    let mut ledger = Ledger::open(root.path(), CHANNEL).unwrap();
    append(&mut ledger, received(10, input(10, &[10, 11]))).unwrap();
    set(&mut ledger, 10, BACK);
    let before = ledger.rows().unwrap();
    for stray in [
        input(10, &[10, 12]),
        json!({"text":"no identity"}),
        json!({"receipt_identity":{"source_ids":[10,11],"author_id":7}}),
    ] {
        refused(append(&mut ledger, received(10, stray.clone())));
        assert_eq!(ledger.rows().unwrap(), before, "{stray}");
        assert_eq!(ledger.records().len(), 2, "WAL+0 for {stray}");
    }
    let alias = identity(11, &[11]);
    assert_eq!(
        before.responsibility(&alias),
        Responsibility::Legacy {
            key: 10,
            received_seq: 1
        }
    );
    // The same provenance with reordered sources and enrichment returns as a new generation.
    let same = json!({"text":"again","reply_context":"r","receipt_identity":{"source_ids":[11,10],"author_id":7,"original_channel_id":8,"execution_channel_id":CHANNEL}});
    let seq = append(&mut ledger, received(10, same)).unwrap();
    let rows = ledger.rows().unwrap();
    assert_eq!(rows.row(10).unwrap().state, RowState::Received);
    assert_eq!(rows.receipt_identity(10), Some(identity(10, &[10, 11])));
    assert_eq!(
        rows.responsibility(&alias),
        Responsibility::Known {
            key: 10,
            received_seq: seq
        }
    );
    // A retained index without its row never lends its identity to a new row.
    let mut state = serde_json::to_value(Rows::default()).unwrap();
    state["receipts"] = json!({"20":identity(20, &[20, 21])});
    let orphan: Rows = serde_json::from_value(state).unwrap();
    refused(orphan.activation(20, &input(20, &[20, 21])));
    assert_eq!(
        orphan.activation(30, &json!({"text":"fresh"})).unwrap(),
        true
    );
}

#[test]
fn g1b_replay_reactivation_failure_is_atomic() {
    let first = [
        received(10, input(10, &[10, 11])),
        Entry::Transition {
            key: 10,
            state: BACK,
            attempt: None,
        },
    ];
    let staged = |key, input| Entry::Staged {
        key,
        input,
        state: RowState::Received,
    };
    for tail in [
        vec![received(10, input(10, &[10, 12]))],
        vec![received(10, json!({"text":"no identity"}))],
        vec![
            staged(20, input(20, &[20])),
            staged(10, input(10, &[10, 12])),
            Entry::MoveCommitted {
                first_staged_seq: 3,
                ids: vec![20, 10],
            },
        ],
    ] {
        let entries: Vec<_> = first.iter().chain(&tail).collect();
        let records: Vec<_> = (entries.iter().enumerate())
            .map(|(index, entry)| record(index as u64 + 1, entry))
            .collect();
        let (last, prefix) = records.split_last().unwrap();
        let mut rows = Rows::fold(None, prefix).unwrap();
        let before = rows.clone();
        assert!(rows.apply(last).is_err(), "{:?}", last.kind);
        assert_eq!(rows, before, "a refused record changes nothing");
        assert!(Rows::fold(None, &records).is_err());
    }
}

#[test]
fn g1b_snapshot_conflict_holds_the_ledger() {
    let root = sandbox();
    let mut ledger = Ledger::open(root.path(), CHANNEL).unwrap();
    append(&mut ledger, received(10, input(10, &[10, 11]))).unwrap();
    set(&mut ledger, 10, RowState::Done(DoneReason::Completed));
    let mut state = serde_json::to_value(ledger.rows().unwrap()).unwrap();
    state["receipts"] = json!({"10":identity(10, &[10, 12])});
    ledger.checkpoint(state).unwrap();
    drop(ledger);
    let mut ledger = Ledger::open(root.path(), CHANNEL).expect("open stays usable");
    refused(ledger.rows());
    refused(ledger.checkpoint_rows());
    refused(append(&mut ledger, received(30, input(30, &[30]))));
}

#[test]
fn g1b_compact_rejects_directly_deserialized_conflict() {
    let row = |sources: Value| json!({"folded_seq":2,"rows":{"10":{"since_seq":1,"received_seq":1,"state":{"state":"done","reason":"completed"},"input":{"receipt_identity":{"source_ids":sources,"author_id":7,"original_channel_id":8,"execution_channel_id":CHANNEL}}}},"receipts":{"10":identity(10, &[10, 11])},"staged":[],"boundary_seq":0,"unbound":[],"ignored":0});
    let conflict: Rows = serde_json::from_value(row(json!([10, 12]))).unwrap();
    refused(conflict.compact());
    let reordered: Rows = serde_json::from_value(row(json!([11, 10]))).unwrap();
    let compacted = reordered.compact().unwrap();
    assert_eq!(compacted["rows"]["10"]["input"], Value::Null);
    assert_eq!(compacted["receipts"]["10"]["source_ids"], json!([10, 11]));
}

#[test]
fn g1b_tracked_handback_after_real_gc_never_reactivates() {
    let root = sandbox();
    let mut ledger = Ledger::open(root.path(), CHANNEL).unwrap();
    append(&mut ledger, received(1, input(1, &[1]))).unwrap();
    tracked(&mut ledger, 1);
    set(&mut ledger, 1, RowState::Ready);
    set(&mut ledger, 1, BACK);
    collect(&mut ledger);
    let kept = ledger.rows().unwrap().row(1).unwrap().clone();
    assert_eq!(kept.attempts.len(), 0, "attempt GC actually occurred");
    assert!(kept.retention.is_some());
    let first = append(&mut ledger, received(1, input(1, &[1]))).unwrap();
    let staged = Entry::Staged {
        key: 1,
        input: input(1, &[1]),
        state: RowState::Received,
    };
    let staged_seq = append(&mut ledger, staged).unwrap();
    let moved = Entry::MoveCommitted {
        first_staged_seq: staged_seq,
        ids: vec![1],
    };
    append(&mut ledger, moved).unwrap();
    let rows = Ledger::open(root.path(), CHANNEL).unwrap().rows().unwrap();
    assert!(first > kept.since_seq);
    assert_eq!(
        rows.row(1).unwrap(),
        &kept,
        "Received and Move both leave the tombstone"
    );
    assert_eq!(rows.owner(1), Owner::Legacy);
    assert_eq!(rows.ignored(), 2);
}

#[test]
fn g1b_nested_identity_survives_received_move_and_gc() {
    let root = sandbox();
    let mut ledger = Ledger::open(root.path(), CHANNEL).unwrap();
    let nested = |key: u64, alias: u64| json!({"legacy_input":{"text":"nested","receipt_identity":identity(key, &[key, alias])}});
    append(&mut ledger, received(1, nested(1, 11))).unwrap();
    tracked(&mut ledger, 1);
    set(&mut ledger, 1, RowState::Done(DoneReason::Completed));
    let staged = Entry::Staged {
        key: 2,
        input: nested(2, 12),
        state: RowState::Received,
    };
    let first_staged_seq = append(&mut ledger, staged).unwrap();
    let moved = Entry::MoveCommitted {
        first_staged_seq,
        ids: vec![2],
    };
    let move_seq = append(&mut ledger, moved).unwrap();
    set(&mut ledger, 2, RowState::Done(DoneReason::Completed));
    collect(&mut ledger);
    drop(ledger);
    let rows = Ledger::open(root.path(), CHANNEL).unwrap().rows().unwrap();
    assert_eq!(
        rows.row(1).unwrap().attempts.len(),
        0,
        "attempt GC actually occurred"
    );
    for (key, alias, received_seq) in [(1, 11, 1), (2, 12, first_staged_seq)] {
        assert_eq!(rows.row(key).unwrap().input, Value::Null);
        assert_eq!(
            rows.receipt_identity(key),
            Some(identity(key, &[key, alias]))
        );
        assert_eq!(
            rows.responsibility(&identity(alias, &[alias])),
            Responsibility::Known { key, received_seq }
        );
    }
    assert_eq!(rows.row(2).unwrap().since_seq, move_seq);
}
