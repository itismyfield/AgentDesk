//! Claude's standard transcript walk, read past `Unknown` session markers only to a running tool;
//! a busy writer's walk also reaches a prompt or tool result still owed a reply.
use super::*;

/// A completed reply survives a non-conversation tail; old terminals across a session marker
/// still cannot prove Idle, while a pending tool call remains Streaming.
pub(super) fn observe(path: &Path) -> TuiTurnState {
    let state = walk(path, claude_envelope_turn_state, claude_partial_turn_state);
    if matches!(
        state,
        TuiTurnState::Unknown | TuiTurnState::Streaming | TuiTurnState::UserSubmitted
    ) && completed_tail(path)
    {
        return TuiTurnState::Idle;
    }
    if state != TuiTurnState::Unknown {
        return state;
    }
    match walk(path, past_marker_envelope, past_marker_partial) {
        TuiTurnState::Streaming => TuiTurnState::Streaming,
        _ => TuiTurnState::Unknown,
    }
}

fn completed_tail(path: &Path) -> bool {
    let Ok(window) = read_recent_jsonl_window(path, TURN_STATE_TAIL_BYTES) else {
        return false;
    };
    let mut metadata = false;
    for line in window
        .lines
        .iter()
        .rev()
        .filter(|line| !line.trim().is_empty())
    {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            return false;
        };
        if completed_reply(&record) {
            return metadata;
        }
        if !idle_metadata(&record) {
            return false;
        }
        metadata = true;
    }
    false
}

pub(crate) fn completed_reply(record: &Value) -> bool {
    record["type"] == "assistant"
        && record["message"]["stop_reason"] == "end_turn"
        && !requests_tool(record)
}

/// Only known non-conversation records may preserve a reply; user meta can itself start work.
/// Compact completion reads past the same records, so a new metadata type is added only here.
pub(crate) fn idle_metadata(record: &Value) -> bool {
    match record["type"].as_str() {
        Some(
            "file-history-snapshot"
            | "queue-operation"
            | "last-prompt"
            | "ai-title"
            | "mode"
            | "permission-mode"
            | "session-owner"
            | "atis-latch"
            | "cost-state"
            | "tools-used"
            | "pr-link"
            | "frame-link"
            | "repl-mode",
        ) => true,
        Some("system") => record["subtype"] == "informational",
        Some("attachment") => {
            let attachment = &record["attachment"];
            // Stop can persist added context before its next user record reaches the log.
            let continues = attachment["type"] == "hook_success"
                && attachment["hookEvent"] == "Stop"
                && attachment["stdout"]
                    .as_str()
                    .and_then(|stdout| serde_json::from_str::<Value>(stdout).ok())
                    .is_some_and(|output| {
                        output["hookSpecificOutput"]["hookEventName"] == "Stop"
                            && output["hookSpecificOutput"]["additionalContext"]
                                .as_str()
                                .is_some_and(|context| !context.is_empty())
                    });
            !continues
                && matches!(
                    attachment["type"].as_str(),
                    Some(
                        "agent_listing_delta"
                            | "skill_listing"
                            | "mcp_instructions_delta"
                            | "environment"
                            | "model"
                            | "language"
                            | "hook_success"
                            | "todo_reminder"
                            | "queued_command"
                    )
                )
        }
        _ => false,
    }
}

/// For a writer that adds input to a running turn: past the markers Claude writes after each
/// prompt, a typed prompt or tool result is also still running. Never a new Idle.
pub(super) fn observe_running(path: &Path) -> TuiTurnState {
    let state = walk(path, claude_envelope_turn_state, claude_partial_turn_state);
    if state != TuiTurnState::Unknown {
        return state;
    }
    match walk(path, past_marker_running, past_marker_partial) {
        state if state.is_busy() => state,
        _ => TuiTurnState::Unknown,
    }
}

/// `past_marker_envelope`, which also keeps a user record unless it is a local command, meta or
/// compaction summary, none of which starts a model reply.
fn past_marker_running(json: &Value) -> Option<TuiTurnState> {
    let reply_owed = || {
        let flag = |key| json.get(key).and_then(Value::as_bool) == Some(true);
        let text = json.pointer("/message/content").and_then(Value::as_str);
        !flag("isMeta")
            && !flag("isCompactSummary")
            && !text.is_some_and(|text| {
                text.starts_with("<local-command-") || text.starts_with("<command-")
            })
    };
    match claude_envelope_turn_state(json)? {
        TuiTurnState::UserSubmitted if reply_owed() => Some(TuiTurnState::UserSubmitted),
        _ => past_marker_envelope(json),
    }
}

fn walk(
    path: &Path,
    classify: fn(&Value) -> Option<TuiTurnState>,
    classify_partial: fn(&str) -> Option<TuiTurnState>,
) -> TuiTurnState {
    observe_jsonl_turn_state(
        path,
        classify,
        classify_partial,
        MalformedJsonlLinePolicy::FallbackToPrevious,
        false,
    )
}

/// Skips the markers; any other decisive record ends the walk as Unknown unless it requests a tool.
fn past_marker_envelope(json: &Value) -> Option<TuiTurnState> {
    match claude_envelope_turn_state(json)? {
        TuiTurnState::Unknown => None,
        TuiTurnState::Streaming if requests_tool(json) => Some(TuiTurnState::Streaming),
        _ => Some(TuiTurnState::Unknown),
    }
}

/// A torn line cannot show a tool call, so past the markers it only ends the walk.
fn past_marker_partial(line: &str) -> Option<TuiTurnState> {
    match claude_partial_turn_state(line)? {
        TuiTurnState::Unknown => None,
        _ => Some(TuiTurnState::Unknown),
    }
}

/// An assistant record with a `tool_use` block: the tool runs until its result record lands, while
/// a finished reply (`end_turn`) may still be waiting on its stop-hook terminator.
fn requests_tool(json: &Value) -> bool {
    json.pointer("/message/content")
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            blocks
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
        })
}

#[cfg(test)]
#[path = "claude_meta_tests.rs"]
mod tests;
