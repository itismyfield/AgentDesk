use super::ledger::input_notices::{MigrationHistory, MigrationSnapshot, NoticeEntry};
use super::*;

fn migration() -> LedgerEntry {
    LedgerEntry::InputNotice {
        entry: Box::new(NoticeEntry::MigrationStarted {
            snapshot: MigrationSnapshot {
                sources: Vec::new(),
                binding_checkpoint: 0,
                history: MigrationHistory::Fixed {
                    lower: 100,
                    upper: 100,
                },
            },
        }),
    }
}

#[test]
fn input_notice_floor_precedes_first_migration_fsync_and_a_failed_ledger_append() {
    for fail_append in [false, true] {
        let runtime = tempfile::tempdir().unwrap();
        let store = tests::enabled(runtime.path());
        let era = tests::sealed(&store, &[7]);
        let mut channel = store.open_channel(&era, 7).unwrap().unwrap();
        let ledger = runtime.path().join("o_store/7/ledger.jsonl");
        let fault = fail_append.then(|| {
            durable::fault::plant(
                &ledger,
                durable::fault::Step::Append(durable::fault::Keep::Nothing),
                io::ErrorKind::StorageFull,
                None,
            )
        });
        let result = channel.append_ledger(migration());
        assert_eq!(result.is_err(), fail_append);
        assert_eq!(
            std::fs::read(runtime.path().join("o_store/input_notice.floor")).unwrap(),
            b"1\n"
        );
        assert_eq!(
            channel.ledger().next_serial(),
            0,
            "migration is not a Prepared or POST"
        );
        drop((fault, channel));
        let reopened = store.open_channel(&era, 7).unwrap().unwrap();
        assert_eq!(
            reopened.ledger().input_notices().migration.is_some(),
            !fail_append
        );
        assert_eq!(reopened.ledger().anchor(), 100);
        assert!(reopened.ledger().unresolved().is_none());
    }
}

#[test]
fn input_notice_floor_failure_writes_no_entry_and_does_not_reach_prepared() {
    let runtime = tempfile::tempdir().unwrap();
    let store = tests::enabled(runtime.path());
    let era = tests::sealed(&store, &[7]);
    let mut channel = store.open_channel(&era, 7).unwrap().unwrap();
    let root = runtime.path().join("o_store");
    let failure = durable::fault::plant(
        &root,
        durable::fault::Step::Write,
        io::ErrorKind::StorageFull,
        None,
    );
    assert!(channel.append_ledger(migration()).is_err());
    assert_eq!(
        std::fs::metadata(root.join("7/ledger.jsonl"))
            .unwrap()
            .len(),
        0
    );
    assert!(!root.join("input_notice.floor").exists());
    assert!(channel.ledger().input_notices().migration.is_none());
    assert_eq!(channel.ledger().next_serial(), 0);
    drop((failure, channel));
    let mut reopened = store.open_channel(&era, 7).unwrap().unwrap();
    reopened.append_ledger(migration()).unwrap();
    assert!(reopened.ledger().input_notices().migration.is_some());
}
