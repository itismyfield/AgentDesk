use serde_json::Value;

use crate::services::agent_protocol::{
    StatusEvent, StatusTodoItem, StatusTodoStatus, status_events_from_workflow_json,
};

use super::super::formatting::format_tool_input;
use super::common::{
    EVENT_LINE_MAX_CHARS, first_content_line, is_harness_task_tool_name, normalize_summary,
    normalize_tool_key, truncate_chars, value_to_compact_string,
};

pub(in crate::services::discord) fn status_events_from_tool_use(
    name: &str,
    input: &str,
) -> Vec<StatusEvent> {
    status_events_from_tool_use_with_id(name, input, None)
}

pub(in crate::services::discord) fn status_events_from_tool_use_with_id(
    name: &str,
    input: &str,
    tool_use_id: Option<&str>,
) -> Vec<StatusEvent> {
    let args_summary = format_tool_input(name, input)
        .trim()
        .is_empty()
        .then(|| first_content_line(input))
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            let summary = format_tool_input(name, input);
            (!summary.trim().is_empty()).then_some(summary)
        })
        .map(|summary| truncate_chars(&summary, EVENT_LINE_MAX_CHARS));

    let mut events = vec![StatusEvent::ToolStart {
        name: name.to_string(),
        args_summary: args_summary.clone(),
    }];
    if is_harness_task_tool_name(name) {
        let value = tool_input_value(input);
        let task_id = task_tool_id(&value);
        let status = task_tool_status(name, &value);
        let summary = task_tool_summary(name, &value).or_else(|| {
            (task_id.is_none() && status.is_none())
                .then(|| args_summary.clone())
                .flatten()
        });
        events.push(StatusEvent::TaskToolUpdate {
            name: name.to_string(),
            task_id,
            summary,
            status,
        });
    }
    if is_task_tool(name) {
        let value = tool_input_value(input);
        let background = value
            .get("run_in_background")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        events.push(StatusEvent::SubagentStart {
            subagent_type: value
                .get("subagent_type")
                .or_else(|| value.get("agent_type"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| Some(name.to_string())),
            desc: subagent_description(&value).or(args_summary.clone()),
            tool_use_id: tool_use_id.map(str::to_string),
            background,
        });
    }
    if is_todo_write_tool(name) {
        let value = tool_input_value(input);
        if let Some(items) = todo_items_from_input(&value) {
            events.push(StatusEvent::TodoUpdate { items });
        }
    }
    if is_schedule_wakeup_tool(name) {
        events.push(StatusEvent::ScheduleWakeup {
            eta_secs: parse_eta_secs(input.into()),
        });
    }
    events
}

fn tool_input_value(input: &str) -> Value {
    match serde_json::from_str::<Value>(input).unwrap_or(Value::Null) {
        Value::String(raw) => serde_json::from_str::<Value>(&raw).unwrap_or(Value::String(raw)),
        value => value,
    }
}

pub(in crate::services::discord) fn status_events_from_tool_result(
    tool_name: Option<&str>,
    is_error: bool,
) -> Vec<StatusEvent> {
    status_events_from_tool_result_with_id(tool_name, is_error, None)
}

pub(in crate::services::discord) fn status_events_from_tool_result_with_id(
    tool_name: Option<&str>,
    is_error: bool,
    tool_use_id: Option<&str>,
) -> Vec<StatusEvent> {
    let mut events = vec![StatusEvent::ToolEnd { success: !is_error }];
    if tool_name.is_some_and(tool_result_completes_subagent) {
        events.push(StatusEvent::SubagentEnd {
            success: !is_error,
            tool_use_id: tool_use_id.map(str::to_string),
            summary: None,
            // The Task `tool_result` always fires when the tool returns. Only a
            // SUCCESSFUL `run_in_background` launch is an ack-only end: the
            // dispatch succeeded and the subagent keeps running (often
            // outliving the launching turn), so the panel must NOT mark it ✓.
            // A FAILED launch (`is_error`) is terminal — the subagent never
            // started — so it is not ack-only and the panel finalizes the slot
            // as failed (✗), exactly like a foreground failure.
            ack_only: !is_error,
        });
    }
    events
}

