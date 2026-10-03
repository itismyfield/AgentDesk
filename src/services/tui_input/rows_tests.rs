use super::handover::{
    Composer, EnqueueOutcome, Handback, MoveEvidence, MoveSource, handback_after_enqueue,
    handback_plan, move_disposition,
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
        Composer, EnqueueOutcome, Handback, MoveEvidence, MoveSource, handback_after_enqueue,
        handback_plan, move_disposition,
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
            .append_entry(&Entry::Transition { key, state }, &[])
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
            match handback_plan(row.state, false, Composer::Empty) {
                Handback::Enqueue => {
                    if let Some(closed) = handback_after_enqueue(outcome) {
                        legacy.insert(key);
                        set(ledger, key, closed);
                    }
                }
                Handback::Close(closed) | Handback::NoticeThenClose(closed) => {
                    set(ledger, key, closed)
                }
                Handback::Settled => {}
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
                _ => set(&mut ledger, 2, RowState::Running),
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
        composer,
    };
    let held = RowState::Held(HeldReason::Ambiguous);
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
    let running_done = Handback::Close(RowState::Done(DoneReason::HandbackRunning));
    let ambiguous =
        Handback::NoticeThenClose(RowState::Abandoned(AbandonReason::HandbackAmbiguous));
    for state in [
        RowState::Received,
        RowState::Ready,
        RowState::Held(HeldReason::Modal),
        RowState::Held(HeldReason::NotReady),
    ] {
        assert_eq!(
            handback_plan(state, false, Composer::Draft),
            Handback::Enqueue
        );
    }
    for state in [
        RowState::Held(HeldReason::Ambiguous),
        RowState::Unaccepted,
        RowState::Injecting,
        RowState::AwaitTurn,
    ] {
        assert_eq!(handback_plan(state, true, Composer::Draft), running_done);
        assert_eq!(
            handback_plan(state, false, Composer::Empty),
            Handback::Enqueue
        );
        assert_eq!(handback_plan(state, false, Composer::Draft), ambiguous);
    }
    assert_eq!(
        handback_plan(RowState::Running, false, Composer::Draft),
        running_done
    );
    for state in [
        RowState::Done(DoneReason::Completed),
        RowState::Abandoned(AbandonReason::UserClear),
        RowState::Abandoned(AbandonReason::Handback),
    ] {
        assert_eq!(
            handback_plan(state, false, Composer::Empty),
            Handback::Settled
        );
    }
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
