//! Claude's standard transcript walk, read past `Unknown` session markers only to a running tool.
use super::*;

/// The standard walk; when it stops at an `Unknown` marker (`permission-mode`), a second walk past
/// such markers may report Streaming for a pending tool call and nothing else, so never a new Idle.
pub(super) fn observe(path: &Path) -> TuiTurnState {
    let state = walk(path, claude_envelope_turn_state, claude_partial_turn_state);
    if state != TuiTurnState::Unknown {
        return state;
    }
    match walk(path, past_marker_envelope, past_marker_partial) {
        TuiTurnState::Streaming => TuiTurnState::Streaming,
        _ => TuiTurnState::Unknown,
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
