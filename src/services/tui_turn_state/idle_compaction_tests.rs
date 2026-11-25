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

/// Record kinds and line lengths measured from a real idle compaction after a relay Stop, from the
/// completed reply on; every body here is synthetic filler.
const MEASURED: &str = "reply:3942 stop:855 system/stop_hook_summary:1083 system/turn_duration:471
    last-prompt:279 ai-title:115 mode:82 permission-mode:114 repl-mode:87 session-owner:186
    atis-latch:82 derived-from:184 data-flow-records:263 system/compact_boundary:1526
    data-flow-records:26268 summary:15629 @file:12360 @file:19582 @file:3508
    @compact_file_reference:1262 @file:3044 @invoked_skills:10740 @agent_listing_delta:20764
    @skill_listing:66752 @mcp_instructions_delta:20141 @environment:2654 @model:954 @language:1101
    start:917 system/informational:658 last-prompt:279 ai-title:115 mode:82 permission-mode:114
    repl-mode:87 session-owner:186 atis-latch:82 data-flow-records:192 cost-state:864";

/// `MEASURED` as records of `native`, each padded to its measured length.
pub(crate) fn measured_compaction_tail(native: &str) -> Vec<Value> {
    let quiet = r#"{"suppressOutput":true}"#;
    let hook = |event: &str, name: &str, id: &str| {
        json!({"type":"attachment","attachment":{"type":"hook_success","hookEvent":event,
            "hookName":name,"command":format!("claude-hook-relay --session-id {native}"),
            "stdout":quiet,"stderr":"","exitCode":0,"toolUseID":id}})
    };
    let notice = tail().remove(7);
    MEASURED
        .split_whitespace()
        .map(|entry| {
            let (kind, len) = entry.split_once(':').unwrap();
            let mut record = match kind.split_once('/') {
                _ if kind == "reply" => end_turn(),
                _ if kind == "stop" => hook("Stop", "Stop", "stop-hook"),
                _ if kind == "start" => hook("SessionStart", "SessionStart:compact", "start-hook"),
                _ if kind == "summary" => json!({"type":"user","isCompactSummary":true,
                    "message":{"role":"user","content":"summary"}}),
                Some(("system", "informational")) => notice.clone(),
                Some((_, subtype)) => json!({"type":"system","subtype":subtype,
                    "compactMetadata":{"trigger":"auto"},"toolUseID":"stop-hook",
                    "preventedContinuation":false,"hookErrors":[],"hookAdditionalContext":[]}),
                None if kind.starts_with('@') => {
                    json!({"type":"attachment","attachment":{"type":&kind[1..]}})
                }
                None => json!({"type":kind}),
            };
            record["sessionId"] = json!(native);
            record["timestamp"] = json!(chrono::Utc::now().to_rfc3339());
            let used = record.to_string().len() + r#","pad":"""#.len();
            record["pad"] = json!("x".repeat(len.parse::<usize>().unwrap().saturating_sub(used)));
            record
        })
        .collect()
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

#[test]
fn a_compaction_hides_the_terminal_beneath_it_until_its_notice_closes_it() {
    let mut records = vec![
        end_turn(),
        json!({"type":"system","subtype":"turn_duration"}),
    ];
    let mut closed = false;
    for record in compacted_tail() {
        closed |= record["subtype"] == "informational";
        // A session-state block would end the walk before the compaction; leave it out.
        if record["type"] != "permission-mode" {
            records.push(record);
        }
        let state = observe(records.clone());
        assert_eq!(
            state == TuiTurnState::Idle,
            closed,
            "{}: {state:?}",
            records.len()
        );
    }
}
