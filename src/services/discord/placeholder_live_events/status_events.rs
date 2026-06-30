use serde_json::Value;

use crate::services::agent_protocol::{
    StatusEvent, StatusTodoItem, StatusTodoStatus, status_events_from_workflow_json,
};

use super::super::formatting::format_tool_input;
use super::background_task_events::{
    events_from_background_notification, notification_is_error, notification_is_terminal,
    slots_enabled_by_footer_flag, start_event_from_bash_tool_use, tool_use_id_from_notification,
};
use super::common::{
    EVENT_LINE_MAX_CHARS, first_content_line, is_harness_task_tool_name, normalize_summary,
    normalize_tool_key, truncate_chars, value_to_compact_string,
};

#[cfg(test)]
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
    status_events_from_tool_use_with_id_for_footer_mode(
        name,
        input,
        tool_use_id,
        background_bash_task_slots_enabled(),
    )
}

pub(in crate::services::discord) fn status_events_from_tool_use_with_id_for_footer_mode(
    name: &str,
    input: &str,
    tool_use_id: Option<&str>,
    footer_mode_enabled: bool,
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
    if footer_mode_enabled {
        let value = tool_input_value(input);
        if let Some(event) = start_event_from_bash_tool_use(
            name,
            &value,
            args_summary.clone(),
            tool_use_id,
            footer_mode_enabled,
        ) {
            events.push(event);
        }
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

fn background_bash_task_slots_enabled() -> bool {
    slots_enabled_by_footer_flag()
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
            // A SUCCESSFUL `run_in_background` launch is ack-only: dispatch
            // succeeded but the subagent keeps running, so don't mark it ✓. A
            // FAILED launch is terminal (never started) → finalizes the slot ✗.
            ack_only: !is_error,
        });
    }
    events
}

#[cfg(test)]
pub(in crate::services::discord) fn status_events_from_task_notification(
    kind: &str,
    status: &str,
    summary: &str,
) -> Vec<StatusEvent> {
    status_events_from_task_notification_with_tool_use_id(kind, status, summary, None)
}

pub(in crate::services::discord) fn status_events_from_task_notification_with_tool_use_id(
    kind: &str,
    status: &str,
    summary: &str,
    tool_use_id: Option<&str>,
) -> Vec<StatusEvent> {
    let mut events = Vec::new();
    match kind {
        "monitor_auto_turn" => events.push(StatusEvent::MonitorWait),
        "subagent" => {
            let summary = first_content_line(summary);
            if !summary.is_empty() {
                events.push(match tool_use_id {
                    Some(tool_use_id) => StatusEvent::SubagentActivity {
                        tool_use_id: Some(tool_use_id.to_string()),
                        summary,
                    },
                    None => StatusEvent::SubagentEvent { summary },
                });
            }
            if notification_is_terminal(status) {
                events.push(StatusEvent::SubagentEnd {
                    success: !notification_is_error(status),
                    tool_use_id: tool_use_id.map(str::to_string),
                    summary: None,
                    // A terminal task_notification is the subagent's REAL
                    // completion (incl. background) → finalizes, not an ack.
                    ack_only: false,
                });
            }
        }
        "background" => {
            let summary = first_content_line(summary);
            events.extend(events_from_background_notification(
                status,
                &summary,
                tool_use_id,
            ));
        }
        "workflow" => {
            // #3393 finding 3: gate WorkflowEnd on a TERMINAL status (success via
            // !is_error), like the subagent/background arms — running emits nothing.
            if notification_is_terminal(status) {
                events.push(StatusEvent::WorkflowEnd {
                    task_id: None,
                    success: !notification_is_error(status),
                    summary: Some(first_content_line(summary)).filter(|value| !value.is_empty()),
                });
            }
        }
        _ => {}
    }
    events
}

/// #3393: bridge a raw `user`-record `<task-notification>` XML payload into the
/// same live-panel [`StatusEvent`]s the (never-occurring) stream-json `system`
/// path produced — background/subagent completions reach the transcript ONLY as
/// this XML; without the bridge slots never flip ✓ (#3391 eviction never fires).
pub(in crate::services::discord) fn status_events_from_task_notification_xml(
    raw: &str,
) -> Vec<StatusEvent> {
    status_events_from_task_notification_xml_for_footer_mode(raw, slots_enabled_by_footer_flag())
}

