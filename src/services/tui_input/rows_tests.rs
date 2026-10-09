use super::handover::{
    Composer, EnqueueOutcome, Handback, MoveEvidence, MoveSource, Reconciliation,
    handback_after_enqueue, handback_plan, move_disposition,
};
use super::rows::{AbandonReason, DoneReason, HeldReason, RowState};

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod supported {
    use std::collections::BTreeSet;
    use std::fs::{self, OpenOptions};
    use std::path::Path;

    use serde_json::json;
    use tempfile::TempDir;

    use super::super::handover::{
        Composer, EnqueueOutcome, Handback, MoveEvidence, MoveSource, Reconciliation,
        handback_after_enqueue, handback_plan, move_disposition,
    };
    use super::super::ledger::Ledger;
    use super::super::rows::{AbandonReason, DoneReason, Entry, Owner, RowState, Rows};

    const CHANNEL: u64 = 9;

    fn sandbox() -> TempDir {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/i01-tmp");
        fs::create_dir_all(&root).unwrap();
        tempfile::Builder::new()
            .prefix("rows-")
            .tempdir_in(root)
            .unwrap()
    }

    fn open(runtime: &Path) -> Ledger {
        Ledger::open(runtime, CHANNEL).unwrap()
    }

    fn stage(ledger: &mut Ledger, key: u64, state: RowState) -> u64 {
        let input = json!({ "text": format!("input {key}") });
        ledger
            .append_entry(&Entry::Staged { key, input, state }, &[])
            .unwrap()
    }

    fn commit(ledger: &mut Ledger, first_staged_seq: u64, ids: &[u64]) -> u64 {
        let ids = ids.to_vec();
        ledger
            .append_entry(
                &Entry::MoveCommitted {
                    first_staged_seq,
                    ids,
                },
                &[],
            )
            .unwrap()
    }

    fn set(ledger: &mut Ledger, key: u64, state: RowState) {
        ledger
            .append_entry(
                &Entry::Transition {
                    key,
                    state,
                    attempt: None,
                },
                &[],
            )
            .unwrap();
    }

    fn open_keys(rows: &Rows) -> Vec<u64> {
        rows.open_rows().map(|(key, _)| key).collect()
    }

    // Cuts the last WAL line mid-record, as a power loss during append would.
    fn cut_tail(runtime: &Path, bytes: u64) {
        let dir = runtime.join("input_ledger").join(CHANNEL.to_string());
        let wal = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .unwrap();
        let file = OpenOptions::new().write(true).open(&wal).unwrap();
        let len = file.metadata().unwrap().len();
        file.set_len(len - bytes).unwrap();
    }

    const QUEUED: MoveEvidence = MoveEvidence {
        user_record: false,
        turn_open: false,
        never_started: true,
        composer: Composer::Empty,
    };

    // Stages every Legacy input, commits, then deletes the originals as the move does.
    fn move_all(ledger: &mut Ledger, legacy: &mut BTreeSet<u64>) -> u64 {
        let state = move_disposition(MoveSource::Queue, QUEUED);
        let first = legacy
            .iter()
            .map(|&key| stage(ledger, key, state))
            .min()
            .unwrap();
        let ids: Vec<u64> = legacy.iter().copied().collect();
        let seq = commit(ledger, first, &ids);
        legacy.clear();
        seq
    }

    fn hand_back_all(ledger: &mut Ledger, legacy: &mut BTreeSet<u64>, outcome: EnqueueOutcome) {
        let rows = ledger.rows().unwrap();
        for (key, row) in rows.open_rows() {
            match handback_plan(row.state, Reconciliation::from_legacy(row.state, false)) {
                Handback::Enqueue => {
                    if let Some(closed) = handback_after_enqueue(outcome) {
                        legacy.insert(key);
                        set(ledger, key, closed);
                    }
                }
                Handback::Close(closed) | Handback::NoticeThenClose(closed) => {
                    set(ledger, key, closed)
                }
                Handback::Hold | Handback::Settled => {}
            }
        }
    }

    fn assert_one_owner(rows: &Rows, legacy: &BTreeSet<u64>, known: &BTreeSet<u64>) {
        for &key in known {
            let in_legacy = legacy.contains(&key);
            match rows.owner(key) {
                Owner::Ledger => assert!(!in_legacy, "input {key} held by both owners"),
                Owner::Legacy => assert!(in_legacy, "input {key} released to nobody"),
                Owner::Settled => assert!(!in_legacy, "settled input {key} is back in Legacy"),
            }
        }
    }

    #[test]
    fn repeated_ledger_legacy_ledger_transitions_keep_each_input_with_one_owner() {
        let root = sandbox();
        let runtime = root.path().join("runtime");
        let mut legacy = BTreeSet::from([1, 2]);
        let mut known = legacy.clone();
        let mut ledger = open(&runtime);
        for (cycle, arriving) in [(0, 3), (1, 4), (2, 0)] {
            let committed: Vec<u64> = legacy.iter().copied().collect();
            let commit_seq = move_all(&mut ledger, &mut legacy);
            ledger = open(&runtime);
            let rows = ledger.rows().unwrap();
            assert_one_owner(&rows, &legacy, &known);
            assert!(rows.ledger_owned());
            for key in committed {
                let row = rows.row(key).unwrap();
                assert_eq!((row.since_seq, row.state), (commit_seq, RowState::Received));
            }
            if arriving == 0 {
                break;
            }
            match cycle {
                0 => {
                    for state in [RowState::Injecting, RowState::Running] {
                        set(&mut ledger, 1, state);
                    }
                    set(&mut ledger, 1, RowState::Done(DoneReason::Completed));
                    hand_back_all(&mut ledger, &mut legacy, EnqueueOutcome::Rejected);
                    let rows = ledger.rows().unwrap();
                    assert_one_owner(&rows, &legacy, &known);
                    assert_eq!(open_keys(&rows), vec![2]);
                }
                _ => {
                    set(&mut ledger, 2, RowState::Running);
                    set(&mut ledger, 2, RowState::Done(DoneReason::Completed));
                }
            }
            hand_back_all(&mut ledger, &mut legacy, EnqueueOutcome::Persisted);
            ledger = open(&runtime);
            let rows = ledger.rows().unwrap();
            assert_one_owner(&rows, &legacy, &known);
            assert!(!rows.ledger_owned());
            legacy.insert(arriving);
            known.insert(arriving);
        }
        assert_eq!(legacy, BTreeSet::new());
        let rows = ledger.rows().unwrap();
        assert_eq!(open_keys(&rows), vec![3, 4]);
        assert_eq!(rows.owner(1), Owner::Settled);
        assert_eq!(rows.owner(2), Owner::Settled);
    }

    #[test]
    fn failed_staging_after_an_earlier_commit_activates_no_new_staged_row() {
        let root = sandbox();
        let runtime = root.path().join("runtime");
        let mut ledger = open(&runtime);
        let first = stage(&mut ledger, 1, RowState::Received);
        commit(&mut ledger, first, &[1]);
        set(&mut ledger, 1, RowState::Running);
        let retry_first = stage(&mut ledger, 2, RowState::Received);
        stage(&mut ledger, 1, RowState::Received);
        stage(&mut ledger, 3, RowState::Received);
        drop(ledger);
        cut_tail(&runtime, 5);

        let mut ledger = open(&runtime);
        let rows = ledger.rows().unwrap();
        assert_eq!(rows.owner(2), Owner::Legacy);
        assert_eq!(rows.owner(3), Owner::Legacy);
        assert_eq!(open_keys(&rows), vec![1]);
        assert_eq!(rows.row(1).unwrap().state, RowState::Running);
        assert!(rows.unbound().is_empty());
        assert_eq!(rows.staged_since(retry_first), BTreeSet::from([1, 2]));

        let next_first = stage(&mut ledger, 2, RowState::Received);
        stage(&mut ledger, 3, RowState::Received);
        let next_commit = commit(&mut ledger, next_first, &[2, 3]);
        let rows = open(&runtime).rows().unwrap();
        assert_eq!(open_keys(&rows), vec![1, 2, 3]);
        assert_eq!(rows.row(1).unwrap().state, RowState::Running);
        assert_eq!(rows.row(2).unwrap().since_seq, next_commit);
    }

    #[test]
    fn committed_input_is_released_only_by_terminal_or_handback_records() {
        let root = sandbox();
        let runtime = root.path().join("runtime");
        let mut ledger = open(&runtime);
        let first = stage(&mut ledger, 1, RowState::Received);
        for key in [2, 3, 4] {
            stage(&mut ledger, key, RowState::Received);
        }
        let commit_seq = commit(&mut ledger, first, &[1, 2, 3, 4]);
        set(&mut ledger, 1, RowState::Running);

        let restaged = stage(&mut ledger, 1, RowState::Received);
        commit(&mut ledger, restaged, &[1]);
        let empty_window = ledger.rows().unwrap().folded_seq() + 1;
        commit(&mut ledger, empty_window, &[1, 2]);
        let rows = ledger.rows().unwrap();
        assert_eq!(rows.owner(1), Owner::Ledger);
        let row = rows.row(1).unwrap();
        assert_eq!((row.since_seq, row.state), (commit_seq, RowState::Running));
        assert!(rows.unbound().is_empty());

        ledger.checkpoint_rows().unwrap();
        let mut ledger = open(&runtime);
        let rows = ledger.rows().unwrap();
        assert_eq!(open_keys(&rows), vec![1, 2, 3, 4]);
        assert_eq!(rows.row(1).unwrap().state, RowState::Running);
        assert_eq!(rows.row(2).unwrap().input, json!({ "text": "input 2" }));

        set(&mut ledger, 1, RowState::Done(DoneReason::Completed));
        set(&mut ledger, 2, RowState::Abandoned(AbandonReason::Handback));
        set(
            &mut ledger,
            3,
            RowState::Abandoned(AbandonReason::UserClear),
        );
        let rows = ledger.rows().unwrap();
        assert_eq!(rows.owner(1), Owner::Settled);
        assert_eq!(rows.owner(2), Owner::Legacy);
        assert_eq!(rows.owner(3), Owner::Settled);
        assert_eq!(rows.owner(4), Owner::Ledger);

        let again = stage(&mut ledger, 1, RowState::Received);
        stage(&mut ledger, 2, RowState::Received);
        stage(&mut ledger, 3, RowState::Received);
        let again_seq = commit(&mut ledger, again, &[1, 2, 3]);
        let rows = open(&runtime).rows().unwrap();
        assert_eq!(open_keys(&rows), vec![2, 4]);
        assert_eq!(rows.row(2).unwrap().since_seq, again_seq);
        assert_eq!(rows.owner(1), Owner::Settled);
        assert_eq!(rows.owner(3), Owner::Settled);
    }

    #[test]
    fn commit_window_binds_only_staged_rows_from_first_seq_to_before_commit() {
        let root = sandbox();
        let runtime = root.path().join("runtime");
        let mut ledger = open(&runtime);
        stage(&mut ledger, 1, RowState::Received);
        let first = stage(&mut ledger, 2, RowState::Received);
        stage(&mut ledger, 3, RowState::Received);
        let commit_seq = commit(&mut ledger, first, &[1, 2, 3, 4]);
        stage(&mut ledger, 4, RowState::Received);

        let rows = open(&runtime).rows().unwrap();
        assert_eq!(open_keys(&rows), vec![2, 3]);
        assert_eq!(rows.row(3).unwrap().since_seq, commit_seq);
        assert!(rows.row(1).is_none());
        assert!(rows.row(4).is_none());
        assert_eq!(rows.unbound(), &BTreeSet::from([1, 4]));
        assert_eq!(rows.owner(1), Owner::Ledger);

        stage(&mut ledger, 5, RowState::Received);
        commit(&mut ledger, commit_seq, &[5]);
        let rows = open(&runtime).rows().unwrap();
        assert!(rows.row(5).is_none());
        assert_eq!(rows.unbound(), &BTreeSet::from([1, 4, 5]));
    }

    #[test]
    fn checkpoint_during_staging_keeps_staged_rows_bindable_after_reopen() {
        let root = sandbox();
        let runtime = root.path().join("runtime");
        let mut ledger = open(&runtime);
        let first = stage(&mut ledger, 1, RowState::Received);
        stage(&mut ledger, 2, RowState::Held(super::HeldReason::Ambiguous));
        ledger.checkpoint_rows().unwrap();

        let mut ledger = open(&runtime);
        assert!(ledger.records().is_empty());
        commit(&mut ledger, first, &[1, 2]);
        let rows = open(&runtime).rows().unwrap();
        assert_eq!(open_keys(&rows), vec![1, 2]);
        assert_eq!(
            rows.row(2).unwrap().state,
            RowState::Held(super::HeldReason::Ambiguous)
        );
        assert!(rows.unbound().is_empty());
    }

    #[test]
    fn replay_is_deterministic_and_a_cut_commit_leaves_its_inputs_with_legacy() {
        let root = sandbox();
        let runtime = root.path().join("runtime");
        let mut ledger = open(&runtime);
        let first = stage(&mut ledger, 1, RowState::Received);
        stage(&mut ledger, 2, RowState::Received);
        commit(&mut ledger, first, &[1, 2]);
        set(&mut ledger, 1, RowState::Running);
        ledger.checkpoint_rows().unwrap();
        set(&mut ledger, 1, RowState::Done(DoneReason::Completed));
        let pending = stage(&mut ledger, 3, RowState::Received);
        let before_commit = ledger.rows().unwrap();
        commit(&mut ledger, pending, &[3]);
        let live = ledger.rows().unwrap();
        drop(ledger);

        let first_replay = open(&runtime).rows().unwrap();
        let second_replay = open(&runtime).rows().unwrap();
        assert_eq!(first_replay, live);
        assert_eq!(second_replay, live);
        assert_eq!(first_replay.compact().unwrap(), live.compact().unwrap());
        assert_eq!(live.owner(3), Owner::Ledger);

        cut_tail(&runtime, 3);
        let cut = open(&runtime).rows().unwrap();
        assert_eq!(cut, before_commit);
        assert_eq!(open(&runtime).rows().unwrap(), cut);
        assert_eq!(cut.owner(3), Owner::Legacy);
        assert_eq!(cut.staged_since(pending), BTreeSet::from([3]));
        assert_eq!(open_keys(&cut), vec![2]);

        let mut ledger = open(&runtime);
        ledger.append("unknown", json!({}), &[]).unwrap();
        assert!(ledger.rows().is_err());
        assert!(open(&runtime).rows().is_err());
    }
}

