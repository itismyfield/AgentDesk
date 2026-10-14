use super::{
    proof::{ProofRequest, prove},
    reader::VerifiedReader,
};
use crate::services::agent_protocol::StreamMessage;
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;

pub(super) fn assistant(text: &str) -> Value {
    json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}})
}

pub(super) fn records() -> Vec<Value> {
    vec![
        json!({"type":"session_meta","payload":{"id":"s1"}}),
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"old"}}),
        assistant("OLD\n"),
        json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"old"}}),
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"cur"}}),
        assistant("FROZEN\n"),
        assistant("BODY 가나다\n"),
        assistant("TAIL\n"),
        json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"cur"}}),
    ]
}

pub(super) fn write(path: &Path, records: &[Value]) {
    std::fs::write(
        path,
        records.iter().map(|v| format!("{v}\n")).collect::<String>(),
    )
    .unwrap();
}

fn reader(path: &Path, prefix: &str) -> VerifiedReader {
    VerifiedReader::from_plan(
        prove(&ProofRequest {
            raw_path: path,
            session_id: Some("s1"),
            native_turn_id: Some("cur"),
            composed: prefix,
        })
        .unwrap(),
    )
}

fn drain(reader: &mut VerifiedReader) -> Vec<StreamMessage> {
    let mut messages = Vec::new();
    for _ in 0..20 {
        messages.extend(reader.poll().unwrap());
        if reader.finished() {
            return messages;
        }
    }
    panic!("verified terminal was not consumed")
}

fn text(messages: &[StreamMessage]) -> String {
    messages
        .iter()
        .filter_map(|m| match m {
            StreamMessage::Text { content } => Some(content.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn opened_fd_utf8_residual_and_terminal_are_consumed_once() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("raw");
    write(&path, &records());
    let plan = prove(&ProofRequest {
        raw_path: &path,
        session_id: Some("s1"),
        native_turn_id: Some("cur"),
        composed: "FROZEN\nBODY 가",
    })
    .unwrap();
    std::fs::rename(&path, tmp.path().join("opened")).unwrap();
    write(&path, &[assistant("WRONG\n")]);
    let mut verified = VerifiedReader::from_plan(plan);
    let messages = drain(&mut verified);
    assert_eq!(text(&messages), "나다\nTAIL\n");
    assert_eq!(
        messages
            .iter()
            .filter(|m| matches!(m, StreamMessage::Done { .. }))
            .count(),
        1
    );
    assert!(verified.poll().unwrap().is_empty());
}

#[test]
fn crossing_record_done_preserves_residual_and_complete_body() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("raw");
    let mut source = records()[..5].to_vec();
    source.push(json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"cur","last_agent_message":"FROZEN\nBODY 가나다\nTAIL\n"}}));
    write(&path, &source);
    let messages = drain(&mut reader(&path, "FROZEN\nBODY 가"));
    assert_eq!(text(&messages), "나다\nTAIL\n");
    assert!(
        matches!(messages.last(),Some(StreamMessage::Done{result,..}) if result == "FROZEN\nBODY 가나다\nTAIL\n")
    );
}

#[test]
fn partial_utf8_record_does_not_advance_and_eof_is_not_done() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("raw");
    write(&path, &records()[..7]);
    let complete = format!("{}\n", assistant("가나다\n"));
    let split = complete.find('가').unwrap() + 1;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    file.write_all(&complete.as_bytes()[..split]).unwrap();
    let mut verified = reader(&path, "FROZEN\nBODY 가나다\n");
    let cursor = verified.cursor();
    for _ in 0..2 {
        assert!(verified.poll().unwrap().is_empty());
        assert_eq!(verified.cursor(), cursor);
        assert!(!verified.finished());
    }
    file.write_all(&complete.as_bytes()[split..]).unwrap();
    assert_eq!(text(&verified.poll().unwrap()), "가나다\n");
    assert_eq!(verified.cursor(), cursor + complete.len() as u64);
    assert!(verified.poll().unwrap().is_empty());
    assert!(!verified.finished());
}

#[test]
fn equal_prose_in_later_ranges_survives_and_next_turn_is_bounded() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("raw");
    let mut source = records()[..7].to_vec();
    source.extend([
        assistant("TAIL\n"),
        assistant("TAIL\n"),
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"next"}}),
        assistant("FOREIGN\n"),
    ]);
    write(&path, &source);
    let messages = drain(&mut reader(&path, "FROZEN\nBODY 가나다\n"));
    assert_eq!(text(&messages), "TAIL\nTAIL\n");
    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, StreamMessage::Done { .. }))
    );
}

#[test]
fn session_change_does_not_fold_foreign_output_or_advance_cursor() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("raw");
    let mut source = records()[..7].to_vec();
    source.extend([
        json!({"type":"session_meta","payload":{"id":"foreign"}}),
        assistant("FOREIGN\n"),
    ]);
    write(&path, &source);
    let mut verified = reader(&path, "FROZEN\nBODY 가나다\n");
    let cursor = verified.cursor();
    assert_eq!(
        verified.poll().unwrap_err(),
        super::CoordinateHold::NeedsEvidence("session_mismatch")
    );
    assert_eq!(verified.cursor(), cursor);
}