/// Footer-mode-injectable variant: legacy mode returns an empty vec (separate-
/// panel path untouched). Parses with the SHARED `tui_task_card` parser, derives
/// kind from the summary prefix, routes through the `_with_tool_use_id` mapper.
pub(in crate::services::discord) fn status_events_from_task_notification_xml_for_footer_mode(
    raw: &str,
    footer_mode_enabled: bool,
) -> Vec<StatusEvent> {
    if !footer_mode_enabled {
        return Vec::new();
    }
    let parsed = super::super::tui_task_card::parse_task_notification(raw);
    let status = parsed.status.as_deref().unwrap_or("");
    if status.is_empty() {
        return Vec::new();
    }
    let events = status_events_from_task_notification_with_tool_use_id(
        parsed.kind(),
        status,
        parsed.summary.as_deref().unwrap_or(""),
        parsed.tool_use_id.as_deref(),
    );
    // #3393 finding 1 (XML-scoped): drop an id-less terminal `SubagentEnd` — it
    // would fall back in the panel to "the last unfinished slot" and flip/evict
    // the WRONG one (permanently, post-#3391). A missing id → no terminal effect
    // (heartbeat/activity kept). The `system` path keeps its id-less fallback.
    events.into_iter().filter(idful_subagent_or_other).collect()
}

/// #3393 finding 1 XML-bridge drop predicate: `false` for an id-less `SubagentEnd`.
fn idful_subagent_or_other(event: &StatusEvent) -> bool {
    !matches!(event, StatusEvent::SubagentEnd { tool_use_id, .. } if tool_use_id.is_none())
}

pub(in crate::services::discord) fn status_events_from_json(value: &Value) -> Vec<StatusEvent> {
    status_events_from_json_for_footer_mode(value, background_bash_task_slots_enabled())
}

pub(in crate::services::discord) fn status_events_from_json_for_footer_mode(
    value: &Value,
    footer_mode_enabled: bool,
) -> Vec<StatusEvent> {
    let workflow_events = status_events_from_workflow_json(value);
    if !workflow_events.is_empty() {
        return workflow_events;
    }

    // A nested subagent record carries the launching Task's `parent_tool_use_id`;
    // its tool activity belongs to that slot, so route it to `SubagentActivity`
    // keyed by the parent id rather than a top-level `ToolStart` that would
    // clobber the panel header / resurrect "tool running".
    if let Some(parent_id) = subagent_parent_tool_use_id(value) {
        return subagent_activity_status_events(value, parent_id);
    }

    match value.get("type").and_then(Value::as_str).unwrap_or("") {
        "assistant" => assistant_status_events(value, footer_mode_enabled),
        "content_block_start" => content_block_start_status_events(value, footer_mode_enabled),
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

fn assistant_status_events(value: &Value, footer_mode_enabled: bool) -> Vec<StatusEvent> {
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
            let tool_use_id = block.get("id").and_then(Value::as_str);
            status_events_from_tool_use_with_id_for_footer_mode(
                name,
                &input,
                tool_use_id,
                footer_mode_enabled,
            )
        })
        .collect()
}

fn content_block_start_status_events(value: &Value, footer_mode_enabled: bool) -> Vec<StatusEvent> {
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
    let tool_use_id = block.get("id").and_then(Value::as_str);
    status_events_from_tool_use_with_id_for_footer_mode(
        name,
        &input,
        tool_use_id,
        footer_mode_enabled,
    )
}

