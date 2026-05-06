use chrono::{DateTime, TimeZone, Utc};
use regex::Regex;
use serde::Serialize;
use serde_json::json;
use sqlx::Row;
use std::sync::OnceLock;

use crate::server::dto::agents::transcript_json;
use crate::server::routes::session_activity::SessionActivityResolver;
use crate::services::agents::query::agent_exists_pg;
use crate::utils::api::clamp_api_limit;

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

#[derive(Debug, Clone)]
pub struct AgentTurnSession {
    pub session_key: String,
    pub provider: Option<String>,
    pub last_heartbeat: Option<String>,
    pub created_at: Option<String>,
    pub thread_channel_id: Option<String>,
    pub runtime_channel_id: Option<String>,
    pub effective_status: &'static str,
    pub effective_active_dispatch_id: Option<String>,
    pub is_working: bool,
}

#[derive(Debug)]
pub enum AgentTurnLookupError {
    AgentNotFound,
    Query(sqlx::Error),
}

impl From<sqlx::Error> for AgentTurnLookupError {
    fn from(error: sqlx::Error) -> Self {
        Self::Query(error)
    }
}

#[derive(Debug, Clone, Default)]
struct AgentTurnSessionRow {
    session_key: String,
    provider: Option<String>,
    raw_status: Option<String>,
    active_dispatch_id: Option<String>,
    last_heartbeat: Option<String>,
    created_at: Option<String>,
    thread_channel_id: Option<String>,
    runtime_channel_id: Option<String>,
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

fn pg_timestamp_to_rfc3339(value: Option<DateTime<Utc>>) -> Option<String> {
    value.map(|value| value.to_rfc3339())
}

fn resolve_agent_turn_session_rows(rows: Vec<AgentTurnSessionRow>) -> Option<AgentTurnSession> {
    let mut resolver = SessionActivityResolver::new();
    let mut latest = None;

    for row in rows {
        let session_key_ref =
            (!row.session_key.trim().is_empty()).then_some(row.session_key.as_str());
        let effective = resolver.resolve(
            session_key_ref,
            row.raw_status.as_deref(),
            row.active_dispatch_id.as_deref(),
            row.last_heartbeat.as_deref(),
        );
        let candidate = AgentTurnSession {
            session_key: row.session_key,
            provider: row.provider,
            last_heartbeat: row.last_heartbeat,
            created_at: row.created_at,
            thread_channel_id: row.thread_channel_id,
            runtime_channel_id: row.runtime_channel_id,
            effective_status: effective.status,
            effective_active_dispatch_id: effective.active_dispatch_id,
            is_working: effective.is_working,
        };
        if latest.is_none() {
            latest = Some(candidate.clone());
        }
        if candidate.is_working {
            return Some(candidate);
        }
    }

    latest
}

pub async fn find_agent_turn_session_pg(
    pool: &sqlx::PgPool,
    agent_id: &str,
) -> Result<Option<AgentTurnSession>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT COALESCE(s.session_key, '') AS session_key,
                s.provider,
                s.status,
                s.active_dispatch_id,
                s.last_heartbeat,
                s.created_at,
                s.thread_channel_id::TEXT AS thread_channel_id,
                COALESCE(
                    s.thread_channel_id::TEXT,
                    a.discord_channel_id,
                    a.discord_channel_alt,
                    a.discord_channel_cc,
                    a.discord_channel_cdx
                ) AS runtime_channel_id
         FROM sessions s
         LEFT JOIN agents a ON a.id = s.agent_id
         WHERE s.agent_id = $1
         ORDER BY s.last_heartbeat DESC NULLS LAST, s.created_at DESC NULLS LAST, s.id DESC",
    )
    .bind(agent_id)
    .fetch_all(pool)
    .await?;

    let rows = rows
        .into_iter()
        .map(|row| {
            Ok(AgentTurnSessionRow {
                session_key: row.try_get("session_key")?,
                provider: row.try_get("provider")?,
                raw_status: row.try_get("status")?,
                active_dispatch_id: row.try_get("active_dispatch_id")?,
                last_heartbeat: pg_timestamp_to_rfc3339(
                    row.try_get::<Option<DateTime<Utc>>, _>("last_heartbeat")?,
                ),
                created_at: pg_timestamp_to_rfc3339(
                    row.try_get::<Option<DateTime<Utc>>, _>("created_at")?,
                ),
                thread_channel_id: row.try_get("thread_channel_id")?,
                runtime_channel_id: row.try_get("runtime_channel_id")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;

    Ok(resolve_agent_turn_session_rows(rows))
}

pub async fn load_agent_turn_status_pg(
    pool: &sqlx::PgPool,
    agent_id: &str,
) -> Result<serde_json::Value, AgentTurnLookupError> {
    if !agent_exists_pg(pool, agent_id).await? {
        return Err(AgentTurnLookupError::AgentNotFound);
    }

    let session = find_agent_turn_session_pg(pool, agent_id).await?;
    Ok(build_agent_turn_status(agent_id, session).await)
}

async fn build_agent_turn_status(
    agent_id: &str,
    session: Option<AgentTurnSession>,
) -> serde_json::Value {
    let Some(session) = session else {
        return json!({
            "agent_id": agent_id,
            "status": "idle",
            "started_at": serde_json::Value::Null,
            "updated_at": serde_json::Value::Null,
            "recent_output": serde_json::Value::Null,
            "recent_output_source": "none",
            "session_key": serde_json::Value::Null,
            "tmux_session": serde_json::Value::Null,
            "provider": serde_json::Value::Null,
            "thread_channel_id": serde_json::Value::Null,
            "active_dispatch_id": serde_json::Value::Null,
            "last_heartbeat": serde_json::Value::Null,
            "current_tool_line": serde_json::Value::Null,
            "prev_tool_status": serde_json::Value::Null,
            "tool_events": Vec::<TurnToolEvent>::new(),
            "tool_count": 0,
        });
    };

    if !session.is_working {
        return json!({
            "agent_id": agent_id,
            "status": "idle",
            "started_at": serde_json::Value::Null,
            "updated_at": serde_json::Value::Null,
            "recent_output": serde_json::Value::Null,
            "recent_output_source": "none",
            "session_key": session.session_key,
            "tmux_session": extract_tmux_name(&session.session_key),
            "provider": session.provider,
            "thread_channel_id": session.thread_channel_id,
            "active_dispatch_id": serde_json::Value::Null,
            "last_heartbeat": session.last_heartbeat,
            "current_tool_line": serde_json::Value::Null,
            "prev_tool_status": serde_json::Value::Null,
            "tool_events": Vec::<TurnToolEvent>::new(),
            "tool_count": 0,
        });
    }

    let tmux_name = extract_tmux_name(&session.session_key);
    let inflight = load_inflight_snapshot(session.provider.as_deref(), tmux_name.as_deref());
    let started_at = inflight
        .as_ref()
        .and_then(|snapshot| snapshot.started_at.clone())
        .or(session.created_at.clone());
    let (recent_output, recent_output_source) = if let Some(ref tmux_name) = tmux_name {
        let tmux_name = tmux_name.clone();
        match tokio::task::spawn_blocking(move || capture_recent_tmux_output(&tmux_name)).await {
            Ok(Some(output)) => (Some(output), "tmux"),
            _ => match inflight.as_ref().and_then(inflight_recent_output) {
                Some(output) => (Some(output), "inflight"),
                None => (None, "none"),
            },
        }
    } else {
        match inflight.as_ref().and_then(inflight_recent_output) {
            Some(output) => (Some(output), "inflight"),
            None => (None, "none"),
        }
    };
    let tool_events = collect_turn_tool_events(recent_output.as_deref(), inflight.as_ref());
    let tool_count = tool_events
        .iter()
        .filter(|event| event.kind == "tool")
        .count();

    json!({
        "agent_id": agent_id,
        "status": session.effective_status,
        "started_at": started_at,
        "updated_at": inflight.as_ref().and_then(|snapshot| snapshot.updated_at.clone()),
        "recent_output": recent_output,
        "recent_output_source": recent_output_source,
        "session_key": session.session_key,
        "tmux_session": tmux_name,
        "provider": session.provider,
        "thread_channel_id": session.thread_channel_id,
        "active_dispatch_id": session.effective_active_dispatch_id,
        "last_heartbeat": session.last_heartbeat,
        "current_tool_line": inflight.as_ref().and_then(|snapshot| snapshot.current_tool_line.as_deref()).and_then(sanitize_status_line),
        "prev_tool_status": inflight.as_ref().and_then(|snapshot| snapshot.prev_tool_status.as_deref()).and_then(sanitize_status_line),
        "tool_events": tool_events,
        "tool_count": tool_count,
    })
}

pub async fn list_agent_turn_history_pg_json(
    pool: &sqlx::PgPool,
    agent_id: &str,
    limit: usize,
) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT st.id,
                st.turn_id,
                st.session_key,
                st.channel_id,
                st.agent_id,
                st.provider,
                st.dispatch_id,
                td.kanban_card_id,
                td.title AS dispatch_title,
                kc.title AS card_title,
                kc.github_issue_number,
                st.user_message,
                st.assistant_message,
                st.events_json::text AS events_json,
                st.duration_ms::BIGINT AS duration_ms,
                to_char(st.created_at, 'YYYY-MM-DD HH24:MI:SS') AS created_at
         FROM session_transcripts st
         LEFT JOIN sessions s ON s.session_key = st.session_key
         LEFT JOIN task_dispatches td ON td.id = st.dispatch_id
         LEFT JOIN kanban_cards kc ON kc.id = td.kanban_card_id
         WHERE COALESCE(NULLIF(BTRIM(st.agent_id), ''), NULLIF(BTRIM(s.agent_id), '')) = $1
            OR (
                COALESCE(NULLIF(BTRIM(st.agent_id), ''), NULLIF(BTRIM(s.agent_id), '')) IS NULL
                AND td.to_agent_id = $1
            )
         ORDER BY st.created_at DESC, st.id DESC
         LIMIT $2",
    )
    .bind(agent_id)
    .bind(clamp_api_limit(Some(limit)) as i64)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|row| {
            let events = row
                .try_get::<Option<String>, _>("events_json")
                .ok()
                .flatten()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
            transcript_json(
                row.try_get::<i64, _>("id").unwrap_or(0),
                row.try_get::<String, _>("turn_id").unwrap_or_default(),
                row.try_get::<Option<String>, _>("session_key")
                    .ok()
                    .flatten(),
                row.try_get::<Option<String>, _>("channel_id")
                    .ok()
                    .flatten(),
                row.try_get::<Option<String>, _>("agent_id").ok().flatten(),
                row.try_get::<Option<String>, _>("provider").ok().flatten(),
                row.try_get::<Option<String>, _>("dispatch_id")
                    .ok()
                    .flatten(),
                row.try_get::<Option<String>, _>("kanban_card_id")
                    .ok()
                    .flatten(),
                row.try_get::<Option<String>, _>("dispatch_title")
                    .ok()
                    .flatten(),
                row.try_get::<Option<String>, _>("card_title")
                    .ok()
                    .flatten(),
                row.try_get::<Option<i64>, _>("github_issue_number")
                    .ok()
                    .flatten(),
                row.try_get::<String, _>("user_message").unwrap_or_default(),
                row.try_get::<String, _>("assistant_message")
                    .unwrap_or_default(),
                events,
                row.try_get::<Option<i64>, _>("duration_ms").ok().flatten(),
                row.try_get::<String, _>("created_at").unwrap_or_default(),
            )
        })
        .collect())
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

    fn session_row(session_key: &str, raw_status: &str) -> AgentTurnSessionRow {
        AgentTurnSessionRow {
            session_key: session_key.to_string(),
            provider: Some("codex".to_string()),
            raw_status: Some(raw_status.to_string()),
            active_dispatch_id: None,
            last_heartbeat: Some("2026-05-06T03:45:52Z".to_string()),
            created_at: Some("2026-05-06T03:40:00Z".to_string()),
            thread_channel_id: Some("thread-1".to_string()),
            runtime_channel_id: Some("thread-1".to_string()),
        }
    }

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
    fn turn_lookup_prefers_working_session_over_latest_idle() {
        let selected = resolve_agent_turn_session_rows(vec![
            session_row("remote:new-idle", "idle"),
            session_row("remote:background", "awaiting_bg"),
        ])
        .expect("selected turn session");

        assert_eq!(selected.session_key, "remote:background");
        assert_eq!(selected.effective_status, "awaiting_bg");
        assert!(selected.is_working);
    }

    #[test]
    fn turn_lookup_falls_back_to_latest_session_when_none_working() {
        let selected = resolve_agent_turn_session_rows(vec![
            session_row("remote:latest-idle", "idle"),
            session_row("remote:older-disconnected", "disconnected"),
        ])
        .expect("selected turn session");

        assert_eq!(selected.session_key, "remote:latest-idle");
        assert_eq!(selected.effective_status, "idle");
        assert_eq!(selected.thread_channel_id.as_deref(), Some("thread-1"));
        assert!(!selected.is_working);
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
