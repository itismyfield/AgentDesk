use super::*;
use serde_json::{Value, json};
use std::io::Write;
use std::sync::mpsc;

fn record(payload: Value) -> Value {
    json!({"type": "event_msg", "payload": payload})
}

fn text(body: &str) -> Value {
    json!({"type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": body}]}})
}

fn write_records(file: &mut tempfile::NamedTempFile, records: &[Value]) {
    for value in records {
        writeln!(file, "{value}").unwrap();
    }
    file.flush().unwrap();
}

fn run(
    file: &tempfile::NamedTempFile,
    origin: u64,
) -> (
    Result<(ReadOutputResult, RolloutTailOutcome), String>,
    Vec<StreamMessage>,
) {
    let (tx, rx) = mpsc::channel();
    let result = tail_fixed_recovery_rollout(
        file.reopen().unwrap(),
        file.path(),
        origin,
        Some("session".into()),
        "own".into(),
        tx,
        None,
    );
    (result, rx.into_iter().collect())
}

#[test]
fn fixed_reader_own_completion_keeps_message_boundaries_and_init() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    write_records(
        &mut file,
        &[json!({"type": "session_meta", "payload": {"id": "session"}})],
    );
    let origin = file.as_file().metadata().unwrap().len();
    write_records(
        &mut file,
        &[
            record(json!({"type": "task_started", "turn_id": "own"})),
            text("first"),
            json!({"type": "compacted", "payload": {}}),
            json!({"type": "turn_context", "payload": {"turn_id": "own"}}),
            text("last"),
            record(json!({"type": "task_complete", "turn_id": "own"})),
            text("foreign tail"),
        ],
    );
    let (result, messages) = run(&file, origin);
    let (_, outcome) = result.unwrap();
    assert!(outcome.final_offset < file.as_file().metadata().unwrap().len());
    assert!(
        matches!(&messages[0], StreamMessage::Init { session_id, .. } if session_id == "session")
    );
    assert_eq!(
        messages
            .iter()
            .filter(|m| matches!(m, StreamMessage::Init { .. }))
            .count(),
        1
    );
    assert!(
        matches!(messages.last(), Some(StreamMessage::Done { result, .. }) if result == "first\n\nlast")
    );
    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, StreamMessage::Text { content } if content.contains("foreign")))
    );
}

#[test]
fn fixed_reader_rejects_foreign_openers_before_content() {
    for opener in [
        record(json!({"type": "task_started", "turn_id": "foreign"})),
        json!({"type": "turn_context", "payload": {"turn_id": "foreign"}}),
        json!({"type": "turn_context", "payload": {}}),
        record(json!({"type": "task_complete", "turn_id": "foreign"})),
        record(json!({"type": "turn_aborted", "turn_id": "foreign"})),
    ] {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_records(
            &mut file,
            &[
                text("own"),
                opener,
                text("foreign"),
                record(json!({"type": "task_complete", "turn_id": "own"})),
            ],
        );
        let (result, messages) = run(&file, 0);
        assert!(result.unwrap_err().contains("foreign or unnamed turn"));
        assert!(
            !messages
                .iter()
                .any(|m| matches!(m, StreamMessage::Done { .. }))
        );
        assert!(
            !messages.iter().any(
                |m| matches!(m, StreamMessage::Text { content } if content.contains("foreign"))
            )
        );
    }
}

#[test]
fn fixed_reader_ignores_quiet_deadline_and_pane_death() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    write_records(
        &mut file,
        &[text("own"), record(json!({"type": "composer_ready"}))],
    );
    let cancel = Arc::new(CancelToken::new());
    let cancel_later = cancel.clone();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        cancel_later
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
    });
    let (tx, rx) = mpsc::channel();
    let (result, _) = tail_source(
        RolloutSource::FixedOpened(file.reopen().unwrap()),
        (file.path(), 0, Some("session".into())),
        &tx,
        Some(cancel),
        || false,
        RolloutTailOptions {
            own_turn: Some("own".into()),
            terminal_drain: Duration::ZERO,
            assistant_response_deadline: Some(Duration::ZERO),
            pending_tool_call_deadline: Some(Duration::ZERO),
            ..Default::default()
        },
    )
    .unwrap();
    drop(tx);
    stopper.join().unwrap();
    assert!(matches!(result, ReadOutputResult::Cancelled { .. }));
    assert!(
        !rx.into_iter()
            .any(|m| matches!(m, StreamMessage::Done { .. }))
    );
}

#[test]
fn fixed_reader_rejects_malformed_and_nonboundary_origin() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(b"{broken}\n").unwrap();
    let (result, messages) = run(&file, 0);
    assert!(
        result
            .unwrap_err()
            .contains("invalid fixed recovery record")
    );
    assert!(messages.is_empty());
    assert!(run(&file, 2).0.unwrap_err().contains("record boundary"));
}

#[test]
fn fixed_reader_abort_never_emits_success() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    write_records(
        &mut file,
        &[
            text("own"),
            record(json!({"type": "turn_aborted", "turn_id": "own"})),
        ],
    );
    let (result, messages) = run(&file, 0);
    assert!(result.unwrap_err().contains("aborted"));
    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, StreamMessage::Done { .. }))
    );
}

#[test]
fn fixed_reader_pins_open_descriptor_across_path_replacement() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("raw.jsonl");
    let records = [
        text("pinned"),
        record(json!({"type": "task_complete", "turn_id": "own"})),
    ];
    std::fs::write(
        &path,
        records.iter().map(|v| format!("{v}\n")).collect::<String>(),
    )
    .unwrap();
    let opened = std::fs::File::open(&path).unwrap();
    std::fs::rename(&path, directory.path().join("old.jsonl")).unwrap();
    std::fs::write(&path, "{broken}\n").unwrap();
    let (tx, rx) = mpsc::channel();
    tail_fixed_recovery_rollout(
        opened,
        &path,
        0,
        Some("session".into()),
        "own".into(),
        tx,
        None,
    )
    .unwrap();
    assert!(
        rx.into_iter()
            .any(|m| matches!(m, StreamMessage::Done { result, .. } if result == "pinned"))
    );
}

#[test]
fn fixed_reader_waits_for_a_split_final_record() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    write_records(&mut file, &[text("own")]);
    file.write_all(b"{\"type\":\"event_msg\",\"payload\":")
        .unwrap();
    let path = file.path().to_path_buf();
    let appender = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        let mut output = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        output
            .write_all(b"{\"type\":\"task_complete\",\"turn_id\":\"own\"}}\n")
            .unwrap();
    });
    let (result, messages) = run(&file, 0);
    appender.join().unwrap();
    result.unwrap();
    assert_eq!(
        messages
            .iter()
            .filter(|m| matches!(m, StreamMessage::Done { .. }))
            .count(),
        1
    );
}