pub(in crate::services::discord) fn status_events_from_task_notification(
    kind: &str,
    status: &str,
    summary: &str,
) -> Vec<StatusEvent> {
    let mut events = Vec::new();
    match kind {
        "monitor_auto_turn" => events.push(StatusEvent::MonitorWait),
        "subagent" => {
            let summary = first_content_line(summary);
            if !summary.is_empty() {
                events.push(StatusEvent::SubagentEvent { summary });
            }
            if task_notification_is_terminal(status) {
                events.push(StatusEvent::SubagentEnd {
                    success: !task_notification_is_error(status),
                    tool_use_id: None,
                    summary: None,
                    // A terminal task_notification is the subagent's REAL
                    // completion (including background subagents), so it always
                    // finalizes the slot — not an ack.
                    ack_only: false,
                });
            }
        }
        "background" => {
            let summary = first_content_line(summary);
            if !summary.is_empty() {
                events.push(StatusEvent::Heartbeat);
            }
        }
        "workflow" => {
            events.push(StatusEvent::WorkflowEnd {
                task_id: None,
                success: !task_notification_is_error(status),
                summary: Some(first_content_line(summary)).filter(|value| !value.is_empty()),
            });
        }
        _ => {}
    }
    events
}

pub(in crate::services::discord) fn status_events_from_json(value: &Value) -> Vec<StatusEvent> {
    let workflow_events = status_events_from_workflow_json(value);
    if !workflow_events.is_empty() {
        return workflow_events;
    }

    match value.get("type").and_then(Value::as_str).unwrap_or("") {
        "assistant" => assistant_status_events(value),
        "content_block_start" => content_block_start_status_events(value),
        "user" => user_status_events(value),
        "system" => system_status_events(value),
        "background_event" => background_status_events(value),
        _ => Vec::new(),
    }
}

pub(super) fn is_task_tool(name: &str) -> bool {
    matches!(
        normalize_tool_key(name).as_str(),
        "task" | "agent" | "spawnagent"
    )
}

fn tool_result_completes_subagent(name: &str) -> bool {
    matches!(
        normalize_tool_key(name).as_str(),
        "task" | "agent" | "spawnagent"
    )
}

fn is_todo_write_tool(name: &str) -> bool {
    matches!(
        normalize_tool_key(name).as_str(),
        "todowrite" | "updateplan"
    )
}

fn task_tool_id(value: &Value) -> Option<String> {
    ["task_id", "taskId", "taskID", "id"]
        .into_iter()
        .find_map(|key| value.get(key).and_then(Value::as_str))
        .map(normalize_summary)
        .filter(|value| !value.is_empty())
}

fn task_tool_summary(name: &str, value: &Value) -> Option<String> {
    [
        "subject",
        "title",
        "description",
        "desc",
        "content",
        "task",
        "message",
    ]
    .into_iter()
    .find_map(|key| value.get(key).and_then(Value::as_str))
    .map(normalize_summary)
    .filter(|value| !value.is_empty())
    .or_else(|| (normalize_tool_key(name) == "tasklist").then(|| "list".to_string()))
}

fn task_tool_status(name: &str, value: &Value) -> Option<String> {
    let status = value
        .get("status")
        .or_else(|| value.get("state"))
        .and_then(Value::as_str)
        .map(normalize_summary)
        .filter(|value| !value.is_empty());
    if status.is_some() {
        return status;
    }
    match normalize_tool_key(name).as_str() {
        "taskstop" => Some("stopped".to_string()),
        _ => None,
    }
}

pub(super) fn is_schedule_wakeup_tool(name: &str) -> bool {
    normalize_tool_key(name) == "schedulewakeup"
}

fn subagent_description(value: &Value) -> Option<String> {
    [
        "description",
        "desc",
        "prompt",
        "task",
        "message",
        "request",
    ]
    .into_iter()
    .find_map(|key| value.get(key).and_then(Value::as_str))
    .map(normalize_summary)
    .filter(|summary| !summary.is_empty())
}

fn todo_items_from_input(value: &Value) -> Option<Vec<StatusTodoItem>> {
    let items = value
        .get("todos")
        .or_else(|| value.get("items"))
        .or_else(|| value.get("todo_list"))
        .or_else(|| value.get("plan"))
        .and_then(Value::as_array)?;
    let parsed = items
        .iter()
        .filter_map(|item| {
            let content = item
                .get("content")
                .or_else(|| item.get("text"))
                .or_else(|| item.get("title"))
                .or_else(|| item.get("task"))
                .or_else(|| item.get("step"))
                .and_then(Value::as_str)
                .map(normalize_summary)
                .filter(|content| !content.is_empty())?;
            let status = item
                .get("status")
                .or_else(|| item.get("state"))
                .and_then(Value::as_str)
                .map(StatusTodoStatus::from_provider_str)
                .unwrap_or(StatusTodoStatus::Pending);
            Some(StatusTodoItem { content, status })
        })
        .collect::<Vec<_>>();
    (!parsed.is_empty()).then_some(parsed)
}

