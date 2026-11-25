//! Claude's standard transcript walk, read past `Unknown` session markers only to a running tool;
//! a busy writer's walk also reaches a prompt or tool result still owed a reply.
use super::*;

/// A completed reply survives a non-conversation tail or finished idle compaction; old terminals
/// across a session marker still cannot prove Idle, while a pending tool call remains Streaming.
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
    // Whether metadata followed the newest completed reply; `None` once anything else did.
    let (mut after, mut segment) = (None, IdleCompaction::default());
    for line in window.lines.iter().filter(|line| !line.trim().is_empty()) {
        let record = serde_json::from_str::<Value>(line).ok();
        if record.as_ref().is_some_and(completed_reply) {
            (after, segment) = (Some(false), IdleCompaction::default());
            continue;
        }
        let keeps = record.is_some_and(|record| match segment.step(&record) {
            CompactionStep::Outside => idle_metadata(&record),
            CompactionStep::Broken => false,
            CompactionStep::Open | CompactionStep::Inside | CompactionStep::Closed => true,
        });
        after = after.filter(|_| keeps).map(|_| true);
    }
    after == Some(true) && segment == IdleCompaction::Outside
}

/// A record's place in an idle compaction read forward: its boundary opens it, and only its
/// summary and idle metadata may follow until the idle-compaction notice closes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CompactionStep {
    Outside,
    Open,
    Inside,
    Closed,
    Broken,
}

/// Both Claude readers share this, so a compaction no notice closed stays conversation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum IdleCompaction {
    #[default]
    Outside,
    Boundary,
    Summary,
}

impl IdleCompaction {
    pub(crate) fn step(&mut self, record: &Value) -> CompactionStep {
        let (next, step) = match *self {
            Self::Outside
                if record["type"] == "system" && record["subtype"] == "compact_boundary" =>
            {
                (Self::Boundary, CompactionStep::Open)
            }
            Self::Outside => (Self::Outside, CompactionStep::Outside),
            Self::Boundary if record["isCompactSummary"] == true => {
                (Self::Summary, CompactionStep::Inside)
            }
            Self::Summary if idle_compaction_notice(record) => {
                (Self::Outside, CompactionStep::Closed)
            }
            open if record["type"] != "permission-mode" && idle_metadata(record) => {
                (open, CompactionStep::Inside)
            }
            _ => (Self::Outside, CompactionStep::Broken),
        };
        *self = next;
        step
    }
}

/// Claude appends this notice only after an idle or away compaction has finished.
fn idle_compaction_notice(record: &Value) -> bool {
    record["type"] == "system"
        && record["subtype"] == "informational"
        && record["content"]
            .as_str()
            .is_some_and(|content| content.contains("\"idleCompaction\""))
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
            | "repl-mode"
            | "data-flow-records"
            | "derived-from",
        ) => true,
        // An away recap is Claude's out-of-turn summary of a long idle, not a turn record.
        Some("system") => matches!(
            record["subtype"].as_str(),
            Some("informational" | "away_summary")
        ),
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
            // Files and skills a compaction or resume re-attaches carry no turn of their own.
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
                            | "file"
                            | "compact_file_reference"
                            | "invoked_skills"
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
