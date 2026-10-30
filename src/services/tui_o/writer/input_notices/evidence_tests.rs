use super::*;
use crate::services::tui_o::shadow::{SourceId, SourceRange};
use crate::services::tui_o::writer::input_notices::{NoticeOrigin, Projection};
use sha2::Digest;

fn native(uuid: &str) -> (CapturedRecord, NoticeOrigin) {
    let line = serde_json::to_vec(&serde_json::json!({"type":"user","uuid":uuid,
        "message":{"role":"user","content":"terminal input"}}))
    .unwrap();
    let row = CapturedRecord {
        start: 0,
        end: line.len() as u64 + 1,
        line,
    };
    let origin = NoticeOrigin {
        record: SourceRange {
            source: SourceId {
                session_id: "native".into(),
                path: "/fixture/source".into(),
                dev: 1,
                ino: 2,
            },
            start: 0,
            end: row.end,
        },
        logical_session: "notice-evidence".into(),
        binding_seq: 1,
        execution_nonce: "original".into(),
    };
    (row, origin)
}

#[test]
fn failed_notice_identity_is_retained_without_blocking_the_body_deriver() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (row, origin) = native("");
    let mut deriver =
        UnitDeriver::new(7, ShadowProvider::Claude).with_input_notices(Projection::for_tests());
    let source = origin.record.source.clone();
    let output = deriver.derive_scoped(
        &row,
        &source,
        crate::services::tui_o::shadow::IDENTITY_VERSION,
        Ok(origin),
    );
    assert!(
        !output
            .iter()
            .any(|item| matches!(item, Derived::Blocked { .. })),
        "notice uncertainty is not a global schema failure"
    );
    let retained = deriver.take_notice_evidence();
    assert_eq!(
        retained.len(),
        1,
        "unknown native identity must remain auditable"
    );
    assert_eq!(retained[0].raw_record, row.line);
    assert!(retained[0].unit_key.is_none());
}

#[test]
fn notice_projection_retains_canonical_record_before_raw_can_be_released() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (row, origin) = native("native-one");
    let mut deriver =
        UnitDeriver::new(7, ShadowProvider::Claude).with_input_notices(Projection::for_tests());
    let source = origin.record.source.clone();
    let output = deriver.derive_scoped(
        &row,
        &source,
        crate::services::tui_o::shadow::IDENTITY_VERSION,
        Ok(origin.clone()),
    );
    assert!(
        output
            .iter()
            .any(|item| matches!(item,Derived::Piece(p) if p.unit_key.kind==UnitKind::InputNotice))
    );
    let retained = deriver.take_notice_evidence();
    assert_eq!(
        retained.len(),
        1,
        "a derived notice must carry its complete native evidence"
    );
    assert_eq!(retained[0].raw_record, row.line);
    assert_eq!(retained[0].origin.as_deref(), Some(&origin));
    assert_eq!(retained[0].prompt.as_deref(), Some("terminal input"));
    assert_eq!(
        retained[0].prompt_sha256.as_deref(),
        Some(hex::encode(sha2::Sha256::digest(b"terminal input")).as_str())
    );
    assert!(deriver.take_notice_evidence().is_empty());
}

#[test]
fn notice_does_not_change_body_tool_card_or_turn_summary_identity() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let configured = || {
        UnitDeriver::new(7, ShadowProvider::Claude)
            .with_tool_cards(ToolCardInputs {
                known: Default::default(),
                recorded_cards: Default::default(),
                switch: std::sync::Arc::new(|| true),
            })
            .with_turn_summary(SummaryInputs {
                turn_marks: Default::default(),
                disposed: Default::default(),
                context_window: Some(1000),
            })
    };
    let mut control = configured();
    let mut enabled = configured().with_input_notices(Projection::for_tests());
    let values = [
        serde_json::json!({"type":"user","uuid":"input","timestamp":"2026-10-19T00:00:00Z","message":{"role":"user","content":"request"}}),
        serde_json::json!({"type":"assistant","uuid":"tool","apiBlockIndex":0,"timestamp":"2026-10-19T00:00:01Z","message":{"id":"tool","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"pwd"}}]}}),
        serde_json::json!({"type":"user","uuid":"result","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"/fixture"}]}}),
        serde_json::json!({"type":"assistant","uuid":"answer","apiBlockIndex":0,"timestamp":"2026-10-19T00:00:02Z","message":{"id":"answer","usage":{"input_tokens":100},"content":[{"type":"text","text":"answer"}]}}),
        serde_json::json!({"type":"system","subtype":"stop_hook_summary","timestamp":"2026-10-19T00:00:10Z"}),
    ];
    let (_, template) = native("template");
    let source = template.record.source.clone();
    let (mut normal, mut projected, mut start) = (Vec::new(), Vec::new(), 0);
    for value in values {
        let line = serde_json::to_vec(&value).unwrap();
        let record = CapturedRecord {
            start,
            end: start + line.len() as u64 + 1,
            line,
        };
        let origin = NoticeOrigin {
            record: SourceRange {
                source: source.clone(),
                start,
                end: record.end,
            },
            ..template.clone()
        };
        normal.extend(control.derive_scoped(
            &record,
            &source,
            crate::services::tui_o::shadow::IDENTITY_VERSION,
            Ok(origin.clone()),
        ));
        projected.extend(enabled.derive_scoped(
            &record,
            &source,
            crate::services::tui_o::shadow::IDENTITY_VERSION,
            Ok(origin),
        ));
        start = record.end;
    }
    assert_eq!(
        projected
            .iter()
            .filter(|d| matches!(d,Derived::Piece(p) if p.unit_key.kind==UnitKind::InputNotice))
            .count(),
        1
    );
    projected.retain(|d| !matches!(d,Derived::Piece(p) if p.unit_key.kind==UnitKind::InputNotice));
    assert_eq!(
        projected, normal,
        "notices must not become the first Body/Tool or shift their identities"
    );
    assert!(
        normal
            .iter()
            .any(|d| matches!(d,Derived::Piece(p) if p.unit_key.native_key.ends_with("#turn_end")))
    );
    assert!(
        normal
            .iter()
            .any(|d| matches!(d,Derived::Piece(p) if p.unit_key.kind==UnitKind::Body))
    );
    assert_eq!(enabled.take_notice_evidence().len(), 1);
}
