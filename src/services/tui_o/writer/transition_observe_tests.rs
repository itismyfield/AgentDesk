use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use super::*;
use crate::services::tui_o::shadow::binding_reader::source_id_for;

/// A drained cursor at the end of `body`, written to `name` under `dir`.
fn drained(dir: &Path, name: &str, body: &[u8]) -> Cursor {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    Cursor {
        source: source_id_for("session", &path).unwrap(),
        captured_through: body.len() as u64,
        prefix_hash: hex::encode(Sha256::digest(body)),
        retired: false,
        transition_cleanup: None,
    }
}

fn present(seen: &Observation) -> &Present {
    match &seen.file {
        FileSeen::Present(present) => present,
        other => panic!("probed as {other:?}"),
    }
}

#[test]
fn transition_observe_reports_a_drained_source_at_its_end_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let cursor = drained(dir.path(), "t.jsonl", b"{\"a\":1}\n{\"b\":2}\n");
    let path = &cursor.source.path;
    let (bytes, modified) = (
        std::fs::read(path).unwrap(),
        Fingerprint::path(&cursor.source),
    );
    let at = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(7);
    let seen = observe(&cursor, at);
    assert!(seen.is_of(&cursor));
    assert_eq!(seen.at, at);
    let expected = Present {
        fingerprint: Fingerprint::path(&cursor.source).unwrap(),
        same_file: true,
        prefix_matches: true,
        whole_tail: true,
        steady: true,
    };
    assert_eq!(present(&seen), &expected);
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    assert_eq!(Fingerprint::path(&cursor.source), modified);
    let listed = std::fs::read_dir(dir.path()).unwrap().count();
    assert_eq!(listed, 1, "the probe left no file behind");
}

#[test]
fn transition_observe_sees_a_torn_tail_growth_and_a_rewritten_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let torn = drained(dir.path(), "torn.jsonl", b"{\"a\":1}\n{\"b\"");
    let seen = observe(&torn, SystemTime::now());
    assert!(present(&seen).prefix_matches);
    assert!(!present(&seen).whole_tail);

    let growing = drained(dir.path(), "grow.jsonl", b"{\"a\":1}\n");
    let first = observe(&growing, SystemTime::now());
    let mut file = OpenOptions::new()
        .append(true)
        .open(&growing.source.path)
        .unwrap();
    file.write_all(b"{\"b\":2}\n").unwrap();
    let second = observe(&growing, SystemTime::now());
    assert_eq!(present(&first).fingerprint.len, growing.captured_through);
    assert_eq!(
        present(&second).fingerprint.len,
        growing.captured_through + 8
    );
    assert!(present(&second).prefix_matches);

    let rewritten = drained(dir.path(), "rewrite.jsonl", b"{\"a\":1}\n");
    std::fs::write(&rewritten.source.path, b"{\"a\":2}\n").unwrap();
    assert!(!present(&observe(&rewritten, SystemTime::now())).prefix_matches);
    let shrunk = drained(dir.path(), "shrunk.jsonl", b"{\"a\":1}\n");
    std::fs::write(&shrunk.source.path, b"{").unwrap();
    let seen = observe(&shrunk, SystemTime::now());
    assert!(!present(&seen).prefix_matches);
    assert_eq!(present(&seen).fingerprint.len, 1);
}

#[test]
fn transition_observe_tells_a_replaced_path_from_a_deleted_one() {
    let dir = tempfile::tempdir().unwrap();
    let body = b"{\"a\":1}\n";
    let replaced = drained(dir.path(), "r.jsonl", body);
    let other = dir.path().join("other.jsonl");
    std::fs::write(&other, body).unwrap();
    std::fs::rename(&other, &replaced.source.path).unwrap();
    let seen = observe(&replaced, SystemTime::now());
    assert_eq!(seen.file, FileSeen::Replaced);

    let deleted = drained(dir.path(), "d.jsonl", body);
    std::fs::remove_file(&deleted.source.path).unwrap();
    assert_eq!(observe(&deleted, SystemTime::now()).file, FileSeen::Missing);
}
