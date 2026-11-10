use super::*;
use crate::services::tui_o::shadow::SourceId;
use serde_json::{Value, json};

const CHANNEL: u64 = 8063;

fn record(value: &Value) -> CapturedRecord {
    let line = serde_json::to_vec(value).unwrap();
    CapturedRecord {
        start: 0,
        end: line.len() as u64 + 1,
        line,
    }
}

fn question(id: &str) -> Value {
    json!({"type":"event_msg", "payload":{"type":"item_completed",
        "item":{"type":"AgentMessage", "id":id, "phase":"final_answer",
            "delivery":"async", "questions":[{"title":"Which scope?", "options":["One","Two"]}]}}})
}

fn announcement(id: &str) -> Value {
    json!({"type":"event_msg", "payload":{"type":"item_completed",
        "item":{"type":"AgentMessage", "id":id}}})
}

fn body(id: &str, text: &str) -> Value {
    json!({"type":"response_item", "payload":{"type":"message",
        "role":"assistant", "id":id, "content":[{"type":"output_text", "text":text}]}})
}

fn key(id: &str, kind: UnitKind) -> UnitKey {
    UnitKey {
        channel_id: CHANNEL,
        provider: ShadowProvider::Codex,
        native_key: id.into(),
        kind,
    }
}

fn body_piece(id: &str, text: &str) -> Derived {
    Derived::Piece(PieceWork {
        unit_key: key(id, UnitKind::Body),
        index: 0,
        payload: text.into(),
        output_origin: None,
        notice_origin: None,
    })
}

fn derive(deriver: &mut UnitDeriver, value: &Value, version: u32) -> Vec<Derived> {
    deriver.derive_with_version(&record(value), version)
}

#[test]
fn async_question_does_not_leave_an_unsealed_body_in_old_spool_versions() {
    for version in 2..=7 {
        for id in ["call_question", "native-question-without-call-prefix"] {
            let value = question(id);
            assert_eq!(
                classify_with_version(ShadowProvider::Codex, &value, version),
                [RecordFact::Announced(id.into(), UnitKind::Body)],
                "public classifier v{version}"
            );
            let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Codex);
            assert!(derive(&mut deriver, &value, version).is_empty());
            assert!(
                !deriver.has_unsealed(),
                "a completed async question has no later Body record: v{version} {id}"
            );
        }
    }
}

#[test]
fn async_question_preserves_units_exclusions_and_fork_copies_in_old_versions() {
    for version in 2..=7 {
        let call = json!({"type":"response_item", "payload":{
            "type":"function_call", "id":"fc_question", "call_id":"call_question",
            "name":"request_user_input_async", "arguments":"{\"questions\":[]}" }});
        let output = json!({"type":"response_item", "payload":{
            "type":"function_call_output", "call_id":"call_question", "output":"{\"accepted\":true}" }});
        let rows = [
            call,
            question("call_question"),
            output,
            announcement("answer"),
            body("answer", "A visible answer."),
        ];
        let mut with_question = UnitDeriver::new(CHANNEL, ShadowProvider::Codex);
        let actual: Vec<_> = rows
            .iter()
            .flat_map(|value| derive(&mut with_question, value, version))
            .collect();
        let expected = vec![
            Derived::Excluded {
                unit_key: key("fc_question", UnitKind::Tool),
                reason: TOOL_CALL_PANEL.into(),
            },
            Derived::Excluded {
                unit_key: key("call_question", UnitKind::ToolResult),
                reason: TOOL_RESULT_INTERNAL.into(),
            },
            body_piece("answer", "A visible answer."),
        ];
        assert_eq!(
            actual, expected,
            "explicit payload/key dispositions v{version}"
        );
        let mut without_question = UnitDeriver::new(CHANNEL, ShadowProvider::Codex);
        let baseline: Vec<_> = rows
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != 1)
            .flat_map(|(_, value)| derive(&mut without_question, value, version))
            .collect();
        assert_eq!(actual, baseline, "question changes no output v{version}");
        assert!(!with_question.has_unsealed(), "v{version}");
        for value in &rows {
            assert!(
                derive(&mut with_question, value, version).is_empty(),
                "a fork copy must not derive output twice: v{version}"
            );
        }
        let changed = derive(
            &mut with_question,
            &body("answer", "A changed answer."),
            version,
        );
        assert!(
            matches!(changed.as_slice(), [Derived::Blocked { .. }]),
            "the ordinary immutable-body conflict remains visible: v{version}"
        );
    }
}

