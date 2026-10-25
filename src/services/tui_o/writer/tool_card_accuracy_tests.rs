use super::super::pieces::UnitDeriver;
use super::*;
use crate::services::tui_o::shadow::CapturedRecord;
use serde_json::json;

#[test]
fn accuracy_codex_rollout_code_mode_and_spawn_launch_card() {
    let mut derive = UnitDeriver::new(7, ShadowProvider::Codex).with_tool_cards(ToolCardInputs {
        known: BTreeSet::new(),
        recorded_cards: BTreeSet::new(),
        switch: Arc::new(|| true),
    });
    let mut work = Vec::new();
    for value in [
        json!({"type":"response_item","timestamp":"2026-10-18T10:00:00Z","payload":{"type":"custom_tool_call","id":"native-js","call_id":"js","name":"exec","input":"text(await tools.exec_command({cmd:'cargo check'}));"}}),
        json!({"type":"response_item","payload":{"type":"function_call","id":"native-spawn","call_id":"spawn","name":"spawn_agent","arguments":"{\"message\":\"PRIVATE_PROMPT\"}"}}),
        json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"spawn","output":"{\"agent_id\":\"agent-1\"}","is_error":false}}),
        json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1"}}),
    ] {
        work.extend(derive.derive(&CapturedRecord {
            start: 0,
            end: 1,
            line: serde_json::to_vec(&value).unwrap(),
        }));
    }
    let cards: Vec<_> = work
        .iter()
        .filter_map(|w| match w {
            Derived::Piece(p) => Some(p.payload.as_str()),
            _ => None,
        })
        .collect();
    let out = cards.join("\n");
    assert!(
        out.contains("cargo check") && out.contains("Task") && !out.contains("PRIVATE_PROMPT"),
        "{out}"
    );
    assert!(!out.contains("Task  ✓"), "{out}");
}
