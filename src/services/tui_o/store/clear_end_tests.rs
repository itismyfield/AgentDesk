use std::io::ErrorKind;
use std::path::PathBuf;

use super::*;
use crate::services::tui_o::store::fault::{self, Step};

fn record(channel: u64, binding_seq: u64) -> ClearEnd {
    ClearEnd {
        channel,
        provider: "claude".into(),
        tmux_session: "pane".into(),
        execution_nonce: "ended".into(),
        source: SourceId {
            session_id: "s1".into(),
            path: PathBuf::from("/t.jsonl"),
            dev: 1,
            ino: 2,
        },
        binding_seq,
        host: None,
        ended_at: Utc::now(),
        basis: "confirmed_clear".into(),
    }
}

fn channel() -> u64 {
    u64::from(uuid::Uuid::new_v4().as_fields().0) + 1
}

#[test]
fn appends_keep_the_newest_records_and_announce_each_one() {
    let (root, channel) = (tempfile::tempdir().unwrap(), channel());
    let mut notice = subscribe(channel);
    for seq in 1..=MAX_RECORDS as u64 + 1 {
        assert_eq!(
            append(root.path(), record(channel, seq)),
            Ok(Appended::Durable)
        );
        assert!(notice.has_changed().unwrap(), "append {seq} is announced");
        notice.borrow_and_update();
    }
    let kept: Vec<u64> = read(root.path(), channel)
        .unwrap()
        .iter()
        .map(|r| r.binding_seq)
        .collect();
    assert_eq!(kept, (2..=MAX_RECORDS as u64 + 1).collect::<Vec<_>>());
}

#[test]
fn a_failed_directory_sync_still_publishes_and_an_unpublished_write_announces_nothing() {
    let (root, channel) = (tempfile::tempdir().unwrap(), channel());
    let dir = root.path().join(DIR_NAME);
    let mut notice = subscribe(channel);
    let sync = fault::plant(&dir, Step::ReplaceDirSync, ErrorKind::Other, Some(1));
    let appended = append(root.path(), record(channel, 1));
    drop(sync);
    assert!(
        matches!(appended, Ok(Appended::PublishedUnsynced(_))),
        "{appended:?}"
    );
    let published = read(root.path(), channel).unwrap();
    let seqs: Vec<u64> = published.iter().map(|r| r.binding_seq).collect();
    assert_eq!(seqs, [1], "the record is in the file");
    assert!(
        notice.has_changed().unwrap(),
        "the published record is announced"
    );
    notice.borrow_and_update();
    let write = fault::plant(&dir, Step::Write, ErrorKind::StorageFull, Some(1));
    let failed = append(root.path(), record(channel, 2));
    drop(write);
    assert!(failed.is_err(), "{failed:?}");
    assert_eq!(
        read(root.path(), channel).unwrap(),
        published,
        "the file is unchanged"
    );
    assert!(
        !notice.has_changed().unwrap(),
        "nothing unpublished is announced"
    );
    assert_eq!(
        append(root.path(), record(channel, 3)),
        Ok(Appended::Durable)
    );
    let seqs: Vec<u64> = read(root.path(), channel)
        .unwrap()
        .iter()
        .map(|r| r.binding_seq)
        .collect();
    assert_eq!(seqs, [1, 3], "a later append keeps the unsynced record");
}
