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

#[test]
fn input_notice_origin_survives_read_only_observer_for_legacy_and_current_prepared() {
    use crate::services::tui_o::shadow::{
        ShadowProvider, SourceId, SourceRange, UnitKey, UnitKind,
    };
    use crate::services::tui_o::writer::input_notices::NoticeOrigin;

    for with_origin in [false, true] {
        let runtime = tempfile::tempdir().unwrap();
        let store = tests::enabled(runtime.path());
        let era = tests::sealed(&store, &[7]);
        let mut channel = store.open_channel(&era, 7).unwrap().unwrap();
        let origin = with_origin.then(|| {
            Box::new(NoticeOrigin {
                record: SourceRange {
                    source: SourceId {
                        session_id: "native".into(),
                        path: runtime.path().join("transcript.jsonl"),
                        dev: 1,
                        ino: 2,
                    },
                    start: 10,
                    end: 90,
                },
                logical_session: "pane".into(),
                binding_seq: 3,
                execution_nonce: "original".into(),
            })
        });
        let key = UnitKey {
            channel_id: 7,
            provider: ShadowProvider::Claude,
            native_key: "native-user".into(),
            kind: if with_origin {
                UnitKind::InputNotice
            } else {
                UnitKind::Body
            },
        };
        channel
            .append_ledger(LedgerEntry::Prepared {
                serial: 0,
                unit_key: key.clone(),
                piece_index: 0,
                payload: "retained content".into(),
                anchor_id: 100,
                epoch: 3,
                local_retry: None,
                notice_origin: origin.clone(),
            })
            .unwrap();
        channel
            .append_ledger(LedgerEntry::Posted {
                serial: 0,
                msg_id: 200,
            })
            .unwrap();
        let expected = channel
            .ledger()
            .latest_piece(&key, 0)
            .map(|(serial, record)| (serial, record.clone()))
            .unwrap();
        drop(channel);
        let path = runtime.path().join("o_store/7/ledger.jsonl");
        let before = std::fs::read(&path).unwrap();
        let first: serde_json::Value =
            serde_json::from_slice(before.split(|byte| *byte == b'\n').next().unwrap()).unwrap();
        assert_eq!(first["entry"].get("notice_origin").is_some(), with_origin);
        let snapshot = delivery_evidence::local(
            runtime.path(),
            7,
            &[(key.clone(), 0)],
            tokio::time::Instant::now() + std::time::Duration::from_secs(10),
        )
        .unwrap();
        let actual = snapshot.latest_piece(&key, 0).unwrap();
        assert_eq!(actual.0, expected.0);
        assert_eq!(actual.1.notice_origin, origin);
        assert_eq!(actual.1, &expected.1);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}
