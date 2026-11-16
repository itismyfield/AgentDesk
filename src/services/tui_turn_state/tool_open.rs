//! Whether the transcript tail shows a tool call of the current turn still waiting for its result.
use super::*;
use std::collections::HashSet;

/// True when a root tool call in `path`'s tail window has no result yet. A call that started
/// before the window, or an unreadable file, reads as `false`.
pub(crate) fn provider_tool_call_open(provider: &ProviderKind, path: &Path) -> bool {
    let Ok(window) = read_recent_jsonl_window(path, TURN_STATE_TAIL_BYTES) else {
        return false;
    };
    let mut open = HashSet::new();
    for json in window
        .lines
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
    {
        match provider {
            ProviderKind::Claude => claude_step(&json, &mut open),
            ProviderKind::Codex => codex_step(&json, &mut open),
            _ => return false,
        }
    }
    !open.is_empty()
}

fn claude_step(json: &Value, open: &mut HashSet<String>) {
    if json.get("isSidechain") == Some(&Value::Bool(true)) || json.get("agentId").is_some() {
        return;
    }
    let blocks = json.pointer("/message/content").and_then(Value::as_array);
    let ids = |kind: &str, field: &str| -> Vec<String> {
        blocks
            .into_iter()
            .flatten()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some(kind))
            .filter_map(|block| block.get(field).and_then(Value::as_str).map(str::to_owned))
            .collect()
    };
    match json.get("type").and_then(Value::as_str) {
        Some("assistant") => open.extend(ids("tool_use", "id")),
        Some("user") => {
            let results = ids("tool_result", "tool_use_id");
            if results.is_empty() && json.get("isMeta") != Some(&Value::Bool(true)) {
                // A new prompt or an interrupt marker ends whatever the previous turn left open.
                open.clear();
            }
            for id in results {
                open.remove(&id);
            }
        }
        Some("result") => open.clear(),
        Some("system")
            if matches!(
                json.get("subtype").and_then(Value::as_str),
                Some("turn_duration" | "stop_hook_summary")
            ) =>
        {
            open.clear()
        }
        _ => {}
    }
}

fn codex_step(json: &Value, open: &mut HashSet<String>) {
    let Some(payload) = json.get("payload") else {
        return;
    };
    let kind = payload.get("type").and_then(Value::as_str);
    let call_id = payload
        .get("call_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match (json.get("type").and_then(Value::as_str), kind) {
        (
            Some("response_item"),
            Some("function_call" | "custom_tool_call" | "local_shell_call"),
        ) => open.extend(call_id),
        (Some("response_item"), Some("function_call_output" | "custom_tool_call_output")) => {
            if let Some(id) = call_id {
                open.remove(&id);
            }
        }
        (Some("event_msg"), Some("task_started" | "task_complete" | "turn_aborted")) => {
            open.clear()
        }
        _ => {}
    }
}
