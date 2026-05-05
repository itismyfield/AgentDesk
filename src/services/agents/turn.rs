use chrono::{TimeZone, Utc};
use regex::Regex;
use serde::Serialize;
use serde_json::json;
use std::sync::OnceLock;

const TURN_CAPTURE_SCROLLBACK_LINES: i32 = -80;
const TURN_CAPTURE_TAIL_LINES: usize = 60;
const TURN_OUTPUT_MAX_CHARS: usize = 4000;

#[derive(Debug, Clone, Default)]
pub struct InflightTurnSnapshot {
    pub started_at: Option<String>,
    pub updated_at: Option<String>,
    pub current_tool_line: Option<String>,
    pub prev_tool_status: Option<String>,
    pub full_response: Option<String>,
    /// #1671: persisted notification kind (`subagent`/`background`/
    /// `monitor_auto_turn`) for the live turn, surfaced through `agentdesk
    /// diag` so operators do not have to hit the watcher-state endpoint.
    pub task_notification_kind: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TurnToolEvent {
    pub kind: &'static str,
    pub status: &'static str,
    pub tool_name: Option<String>,
    pub summary: String,
    pub line: String,
}

#[derive(Debug, Clone)]
struct ParsedTurnToolEvent {
    event: TurnToolEvent,
    identity_kind: &'static str,
    identity_value: String,
}

pub fn extract_tmux_name(session_key: &str) -> Option<String> {
    session_key
        .split_once(':')
        .map(|(_, tmux_name)| tmux_name.trim())
        .filter(|tmux_name| !tmux_name.is_empty())
        .map(str::to_string)
}

fn ansi_escape_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\x1B\[[0-?]*[ -/]*[@-~]").expect("valid ANSI regex"))
}

fn bearer_token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(authorization\s*:\s*bearer\s+)[^\s]+").expect("valid bearer regex")
    })
}

fn secret_assignment_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b([A-Z0-9_]*(?:TOKEN|API[_-]?KEY|SECRET)[A-Z0-9_]*)\b(\s*[:=]\s*)([^\s]+)",
        )
        .expect("valid secret assignment regex")
    })
}

fn strip_ansi(text: &str) -> String {
    ansi_escape_re().replace_all(text, "").replace('\r', "")
}

fn sanitize_sensitive_text(text: &str) -> String {
    let masked_bearer = bearer_token_re().replace_all(text, "$1[REDACTED]");
    secret_assignment_re()
        .replace_all(&masked_bearer, "$1$2[REDACTED]")
        .into_owned()
}

fn tail_chars(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    let tail: String = text.chars().skip(total - max_chars).collect();
    format!("…{tail}")
}

pub fn normalize_recent_output(text: &str) -> Option<String> {
    let stripped = strip_ansi(text);
    let lines: Vec<&str> = stripped.lines().collect();
    let start = lines.len().saturating_sub(TURN_CAPTURE_TAIL_LINES);
    let mut out = String::new();
    let mut prev_blank = false;

    for line in &lines[start..] {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            if prev_blank {
                continue;
            }
            prev_blank = true;
            if !out.is_empty() {
                out.push('\n');
            }
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&sanitize_sensitive_text(trimmed));
        prev_blank = false;
    }

    let normalized = out.trim();
    (!normalized.is_empty()).then(|| tail_chars(normalized, TURN_OUTPUT_MAX_CHARS))
}

pub fn sanitize_status_line(text: &str) -> Option<String> {
    let stripped = strip_ansi(text);
    let sanitized = sanitize_sensitive_text(stripped.trim());
    let normalized = sanitized.trim();
    (!normalized.is_empty()).then(|| normalized.to_string())
}

pub fn capture_recent_tmux_output(tmux_name: &str) -> Option<String> {
    let capture =
        crate::services::platform::tmux::capture_pane(tmux_name, TURN_CAPTURE_SCROLLBACK_LINES)?;
    normalize_recent_output(&capture)
}

