use serde_json::Value;

use super::super::formatting::{canonical_tool_name, redact_sensitive_for_placeholder};

pub(super) const CHANNEL_EVENT_CAPACITY: usize = 20;
pub(super) const EVENT_RENDER_LIMIT: usize = 5;
pub(super) const EVENT_LINE_MAX_CHARS: usize = 100;
pub(super) const EVENT_BLOCK_MAX_CHARS: usize = 1500;
pub(super) const STATUS_PANEL_MAX_CHARS: usize = 4096;
pub(super) const STATUS_PANEL_TODO_LIMIT: usize = 8;
pub(super) const STATUS_PANEL_SUBAGENT_LIMIT: usize = 6;
pub(super) const SESSION_PANEL_LINE_MAX_CHARS: usize = 100;
pub(super) const TASK_PANEL_LINE_MAX_CHARS: usize = 140;
pub(super) const CONTEXT_PANEL_LINE_MAX_CHARS: usize = 120;

pub(super) fn sanitize_for_code_fence(raw: &str) -> String {
    raw.replace('`', "")
}

pub(super) fn first_json_string<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub(super) fn first_json_bool(value: &Value, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_bool))
}

pub(super) fn first_json_usize(value: &Value, keys: &[&str]) -> Option<usize> {
    keys.iter().find_map(|key| {
        value.get(*key).and_then(|raw| match raw {
            Value::Number(num) => num
                .as_u64()
                .map(|value| value as usize)
                .or_else(|| num.as_i64().filter(|value| *value >= 0).map(|v| v as usize)),
            Value::String(text) => text.trim().parse::<usize>().ok(),
            _ => None,
        })
    })
}

pub(super) fn normalize_tool_key(name: &str) -> String {
    name.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

pub(super) fn tool_prefix(name: &str) -> String {
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

pub(super) fn sanitized_tool_name(name: &str) -> Option<String> {
    let sanitized = name
        .trim()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
        .take(32)
        .collect::<String>();
    (!sanitized.is_empty()).then_some(sanitized)
}

pub(super) fn value_to_compact_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(value) => value.clone(),
        _ => serde_json::to_string(value).unwrap_or_default(),
    }
}

pub(super) fn normalize_summary(raw: &str) -> String {
    let redacted = redact_sensitive_for_placeholder(raw);
    let line = first_content_line(&redacted);
    truncate_chars(&line, EVENT_LINE_MAX_CHARS)
}

pub(super) fn first_content_line(raw: &str) -> String {
    raw.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn truncate_chars(raw: &str, max_chars: usize) -> String {
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

pub(super) fn escape_status_panel_markdown(raw: &str) -> String {
    raw.chars()
        .flat_map(|ch| match ch {
            '\\' | '`' | '*' | '_' | '~' | '|' => ['\\', ch],
            _ => ['\0', ch],
        })
        .filter(|ch| *ch != '\0')
        .collect()
}
