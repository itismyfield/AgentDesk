use serde_json::Value;

use super::super::formatting::format_tool_input;
#[cfg(test)]
use super::common::{
    EVENT_BLOCK_MAX_CHARS, EVENT_LINE_MAX_CHARS, sanitize_for_code_fence, truncate_chars,
};
use super::common::{
    EVENT_RENDER_LIMIT, first_content_line, is_harness_task_tool_name, is_internal_tool_error,
    normalize_summary_multiline, tool_prefix, value_to_compact_string,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct RecentPlaceholderEvent {
    pub(super) prefix: String,
    pub(super) summary: String,
}

impl RecentPlaceholderEvent {
    pub(in crate::services::discord) fn tool_use(name: &str, input: &str) -> Option<Self> {
        if is_harness_task_tool_name(name) {
            return None;
        }
        let summary = format_tool_input(name, input);
        let summary = if summary.trim().is_empty() {
            first_content_line(input)
        } else {
            summary
        };
        Self::new(tool_prefix(name), summary)
    }

    pub(in crate::services::discord) fn tool_error(content: &str) -> Option<Self> {
        // Internal cancellation/abort diagnostics (e.g. "Cancelled: parallel
        // tool call ...") are harness noise, not genuine tool failures, so they
        // must not leak into the user-facing Recent mirror.
        if is_internal_tool_error(content) {
            return None;
        }
        let projected = crate::services::tool_output_guard::project_for_relay(None, true, content);
        Self::new("[tool error]", projected.content)
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
        // #3477 item 1: keep up to RECENT_EVENT_MAX_LINES so multi-line TUI output
        // stays readable in the Recent/terminal block (panel cells still collapse
        // to one line via `normalize_summary`).
        let summary = normalize_summary_multiline(summary.as_ref());
        if summary.is_empty() {
            return None;
        }
        Some(Self {
            prefix: prefix.into(),
            summary,
        })
    }

    #[cfg(test)]
    pub(super) fn render_line(&self) -> String {
        // #3477 item 1: the summary may span several lines. Put the prefix on the
        // first line and indent the continuation lines so the entry stays visually
        // grouped inside the fenced block. Each line is sanitized (fence-safe) and
        // char-clamped individually so one long line cannot blow the budget.
        let mut lines = self.summary.lines();
        let first = lines.next().unwrap_or("");
        let head = truncate_chars(
            &sanitize_for_code_fence(format!("{} {first}", self.prefix).trim()),
            EVENT_LINE_MAX_CHARS,
        );
        let mut out = head;
        for cont in lines {
            let cont = truncate_chars(&sanitize_for_code_fence(cont.trim()), EVENT_LINE_MAX_CHARS);
            if cont.is_empty() {
                continue;
            }
            out.push_str("\n  ");
            out.push_str(&cont);
        }
        out
    }

    fn compact_bucket(&self) -> (&str, &'static str) {
        let action = match self.prefix.as_str() {
            "[tool error]" => "오류",
            "[background]" => "백그라운드",
            "[Monitor]" => "monitor",
            "[Task]" => "작업",
            "[system]" => "시스템",
            _ => "실행",
        };
        (self.prefix.as_str(), action)
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

#[cfg(test)]
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

pub(super) fn render_compact_events<'a>(
    events: impl DoubleEndedIterator<Item = &'a RecentPlaceholderEvent>,
) -> Option<String> {
    let mut buckets: Vec<(String, &'static str, usize)> = Vec::new();
    for event in events.rev().take(EVENT_RENDER_LIMIT) {
        let (prefix, action) = event.compact_bucket();
        if let Some((_, _, count)) =
            buckets
                .iter_mut()
                .find(|(existing_prefix, existing_action, _)| {
                    existing_prefix == prefix && *existing_action == action
                })
        {
            *count += 1;
            continue;
        }
        buckets.push((prefix.to_string(), action, 1));
    }
    if buckets.is_empty() {
        return None;
    }

    buckets.reverse();
    Some(
        buckets
            .into_iter()
            .map(|(prefix, action, count)| {
                let suffix = if count > 1 {
                    format!(" · {count}회")
                } else {
                    String::new()
                };
                format!("• {prefix} {action}{suffix}")
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}
