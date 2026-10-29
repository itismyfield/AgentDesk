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
    json!({"type":"attachment", "attachment":{"type":"queued_command", "prompt":text,
        "commandMode":"prompt", "origin":{"kind":"human"}, "humanTurn":true}})
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

fn queue(operation: &str, content: Option<&str>, reason: Option<&str>) -> Value {
    let mut record = json!({"type":"queue-operation", "operation":operation});
    if let Some(content) = content {
        record["content"] = json!(content);
    }
    if let Some(reason) = reason {
        record["reason"] = json!(reason);
    }
    record
}

fn closed_with(rows: &[Value]) -> InputFacts {
    let root = tempfile::tempdir().unwrap();
    let (path, mut facts) = reader(&root);
    append(&path, rows);
    facts.poll(u64::MAX).unwrap();
    facts
}

/// A background notification command is no human input: it consumes none.
#[test]
fn a_task_notification_command_consumes_no_human_input() {
    let notification = json!({"type":"attachment", "attachment":{"type":"queued_command",
        "prompt":"<task-notification>done</task-notification>", "commandMode":"task-notification",
        "origin":{"kind":"task-notification"}}});
    let facts = closed_with(&[user("a", "first"), enqueue("b"), notification, idle()]);
    assert!(facts.pending_receipt());
}

/// Two inputs of the same body are two inputs: running one leaves the other owed.
#[test]
fn equal_bodies_are_separate_inputs() {
    let rows = [
        user("a", "first"),
        enqueue("same"),
        enqueue("same"),
        queued_command("same"),
        idle(),
    ];
    assert!(closed_with(&rows).pending_receipt());
}

/// A human command for another body does not settle this input.
#[test]
fn a_command_for_another_input_leaves_this_one_owed() {
    let rows = [
        user("a", "first"),
        enqueue("b"),
        queued_command("c"),
        idle(),
    ];
    assert!(closed_with(&rows).pending_receipt());
}

/// Claude's FIFO dequeue ties the next opener to the input it took, whatever that opener says.
#[test]
fn a_dequeued_input_runs_in_the_turn_it_opens() {
    let rows = [
        user("a", "first"),
        enqueue("b"),
        idle(),
        queue("dequeue", None, None),
        assistant("b's answer"),
        idle(),
    ];
    assert!(!closed_with(&rows).pending_receipt());
}

/// Inputs withdrawn before running owe nothing: a cancel-cleared removal and a pull-back.
#[test]
fn withdrawn_inputs_owe_no_run() {
    let cleared = [
        user("a", "first"),
        enqueue("b"),
        queue("remove", Some("b"), Some("cleared_on_cancel")),
        idle(),
    ];
    assert!(!closed_with(&cleared).pending_receipt());
    let pulled = [
        user("a", "first"),
        enqueue("b"),
        enqueue("c"),
        queue("popAll", Some("b\nc"), None),
        idle(),
    ];
    assert!(!closed_with(&pulled).pending_receipt());
    let absorbed = [
        user("a", "first"),
        enqueue("b"),
        queue("remove", Some("b"), Some("absorbed_mid_turn")),
        idle(),
    ];
    assert!(
        closed_with(&absorbed).pending_receipt(),
        "absorption alone is not the run: its command must follow"
    );
}

/// A reader rebuilt from the file start replays the same owed inputs.
#[test]
fn a_rebuilt_reader_replays_owed_inputs() {
    let root = tempfile::tempdir().unwrap();
    let (path, mut facts) = reader(&root);
    append(&path, &[user("a", "first"), enqueue("b"), idle()]);
    facts.poll(u64::MAX).unwrap();
    let (dev, ino) = file_identity(&std::fs::metadata(&path).unwrap());
    let binding = SourceBinding {
        channel_id: 7_533_003,
        provider: ShadowProvider::Claude,
        source: SourceId {
            session_id: "parent".into(),
            path: path.clone(),
            dev,
            ino,
        },
    };
    let mut rebuilt = InputFacts::open(binding).unwrap();
    rebuilt.poll(u64::MAX).unwrap();
    assert_eq!(rebuilt.pending_receipt(), facts.pending_receipt());
    assert!(rebuilt.pending_receipt());
}
