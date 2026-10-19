use super::*;
use crate::services::provider::ProviderKind;
use serde_json::json;
use std::io::Write;

fn row() -> InflightTurnState {
    let mut row = InflightTurnState::new(
        ProviderKind::Codex,
        7138,
        None,
        77,
        101,
        102,
        "prompt".into(),
        Some("session".into()),
        Some("proof-only".into()),
        None,
        None,
        0,
    );
    row.started_at = "2020-01-01 00:00:00".into();
    row.full_response = "old".into();
    row
}

fn event(kind: &str, turn: &str) -> Value {
    json!({"type":"event_msg", "payload":{"type":kind, "turn_id":turn}})
}

fn prompt(text: &str) -> Value {
    json!({"type":"response_item", "timestamp":"2026-10-19T00:00:01Z",
        "payload":{"type":"message", "role":"user", "content":[{"type":"input_text","text":text}]}})
}

fn text(text: &str) -> Value {
    json!({"type":"response_item", "payload":{"type":"message", "role":"assistant",
        "content":[{"type":"output_text", "text":text}]}})
}

fn native() -> Vec<Value> {
    vec![
        json!({"type":"session_meta", "payload":{"id":"session"}}),
        event("task_started", "turn"),
        prompt("[User: Alice (ops) team (ID: 77)] prompt"),
        text("old"),
        text("tail"),
        event("task_complete", "turn"),
    ]
}

fn normalized() -> Vec<Value> {
    vec![
        json!({"type":"system","subtype":"init","session_id":"session"}),
        json!({"type":"assistant","message":{"content":[{"type":"text","text":"old"}]}}),
    ]
}

fn run(row: &InflightTurnState, native: &[Value], wrapper: &[Value]) -> Result<Proof, String> {
    let dir = tempfile::tempdir().unwrap();
    let raw_path = dir.path().join("raw");
    let wrapper_path = dir.path().join("wrapper");
    for (path, records) in [(&raw_path, native), (&wrapper_path, wrapper)] {
        let mut file = File::create(path).unwrap();
        for record in records {
            writeln!(file, "{record}").unwrap();
        }
    }
    prove(
        row,
        File::open(raw_path).unwrap(),
        File::open(wrapper_path).unwrap(),
    )
}

#[test]
fn six_compactions_and_attributed_old_row_keep_exact_order() {
    let mut native = native();
    for _ in 0..6 {
        native.insert(
            4,
            json!({"type":"turn_context", "payload":{"turn_id":"turn"}}),
        );
        native.insert(4, json!({"type":"compacted", "payload":{}}));
    }
    let plan = run(&row(), &native, &normalized()).unwrap();
    assert_eq!(plan.own_turn, "turn");
    assert!(plan.origin > 0);
    assert_eq!(plan.replay, normalized());
    assert_eq!(
        plan.raw.metadata().unwrap().len(),
        serde_json_lines(&native).len() as u64
    );
    assert!(plan.wrapper.metadata().unwrap().len() > 0);
}

#[test]
fn wrapper_without_init_uses_virtual_init_without_reordering_stored_events() {
    let plan = run(&row(), &native(), &normalized()[1..]).unwrap();
    assert_eq!(plan.replay, normalized());
}

#[test]
fn ordered_prefix_mismatch_and_missing_middle_are_held() {
    let mut wrapper = normalized();
    wrapper[1]["message"]["content"][0]["text"] = json!("tail");
    assert_eq!(
        run(&row(), &native(), &wrapper).err().as_deref(),
        Some("normalized_prefix_mismatch")
    );
    let mut wrapper = normalized();
    wrapper.insert(
        1,
        json!({"type":"assistant","message":{"content":[{"type":"thinking"}]}}),
    );
    assert_eq!(
        run(&row(), &native(), &wrapper).err().as_deref(),
        Some("normalized_prefix_mismatch")
    );
}

#[test]
fn owner_mismatch_is_not_native_prompt_evidence() {
    let mut source = native();
    source[2] = prompt("[User: Alice (ID: 78)] prompt");
    assert_eq!(
        run(&row(), &source, &normalized()).err().as_deref(),
        Some("prompt_not_found")
    );
}

#[test]
fn source_session_must_equal_row_even_when_wrapper_agrees_with_source() {
    let mut source = native();
    source[0]["payload"]["id"] = json!("another");
    let mut wrapper = normalized();
    wrapper[0]["session_id"] = json!("another");
    assert_eq!(
        run(&row(), &source, &wrapper).err().as_deref(),
        Some("session_mismatch")
    );
}

