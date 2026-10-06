#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::fs::{self, OpenOptions};
use std::io;
use std::path::Path;

use serde_json::json;

use super::attempt::{
    AttemptMeta, Disposition, Effect, QueueEnd, Retire, TOMBSTONE_HORIZON_MS, Tracking, Witness,
    WitnessKind, fresh_token,
};
use super::durability_tests::supported::Recording;
use super::handover::Reconciliation;
use super::ledger::Ledger;
use super::rows::{AbandonReason, AttemptEvidence, DoneReason, Entry, Owner, Row, RowState};
use crate::services::tui_o::shadow::{ShadowProvider, SourceBinding, SourceId, SourceRange};

const CHANNEL: u64 = 9;
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

fn sandbox() -> tempfile::TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/i01-tmp");
    fs::create_dir_all(&root).unwrap();
    tempfile::Builder::new()
        .prefix("attempt-")
        .tempdir_in(root)
        .unwrap()
}

fn open(root: &Path) -> Ledger {
    Ledger::open(root, CHANNEL).unwrap()
}

fn source() -> SourceId {
    SourceId {
        session_id: "session".into(),
        path: "/nonexistent/session.jsonl".into(),
        dev: 1,
        ino: 1,
    }
}

fn meta(generation: u32, nonce: &str, anchor: u64) -> AttemptMeta {
    AttemptMeta {
        generation,
        token: fresh_token(),
        frame_digest: "ab".repeat(32),
        frame_profile: None,
        execution_nonce: nonce.into(),
        source: source(),
        anchor,
        effect: Effect::Intent,
        incarnation: None,
        queue_end: None,
    }
}

fn received(ledger: &mut Ledger, key: u64) {
    let input = json!({ "text": format!("input {key}") });
    ledger
        .append_entry(&Entry::Received { key, input }, &[])
        .unwrap();
}

fn attempt(ledger: &mut Ledger, key: u64, meta: &AttemptMeta) -> io::Result<u64> {
    let evidence = AttemptEvidence {
        binding: SourceBinding {
            channel_id: CHANNEL,
            provider: ShadowProvider::Claude,
            source: meta.source.clone(),
        },
        execution_nonce: meta.execution_nonce.clone(),
        eof: meta.anchor,
        rendered_prompt: format!("frame {key}"),
        source_ids: vec![key],
        record_end: None,
        native_turn_id: None,
    };
    let tracking = Tracking {
        attempt: Some(meta.clone()),
        ..Tracking::default()
    };
    ledger.append_tracked(key, RowState::Injecting, Some(evidence), &tracking)
}

fn set(ledger: &mut Ledger, key: u64, state: RowState) {
    let entry = Entry::Transition {
        key,
        state,
        attempt: None,
    };
    ledger.append_entry(&entry, &[]).unwrap();
}

fn witness(meta: &AttemptMeta, kind: WitnessKind, start: u64) -> Witness {
    Witness {
        generation: meta.generation,
        token: meta.token.clone(),
        kind,
        range: Some(SourceRange {
            source: source(),
            start,
            end: start + 10,
        }),
        record_key: Some(format!("record-{start}")),
        turn_ref: None,
    }
}

fn row(ledger: &Ledger, key: u64) -> Row {
    ledger.rows().unwrap().row(key).unwrap().clone()
}

fn refused(result: io::Result<impl std::fmt::Debug>) {
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
}

