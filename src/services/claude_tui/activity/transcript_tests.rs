use super::*;
use serde_json::json;
use std::{fs, io::Write, path::Path};
fn read(source: &Source) -> Option<Frame> {
    super::read(source, None)
}
fn fixture(rows: &[Value]) -> (tempfile::TempDir, Source) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("root.jsonl");
    for row in rows {
        append(&path, row.clone());
    }
    let meta = fs::metadata(&path).unwrap();
    use std::os::unix::fs::MetadataExt;
    let source = Source {
        session: "pane".into(),
        channel: 1,
        owner: "owner".into(),
        host: Some("host".into()),
        nonce: "epoch-a".into(),
        native: "root".into(),
        path,
        file: (meta.dev(), meta.ino()),
        visit: DateTime::from_timestamp(0, 0).unwrap(),
    };
    (dir, source)
}
fn append(path: &Path, mut row: Value) {
    row["sessionId"] = json!("root");
    row["timestamp"] = json!("2026-11-12T00:00:00Z");
    writeln!(
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap(),
        "{row}"
    )
    .unwrap();
}
fn prompt() -> Value {
    json!({"type":"user","uuid":"prompt-a","message":{"content":"work"}})
}
fn tool() -> Value {
    json!({"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","id":"a","name":"Bash","input":{}}]}})
}
#[test]
fn root_child_results_error_and_terminal_are_structural() {
    let (_dir, source) = fixture(&[
        prompt(),
        tool(),
        json!({"type":"assistant","isSidechain":true,
        "message":{"content":[{"type":"tool_use","id":"child","name":"Bash"}]}}),
    ]);
    let frame = read(&source).unwrap();
    assert!(frame.uses.contains_key("a"));
    assert!(!frame.uses.contains_key("child"));
    append(
        &source.path,
        json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"a","is_error":true}]}}),
    );
    let closed = read(&source).unwrap();
    assert_eq!(closed.turn, frame.turn);
    assert!(closed.results.contains("a"));
    append(
        &source.path,
        json!({"type":"system","subtype":"turn_duration"}),
    );
    let terminal = read(&source).unwrap();
    assert!(terminal.terminal > frame.turn.unwrap().floor);
    assert!(terminal.uses.is_empty());
}
#[test]
fn end_turn_and_child_progress_never_create_generation_evidence() {
    let (_dir, source) = fixture(&[
        prompt(),
        json!({"type":"assistant","message":{"content":[{"type":"text","text":"working"}]}}),
    ]);
    assert!(read(&source).unwrap().progress.is_some());
    append(
        &source.path,
        json!({"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"finished"}]}}),
    );
    assert!(read(&source).unwrap().progress.is_none());
    append(
        &source.path,
        json!({"type":"progress","data":{"agentId":"child"}}),
    );
    assert!(read(&source).unwrap().progress.is_none());
}

#[test]
fn interrupted_text_block_closes_its_turn_without_creating_a_submission() {
    let (_dir, source) = fixture(&[prompt(), tool()]);
    let before = read(&source).unwrap().turn;
    append(
        &source.path,
        json!({"type":"user","message":{"content":[{
            "type":"text","text":"[Request interrupted by user]"
        }]}}),
    );
    let stopped = read(&source).unwrap();
    assert_eq!(stopped.turn, before);
    assert!(stopped.terminal > 0);
    assert!(stopped.uses.is_empty());
}

#[test]
fn fresh_assistant_progress_after_a_completed_tool_can_recover_generation() {
    let (_dir, source) = fixture(&[prompt(), tool()]);
    append(
        &source.path,
        json!({"type":"user","message":{"content":[{
            "type":"tool_result","tool_use_id":"a"
        }]}}),
    );
    append(
        &source.path,
        json!({"type":"assistant","message":{"content":[{
            "type":"text","text":"new progress after the tool"
        }]}}),
    );
    let frame = read(&source).unwrap();
    assert!(frame.results.contains("a"));
    assert!(frame.progress.is_some());
}
#[test]
fn same_mtime_replay_reads_new_terminal_and_torn_or_foreign_records_fail_closed() {
    let (_dir, source) = fixture(&[prompt(), tool()]);
    let before = source.stamp().unwrap();
    append(
        &source.path,
        json!({"type":"system","subtype":"turn_duration"}),
    );
    fs::OpenOptions::new()
        .write(true)
        .open(&source.path)
        .unwrap()
        .set_modified(before.modified)
        .unwrap();
    assert!(
        read(&source).unwrap().terminal > 0,
        "mtime equality cannot suppress replay"
    );
    write!(
        fs::OpenOptions::new()
            .append(true)
            .open(&source.path)
            .unwrap(),
        "{{\"type\":"
    )
    .unwrap();
    assert!(!read(&source).unwrap().caught_up());
    fs::write(
        &source.path,
        "{\"type\":\"result\",\"sessionId\":\"foreign\"}\n",
    )
    .unwrap();
    assert!(read(&source).is_none());
}

#[test]
fn restart_replays_complete_prefix_in_bounded_reads_and_finishes_a_split_record() {
    let (_dir, source) = fixture(&[prompt(), tool()]);
    append(
        &source.path,
        json!({"type":"summary","summary":"x".repeat(READ_LIMIT as usize+512)}),
    );
    append(
        &source.path,
        json!({"type":"system","subtype":"turn_duration"}),
    );
    let first = super::read(&source, None).unwrap();
    assert_eq!(first.through, READ_LIMIT);
    assert!(!first.caught_up());
    let turn = first.turn.clone();
    let second = super::read(&source, Some(first)).unwrap();
    assert!(second.caught_up());
    assert_eq!(second.turn, turn);
    assert!(second.terminal > 0);
    assert!(second.uses.is_empty());
}
#[test]
fn turn_floor_changes_and_terminal_fences_old_generation() {
    let (_dir, source) = fixture(&[prompt(), tool()]);
    let first = read(&source).unwrap().turn.unwrap();
    append(
        &source.path,
        json!({"type":"system","subtype":"turn_duration"}),
    );
    let high = read(&source).unwrap().terminal;
    append(
        &source.path,
        json!({"type":"assistant","message":{"content":[{"type":"text","text":"continuing"}]}}),
    );
    assert_eq!(read(&source).unwrap().turn.unwrap().floor, high);
    append(
        &source.path,
        json!({"type":"user","uuid":"prompt-b","message":{"content":"new"}}),
    );
    let next = read(&source).unwrap().turn.unwrap();
    assert!(next.floor > first.floor);
    assert_ne!(next.prompt, first.prompt);
}
