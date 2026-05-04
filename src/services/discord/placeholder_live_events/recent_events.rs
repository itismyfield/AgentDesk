use serde_json::Value;

use super::super::formatting::format_tool_input;
use super::common::{
    EVENT_BLOCK_MAX_CHARS, EVENT_LINE_MAX_CHARS, EVENT_RENDER_LIMIT, first_content_line,
    normalize_summary, sanitize_for_code_fence, tool_prefix, truncate_chars,
    value_to_compact_string,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct RecentPlaceholderEvent {
    pub(super) prefix: String,
    pub(super) summary: String,
}

impl RecentPlaceholderEvent {
    pub(in crate::services::discord) fn tool_use(name: &str, input: &str) -> Option<Self> {
        let summary = format_tool_input(name, input);
        let summary = if summary.trim().is_empty() {
            first_content_line(input)
        } else {
            summary
        };
        Self::new(tool_prefix(name), summary)
    }

    pub(in crate::services::discord) fn tool_error(content: &str) -> Option<Self> {
        Self::new("[tool error]", content)
    }

    pub(in crate::services::discord) fn task_notification(
        kind: &str,
        status: &str,
        summary: &str,
    ) -> Option<Self> {
        let prefix = match kind {
            "monitor_auto_turn" => "[Monitor]",
            "subagent" => "[Task]",
            "background" => "[background]",
            _ => "[system]",
        };
        let mut detail = first_content_line(summary);
        let status = status.trim();
        if !status.is_empty() {
            detail = if detail.is_empty() {
                status.to_string()
            } else {
                format!("{status}: {detail}")
            };
        }
        Self::new(prefix, detail)
    }

    fn new(prefix: impl Into<String>, summary: impl AsRef<str>) -> Option<Self> {
        let summary = normalize_summary(summary.as_ref());
        if summary.is_empty() {
            return None;
        }
        Some(Self {
            prefix: prefix.into(),
            summary,
        })
    }

    pub(super) fn render_line(&self) -> String {
        let raw = format!("{} {}", self.prefix, self.summary);
        let sanitized = sanitize_for_code_fence(raw.trim());
        truncate_chars(&sanitized, EVENT_LINE_MAX_CHARS)
    }
}

pub(in crate::services::discord) fn events_from_json(value: &Value) -> Vec<RecentPlaceholderEvent> {
    match value.get("type").and_then(Value::as_str).unwrap_or("") {
        "assistant" => assistant_events(value),
        "content_block_start" => content_block_start_events(value),
        "user" => user_events(value),
        "system" => system_events(value),
        "background_event" => background_event(value).into_iter().collect(),
        "result" => result_event(value).into_iter().collect(),
        _ => Vec::new(),
    }
}

fn assistant_events(value: &Value) -> Vec<RecentPlaceholderEvent> {
    value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|block| {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                return None;
            }
            let name = block.get("name").and_then(Value::as_str).unwrap_or("Tool");
            let input = value_to_compact_string(block.get("input").unwrap_or(&Value::Null));
            RecentPlaceholderEvent::tool_use(name, &input)
        })
        .collect()
}

fn content_block_start_events(value: &Value) -> Vec<RecentPlaceholderEvent> {
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
        .unwrap_or_else(|| "started".to_string());
    RecentPlaceholderEvent::tool_use(name, &input)
        .into_iter()
        .collect()
}

fn user_events(value: &Value) -> Vec<RecentPlaceholderEvent> {
    value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|block| {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                return None;
            }
            let is_error = block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !is_error {
                return None;
            }
            RecentPlaceholderEvent::tool_error(&tool_result_content(block))
        })
        .collect()
}

fn system_events(value: &Value) -> Vec<RecentPlaceholderEvent> {
    if value.get("subtype").and_then(Value::as_str) != Some("task_notification") {
        return Vec::new();
    }
    let kind = value
        .get("task_notification_kind")
        .and_then(Value::as_str)
        .unwrap_or("system");
    let status = value.get("status").and_then(Value::as_str).unwrap_or("");
    let summary = value.get("summary").and_then(Value::as_str).unwrap_or("");
    RecentPlaceholderEvent::task_notification(kind, status, summary)
        .into_iter()
        .collect()
}

fn background_event(value: &Value) -> Option<RecentPlaceholderEvent> {
    let summary = value
        .get("message")
        .or_else(|| value.get("summary"))
        .and_then(Value::as_str)
        .unwrap_or("");
    RecentPlaceholderEvent::task_notification("background", "", summary)
}

fn result_event(value: &Value) -> Option<RecentPlaceholderEvent> {
    let is_error = value
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !is_error {
        return None;
    }
    let summary = value
        .get("errors")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .or_else(|| {
            value
                .get("result")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "error".to_string());
    RecentPlaceholderEvent::tool_error(&summary)
}

fn tool_result_content(block: &Value) -> String {
    if let Some(text) = block.get("content").and_then(Value::as_str) {
        return text.to_string();
    }
    block
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn render_events<'a>(
    events: impl DoubleEndedIterator<Item = &'a RecentPlaceholderEvent>,
) -> Option<String> {
    let mut lines = Vec::new();
    let mut used = 0usize;
    // Reserve room for the surrounding ```text``` fence so the total block
    // (fence + content) stays under EVENT_BLOCK_MAX_CHARS. Inner backticks
    // are already stripped by `sanitize_for_code_fence` so the fence is
    // safe to apply.
    let inner_limit = EVENT_BLOCK_MAX_CHARS.saturating_sub("```text\n\n```".len());
    for line in events
        .rev()
        .take(EVENT_RENDER_LIMIT)
        .map(RecentPlaceholderEvent::render_line)
    {
        let line_len = line.chars().count();
        let extra_newline = usize::from(!lines.is_empty());
        if used + extra_newline + line_len > inner_limit {
            continue;
        }
        used += extra_newline + line_len;
        lines.push(line);
    }
    if lines.is_empty() {
        return None;
    }
    lines.reverse();
    Some(format!("```text\n{}\n```", lines.join("\n")))
}
