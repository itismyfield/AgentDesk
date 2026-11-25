use super::*;
use serde_json::json;

pub(crate) fn end_turn() -> Value {
    json!({"type":"assistant","uuid":uuid::Uuid::new_v4().to_string(),"apiBlockIndex":0,
        "message":{"id":"idle-completion","role":"assistant","stop_reason":"end_turn",
        "content":[{"type":"text","text":"Finished."}]}})
}

pub(crate) fn tail() -> Vec<Value> {
    let mut records: Vec<_> = [
        "agent_listing_delta",
        "skill_listing",
        "mcp_instructions_delta",
        "environment",
        "model",
        "language",
        "hook_success",
    ]
    .into_iter()
    .map(|kind| {
        json!({"type":"attachment","uuid":uuid::Uuid::new_v4().to_string(),"timestamp":chrono::Utc::now(),
            "attachment":{"type":kind}})
    })
    .collect();
    records.push(json!({"type":"system","subtype":"informational","level":"notice",
        "content":"Compacted while idle, before the prompt cache expired · turn off with \"idleCompaction\": false in settings.json"}));
    for kind in [
        "last-prompt",
        "ai-title",
        "mode",
        "permission-mode",
        "session-owner",
        "atis-latch",
        "repl-mode",
    ] {
        records.push(json!({"type":kind}));
    }
    records
}

/// A synthetic idle compaction: boundary, summary and re-attachments, then `tail`.
pub(crate) fn compacted_tail() -> Vec<Value> {
    let mut records = vec![
        json!({"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":"auto"}}),
        json!({"type":"user","isCompactSummary":true,"message":{"role":"user","content":"summary"}}),
        json!({"type":"derived-from"}),
    ];
    for kind in ["file", "compact_file_reference", "invoked_skills"] {
        records.push(json!({"type":"attachment","attachment":{"type":kind}}));
    }
    records.push(json!({"type":"attachment","attachment":{"type":"hook_success","hookEvent":"SessionStart"}}));
    records.push(json!({"type":"data-flow-records"}));
    records.extend(tail());
    records
}

fn observe(records: Vec<Value>) -> TuiTurnState {
    let file = tempfile::NamedTempFile::new().unwrap();
    let text = records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(file.path(), text).unwrap();
    observe_claude_jsonl_turn_state(file.path())
}

#[test]
fn idle_compaction_tail_preserves_the_completed_reply() {
    let mut records = vec![end_turn()];
    records.extend(tail());
    assert_eq!(observe(records), TuiTurnState::Idle);
}

#[test]
fn idle_compaction_metadata_does_not_finish_running_or_compacting_turns() {
    for active in [
        json!({"type":"assistant","message":{"stop_reason":null,"content":[{"type":"text","text":"Working"}]}}),
        json!({"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","id":"tool","name":"Read","input":{}}]}}),
        json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool","content":"done"}]}}),
        json!({"type":"user","message":{"content":"next prompt"}}),
        json!({"type":"progress","data":{"type":"hook_progress"}}),
        json!({"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":"auto"}}),
        json!({"type":"user","isCompactSummary":true,"message":{"content":"summary"}}),
        json!({"type":"user","isMeta":true,"origin":{"kind":"task-notification"},"message":{"content":"new task"}}),
        json!({"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"tool_use","id":"tool","name":"Read","input":{}}]}}),
    ] {
        for records in [
            [vec![end_turn(), active.clone()], tail()].concat(),
            [vec![end_turn(), active.clone()], compacted_tail()].concat(),
            [vec![end_turn()], compacted_tail(), vec![active.clone()]].concat(),
        ] {
            assert_ne!(observe(records), TuiTurnState::Idle, "{active}");
        }
    }
}

#[test]
fn idle_compaction_tail_does_not_cross_an_old_terminal_or_unknown_record() {
    for barrier in [
        json!({"type":"result","result":"old result"}),
        json!({"type":"system","subtype":"turn_duration"}),
        json!({"type":"system","subtype":"stop_hook_summary"}),
        json!({"type":"future-record"}),
        json!({"type":"attachment","attachment":{"type":"future-attachment"}}),
    ] {
        let mut records = vec![end_turn(), barrier.clone()];
        records.extend(tail());
        assert_ne!(observe(records), TuiTurnState::Idle, "{barrier}");
    }
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        file.path(),
        format!("{}\n{{\"type\":\"permission-mode\",", end_turn()),
    )
    .unwrap();
    assert_ne!(
        observe_claude_jsonl_turn_state(file.path()),
        TuiTurnState::Idle
    );
}