pub fn load_inflight_snapshot(
    provider: Option<&str>,
    tmux_name: Option<&str>,
) -> Option<InflightTurnSnapshot> {
    let tmux_name = tmux_name?.trim();
    if tmux_name.is_empty() {
        return None;
    }

    let inflight_root = crate::config::runtime_root()?
        .join("runtime")
        .join("discord_inflight");
    let provider_dirs: Vec<std::path::PathBuf> =
        match provider.map(str::trim).filter(|value| !value.is_empty()) {
            Some(provider) => vec![inflight_root.join(provider)],
            None => std::fs::read_dir(&inflight_root)
                .ok()?
                .flatten()
                .map(|entry| entry.path())
                .collect(),
        };

    for dir in provider_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Ok(data) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(state) = serde_json::from_str::<serde_json::Value>(&data) else {
                continue;
            };
            if state
                .get("tmux_session_name")
                .and_then(|value| value.as_str())
                != Some(tmux_name)
            {
                continue;
            }
            return Some(InflightTurnSnapshot {
                started_at: state
                    .get("started_at")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
                updated_at: state
                    .get("updated_at")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
                current_tool_line: state
                    .get("current_tool_line")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
                prev_tool_status: state
                    .get("prev_tool_status")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
                full_response: state
                    .get("full_response")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
                task_notification_kind: state
                    .get("task_notification_kind")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
            });
        }
    }

    None
}

pub fn inflight_recent_output(snapshot: &InflightTurnSnapshot) -> Option<String> {
    let mut sections = Vec::new();
    if let Some(tool_line) = snapshot
        .prev_tool_status
        .as_deref()
        .and_then(sanitize_status_line)
    {
        sections.push(tool_line);
    }
    if let Some(tool_line) = snapshot
        .current_tool_line
        .as_deref()
        .and_then(sanitize_status_line)
    {
        sections.push(tool_line);
    }
    if let Some(response) = snapshot
        .full_response
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        sections.push(response.to_string());
    }
    (!sections.is_empty())
        .then(|| normalize_recent_output(&sections.join("\n\n")))
        .flatten()
}

fn parse_turn_tool_event(line: &str) -> Option<ParsedTurnToolEvent> {
    let trimmed = sanitize_status_line(line)?;

    if trimmed.starts_with("💭") {
        return Some(ParsedTurnToolEvent {
            event: TurnToolEvent {
                kind: "thinking",
                status: "info",
                tool_name: None,
                summary: trimmed.trim_start_matches("💭").trim().to_string(),
                line: trimmed.to_string(),
            },
            identity_kind: "thinking",
            identity_value: "thinking".to_string(),
        });
    }

    let (status, stripped) = if let Some(rest) = trimmed.strip_prefix("⚙") {
        ("running", rest)
    } else if let Some(rest) = trimmed.strip_prefix("✓") {
        ("success", rest)
    } else if let Some(rest) = trimmed.strip_prefix("✗") {
        ("error", rest)
    } else {
        return None;
    };

    let stripped = stripped.trim();
    if stripped.is_empty() {
        return None;
    }
    let (tool_name, summary) = match stripped.split_once(':') {
        Some((name, summary)) => (
            Some(name.trim().to_string()).filter(|value| !value.is_empty()),
            summary.trim().to_string(),
        ),
        None => (Some(stripped.to_string()), String::new()),
    };
    let summary = if summary.is_empty() {
        tool_name.clone().unwrap_or_else(|| stripped.to_string())
    } else {
        summary
    };

    Some(ParsedTurnToolEvent {
        event: TurnToolEvent {
            kind: "tool",
            status,
            tool_name,
            summary,
            line: trimmed.to_string(),
        },
        identity_kind: "tool",
        identity_value: stripped.to_string(),
    })
}

