//! What a queued input's receipt and a close leave for the carried-turn close to read.

use std::io::Write;

use serde_json::{Value, json};

use super::*;
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::shadow::capture::file_identity;

fn reader(root: &tempfile::TempDir) -> (std::path::PathBuf, InputFacts) {
    let path = root.path().join("parent.jsonl");
    std::fs::write(&path, "").unwrap();
    let (dev, ino) = file_identity(&std::fs::metadata(&path).unwrap());
    let binding = SourceBinding {
        channel_id: 7_533_001,
        provider: ShadowProvider::Claude,
        source: SourceId {
            session_id: "parent".into(),
            path: path.clone(),
            dev,
            ino,
        },
    };
    (path.clone(), InputFacts::open(binding).unwrap())
}

fn append(path: &std::path::Path, records: &[Value]) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    for record in records {
        writeln!(file, "{record}").unwrap();
    }
}

fn user(id: &str, text: &str) -> Value {
    json!({"type":"user", "uuid":id, "message":{"content":text}})
}

fn idle() -> Value {
    json!({"type":"system", "subtype":"turn_duration"})
}

fn enqueue(text: &str) -> Value {
    json!({"type":"queue-operation", "operation":"enqueue", "content":text})
}

fn queued_command(text: &str) -> Value {
    json!({"type":"attachment", "attachment":{"type":"queued_command", "prompt":text}})
}

fn assistant(text: &str) -> Value {
    json!({"type":"assistant", "uuid":"u-answer", "apiBlockIndex":0,
        "message":{"id":"answer", "content":[{"type":"text","text":text}]}})
}

/// Both receipt shapes after a close leave the reader Idle, so only `pending_receipt` tells the
/// close that a run is still owed; the turn the input opens clears it.
#[test]
fn a_receipt_after_a_close_is_pending_until_its_turn_opens() {
    for receipt in [enqueue("next"), queued_command("next")] {
        let root = tempfile::tempdir().unwrap();
        let (path, mut facts) = reader(&root);
        append(&path, &[user("a", "first"), idle()]);
        assert_eq!(facts.poll(u64::MAX).unwrap().state, TurnState::Idle);
        assert!(!facts.pending_receipt());
        assert_eq!(facts.last_close_aborted(), Some(false));
        append(&path, std::slice::from_ref(&receipt));
        assert_eq!(
            facts.poll(u64::MAX).unwrap().state,
            TurnState::Idle,
            "{receipt} opens no turn"
        );
        assert!(facts.pending_receipt(), "{receipt} owes a run");
        append(&path, &[user("b", "next")]);
        assert!(matches!(
            facts.poll(u64::MAX).unwrap().state,
            TurnState::Open { .. }
        ));
        assert!(!facts.pending_receipt(), "the opened turn runs it");
    }
}

/// An input queued while a turn runs and not taken by it is still owed after that turn closes.
#[test]
fn an_enqueue_inside_a_turn_stays_pending_past_its_close() {
    let root = tempfile::tempdir().unwrap();
    let (path, mut facts) = reader(&root);
    append(&path, &[user("a", "first"), enqueue("later"), idle()]);
    assert_eq!(facts.poll(u64::MAX).unwrap().state, TurnState::Idle);
    assert!(facts.pending_receipt());
}

/// A queued command attached inside the running turn is that turn consuming it.
#[test]
fn a_queued_command_taken_inside_a_turn_is_not_pending() {
    let root = tempfile::tempdir().unwrap();
    let (path, mut facts) = reader(&root);
    append(
        &path,
        &[
            user("a", "first"),
            enqueue("later"),
            queued_command("later"),
            assistant("done"),
            idle(),
        ],
    );
    assert_eq!(facts.poll(u64::MAX).unwrap().state, TurnState::Idle);
    assert!(!facts.pending_receipt());
}

/// A close whose opener this reader never read cannot vouch for the turn's receipts.
#[test]
fn a_close_without_its_opener_has_no_abort_answer() {
    let root = tempfile::tempdir().unwrap();
    let (path, _) = reader(&root);
    append(&path, &[user("a", "first")]);
    let offset = std::fs::metadata(&path).unwrap().len();
    append(&path, &[idle()]);
    let (dev, ino) = file_identity(&std::fs::metadata(&path).unwrap());
    let binding = SourceBinding {
        channel_id: 7_533_002,
        provider: ShadowProvider::Claude,
        source: SourceId {
            session_id: "parent".into(),
            path: path.clone(),
            dev,
            ino,
        },
    };
    let mut tail = InputFacts::open_at(binding, offset).unwrap();
    tail.poll(u64::MAX).unwrap();
    assert_eq!(tail.last_close_aborted(), None);
}

/// The interrupt marker's close is kept as aborted.
#[test]
fn an_interrupted_close_is_aborted() {
    let root = tempfile::tempdir().unwrap();
    let (path, mut facts) = reader(&root);
    let stop = json!({"type":"user","message":{"content":"[Request interrupted by user]"}});
    append(&path, &[user("a", "first"), stop]);
    assert_eq!(facts.poll(u64::MAX).unwrap().state, TurnState::Idle);
    assert_eq!(facts.last_close_aborted(), Some(true));
}
