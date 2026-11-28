//! Idle-detector reads of a transcript tail, widened until a whole record or a turn boundary is in
//! view so one oversized record never hides the progress behind it.
use super::*;
use std::collections::HashSet;

/// The widest tail these reads load before calling the answer unknown.
const MAX_PROGRESS_TAIL_BYTES: u64 = 16 * 1024 * 1024;

/// The current turn's root tool calls as far as the transcript tail shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCalls {
    /// A call has no result yet.
    Open,
    /// Every call since the turn boundary has its result.
    Closed,
    /// No turn boundary fits in the widest tail, or the file could not be read.
    Inconclusive,
}

/// Unix nanos of the newest stamped conversation record, also when that record is longer than
/// the standard tail window.
pub fn latest_provider_record_unix_nanos_widening(
    provider: &ProviderKind,
    path: &Path,
) -> Option<i64> {
    widen(path, |window| {
        let found = latest_record_in(provider, &window.lines);
        (found.is_some() || window.window_covers_file).then_some(found)
    })
    .flatten()
}

/// Whether the current turn has a root tool call still waiting for its result.
pub fn provider_tool_calls(provider: &ProviderKind, path: &Path) -> ToolCalls {
    let step = match provider {
        ProviderKind::Claude => claude_step,
        ProviderKind::Codex => codex_step,
        _ => return ToolCalls::Inconclusive,
    };
    widen(path, |window| {
        // A last record still being written (or cut off) cannot show whether a call is open.
        let last = window
            .lines
            .iter()
            .rev()
            .find(|line| !line.trim().is_empty());
        if last.is_some_and(|line| serde_json::from_str::<Value>(line).is_err()) {
            return Some(ToolCalls::Inconclusive);
        }
        let mut open = HashSet::new();
        let mut bounded = window.window_covers_file;
        for json in window
            .lines
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        {
            bounded |= step(&json, &mut open);
        }
        // Without a boundary a call may have started before the window.
        bounded.then(|| match open.is_empty() {
            true => ToolCalls::Closed,
            false => ToolCalls::Open,
        })
    })
    .unwrap_or(ToolCalls::Inconclusive)
}

/// Re-reads a growing tail until `judge` can answer; `None` past the widest tail or on a read error.
fn widen<T>(path: &Path, mut judge: impl FnMut(&JsonlTailWindow) -> Option<T>) -> Option<T> {
    let mut bytes = TURN_STATE_TAIL_BYTES;
    loop {
        let window = read_recent_jsonl_window(path, bytes).ok()?;
        if let Some(found) = judge(&window) {
            return Some(found);
        }
        if bytes >= MAX_PROGRESS_TAIL_BYTES {
            return None;
        }
        bytes = (bytes * 4).min(MAX_PROGRESS_TAIL_BYTES);
    }
}

/// Folds one Claude record into the open calls; true when the record is a turn boundary.
fn claude_step(json: &Value, open: &mut HashSet<String>) -> bool {
    if json.get("isSidechain") == Some(&Value::Bool(true)) || json.get("agentId").is_some() {
        return false;
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
    let boundary = match json.get("type").and_then(Value::as_str) {
        Some("assistant") => {
            open.extend(ids("tool_use", "id"));
            false
        }
        Some("user") => {
            let results = ids("tool_result", "tool_use_id");
            for id in &results {
                open.remove(id);
            }
            // A new prompt or an interrupt marker ends whatever the previous turn left open.
            results.is_empty() && json.get("isMeta") != Some(&Value::Bool(true))
        }
        Some("result") => true,
        Some("system") => matches!(
            json.get("subtype").and_then(Value::as_str),
            Some("turn_duration" | "stop_hook_summary")
        ),
        _ => false,
    };
    if boundary {
        open.clear();
    }
    boundary
}

/// Folds one Codex rollout record into the open calls; true when the record is a turn boundary.
fn codex_step(json: &Value, open: &mut HashSet<String>) -> bool {
    let Some(payload) = json.get("payload") else {
        return false;
    };
    let kind = payload.get("type").and_then(Value::as_str);
    let call_id = payload
        .get("call_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match (json.get("type").and_then(Value::as_str), kind) {
        (
            Some("response_item"),
            Some("function_call" | "custom_tool_call" | "local_shell_call" | "tool_search_call"),
        ) => open.extend(call_id),
        (
            Some("response_item"),
            Some("function_call_output" | "custom_tool_call_output" | "tool_search_output"),
        ) => {
            if let Some(id) = call_id {
                open.remove(&id);
            }
        }
        (Some("event_msg"), Some("task_started" | "task_complete" | "turn_aborted")) => {
            open.clear();
            return true;
        }
        _ => {}
    }
    false
}
