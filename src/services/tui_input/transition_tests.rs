#[cfg(any(target_os = "macos", target_os = "linux"))]
mod supported {
    use super::super::ledger::Ledger;
    use super::super::rows::{Entry, RowState};
    use serde_json::json;

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
}