#[test]
fn move_disposition_follows_the_move_table() {
    let evidence = |user_record, turn_open, composer| MoveEvidence {
        user_record,
        turn_open,
        never_started: true,
        composer,
    };
    let held = RowState::Held(HeldReason::Ambiguous);
    for source in [MoveSource::DispatchOnly, MoveSource::TurnRow] {
        assert_eq!(
            move_disposition(
                source,
                MoveEvidence {
                    never_started: false,
                    ..evidence(false, false, Composer::Empty)
                }
            ),
            held,
            "empty composer alone is not pre-effect evidence"
        );
    }
    let done = RowState::Done(DoneReason::Completed);
    let cases = [
        (
            MoveSource::Queue,
            evidence(false, false, Composer::Draft),
            RowState::Received,
        ),
        (
            MoveSource::Queue,
            evidence(true, true, Composer::Empty),
            RowState::Done(DoneReason::AcceptedBeforeMove),
        ),
        (
            MoveSource::DispatchOnly,
            evidence(false, false, Composer::Empty),
            RowState::Received,
        ),
        (
            MoveSource::DispatchOnly,
            evidence(false, false, Composer::Draft),
            held,
        ),
        (
            MoveSource::DispatchOnly,
            evidence(true, true, Composer::Empty),
            RowState::Running,
        ),
        (
            MoveSource::DispatchOnly,
            evidence(true, false, Composer::Draft),
            done,
        ),
        (
            MoveSource::TurnRow,
            evidence(true, true, Composer::Draft),
            RowState::Running,
        ),
        (
            MoveSource::TurnRow,
            evidence(true, false, Composer::Empty),
            done,
        ),
        (
            MoveSource::TurnRow,
            evidence(false, false, Composer::Empty),
            RowState::Received,
        ),
        (
            MoveSource::TurnRow,
            evidence(false, true, Composer::Draft),
            held,
        ),
    ];
    for (source, evidence, expected) in cases {
        assert_eq!(
            move_disposition(source, evidence),
            expected,
            "{source:?} {evidence:?}"
        );
    }
}