// Q and H accept into the provider queue only; U and A confirm, and a re-read appends nothing.
#[test]
fn queue_witnesses_queue_a_row_and_only_model_records_run_it() {
    let root = sandbox();
    let mut ledger = open(root.path());
    received(&mut ledger, 1);
    received(&mut ledger, 2);
    let (one, two) = (meta(1, "n1", 0), meta(1, "n1", 0));
    attempt(&mut ledger, 1, &one).unwrap();
    set(&mut ledger, 1, RowState::AwaitTurn);
    attempt(&mut ledger, 2, &two).unwrap();
    set(&mut ledger, 2, RowState::AwaitTurn);
    assert_eq!(row(&ledger, 1).attempts[0].effect, Effect::Sent);

    for (kind, at) in [
        (WitnessKind::Hook, 10),
        (WitnessKind::Queued, 20),
        (WitnessKind::Removed, 30),
    ] {
        ledger.append_witness(1, witness(&one, kind, at)).unwrap();
        assert_eq!(row(&ledger, 1).state, RowState::Queued, "{kind:?}");
    }
    let records = ledger.records().len();
    let again = ledger.append_witness(1, witness(&one, WitnessKind::Queued, 20));
    assert_eq!(again.unwrap(), None, "a re-read record appends nothing");
    assert_eq!(ledger.records().len(), records);

    ledger
        .append_witness(1, witness(&one, WitnessKind::Attachment, 40))
        .unwrap();
    ledger
        .append_witness(1, witness(&one, WitnessKind::Hook, 50))
        .unwrap();
    assert_eq!(row(&ledger, 1).state, RowState::Running, "Q never undoes A");
    ledger
        .append_witness(2, witness(&two, WitnessKind::User, 40))
        .unwrap();
    assert_eq!(row(&ledger, 2).state, RowState::Running);
    assert!(!row(&ledger, 2).duplicate_delivery());
    ledger
        .append_witness(2, witness(&two, WitnessKind::User, 60))
        .unwrap();
    assert!(
        row(&ledger, 2).duplicate_delivery(),
        "two records, two deliveries"
    );

    let replayed = Tracking {
        witness: Some(witness(&two, WitnessKind::User, 60)),
        ..Tracking::default()
    };
    ledger
        .append_tracked(2, RowState::Running, None, &replayed)
        .unwrap();
    assert_eq!(
        row(&ledger, 2).witnesses.len(),
        2,
        "a replayed record is one witness"
    );

    set(&mut ledger, 1, RowState::Done(DoneReason::Completed));
    let live = ledger.rows().unwrap();
    let sent = AttemptMeta {
        effect: Effect::Sent,
        ..one.clone()
    };
    assert_eq!(
        live.row(1).unwrap().attempts,
        vec![sent],
        "a bare transition erases nothing"
    );
    assert_eq!(live.row(1).unwrap().witnesses.len(), 5);
    drop(ledger);
    assert_eq!(open(root.path()).rows().unwrap(), live);
}

// A cut input keeps its token so a later delivery is reported, never revived or re-run.
#[test]
fn a_settled_row_records_a_late_delivery_without_reviving() {
    let root = sandbox();
    let mut ledger = open(root.path());
    received(&mut ledger, 1);
    let first = meta(1, "n1", 0);
    attempt(&mut ledger, 1, &first).unwrap();
    set(&mut ledger, 1, RowState::AwaitTurn);
    ledger
        .append_witness(1, witness(&first, WitnessKind::Queued, 10))
        .unwrap();
    set(
        &mut ledger,
        1,
        RowState::Abandoned(AbandonReason::UserClear),
    );
    assert!(!row(&ledger, 1).delivered_after_clear());

    ledger
        .append_witness(1, witness(&first, WitnessKind::User, 20))
        .unwrap()
        .expect("a late delivery is recorded");
    refused(ledger.append_tracked(
        1,
        RowState::Running,
        None,
        &Tracking {
            witness: Some(witness(&first, WitnessKind::User, 30)),
            ..Tracking::default()
        },
    ));
    let cancelled = Tracking {
        disposition: Some(Disposition::Cancelled),
        ..Tracking::default()
    };
    let cut = RowState::Abandoned(AbandonReason::UserClear);
    ledger.append_tracked(1, cut, None, &cancelled).unwrap();
    let late = row(&ledger, 1);
    assert_eq!(late.state, cut);
    assert!(late.delivered_after_clear());
    assert!(late.witnesses[1].late && !late.witnesses[0].late);
    assert_eq!(late.dispositions, vec![Disposition::Cancelled]);

    ledger.checkpoint_rows().unwrap();
    received(&mut ledger, 1);
    drop(ledger);
    let rows = open(root.path()).rows().unwrap();
    let settled = rows.row(1).unwrap();
    assert_eq!(
        rows.owner(1),
        Owner::Settled,
        "the same key never runs again"
    );
    assert_eq!(settled.input, json!(null));
    assert_eq!(
        (settled.attempts.len(), settled.witnesses.len()),
        (1, 2),
        "the tombstone outlives the input"
    );
    assert!(settled.delivered_after_clear());
}

