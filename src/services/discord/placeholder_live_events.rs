use std::collections::VecDeque;
use std::sync::Mutex;

use poise::serenity_prelude::ChannelId;
use serde_json::Value;

use super::formatting::{canonical_tool_name, format_tool_input, redact_sensitive_for_placeholder};

const CHANNEL_EVENT_CAPACITY: usize = 20;
const EVENT_LINE_MAX_CHARS: usize = 100;
const EVENT_BLOCK_MAX_CHARS: usize = 1500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RecentPlaceholderEvent {
    prefix: String,
    summary: String,
}

impl RecentPlaceholderEvent {
    pub(super) fn tool_use(name: &str, input: &str) -> Option<Self> {
        let summary = format_tool_input(name, input);
        let summary = if summary.trim().is_empty() {
            first_content_line(input)
        } else {
            summary
        };
        Self::new(tool_prefix(name), summary)
    }

    pub(super) fn tool_error(content: &str) -> Option<Self> {
        Self::new("[tool error]", content)
    }

    pub(super) fn task_notification(kind: &str, status: &str, summary: &str) -> Option<Self> {
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

    fn render_line(&self) -> String {
        truncate_chars(
            format!("{} {}", self.prefix, self.summary).trim(),
            EVENT_LINE_MAX_CHARS,
        )
    }
}

#[derive(Debug, Default)]
pub(super) struct PlaceholderLiveEvents {
    by_channel: dashmap::DashMap<ChannelId, Mutex<VecDeque<RecentPlaceholderEvent>>>,
}

impl PlaceholderLiveEvents {
    pub(super) fn clear_channel(&self, channel_id: ChannelId) {
        self.by_channel.remove(&channel_id);
    }

    pub(super) fn push_event(&self, channel_id: ChannelId, event: RecentPlaceholderEvent) {
        let entry = self
            .by_channel
            .entry(channel_id)
            .or_insert_with(|| Mutex::new(VecDeque::with_capacity(CHANNEL_EVENT_CAPACITY)));
        let mut guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.len() >= CHANNEL_EVENT_CAPACITY {
            guard.pop_front();
        }
        guard.push_back(event);
    }

    pub(super) fn push_many<I>(&self, channel_id: ChannelId, events: I)
    where
        I: IntoIterator<Item = RecentPlaceholderEvent>,
    {
        for event in events {
            self.push_event(channel_id, event);
        }
    }