#[test]
fn async_question_does_not_hide_a_real_announcement_from_a_later_source() {
    let old = SourceId {
        session_id: "old".into(),
        path: "/fixture/old.jsonl".into(),
        dev: 1,
        ino: 1,
    };
    let fresh = SourceId {
        session_id: "fresh".into(),
        path: "/fixture/fresh.jsonl".into(),
        dev: 1,
        ino: 2,
    };
    for version in 2..=7 {
        let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Codex);
        deriver.enter_source(&old);
        assert!(derive(&mut deriver, &question("question"), version).is_empty());
        assert!(!deriver.has_unsealed(), "question only v{version}");
        deriver.enter_source(&fresh);
        assert!(derive(&mut deriver, &announcement("real"), version).is_empty());
        assert!(deriver.has_unsealed(), "real body remains owed v{version}");
        assert_eq!(
            derive(&mut deriver, &body("real", "The real reply."), version),
            [body_piece("real", "The real reply.")],
        );
        assert!(!deriver.has_unsealed(), "the real record seals v{version}");
    }
}

#[test]
fn async_question_shape_filter_keeps_text_and_malformed_announcements_unsealed() {
    let edits = [
        ("sync delivery", json!({"delivery":"sync"})),
        ("missing delivery value", json!({"delivery":null})),
        ("empty questions", json!({"questions":[]})),
        ("null questions", json!({"questions":null})),
        ("non-array questions", json!({"questions":"Which scope?"})),
        ("missing title", json!({"questions":[{}]})),
        ("non-string title", json!({"questions":[{"title":3}]})),
        (
            "invalid second question",
            json!({"questions":[{"title":"First"},{}]}),
        ),
        ("text present", json!({"text":"A real reply."})),
        ("empty text present", json!({"text":""})),
        ("null text present", json!({"text":null})),
        (
            "content present",
            json!({"content":[{"type":"output_text","text":"Reply"}]}),
        ),
        ("empty content present", json!({"content":[]})),
        ("null content present", json!({"content":null})),
        ("empty native id", json!({"id":""})),
    ];
    let mut cases = Vec::new();
    for (name, edit) in edits {
        let mut value = question("pending");
        for (field, replacement) in edit.as_object().unwrap() {
            value["payload"]["item"][field] = replacement.clone();
        }
        cases.push((name, value));
    }
    for (name, field) in [
        ("delivery field absent", "delivery"),
        ("questions field absent", "questions"),
    ] {
        let mut value = question("pending");
        value["payload"]["item"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        cases.push((name, value));
    }
    cases.push(("ordinary body announcement", announcement("pending")));
    for version in 2..=7 {
        for (name, value) in &cases {
            assert!(
                matches!(
                    classify_with_version(ShadowProvider::Codex, value, version).as_slice(),
                    [RecordFact::Announced(_, UnitKind::Body)]
                ),
                "{name}: public fact remains announced v{version}"
            );
            let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Codex);
            assert!(derive(&mut deriver, value, version).is_empty());
            assert!(
                deriver.has_unsealed(),
                "{name}: only the observed question UI is exempt v{version}"
            );
        }
    }
}

#[test]
fn async_question_does_not_seal_a_later_body_with_the_same_native_id() {
    for version in 2..=7 {
        let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Codex);
        assert!(derive(&mut deriver, &question("shared"), version).is_empty());
        assert!(!deriver.has_unsealed(), "question itself v{version}");
        assert!(derive(&mut deriver, &announcement("shared"), version).is_empty());
        assert!(
            deriver.has_unsealed(),
            "a later normal announcement v{version}"
        );
        assert_eq!(
            derive(&mut deriver, &body("shared", "Still visible."), version),
            [body_piece("shared", "Still visible.")],
        );
        assert!(
            !deriver.has_unsealed(),
            "normal seal still works v{version}"
        );
    }
}