// Every refusal leaves the WAL untouched, and a forged record that breaks a rule fails the fold.
#[test]
fn the_ledger_refuses_reused_unknown_or_unproven_attempts_before_writing() {
    let root = sandbox();
    let mut ledger = open(root.path());
    received(&mut ledger, 1);
    received(&mut ledger, 2);
    let first = meta(1, "n1", 0);
    attempt(&mut ledger, 1, &first).unwrap();
    set(&mut ledger, 1, RowState::AwaitTurn);
    let records = ledger.records().len();

    refused(attempt(&mut ledger, 2, &first));
    refused(attempt(&mut ledger, 2, &meta(2, "n1", 0)));
    refused(ledger.append_witness(2, witness(&first, WitnessKind::User, 0)));
    let forged_queue = Tracking {
        witness: Some(witness(&first, WitnessKind::Queued, 0)),
        ..Tracking::default()
    };
    refused(ledger.append_tracked(1, RowState::Running, None, &forged_queue));
    assert_eq!(ledger.records().len(), records);

    let end = |old: &AttemptMeta, new: &str| QueueEnd {
        prior_generation: old.generation,
        old_nonce: old.execution_nonce.clone(),
        new_nonce: new.into(),
        old_source: source(),
        old_end: 90,
        stable_reads: 2,
        settle_profile: "claude-2.1.289".into(),
    };
    let mut second = meta(2, "n2", 100);
    second.queue_end = Some(end(&first, "n2"));
    ledger
        .append_witness(1, witness(&first, WitnessKind::Hook, 10))
        .unwrap();
    refused(attempt(&mut ledger, 1, &second));
    ledger
        .append_witness(1, witness(&first, WitnessKind::Queued, 20))
        .unwrap();
    let mut same_process = meta(2, "n1", 100);
    same_process.queue_end = Some(end(&first, "n1"));
    refused(attempt(&mut ledger, 1, &same_process));
    attempt(&mut ledger, 1, &second).expect("a verified queue end offers once");
    let mut replay = meta(3, "n3", 200);
    replay.queue_end = Some(end(&first, "n3"));
    refused(attempt(&mut ledger, 1, &replay));

    let retried = meta(1, "n1", 0);
    attempt(&mut ledger, 2, &retried).unwrap();
    set(&mut ledger, 2, RowState::Ready);
    assert_eq!(row(&ledger, 2).attempts[0].effect, Effect::NotSent);
    attempt(&mut ledger, 2, &meta(2, "n1", 5)).expect("a NotSent generation may retry");

    let unknown = serde_json::to_value(witness(&meta(1, "n1", 0), WitnessKind::User, 0)).unwrap();
    let payload =
        json!({ "key": 2, "state": { "state": "running" }, "tracking": { "witness": unknown } });
    ledger.append("transition", payload, &[]).unwrap();
    assert!(ledger.rows().is_err());
    drop(ledger);
    assert!(open(root.path()).rows().is_err(), "the channel holds");
}