#[test]
fn handback_plan_follows_the_revert_table() {
    use Reconciliation::{Ambiguous, Consumed, ModelConfirmed, NeverSent, QueueOnly};
    let running_done = Handback::Close(RowState::Done(DoneReason::HandbackRunning));
    let ambiguous = Handback::NoticeThenClose(RowState::Held(HeldReason::Ambiguous));
    let consumed = Handback::NoticeThenClose(RowState::Done(DoneReason::HandbackRunning));
    let before_paste = [
        RowState::Received,
        RowState::Ready,
        RowState::Held(HeldReason::Modal),
        RowState::Held(HeldReason::NotReady),
    ];
    let attempted = [
        RowState::Held(HeldReason::Ambiguous),
        RowState::Unaccepted,
        RowState::Injecting,
        RowState::AwaitTurn,
        RowState::Running,
    ];
    for state in before_paste {
        assert_eq!(Reconciliation::from_legacy(state, false), NeverSent);
        assert_eq!(handback_plan(state, NeverSent), Handback::Enqueue);
    }
    for state in attempted {
        assert_eq!(
            Reconciliation::from_legacy(state, false),
            Ambiguous,
            "an empty composer is not evidence for {state:?}"
        );
        assert_eq!(handback_plan(state, Ambiguous), ambiguous);
    }
    for state in before_paste.into_iter().chain(attempted) {
        assert_eq!(handback_plan(state, ModelConfirmed), running_done);
        assert_eq!(handback_plan(state, Consumed), consumed);
        assert_eq!(handback_plan(state, QueueOnly), Handback::Hold);
    }
    for evidence in [QueueOnly, Ambiguous, NeverSent] {
        assert_eq!(handback_plan(RowState::Queued, evidence), Handback::Hold);
    }
    assert_eq!(handback_plan(RowState::Running, NeverSent), ambiguous);
    for state in [
        RowState::Done(DoneReason::Completed),
        RowState::Abandoned(AbandonReason::UserClear),
        RowState::Abandoned(AbandonReason::Handback),
    ] {
        assert_eq!(handback_plan(state, ModelConfirmed), Handback::Settled);
    }
    assert!(ModelConfirmed < Consumed && Consumed < QueueOnly);
    assert!(QueueOnly < Ambiguous && Ambiguous < NeverSent);
    let handed_back = Some(RowState::Abandoned(AbandonReason::Handback));
    assert_eq!(
        handback_after_enqueue(EnqueueOutcome::Persisted),
        handed_back
    );
    assert_eq!(
        handback_after_enqueue(EnqueueOutcome::AlreadyPreserved),
        handed_back
    );
    assert_eq!(handback_after_enqueue(EnqueueOutcome::Rejected), None);
}

