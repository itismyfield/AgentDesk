use super::*;
use serde_json::json;
use std::path::PathBuf;

#[path = "history_tests.rs"]
mod history_tests;

fn record(uuid: &str, text: &str) -> CapturedRecord {
    let line = serde_json::to_vec(&json!({"type":"user", "uuid":uuid,
        "promptId":"shared-hook-id", "message":{"role":"user","content":text}}))
    .unwrap();
    CapturedRecord {
        start: 10,
        end: 11 + line.len() as u64,
        line,
    }
}

fn origin(record: &CapturedRecord) -> NoticeOrigin {
    NoticeOrigin {
        record: SourceRange {
            source: crate::services::tui_o::shadow::SourceId {
                session_id: "native-session".into(),
                path: PathBuf::from("/fixture/input.jsonl"),
                dev: 1,
                ino: 2,
            },
            start: record.start,
            end: record.end,
        },
        logical_session: "notice-test-pane".into(),
        binding_seq: 3,
        execution_nonce: "execution".into(),
    }
}

fn project(projection: &mut Projection, row: &CapturedRecord) -> Result<Option<PieceWork>, String> {
    projection.project(7332, ShadowProvider::Claude, row, Ok(origin(row)))
}

#[test]
fn native_uuid_separates_identical_inputs_and_rejects_changed_identity() {
    let mut projection = Projection::for_tests();
    let first = record("native-one", "계속");
    let piece = project(&mut projection, &first)
        .unwrap()
        .expect("native input must project");
    assert_eq!(piece.unit_key.native_key, "native-one");
    assert_eq!(piece.unit_key.kind, UnitKind::InputNotice);
    assert_eq!(piece.notice_origin, Some(Box::new(origin(&first))));
    assert!(piece.payload.starts_with("-# ⌨️ "));
    assert!(project(&mut projection, &first).unwrap().is_none());
    let mut metadata: serde_json::Value = serde_json::from_slice(&first.line).unwrap();
    metadata["promptId"] = json!("rewritten-hook-id");
    metadata["sessionId"] = json!("forked-native-session");
    let line = serde_json::to_vec(&metadata).unwrap();
    let rewritten = CapturedRecord {
        start: first.start,
        end: first.start + line.len() as u64 + 1,
        line,
    };
    assert!(project(&mut projection, &rewritten).unwrap().is_none());
    let second = project(&mut projection, &record("native-two", "계속"))
        .unwrap()
        .unwrap();
    assert_eq!(second.payload, piece.payload);
    assert_ne!(second.unit_key, piece.unit_key);
    assert!(project(&mut projection, &record("native-one", "changed")).is_err());
    let prefix = "x".repeat(3000);
    let before = record("long-uuid", &format!("{prefix}a"));
    let after = record("long-uuid", &format!("{prefix}b"));
    assert_eq!(
        discord::input_notice_content(&format!("{prefix}a")),
        discord::input_notice_content(&format!("{prefix}b"))
    );
    assert!(project(&mut projection, &before).unwrap().is_some());
    assert!(project(&mut projection, &after).is_err());

    let (x, y) = ("x".repeat(1990), "y".repeat(1990));
    let before = format!("{x}\n{y}z");
    let after = format!("{x}{y}\nz");
    assert_ne!(before.as_bytes(), after.as_bytes());
    assert_eq!(
        discord::input_notice_content(&before),
        discord::input_notice_content(&after)
    );
    let display = |text: &str| {
        crate::services::tui_o::shadow::unit_plan::plan(
            &crate::services::tui_o::shadow::identity::UnitContent::Payload(text.into()),
        )
        .unwrap()
    };
    assert_eq!(
        display(&before),
        display(&after),
        "splitter hides moved newline"
    );
    let first = record("newline-uuid", &before);
    assert!(project(&mut projection, &first).unwrap().is_some());
    assert!(project(&mut projection, &first).unwrap().is_none());
    assert!(
        project(&mut projection, &record("newline-uuid", &after)).is_err(),
        "native UUID must seal the exact extracted prompt bytes"
    );
    assert!(
        project(&mut projection, &record("newline-other-uuid", &after))
            .unwrap()
            .is_some()
    );
}

#[test]
fn native_record_and_shared_source_policy_exclude_machine_inputs() {
    for text in [
        "[User: tester (ID: 123)] from Discord",
        "<command-name>/clear</command-name>",
        "<local-command-stdout>done</local-command-stdout>",
        "<task-notification>done</task-notification>",
        "<subagent_notification>done</subagent_notification>",
        "This session is being continued from a previous conversation",
    ] {
        assert!(
            project(&mut Projection::for_tests(), &record("machine", text))
                .unwrap()
                .is_none(),
            "{text}"
        );
    }
    let human = record("human", "I quoted <task-notification> in my question");
    assert!(
        project(&mut Projection::for_tests(), &human)
            .unwrap()
            .is_some()
    );
    for value in [
        json!({"type":"queue-operation","operation":"enqueue","content":"go"}),
        json!({"type":"user","uuid":"meta","isMeta":true,"message":{"content":"meta"}}),
        json!({"type":"user","uuid":"tool","message":{"content":[{"type":"tool_result","tool_use_id":"tool","content":"result"}]}}),
    ] {
        let line = serde_json::to_vec(&value).unwrap();
        let row = CapturedRecord {
            start: 0,
            end: line.len() as u64 + 1,
            line,
        };
        assert!(
            project(&mut Projection::for_tests(), &row)
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn missing_identity_or_original_binding_remains_unresolved() {
    let mut row = record("", "direct input");
    assert!(project(&mut Projection::for_tests(), &row).is_err());
    row = record("known", "direct input");
    assert!(
        Projection::for_tests()
            .project(
                7332,
                ShadowProvider::Claude,
                &row,
                Err("binding unreadable".into())
            )
            .is_err()
    );
    let mut wrong = origin(&row);
    wrong.record.end += 1;
    assert!(
        Projection::for_tests()
            .project(7332, ShadowProvider::Claude, &row, Ok(wrong))
            .is_err()
    );
    assert!(
        Projection::for_tests()
            .project(7332, ShadowProvider::Codex, &row, Ok(origin(&row)))
            .unwrap()
            .is_none()
    );
}

#[test]
fn disabled_projection_never_creates_a_notice() {
    let row = record("ready", "go");
    assert!(!Projection::default().enabled());
    assert!(project(&mut Projection::default(), &row).unwrap().is_none());
}