pub(super) fn parse_eta_secs(raw: Option<&str>) -> Option<u64> {
    let value = raw?.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(parsed) = value.parse::<u64>() {
        return Some(parsed);
    }
    serde_json::from_str::<Value>(value)
        .ok()
        .and_then(|json| eta_secs_from_value(&json))
        .or_else(|| {
            value
                .split(|ch: char| !ch.is_ascii_digit())
                .find(|part| !part.is_empty())
                .and_then(|part| part.parse::<u64>().ok())
        })
}

fn eta_secs_from_value(value: &Value) -> Option<u64> {
    if let Some(value) = value.as_u64() {
        return Some(value);
    }
    if let Some(value) = value.as_str() {
        return parse_eta_secs(Some(value));
    }
    for key in [
        "eta_secs",
        "seconds",
        "delay_secs",
        "delay_seconds",
        "duration_secs",
    ] {
        if let Some(value) = value.get(key).and_then(eta_secs_from_value) {
            return Some(value);
        }
    }
    None
}

fn task_notification_is_terminal(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "completed"
            | "done"
            | "finished"
            | "success"
            | "failed"
            | "error"
            | "aborted"
            | "cancelled"
            | "canceled"
    )
}

fn task_notification_is_error(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "failed" | "error" | "aborted" | "cancelled" | "canceled"
    )
}

fn assistant_status_events(value: &Value) -> Vec<StatusEvent> {
    value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|block| {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                return Vec::new();
            }
            let name = block.get("name").and_then(Value::as_str).unwrap_or("Tool");
            let input = value_to_compact_string(block.get("input").unwrap_or(&Value::Null));
            status_events_from_tool_use(name, &input)
        })
        .collect()
}

fn content_block_start_status_events(value: &Value) -> Vec<StatusEvent> {
    let Some(block) = value.get("content_block") else {
        return Vec::new();
    };
    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
        return Vec::new();
    }
    let name = block.get("name").and_then(Value::as_str).unwrap_or("Tool");
    let input = block
        .get("input")
        .map(value_to_compact_string)
        .unwrap_or_default();
    status_events_from_tool_use(name, &input)
}