#[test]
fn prompt_timestamp_is_required_for_managed_birth_evidence() {
    let mut source = native();
    source[2].as_object_mut().unwrap().remove("timestamp");
    assert_eq!(
        run(&row(), &source, &normalized()).err().as_deref(),
        Some("timestamp_missing")
    );
}

#[test]
fn prompt_before_managed_birth_cannot_select_the_turn() {
    let mut source = native();
    source[2]["timestamp"] = json!("2000-01-01T00:00:00Z");
    assert_eq!(
        run(&row(), &source, &normalized()).err().as_deref(),
        Some("prompt_not_found")
    );
}

#[test]
fn pinned_native_turn_must_equal_prompt_context() {
    let mut state = row();
    state.native_turn_id = Some("another".into());
    assert_eq!(
        run(&state, &native(), &normalized()).err().as_deref(),
        Some("prompt_turn_mismatch")
    );
}

#[test]
fn composed_response_must_prefix_native_text() {
    let mut state = row();
    state.full_response = "different".into();
    assert_eq!(
        run(&state, &native(), &normalized()).err().as_deref(),
        Some("composed_prefix_mismatch")
    );
}

#[test]
fn two_complete_turns_with_same_prompt_are_ambiguous() {
    let mut source = native();
    source.extend([
        event("task_started", "next"),
        prompt("prompt"),
        text("old"),
        text("tail"),
        event("task_complete", "next"),
    ]);
    assert_eq!(
        run(&row(), &source, &normalized()).err().as_deref(),
        Some("ambiguous_prompt")
    );
}

#[test]
fn unfinished_foreign_opener_holds_before_its_prose() {
    let mut source = native();
    source.insert(4, event("task_started", "next"));
    assert_eq!(
        run(&row(), &source, &normalized()).err().as_deref(),
        Some("foreign_turn_opener")
    );
}

#[test]
fn recovery_prompt_matching_preserves_exact_suffix_and_envelope_grammar() {
    assert!(prompt_matches("hello", "hello", 77));
    assert!(prompt_matches(
        "hello",
        "[User: Alice (ops) team (ID: 77)] hello",
        77
    ));
    assert!(prompt_matches(
        "hello\nworld",
        "[User: Alice (ID: 77)]\nhello\nworld",
        77
    ));
    for candidate in [
        "HELLO",
        " hello",
        "hello ",
        "[User: Alice (ID: 077)] hello",
        "[User: Alice  team (ID: 77)] hello",
        "[User: Alice] (ID: 77)] hello",
        "prefix [User: Alice (ID: 77)] hello",
        "[User: Alice (ID: 77)]\nhello",
    ] {
        assert!(!prompt_matches("hello", candidate, 77), "{candidate}");
    }
}

#[test]
fn torn_or_malformed_wrapper_and_partial_source_are_held_without_repair() {
    let dir = tempfile::tempdir().unwrap();
    for bad in [
        b"{\"type\":\"assistant\"}".as_slice(),
        b"not-json\n".as_slice(),
    ] {
        let raw_path = dir.path().join("raw");
        let wrapper_path = dir.path().join("wrapper");
        std::fs::write(&raw_path, serde_json_lines(&native())).unwrap();
        std::fs::write(&wrapper_path, bad).unwrap();
        assert!(
            prove(
                &row(),
                File::open(&raw_path).unwrap(),
                File::open(&wrapper_path).unwrap()
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&wrapper_path).unwrap(), bad);
    }
    let mut source = serde_json_lines(&native());
    source.pop();
    let raw_path = dir.path().join("raw");
    let wrapper_path = dir.path().join("wrapper");
    std::fs::write(&raw_path, source).unwrap();
    std::fs::write(&wrapper_path, serde_json_lines(&normalized())).unwrap();
    assert_eq!(
        prove(
            &row(),
            File::open(raw_path).unwrap(),
            File::open(wrapper_path).unwrap()
        )
        .err()
        .as_deref(),
        Some("torn_tail")
    );
}

fn serde_json_lines(records: &[Value]) -> Vec<u8> {
    records
        .iter()
        .flat_map(|record| format!("{record}\n").into_bytes())
        .collect()
}