// Old snapshots and untracked records fold as before and never get an attempt guessed for them.
#[test]
fn an_old_snapshot_folds_without_guessing_a_token() {
    let root = sandbox();
    let mut ledger = open(root.path());
    received(&mut ledger, 1);
    received(&mut ledger, 2);
    let legacy = serde_json::to_value(AttemptEvidence {
        binding: SourceBinding {
            channel_id: CHANNEL,
            provider: ShadowProvider::Claude,
            source: source(),
        },
        execution_nonce: "n0".into(),
        eof: 0,
        rendered_prompt: "frame".into(),
        source_ids: vec![1],
        record_end: None,
        native_turn_id: None,
    })
    .unwrap();
    let mut snapshot = ledger.rows().unwrap().compact().unwrap();
    snapshot["rows"]["1"]["state"] = json!({ "state": "injecting" });
    snapshot["rows"]["1"]["attempt"] = legacy;
    ledger.checkpoint(snapshot).unwrap();
    drop(ledger);

    let mut ledger = open(root.path());
    let old = row(&ledger, 1);
    assert!(old.attempts.is_empty() && old.witnesses.is_empty());
    assert_eq!(Reconciliation::from_row(&old), Reconciliation::Ambiguous);
    refused(attempt(&mut ledger, 1, &meta(1, "n0", 0)));
    assert_eq!(
        Reconciliation::from_row(&row(&ledger, 2)),
        Reconciliation::NeverSent
    );
}

#[derive(Clone, Copy)]
struct Facts {
    now: Option<u64>,
    retired: bool,
    obligated: bool,
}

impl Retire for Facts {
    fn now_ms(&self) -> Option<u64> {
        self.now
    }
    fn retired(&self, _: &AttemptMeta) -> bool {
        self.retired
    }
    fn obligated(&self, _: u64) -> bool {
        self.obligated
    }
}

fn settled_with_detail(root: &Path) -> (Ledger, AttemptMeta) {
    let mut ledger = open(root);
    received(&mut ledger, 1);
    let first = meta(1, "n1", 0);
    attempt(&mut ledger, 1, &first).unwrap();
    set(&mut ledger, 1, RowState::AwaitTurn);
    ledger
        .append_witness(1, witness(&first, WitnessKind::User, 10))
        .unwrap();
    set(&mut ledger, 1, RowState::Done(DoneReason::Completed));
    (ledger, first)
}

fn detail(ledger: &Ledger) -> (usize, usize) {
    let row = row(ledger, 1);
    (row.attempts.len(), row.witnesses.len())
}