fn user_status_events(value: &Value) -> Vec<StatusEvent> {
    // #3086: a finished subagent's `tool_result` carries a `toolUseResult`
    // aggregate with subagent accounting (`agentId` / `total*`). Surface a
    // TUI-parity `Done (N tools · M tokens · Xs)` summary by pairing the result
    // to its slot via the content block's own `tool_use_id`. The accounting
    // comes from the in-stream `toolUseResult` (no IO).
    //
    // #3086 P1: a single `user` record may BATCH several finished subagents'
    // `tool_result` blocks, and each Task subagent result carries its OWN
    // `toolUseResult` aggregate (its own `agentId`/`total*`). The aggregate for
    // subagent A lives in A's own block; B's lives in B's. We therefore compute
    // each block's summary FROM THAT SAME BLOCK and key it by THAT block's
    // `tool_use_id` — the slot key the panel pairs on (#3084). We must NOT
    // attach a single record-level aggregate to "the first id-bearing block":
    // with multiple aggregate-bearing blocks that would put subagent A's Done
    // summary on subagent B's slot.
    //
    // Legacy single-subagent records put the aggregate at the RECORD top level
    // (one `tool_result` block, top-level `toolUseResult`). When no block
    // carries its own aggregate, we fall back to attaching that record-level
    // aggregate to the first id-bearing `tool_result` block (there is exactly
    // one finished subagent in that shape, so a single owner is correct).
    //
    // We cannot read slot state here (it lives in the panel), so each
    // summary-bearing `SubagentEnd` is keyed by the block's `tool_use_id`; the
    // panel (status_panel.rs) requires that id to match a tracked subagent slot
    // before applying it — a summary-bearing end with an unmatched id is dropped
    // rather than mis-routed to the last unfinished slot.
    let blocks = value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);

    // Per-block aggregates take precedence: when ANY `tool_result` block carries
    // its own `toolUseResult` aggregate, attribute each summary to its own block
    // and never fall back to the record-level aggregate (which, in the batched
    // shape, would be absent or ambiguous).
    let any_block_aggregate = blocks.iter().any(|block| {
        block.get("type").and_then(Value::as_str) == Some("tool_result")
            && super::subagent_rollout::summary_from_tool_use_result(block).is_some()
    });

    // Legacy single-subagent fallback: the aggregate sits at the record top
    // level. Attribute it to the first id-bearing `tool_result` block (only one
    // finished subagent exists in that shape). Disabled when blocks carry their
    // own aggregates, to avoid double-counting / mis-attribution.
    let record_summary = if any_block_aggregate {
        None
    } else {
        subagent_summary_from_record(value)
    };
    let record_summary_owner_idx = record_summary.as_ref().and_then(|_| {
        blocks.iter().position(|block| {
            block.get("type").and_then(Value::as_str) == Some("tool_result")
                && block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| !id.trim().is_empty())
        })
    });

    blocks
        .iter()
        .enumerate()
        .flat_map(|(idx, block)| {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                return Vec::new();
            }
            let is_error = block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            // This block's OWN aggregate (batched multi-subagent case): attach
            // the per-subagent summary here, keyed by THIS block's tool_use_id.
            let block_summary = subagent_summary_from_record(block);
            // Or, for the legacy single-subagent shape, the record-level
            // aggregate owned by the first id-bearing block.
            let summary = block_summary.or_else(|| {
                if Some(idx) == record_summary_owner_idx {
                    record_summary.clone()
                } else {
                    None
                }
            });

            if let Some(summary) = summary {
                // Pair by this block's own tool_use_id. The panel refuses to
                // apply the summary unless the id matches a real, tracked slot,
                // so a stray summary can never land on an unrelated running slot.
                let tool_use_id = block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                return vec![
                    StatusEvent::ToolEnd { success: !is_error },
                    StatusEvent::SubagentEnd {
                        success: !is_error,
                        tool_use_id,
                        summary: Some(summary),
                        // A summary-bearing end carries real accounting
                        // (`toolUseResult`/rollout) — a genuine completion that
                        // always finalizes the slot, never just an ack.
                        ack_only: false,
                    },
                ];
            }

            status_events_from_tool_result(None, is_error)
        })
        .collect()
}

/// Builds the subagent [`SubagentSummary`](crate::services::agent_protocol::SubagentSummary)
/// from a JSON object's `toolUseResult` aggregate. The object may be either an
/// individual `tool_result` content block (batched multi-subagent case, where
/// each Task result carries its own `toolUseResult`) or the whole `user` record
/// (legacy single-subagent case, where the aggregate sits at the record top
/// level). Returns `None` for ordinary (non-subagent) tool results.
///
/// #3086 P1: this runs on the live relay/status hot path, so it uses ONLY the
/// in-stream `toolUseResult` aggregate — no disk IO. The aggregate is the exact
/// accounting the TUI renders and is normally complete; any field it omits is
/// simply left empty, and the render layer degrades to a partial `Done (...)`
/// line. The previous synchronous per-subagent rollout fallback
/// (`std::fs::read_to_string` of a potentially large `agent-<id>.jsonl`) was an
/// unbounded blocking read on the async relay loop and is removed from this
/// path. The IO-free rollout parser (`summary_from_rollout_str`) remains
/// available for any off-hot-path / offline use.
fn subagent_summary_from_record(
    value: &Value,
) -> Option<crate::services::agent_protocol::SubagentSummary> {
    let (summary, _agent_id) = super::subagent_rollout::summary_from_tool_use_result(value)?;
    Some(summary)
}

fn system_status_events(value: &Value) -> Vec<StatusEvent> {
    let workflow_events = status_events_from_workflow_json(value);
    if !workflow_events.is_empty() {
        return workflow_events;
    }

    if value.get("subtype").and_then(Value::as_str) != Some("task_notification") {
        return Vec::new();
    }
    let kind = value
        .get("task_notification_kind")
        .and_then(Value::as_str)
        .unwrap_or("system");
    let status = value.get("status").and_then(Value::as_str).unwrap_or("");
    let summary = value.get("summary").and_then(Value::as_str).unwrap_or("");
    status_events_from_task_notification(kind, status, summary)
}

fn background_status_events(value: &Value) -> Vec<StatusEvent> {
    let summary = value
        .get("message")
        .or_else(|| value.get("summary"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if summary.trim().is_empty() {
        Vec::new()
    } else {
        vec![StatusEvent::Heartbeat]
    }
}