#[test]
fn incident_rollout_prefix_matches_recorded_wrapper_hash_in_actual_rust_path() {
    let Some(fixture) = std::env::var_os("AGENTDESK_PRODUCER_RESUME_INCIDENT_DIR") else {
        let plan = run(&row(), &native(), &normalized()).unwrap();
        assert_eq!(plan.replay, normalized());
        eprintln!(
            "REDUCED_FIXTURE_ONLY: set AGENTDESK_PRODUCER_RESUME_INCIDENT_DIR for recorded incident hash verification"
        );
        return;
    };
    use crate::services::agent_protocol::StreamMessage;
    use sha2::{Digest, Sha256};
    let fixture = std::path::PathBuf::from(fixture);
    let raw = std::fs::read(fixture.join("raw.jsonl")).unwrap();
    assert_eq!(raw.len(), 21_742_168);
    assert_eq!(
        format!("{:x}", Sha256::digest(&raw)),
        "8efcff444c2a5c114723243d13d09666f3486a5507866d617c5e6a2715b6b6f8"
    );
    let state: InflightTurnState =
        serde_json::from_slice(&std::fs::read(fixture.join("row.json")).unwrap()).unwrap();
    assert_eq!(state.full_response.len(), 34_650);
    assert_eq!(
        format!("{:x}", Sha256::digest(state.full_response.as_bytes())),
        "fea4b493e83e7c7d1e089553113e93a9b156dc4098c9a388b66163153287ae64"
    );
    assert_eq!(state.native_turn_id, None);
    assert_eq!((state.last_offset, state.turn_start_offset), (0, Some(0)));
    let mut decoder = RolloutRecordDecoder::default();
    let mut replay = Vec::new();
    for line in raw.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
        let record: Value = serde_json::from_slice(line).unwrap();
        for message in decoder.decode(&record).unwrap_or_default() {
            if let Some(json) =
                super::super::super::rebind_runtime::codex_rebind_stream_message_json(message)
            {
                replay.push(json);
                if replay.len() == 2162 {
                    break;
                }
            }
        }
        if replay.len() == 2162 {
            break;
        }
    }
    let wrapper = serde_json_lines(&replay);
    assert_eq!(wrapper.len(), 1_508_159);
    assert_eq!(
        format!("{:x}", Sha256::digest(&wrapper)),
        "3905981b80ae60bd1ba0fef2f0f991a9ac326f151ad4bbd1e4281bdbc3235b08"
    );
    let temp = tempfile::tempdir().unwrap();
    let wrapper_path = temp.path().join("wrapper.jsonl");
    std::fs::write(&wrapper_path, &wrapper).unwrap();
    let plan = prove(
        &state,
        File::open(fixture.join("raw.jsonl")).unwrap(),
        File::open(&wrapper_path).unwrap(),
    )
    .unwrap();
    assert_eq!(plan.replay.len(), 2162);
    let (sender, receiver) = std::sync::mpsc::channel();
    let own_turn = plan.own_turn.clone();
    let session = state.session_id.clone();
    let path = fixture.join("raw.jsonl");
    let tail = std::thread::spawn(move || {
        crate::services::codex_tui::rollout_tail::tail_fixed_recovery_rollout(
            plan.raw.into(),
            &path,
            plan.origin,
            session,
            own_turn,
            sender,
            None,
        )
    });
    let mut remaining = std::collections::VecDeque::from(plan.replay);
    let mut appended = Vec::new();
    let mut result = None;
    super::super::super::rebind_runtime::normalized_writer::write_stream(
        receiver,
        String::new(),
        Vec::new(),
        || Ok(()),
        |message, _| {
            if let StreamMessage::Done { result: body, .. } = &message {
                result = Some(body.clone());
            }
            let Some(json) =
                super::super::super::rebind_runtime::codex_rebind_stream_message_json(message)
            else {
                return Ok(false);
            };
            if let Some(expected) = remaining.pop_front() {
                assert_eq!(expected, json);
                return Ok(false);
            }
            appended.push(json);
            Ok(true)
        },
    )
    .unwrap();
    tail.join().unwrap().unwrap();
    assert!(remaining.is_empty());
    assert_eq!(appended.len(), 4348);
    assert_eq!(appended.last().unwrap()["type"], "result");
    let final_record: Value = serde_json::from_slice(
        raw.split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .nth(13252)
            .unwrap(),
    )
    .unwrap();
    let final_body = final_record
        .pointer("/payload/content/0/text")
        .and_then(Value::as_str)
        .unwrap();
    assert_eq!(final_body.len(), 699);
    let result = result.unwrap();
    assert_eq!(result.len(), 111_351);
    assert_eq!(
        format!("{:x}", Sha256::digest(result.as_bytes())),
        "0400b490a5abf22b880eee2d8e5020ad54e88db8ebffa7d2409cac9720a267e7"
    );
    assert!(result.starts_with(&state.full_response));
    assert!(result.contains(final_body));
    eprintln!(
        "ACTUAL_INCIDENT raw_bytes={} prefix_events=2162 appended_events={} final_bytes=699",
        raw.len(),
        appended.len()
    );
}