    pub(super) fn render_block(&self, channel_id: ChannelId) -> Option<String> {
        let entry = self.by_channel.get(&channel_id)?;
        let guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        render_events(guard.iter())
    }
}

pub(super) fn events_from_json(value: &Value) -> Vec<RecentPlaceholderEvent> {
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

fn render_events<'a>(
    events: impl DoubleEndedIterator<Item = &'a RecentPlaceholderEvent>,
) -> Option<String> {
    let mut lines = Vec::new();
    let mut used = 0usize;
    let inner_limit = EVENT_BLOCK_MAX_CHARS.saturating_sub("```text\n\n```".len());
    for line in events.rev().map(RecentPlaceholderEvent::render_line) {
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

fn tool_prefix(name: &str) -> String {
    let lower = name.trim().to_ascii_lowercase();
    let prefix = match lower.as_str() {
        "bash" | "bashoutput" | "killbash" | "command_execution" => Some("Bash"),
        "edit" | "multiedit" | "write" | "notebookedit" => Some("Edit"),
        "read" => Some("Read"),
        "grep" => Some("Grep"),
        "glob" => Some("Glob"),
        "monitor" => Some("Monitor"),
        "schedulewakeup" | "schedule_wakeup" => Some("ScheduleWakeup"),
        "toolsearch" | "tool_search" | "tool_search_tool" => Some("ToolSearch"),
        "task" | "agent" | "taskcreate" | "taskget" | "taskupdate" | "tasklist" => Some("Task"),
        "webfetch" => Some("WebFetch"),
        "websearch" => Some("WebSearch"),
        _ => canonical_tool_name(name),
    };
    if let Some(prefix) = prefix {
        return format!("[{prefix}]");
    }
    sanitized_tool_name(name)
        .map(|name| format!("[{name}]"))
        .unwrap_or_else(|| "[Tool]".to_string())
}

fn sanitized_tool_name(name: &str) -> Option<String> {
    let sanitized = name
        .trim()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
        .take(32)
        .collect::<String>();
    (!sanitized.is_empty()).then_some(sanitized)
}

fn value_to_compact_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(value) => value.clone(),
        _ => serde_json::to_string(value).unwrap_or_default(),
    }
}

fn normalize_summary(raw: &str) -> String {
    let redacted = redact_sensitive_for_placeholder(raw);
    let line = first_content_line(&redacted);
    truncate_chars(&line, EVENT_LINE_MAX_CHARS)
}

fn first_content_line(raw: &str) -> String {
    raw.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate_chars(raw: &str, max_chars: usize) -> String {
    if raw.chars().count() <= max_chars {
        return raw.to_string();
    }
    let mut out = raw
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::super::formatting::{
        MonitorHandoffReason, MonitorHandoffStatus,
        build_monitor_handoff_placeholder_with_live_events,
    };
    use super::*;
    use serde_json::json;

    #[test]
    fn render_block_keeps_newest_events_under_limit() {
        let events = PlaceholderLiveEvents::default();
        let channel_id = ChannelId::new(42);
        for idx in 0..25 {
            events.push_event(
                channel_id,
                RecentPlaceholderEvent::tool_use("Bash", &format!(r#"{{"command":"echo {idx}"}}"#))
                    .unwrap(),
            );
        }

        let block = events.render_block(channel_id).unwrap();
        assert!(block.starts_with("```text\n"));
        assert!(block.chars().count() <= EVENT_BLOCK_MAX_CHARS);
        assert!(!block.contains("echo 0"));
        assert!(block.contains("echo 24"));
    }

    #[test]
    fn events_from_json_redacts_and_normalizes_tool_use() {
        let events = events_from_json(&json!({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "tool_use",
                    "name": "Bash",
                    "input": {"command": "curl -H 'Authorization: Bearer abc123' https://example.test?token=secret"}
                }]
            }
        }));

        assert_eq!(events.len(), 1);
        let line = events[0].render_line();
        assert!(line.starts_with("[Bash]"));
        assert!(line.contains("Bearer ***"));
        assert!(line.contains("token=***"));
        assert!(!line.contains("abc123"));
        assert!(!line.contains("secret"));
    }

    #[test]
    fn redact_sensitive_for_placeholder_masks_required_patterns() {
        let redacted = redact_sensitive_for_placeholder(
            "sk-abcdefghijklmnopqrstuvwxyz \
             Authorization: Bearer live-token \
             password=hunter2 token=secret api_key=key1 api-key=key2",
        );

        assert!(redacted.contains("***"));
        assert!(redacted.contains("Bearer ***"));
        assert!(redacted.contains("password=***"));
        assert!(redacted.contains("token=***"));
        assert!(redacted.contains("api_key=***"));
        assert!(redacted.contains("api-key=***"));
        assert!(!redacted.contains("sk-abcdefghijklmnopqrstuvwxyz"));
        assert!(!redacted.contains("live-token"));
        assert!(!redacted.contains("hunter2"));
        assert!(!redacted.contains("secret"));
        assert!(!redacted.contains("key1"));
        assert!(!redacted.contains("key2"));
    }

    #[test]
    fn monitor_handoff_live_events_stays_under_description_limit_with_long_command() {
        let events = PlaceholderLiveEvents::default();
        let channel_id = ChannelId::new(99);
        let long_command = format!(
            "printf '{}' && curl -H 'Authorization: Bearer secret-token' https://example.test?api_key=secret",
            "x".repeat(800)
        );
        for idx in 0..20 {
            events.push_event(
                channel_id,
                RecentPlaceholderEvent::tool_use(
                    "Bash",
                    &json!({"command": format!("{long_command}-{idx}")}).to_string(),
                )
                .unwrap(),
            );
        }

        let block = events.render_block(channel_id).unwrap();
        let live_lines = block
            .lines()
            .filter(|line| line.starts_with("[Bash]"))
            .collect::<Vec<_>>();
        assert!(!live_lines.is_empty());
        assert!(
            live_lines
                .iter()
                .all(|line| line.chars().count() <= EVENT_LINE_MAX_CHARS)
        );
        assert!(block.contains("..."));
        assert!(!block.contains("secret-token"));
        assert!(!block.contains("api_key=secret"));

        let rendered = build_monitor_handoff_placeholder_with_live_events(
            MonitorHandoffStatus::Active,
            MonitorHandoffReason::AsyncDispatch,
            1_700_000_000,
            Some(&"tool ".repeat(200)),
            Some(&long_command),
            Some(&"reason ".repeat(200)),
            Some(&"context ".repeat(200)),
            Some(&"request ".repeat(200)),
            Some(&"progress ".repeat(200)),
            Some(&block),
        );

        assert!(
            rendered.len() <= 4096,
            "monitor handoff placeholder exceeded embed description limit: {}",
            rendered.len()
        );
        assert!(rendered.contains("```text\n"));
    }

    #[test]
    fn events_from_json_captures_task_notification() {
        let events = events_from_json(&json!({
            "type": "system",
            "subtype": "task_notification",
            "task_notification_kind": "background",
            "status": "completed",
            "summary": "CI green"
        }));

        assert_eq!(
            events,
            vec![RecentPlaceholderEvent {
                prefix: "[background]".to_string(),
                summary: "completed: CI green".to_string()
            }]
        );
    }
}