fn user_status_events(value: &Value) -> Vec<StatusEvent> {
    // #3086: surface a TUI-parity `Done (...)` from each finished subagent's
    // in-stream `toolUseResult` aggregate (no IO), keyed by the block's own
    // `tool_use_id` (slot key, #3084). #3086 P1: a BATCHED record has one
    // aggregate PER subagent — compute each from its own block (never attach the
    // record-level aggregate to "the first id-bearing block", which mis-routes
    // A's Done onto B). Legacy single-subagent keeps the record-level aggregate on
    // the first id-bearing block; the panel drops an end whose id matches no slot.
    let blocks = value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);

    // Per-block aggregates take precedence (each summary attributed to its own
    // block); the record-level fallback is disabled when any block carries one.
    let any_block_aggregate = blocks.iter().any(|block| {
        block.get("type").and_then(Value::as_str) == Some("tool_result")
            && super::subagent_rollout::summary_from_tool_use_result(block).is_some()
    });

    // Legacy single-subagent fallback: the record-level aggregate, owned by the
    // first id-bearing block (exactly one finished subagent in that shape).
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
            // #3920: a successful async Agent launch ack keeps its slot alive as a
            // background subagent across turns (it is NOT a completion).
            if let Some(events) =
                super::subagent_rollout::async_launch_promote_events(value, blocks, idx, is_error)
            {
                return events;
            }

            // This block's OWN aggregate (batched case), keyed by THIS block's
            // tool_use_id; else the legacy record-level aggregate on the first
            // id-bearing block.
            let block_summary = subagent_summary_from_record(block);
            let summary = block_summary.or_else(|| {
                if Some(idx) == record_summary_owner_idx {
                    record_summary.clone()
                } else {
                    None
                }
            });

            if let Some(summary) = summary {
                // Pair by this block's own tool_use_id; the panel refuses the
                // summary unless the id matches a real tracked slot.
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
                        // A summary-bearing end carries real accounting — a
                        // genuine completion that always finalizes, never an ack.
                        ack_only: false,
                    },
                ];
            }

            status_events_from_tool_result(None, is_error)
        })
        .collect()
}

/// Builds the subagent [`SubagentSummary`](crate::services::agent_protocol::SubagentSummary)
/// from a JSON object's `toolUseResult` aggregate — an individual `tool_result`
/// block (batched) or the whole `user` record (legacy single). `None` for
/// ordinary results. #3086 P1: live hot path — in-stream aggregate only (no disk
/// IO); the prior synchronous rollout `read_to_string` was removed.
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
    let tool_use_id = tool_use_id_from_notification(value);
    status_events_from_task_notification_with_tool_use_id(
        kind,
        status,
        summary,
        tool_use_id.as_deref(),
    )
}

/// Returns the launching Task's tool-use id from a nested subagent record's
/// top-level `parent_tool_use_id` (Claude Code marks every subagent-internal
/// record with it). `None` for top-level records → normal panel path.
fn subagent_parent_tool_use_id(value: &Value) -> Option<String> {
    ["parent_tool_use_id", "parentToolUseId"]
        .into_iter()
        .find_map(|key| value.get(key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

/// Builds [`StatusEvent::SubagentActivity`] events for a nested subagent record,
/// one per tool_use block, keyed by the parent Task id so the panel updates the
/// owning slot's recent line with the tool class. Raw nested tool arguments stay
/// in transcript/log retrieval paths, not normal Discord relay panels.
fn subagent_activity_status_events(value: &Value, parent_id: String) -> Vec<StatusEvent> {
    let blocks: Vec<(&str, String)> = match value.get("type").and_then(Value::as_str) {
        Some("assistant") => value
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
            .map(|block| {
                let name = block.get("name").and_then(Value::as_str).unwrap_or("Tool");
                let input = value_to_compact_string(block.get("input").unwrap_or(&Value::Null));
                (name, input)
            })
            .collect(),
        Some("content_block_start") => value
            .get("content_block")
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
            .map(|block| {
                let name = block.get("name").and_then(Value::as_str).unwrap_or("Tool");
                let input = block
                    .get("input")
                    .map(value_to_compact_string)
                    .unwrap_or_default();
                vec![(name, input)]
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    };

    blocks
        .into_iter()
        .filter_map(|(name, input)| {
            subagent_activity_line(name, &input).map(|summary| StatusEvent::SubagentActivity {
                tool_use_id: Some(parent_id.clone()),
                summary,
            })
        })
        .collect()
}

/// Formats a subagent's tool step into a compact activity line such as `[Bash]`.
/// Returns `None` only when the tool name is unusable.
fn subagent_activity_line(name: &str, _input: &str) -> Option<String> {
    use super::common::tool_prefix;
    let line = tool_prefix(name);
    let line = normalize_summary(&line);
    (!line.trim().is_empty()).then_some(truncate_chars(&line, EVENT_LINE_MAX_CHARS))
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
