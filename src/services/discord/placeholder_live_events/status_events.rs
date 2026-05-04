use serde_json::Value;

use crate::services::agent_protocol::{StatusEvent, StatusTodoItem, StatusTodoStatus};

use super::super::formatting::format_tool_input;
use super::common::{
    EVENT_LINE_MAX_CHARS, first_content_line, normalize_summary, normalize_tool_key,
    truncate_chars, value_to_compact_string,
};

pub(in crate::services::discord) fn status_events_from_tool_use(
    name: &str,
    input: &str,
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
    if is_task_tool(name) {
        let value = serde_json::from_str::<Value>(input).unwrap_or(Value::Null);
        events.push(StatusEvent::SubagentStart {
            subagent_type: value
                .get("subagent_type")
                .or_else(|| value.get("agent_type"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| Some(name.to_string())),
            desc: subagent_description(&value).or(args_summary.clone()),
        });
    }
    if is_todo_write_tool(name) {
        let value = serde_json::from_str::<Value>(input).unwrap_or(Value::Null);
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

pub(in crate::services::discord) fn status_events_from_tool_result(
    tool_name: Option<&str>,
    is_error: bool,
) -> Vec<StatusEvent> {
    let mut events = vec![StatusEvent::ToolEnd { success: !is_error }];
    if tool_name.is_some_and(is_task_tool) {
        events.push(StatusEvent::SubagentEnd { success: !is_error });
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
                });
            }
        }
        "background" => {
            let summary = first_content_line(summary);
            if !summary.is_empty() {
                events.push(StatusEvent::Heartbeat);
            }
        }
        _ => {}
    }
    events
}

pub(in crate::services::discord) fn status_events_from_json(value: &Value) -> Vec<StatusEvent> {
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
        "task" | "taskcreate" | "agent" | "spawnagent"
    )
}

fn is_todo_write_tool(name: &str) -> bool {
    matches!(
        normalize_tool_key(name).as_str(),
        "todowrite" | "updateplan"
    )
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
        .and_then(Value::as_array)?;
    let parsed = items
        .iter()
        .filter_map(|item| {
            let content = item
                .get("content")
                .or_else(|| item.get("text"))
                .or_else(|| item.get("title"))
                .or_else(|| item.get("task"))
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
    value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|block| {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                return Vec::new();
            }
            let is_error = block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            status_events_from_tool_result(None, is_error)
        })
        .collect()
}

fn system_status_events(value: &Value) -> Vec<StatusEvent> {
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