// Tracked attempts and witnesses on the same ledger: admission, late delivery and retention.
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod tracked {
    use std::fs::{self, OpenOptions};
    use std::io;
    use std::path::Path;

    use serde_json::json;

    use super::super::attempt::{
        AttemptMeta, Disposition, Effect, QueueEnd, Retire, TOMBSTONE_HORIZON_MS, Tracking,
        Witness, WitnessKind, fresh_token,
    };
    use super::super::durability_tests::supported::Recording;
    use super::super::handover::Reconciliation;
    use super::super::ledger::Ledger;
    use super::super::rows::{
        AbandonReason, AttemptEvidence, DoneReason, Entry, HeldReason, Owner, Row, RowState,
    };
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
        let reason = Tracking {
            disposition: Some(Disposition::Interrupted),
            ..Tracking::default()
        };
        refused(ledger.append_tracked(1, RowState::Running, None, &reason));
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
        // The old exit's EOF must sit on the prior source after every prior-generation record.
        for broken in [
            QueueEnd {
                old_source: SourceId { ino: 2, ..source() },
                ..end(&first, "n2")
            },
            QueueEnd {
                old_end: 25,
                ..end(&first, "n2")
            },
        ] {
            let mut early = meta(2, "n2", 100);
            early.queue_end = Some(broken);
            refused(attempt(&mut ledger, 1, &early));
        }
        attempt(&mut ledger, 1, &second).expect("a verified queue end offers once");
        let mut replay = meta(3, "n3", 200);
        replay.queue_end = Some(end(&first, "n3"));
        refused(attempt(&mut ledger, 1, &replay));

        let retried = meta(1, "n1", 0);
        attempt(&mut ledger, 2, &retried).unwrap();
        set(&mut ledger, 2, RowState::Ready);
        assert_eq!(row(&ledger, 2).attempts[0].effect, Effect::NotSent);
        attempt(&mut ledger, 2, &meta(2, "n1", 5)).expect("a NotSent generation may retry");

        let unknown =
            serde_json::to_value(witness(&meta(1, "n1", 0), WitnessKind::User, 0)).unwrap();
        let payload = json!({ "key": 2, "state": { "state": "running" }, "tracking": { "witness": unknown } });
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

    #[test]
    fn receipt_identity_survives_terminal_compact_reopen_and_attempt_collection() {
        use super::super::receipt_identity::{ReceiptIdentity, Responsibility};

        let root = sandbox();
        let mut ledger = open(root.path());
        let identity = ReceiptIdentity::new(1, vec![1, 11], 7, 8, CHANNEL).unwrap();
        let input = json!({"text":"input 1","receipt_identity":identity});
        ledger
            .append_entry(&Entry::Received { key: 1, input }, &[])
            .unwrap();
        let first = meta(1, "n1", 0);
        attempt(&mut ledger, 1, &first).unwrap();
        set(&mut ledger, 1, RowState::AwaitTurn);
        ledger
            .append_witness(1, witness(&first, WitnessKind::User, 10))
            .unwrap();
        set(&mut ledger, 1, RowState::Done(DoneReason::Completed));
        let facts = Facts {
            now: Some(1_000 * DAY_MS),
            retired: true,
            obligated: false,
        };
        ledger.checkpoint_rows_with(&facts).unwrap();
        drop(ledger);

        let mut ledger = open(root.path());
        assert_eq!(row(&ledger, 1).input, json!(null));
        assert_eq!(row(&ledger, 1).receipt_identity.as_ref(), Some(&identity));
        let later = Facts {
            now: Some(facts.now.unwrap() + TOMBSTONE_HORIZON_MS),
            ..facts
        };
        ledger.checkpoint_rows_with(&later).unwrap();
        ledger.checkpoint_rows_with(&later).unwrap();
        assert_eq!(detail(&ledger), (0, 0), "attempt GC actually occurred");
        drop(ledger);

        let restored = open(root.path()).rows().unwrap();
        assert_eq!(
            restored.row(1).unwrap().receipt_identity.as_ref(),
            Some(&identity)
        );
        let alias = ReceiptIdentity::new(11, vec![11], 7, 8, CHANNEL).unwrap();
        assert_eq!(
            restored.responsibility(&alias),
            Responsibility::Known {
                key: 1,
                received_seq: 1
            }
        );
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

    // A close reason never moves an open row anywhere a model record or the actor must decide.
    #[test]
    fn a_close_reason_only_settles_or_holds_an_open_row() {
        let root = sandbox();
        let mut ledger = open(root.path());
        received(&mut ledger, 1);
        let first = meta(1, "n1", 0);
        attempt(&mut ledger, 1, &first).unwrap();
        set(&mut ledger, 1, RowState::AwaitTurn);
        let records = ledger.records().len();
        let reason = Tracking {
            disposition: Some(Disposition::Cancelled),
            ..Tracking::default()
        };
        for state in [
            RowState::Running,
            RowState::Queued,
            RowState::Ready,
            RowState::AwaitTurn,
        ] {
            refused(ledger.append_tracked(1, state, None, &reason));
        }
        assert_eq!(ledger.records().len(), records);
        let held = RowState::Held(HeldReason::Ambiguous);
        ledger.append_tracked(1, held, None, &reason).unwrap();
        assert_eq!(row(&ledger, 1).dispositions, vec![Disposition::Cancelled]);

        let forged = serde_json::to_value(&reason).unwrap();
        let payload = json!({ "key": 1, "state": { "state": "running" }, "tracking": forged });
        ledger.append("transition", payload, &[]).unwrap();
        assert!(ledger.rows().is_err());
    }

    // Without a ranged prior record, the queue end alone must name the prior source and a later EOF.
    #[test]
    fn a_queue_end_names_the_prior_source_and_an_eof_past_its_anchor() {
        let root = sandbox();
        let mut ledger = open(root.path());
        received(&mut ledger, 1);
        let first = meta(1, "n1", 40);
        attempt(&mut ledger, 1, &first).unwrap();
        set(&mut ledger, 1, RowState::AwaitTurn);
        let mut queued = witness(&first, WitnessKind::Queued, 0);
        queued.range = None;
        ledger.append_witness(1, queued).unwrap();
        let end = QueueEnd {
            prior_generation: 1,
            old_nonce: "n1".into(),
            new_nonce: "n2".into(),
            old_source: source(),
            old_end: 60,
            stable_reads: 2,
            settle_profile: "claude-2.1.289".into(),
        };
        let records = ledger.records().len();
        for broken in [
            QueueEnd {
                old_source: SourceId { ino: 2, ..source() },
                ..end.clone()
            },
            QueueEnd {
                old_end: 39,
                ..end.clone()
            },
        ] {
            let mut next = meta(2, "n2", 100);
            next.queue_end = Some(broken);
            refused(attempt(&mut ledger, 1, &next));
        }
        assert_eq!(ledger.records().len(), records);
        let mut next = meta(2, "n2", 100);
        next.queue_end = Some(end);
        attempt(&mut ledger, 1, &next).expect("a queue end on the prior source offers once");
    }

    // A tracked key handed back to Legacy keeps its tombstone; no later move activates it again.
    #[test]
    fn a_tracked_handback_key_keeps_its_tombstone_against_reactivation() {
        let root = sandbox();
        let mut ledger = open(root.path());
        received(&mut ledger, 1);
        received(&mut ledger, 2);
        let first = meta(1, "n1", 0);
        attempt(&mut ledger, 1, &first).unwrap();
        set(&mut ledger, 1, RowState::Ready);
        let back = RowState::Abandoned(AbandonReason::Handback);
        set(&mut ledger, 1, back);
        set(&mut ledger, 2, back);
        assert!(ledger.rows().unwrap().keeps_tracked_history(1));
        assert!(!ledger.rows().unwrap().keeps_tracked_history(2));

        received(&mut ledger, 1);
        received(&mut ledger, 2);
        let staged = Entry::Staged {
            key: 1,
            input: json!({ "text": "again" }),
            state: RowState::Received,
        };
        let first_staged_seq = ledger.append_entry(&staged, &[]).unwrap();
        let moved = Entry::MoveCommitted {
            first_staged_seq,
            ids: vec![1],
        };
        ledger.append_entry(&moved, &[]).unwrap();
        ledger.checkpoint_rows().unwrap();
        drop(ledger);

        let rows = open(root.path()).rows().unwrap();
        let kept = rows.row(1).unwrap();
        let tokens: Vec<&str> = kept.attempts.iter().map(|m| m.token.as_str()).collect();
        assert_eq!((kept.state, tokens), (back, vec![first.token.as_str()]));
        assert_eq!(kept.attempts[0].effect, Effect::NotSent);
        assert_eq!(rows.owner(1), Owner::Legacy);
        assert_eq!(
            rows.row(2).unwrap().state,
            RowState::Received,
            "an untracked key returns"
        );
    }

    // A re-read may name the turn the first copy lacked: one more record, same count and state.
    #[test]
    fn a_witness_learns_its_turn_once_and_keeps_the_row_state() {
        let root = sandbox();
        let mut ledger = open(root.path());
        received(&mut ledger, 1);
        let first = meta(1, "n1", 0);
        attempt(&mut ledger, 1, &first).unwrap();
        set(&mut ledger, 1, RowState::AwaitTurn);
        let user = witness(&first, WitnessKind::User, 10);
        assert!(ledger.append_witness(1, user.clone()).unwrap().is_some());
        let held = RowState::Held(HeldReason::Ambiguous);
        set(&mut ledger, 1, held);

        let named = |turn: &str| Witness {
            turn_ref: Some(turn.into()),
            ..user.clone()
        };
        assert!(ledger.append_witness(1, named("t1")).unwrap().is_some());
        for again in [named("t1"), named("t2"), user.clone()] {
            assert_eq!(ledger.append_witness(1, again).unwrap(), None);
        }
        let learned = row(&ledger, 1);
        assert_eq!(learned.state, held);
        assert_eq!(learned.witnesses.len(), 1);
        assert_eq!(learned.witnesses[0].witness.turn_ref.as_deref(), Some("t1"));
        drop(ledger);
        assert_eq!(row(&open(root.path()), 1), learned);
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
}
