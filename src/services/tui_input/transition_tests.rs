#[cfg(any(target_os = "macos", target_os = "linux"))]
mod supported {
    use super::super::handover::{Composer, EnqueueOutcome, MoveEvidence};
    use super::super::ledger::{Ledger, LedgerLease};
    use super::super::rows::Row;
    use super::super::rows::{Entry, RowState};
    use super::super::transition::{DeletePhase, Host, Input, Outcome, handback};
    use serde_json::json;
    use std::io;

    struct Destination(Vec<u64>);
    impl Host for Destination {
        fn collect(&mut self, _: &Ledger) -> io::Result<Vec<Input>> {
            unreachable!()
        }
        fn evidence(&mut self, _: &Input) -> io::Result<MoveEvidence> {
            unreachable!()
        }
        fn delete(&mut self, _: DeletePhase) -> io::Result<()> {
            unreachable!()
        }
        fn start_actor(&mut self) -> io::Result<()> {
            unreachable!()
        }
        fn reconcile(&mut self, _: u64, _: &Row) -> io::Result<(bool, Composer)> {
            Ok((false, Composer::Empty))
        }
        fn enqueue(&mut self, key: u64, _: &Row) -> io::Result<EnqueueOutcome> {
            self.0.push(key);
            Ok(EnqueueOutcome::Persisted)
        }
        fn notice(&mut self, _: Option<u64>, _: &'static str) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn staged_order_survives_commit_and_checkpoint() {
        let parent = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/i01-tmp");
        std::fs::create_dir_all(&parent).unwrap();
        let root = tempfile::tempdir_in(parent).unwrap();
        for (channel, keys) in [(1, [9, 2]), (2, [2, 9])] {
            let mut ledger = Ledger::open(root.path(), channel).unwrap();
            for key in keys {
                ledger
                    .append_entry(
                        &Entry::Staged {
                            key,
                            input: json!({"text": format!("input {key}")}),
                            state: RowState::Received,
                        },
                        &[],
                    )
                    .unwrap();
            }
            ledger
                .append_entry(
                    &Entry::MoveCommitted {
                        first_staged_seq: 1,
                        ids: vec![2, 9],
                    },
                    &[],
                )
                .unwrap();
            ledger.checkpoint_rows().unwrap();
            drop(ledger);
            let reopened = Ledger::open(root.path(), channel).unwrap();
            let rows = reopened.rows().unwrap();
            let mut ordered: Vec<_> = rows.open_rows().collect();
            ordered.sort_by_key(|(_, row)| row.received_seq);
            assert_eq!(
                ordered.iter().map(|(key, _)| *key).collect::<Vec<_>>(),
                keys,
                "staging order must survive a checkpoint"
            );
        }
    }
    #[test]
    fn staged_order_survives_checkpoint_handback() {
        let parent = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/i01-tmp");
        std::fs::create_dir_all(&parent).unwrap();
        let root = tempfile::tempdir_in(parent).unwrap();
        for (channel, keys) in [(1, [9, 2]), (2, [2, 9])] {
            let mut ledger = Ledger::open(root.path(), channel).unwrap();
            for key in keys {
                ledger
                    .append_entry(
                        &Entry::Staged {
                            key,
                            input: json!({"text": format!("input {key}")}),
                            state: RowState::Received,
                        },
                        &[],
                    )
                    .unwrap();
            }
            ledger
                .append_entry(
                    &Entry::MoveCommitted {
                        first_staged_seq: 1,
                        ids: vec![2, 9],
                    },
                    &[],
                )
                .unwrap();
            ledger.checkpoint_rows().unwrap();
            drop(ledger);
            let mut destination = Destination(vec![99]);
            assert_eq!(
                handback(
                    &mut LedgerLease::new(root.path(), channel),
                    &mut destination
                )
                .unwrap(),
                Outcome::Legacy
            );
            assert_eq!(
                destination.0,
                vec![99, keys[0], keys[1]],
                "staging order must survive a checkpoint"
            );
        }
    }

    #[test]
    fn handback_with_external_row_has_zero_partial_effects() {
        let parent = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/i01-tmp");
        std::fs::create_dir_all(&parent).unwrap();
        let root = tempfile::tempdir_in(parent).unwrap();
        let external = super::super::input_key::EXTERNAL_KEY_BASE + 7;
        let mut ledger = Ledger::open(root.path(), 3).unwrap();
        // The Discord row is older, so a per-row stop would already have returned it.
        for key in [2, external] {
            let input = json!({"text": format!("input {key}")});
            let entry = Entry::Received { key, input };
            ledger.append_entry(&entry, &[]).unwrap();
        }
        let seq = ledger.rows().unwrap().folded_seq();
        drop(ledger);
        let mut destination = Destination(Vec::new());
        assert_eq!(
            handback(&mut LedgerLease::new(root.path(), 3), &mut destination).unwrap(),
            Outcome::Held
        );
        assert!(destination.0.is_empty(), "nothing enqueued to Legacy");
        let rows = Ledger::open(root.path(), 3).unwrap().rows().unwrap();
        assert_eq!(rows.folded_seq(), seq, "no row closed");
        assert_eq!(rows.open_rows().count(), 2);
    }
}