#[test]
fn seeded_writer_emits_terminal_fallback_suffix_as_text_without_replaying_prefix() {
    use super::super::rebind_runtime::codex_spawn::prepare_verified_writer;
    use std::sync::{Arc, Mutex};
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("raw");
    let seed = "FROZEN\nBODY 가나다\n";
    let mut source = records()[..5].to_vec();
    source.push(json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"cur","last_agent_message":format!("{seed}TAIL\n")}}));
    write(&path, &source);
    let output = tmp.path().join("normalized");
    let prepared = prepare_verified_writer(
        output.clone(),
        reader(&path, seed),
        seed.into(),
        (Arc::new(Mutex::new(1)), 1),
    )
    .unwrap();
    prepared.gate.finish(true);
    prepared.join().unwrap();
    let records: Vec<Value> = std::fs::read_to_string(&output)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(records.len(), 2);
    assert_eq!(
        records[0]
            .pointer("/message/content/0/text")
            .and_then(Value::as_str),
        Some("TAIL\n")
    );
    assert_eq!(
        records[1].get("result").and_then(Value::as_str),
        Some(format!("{seed}TAIL\n").as_str())
    );
}

#[test]
fn verified_writer_keeps_seed_without_resuppressing_tail_and_matches_ordinary_done() {
    use super::super::rebind_runtime::{
        codex_spawn::prepare_verified_writer, write_codex_rebind_normalized_stream,
    };
    use std::sync::{Arc, Mutex};
    let tmp = tempfile::tempdir().unwrap();
    let raw = tmp.path().join("raw");
    let verified_path = tmp.path().join("verified");
    let seed = "FROZEN\nBODY 가나다\n";
    let mut source = records();
    source[7] = assistant(&format!("{seed}TAIL\n"));
    write(&raw, &source);
    let prepared = prepare_verified_writer(
        verified_path.clone(),
        reader(&raw, seed),
        seed.into(),
        (Arc::new(Mutex::new(1)), 1),
    )
    .unwrap();
    assert_eq!(std::fs::read(&verified_path).unwrap(), b"");
    prepared.gate.finish(true);
    prepared.join().unwrap();
    let ordinary_path = tmp.path().join("ordinary");
    let (sender, receiver) = std::sync::mpsc::channel();
    for message in [
        StreamMessage::Text {
            content: seed.into(),
        },
        StreamMessage::Text {
            content: format!("{seed}TAIL\n"),
        },
        StreamMessage::Done {
            result: format!("{seed}{seed}TAIL\n"),
            session_id: Some("s1".into()),
        },
    ] {
        sender.send(message).unwrap();
    }
    drop(sender);
    write_codex_rebind_normalized_stream(&ordinary_path, receiver, seed.into(), Vec::new())
        .unwrap();
    let bytes = std::fs::read(&verified_path).unwrap();
    assert_eq!(bytes, std::fs::read(&ordinary_path).unwrap());
    let normalized: Vec<Value> = std::str::from_utf8(&bytes)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(normalized.len(), 2);
    assert_eq!(
        normalized[0]
            .pointer("/message/content/0/text")
            .and_then(Value::as_str),
        Some(format!("{seed}TAIL\n").as_str())
    );
    assert_eq!(
        normalized[1].get("result").and_then(Value::as_str),
        Some(format!("{seed}{seed}TAIL\n").as_str())
    );
}

#[test]
fn ordinary_codex_tui_terminal_done_preserves_normalized_done_semantics() {
    use super::super::rebind_runtime::write_codex_rebind_normalized_stream;
    let temp = tempfile::tempdir().unwrap();
    let ordinary = StreamMessage::Done {
        result: "STAIL".into(),
        session_id: Some("s1".into()),
    };
    let native = StreamMessage::CodexTuiTerminalDone {
        result: "STAIL".into(),
        session_id: Some("s1".into()),
        rollout_path: "raw".into(),
        tmux_session_name: "fixture".into(),
        turn_nonce: "nonce".into(),
        source_start: 128,
        complete_record_end: 512,
        captured_source: None,
        kind: crate::services::agent_protocol::NativeTerminalKind::Completed,
    };
    let mut outputs = Vec::new();
    for (i, message) in [ordinary, native].into_iter().enumerate() {
        let path = temp.path().join(i.to_string());
        let (send, recv) = std::sync::mpsc::channel();
        send.send(message).unwrap();
        drop(send);
        write_codex_rebind_normalized_stream(&path, recv, "S".into(), Vec::new()).unwrap();
        outputs.push(std::fs::read(&path).unwrap());
    }
    assert_eq!(outputs[0], outputs[1]);
    let records: Vec<Value> = std::str::from_utf8(&outputs[0])
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(
        records[0]
            .pointer("/message/content/0/text")
            .and_then(Value::as_str),
        Some("TAIL")
    );
    assert_eq!(
        records[1].get("result").and_then(Value::as_str),
        Some("STAIL")
    );
}