pub fn collect_turn_tool_events(
    recent_output: Option<&str>,
    inflight: Option<&InflightTurnSnapshot>,
) -> Vec<TurnToolEvent> {
    let mut parsed = Vec::<ParsedTurnToolEvent>::new();
    let mut push_line = |line: &str| {
        let Some(event) = parse_turn_tool_event(line) else {
            return;
        };

        if let Some(last) = parsed.last_mut() {
            if last.identity_kind == event.identity_kind
                && last.identity_value == event.identity_value
            {
                *last = event;
                return;
            }
        }

        parsed.push(event);
    };

    if let Some(previous) = inflight
        .and_then(|snapshot| snapshot.prev_tool_status.as_deref())
        .and_then(sanitize_status_line)
    {
        push_line(&previous);
    }

    if let Some(output) = recent_output {
        for line in output.lines() {
            push_line(line);
        }
    }

    if let Some(current) = inflight
        .and_then(|snapshot| snapshot.current_tool_line.as_deref())
        .and_then(sanitize_status_line)
    {
        push_line(&current);
    }

    let len = parsed.len();
    parsed
        .into_iter()
        .skip(len.saturating_sub(24))
        .map(|entry| entry.event)
        .collect()
}

pub fn loop_suspicion(events: &[TurnToolEvent]) -> serde_json::Value {
    let mut tail = events
        .iter()
        .rev()
        .filter(|event| event.kind == "tool")
        .filter_map(|event| {
            let tool = event.tool_name.as_deref()?.trim();
            if tool.is_empty() {
                return None;
            }
            let prefix: String = event.summary.chars().take(80).collect();
            Some((tool.to_ascii_lowercase(), prefix))
        });

    let Some((tool, prefix)) = tail.next() else {
        return json!({
            "suspected": false,
            "reason": null,
            "repeat_count": 0,
            "tool": null,
        });
    };
    let mut count = 1usize;
    for (next_tool, next_prefix) in tail {
        if next_tool == tool && next_prefix == prefix {
            count += 1;
        } else {
            break;
        }
    }

    if count >= 5 {
        json!({
            "suspected": true,
            "reason": format!("same tool/input prefix repeated {count} times"),
            "repeat_count": count,
            "tool": tool,
        })
    } else {
        json!({
            "suspected": false,
            "reason": null,
            "repeat_count": count,
            "tool": tool,
        })
    }
}

/// #1671 — parse the inflight `started_at`/`updated_at` localtime encoding
/// (`YYYY-MM-DD HH:MM:SS`) into a Unix timestamp.
pub fn parse_local_timestamp_to_unix(value: &str) -> Option<i64> {
    let naive = chrono::NaiveDateTime::parse_from_str(value.trim(), "%Y-%m-%d %H:%M:%S").ok()?;
    chrono::Local
        .from_local_datetime(&naive)
        .single()
        .map(|local| local.with_timezone(&Utc).timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_event(tool_name: &str, summary: &str) -> TurnToolEvent {
        TurnToolEvent {
            kind: "tool",
            status: "ok",
            tool_name: Some(tool_name.to_string()),
            summary: summary.to_string(),
            line: format!("{tool_name}: {summary}"),
        }
    }

    #[test]
    fn normalize_recent_output_masks_bearer_and_key_assignments() {
        let output = normalize_recent_output(
            "\u{1b}[32mAuthorization: Bearer secret-token\u{1b}[0m\nOPENAI_API_KEY=sk-secret\nvisible line",
        )
        .expect("normalized output");

        assert!(output.contains("Authorization: Bearer [REDACTED]"));
        assert!(output.contains("OPENAI_API_KEY=[REDACTED]"));
        assert!(output.contains("visible line"));
        assert!(!output.contains("secret-token"));
        assert!(!output.contains("sk-secret"));
    }

    #[test]
    fn loop_suspicion_reports_repeated_tail() {
        let events = vec![
            tool_event("read", "different"),
            tool_event("bash", "same input"),
            tool_event("bash", "same input"),
            tool_event("bash", "same input"),
            tool_event("bash", "same input"),
            tool_event("bash", "same input"),
        ];
        let value = loop_suspicion(&events);

        assert_eq!(value["suspected"], true);
        assert_eq!(value["repeat_count"], 5);
        assert_eq!(value["tool"], "bash");
    }
}