// Detail goes only after thirty days, retirement, no obligation and a second checkpoint agree.
#[test]
fn a_tombstone_is_collected_only_after_every_condition_holds_at_two_checkpoints() {
    let root = sandbox();
    let (mut ledger, first) = settled_with_detail(root.path());
    let t0 = 1_000 * DAY_MS;
    let ready = Facts {
        now: Some(t0),
        retired: true,
        obligated: false,
    };
    ledger.checkpoint_rows_with(&ready).unwrap();
    let later = Facts {
        now: Some(t0 + TOMBSTONE_HORIZON_MS),
        ..ready
    };
    for blocked in [
        Facts { now: None, ..later },
        Facts {
            now: Some(t0 + TOMBSTONE_HORIZON_MS - 1),
            ..later
        },
        Facts {
            now: Some(t0 - 1),
            ..later
        },
        Facts {
            retired: false,
            ..later
        },
        Facts {
            obligated: true,
            ..later
        },
    ] {
        ledger.checkpoint_rows_with(&blocked).unwrap();
        ledger.checkpoint_rows_with(&blocked).unwrap();
        assert_eq!(detail(&ledger), (1, 1), "kept while any condition fails");
    }

    received(&mut ledger, 2);
    let shared = meta(1, "n1", 50);
    attempt(&mut ledger, 2, &shared).unwrap();
    set(&mut ledger, 2, RowState::AwaitTurn);
    ledger
        .append_witness(2, witness(&shared, WitnessKind::User, 10))
        .unwrap();
    ledger.checkpoint_rows_with(&later).unwrap();
    ledger.checkpoint_rows_with(&later).unwrap();
    assert_eq!(
        detail(&ledger),
        (1, 1),
        "an open row still reads the same record"
    );
    set(&mut ledger, 2, RowState::Done(DoneReason::Completed));

    ledger.checkpoint_rows_with(&later).unwrap();
    assert_eq!(detail(&ledger), (1, 1), "one checkpoint is not enough");
    let recording = Recording::start(root.path());
    recording.arm(None);
    ledger.checkpoint_rows_with(&later).unwrap();
    let steps = recording.events().len();
    assert_eq!(detail(&ledger), (0, 0));
    drop(recording);
    let collected = ledger.rows().unwrap();
    let base = collected.row(1).unwrap();
    assert_eq!(
        (base.state, base.received_seq, collected.owner(1)),
        (
            RowState::Done(DoneReason::Completed),
            Some(1),
            Owner::Settled
        )
    );

    for cut in 1..=steps {
        let crashed = sandbox();
        let (mut ledger, _) = settled_with_detail(crashed.path());
        ledger.checkpoint_rows_with(&ready).unwrap();
        ledger.checkpoint_rows_with(&later).unwrap();
        let recording = Recording::start(crashed.path());
        recording.arm(Some(cut));
        assert!(ledger.checkpoint_rows_with(&later).is_err());
        drop((recording, ledger));
        let reopened = open(crashed.path());
        let base = reopened.rows().unwrap().row(1).unwrap().clone();
        assert!(matches!(detail(&reopened), (1, 1) | (0, 0)), "cut {cut}");
        assert_eq!(
            base.state,
            RowState::Done(DoneReason::Completed),
            "cut {cut}"
        );
    }

    refused(ledger.append_witness(1, witness(&first, WitnessKind::User, 99)));
    received(&mut ledger, 1);
    drop(ledger);
    let reopened = open(root.path()).rows().unwrap();
    assert_eq!(
        reopened.owner(1),
        Owner::Settled,
        "the base key still dedups"
    );
    assert!(reopened.row(1).unwrap().attempts.is_empty());
}

// A merged record confirms each row once even when an append fails between two of its rows.
#[test]
fn a_failed_append_inside_a_merged_record_resumes_without_double_counting() {
    for adopted in [true, false] {
        let root = sandbox();
        let mut ledger = open(root.path());
        let metas: Vec<_> = (1..=3)
            .map(|key| {
                received(&mut ledger, key);
                let meta = meta(1, "n1", 0);
                attempt(&mut ledger, key, &meta).unwrap();
                set(&mut ledger, key, RowState::AwaitTurn);
                meta
            })
            .collect();
        let merged = |meta: &AttemptMeta| witness(meta, WitnessKind::User, 70);
        ledger.append_witness(1, merged(&metas[0])).unwrap();
        let recording = Recording::start(root.path());
        recording.arm(Some(1));
        assert!(ledger.append_witness(2, merged(&metas[1])).is_err());
        drop((recording, ledger));
        if !adopted {
            cut_tail(root.path(), 3);
        }

        let mut ledger = open(root.path());
        let running = |ledger: &Ledger, key| row(ledger, key).state == RowState::Running;
        assert!(running(&ledger, 1));
        assert_eq!(running(&ledger, 2), adopted);
        for (key, meta) in (1..=3).zip(&metas) {
            ledger.append_witness(key, merged(meta)).unwrap();
        }
        for key in 1..=3 {
            let row = row(&ledger, key);
            assert_eq!(
                (row.state, row.witnesses.len()),
                (RowState::Running, 1),
                "row {key}"
            );
        }
        let live = ledger.rows().unwrap();
        drop(ledger);
        assert_eq!(open(root.path()).rows().unwrap(), live);
    }
}

// Cuts the last WAL line mid-record, as a power loss during append would.
fn cut_tail(root: &Path, bytes: u64) {
    let dir = root.join("input_ledger").join(CHANNEL.to_string());
    let wal = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .unwrap();
    let file = OpenOptions::new().write(true).open(&wal).unwrap();
    let len = file.metadata().unwrap().len();
    file.set_len(len - bytes).unwrap();
}
