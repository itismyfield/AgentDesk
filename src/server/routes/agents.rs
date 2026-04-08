use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use regex::Regex;
use serde::Deserialize;
use serde_json::json;
use std::sync::OnceLock;

use super::AppState;
use super::session_activity::SessionActivityResolver;

// ── Query types ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct TimelineQuery {
    pub limit: Option<i64>,
}

const TURN_CAPTURE_SCROLLBACK_LINES: i32 = -80;
const TURN_CAPTURE_TAIL_LINES: usize = 60;
const TURN_OUTPUT_MAX_CHARS: usize = 4000;

#[derive(Debug, Clone)]
struct AgentTurnSession {
    session_key: String,
    provider: Option<String>,
    last_heartbeat: Option<String>,
    created_at: Option<String>,
    thread_channel_id: Option<String>,
    effective_status: &'static str,
    effective_active_dispatch_id: Option<String>,
    is_working: bool,
}

#[derive(Debug, Clone, Default)]
struct InflightTurnSnapshot {
    started_at: Option<String>,
    current_tool_line: Option<String>,
    full_response: Option<String>,
}

fn agent_exists(conn: &rusqlite::Connection, id: &str) -> bool {
    conn.query_row("SELECT COUNT(*) FROM agents WHERE id = ?1", [id], |row| {
        row.get::<_, i64>(0)
    })
    .map(|count| count > 0)
    .unwrap_or(false)
}

fn extract_tmux_name(session_key: &str) -> Option<String> {
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

fn normalize_recent_output(text: &str) -> Option<String> {
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

fn capture_recent_tmux_output(tmux_name: &str) -> Option<String> {
    let capture =
        crate::services::platform::tmux::capture_pane(tmux_name, TURN_CAPTURE_SCROLLBACK_LINES)?;
    normalize_recent_output(&capture)
}

fn load_inflight_snapshot(
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
                current_tool_line: state
                    .get("current_tool_line")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
                full_response: state
                    .get("full_response")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
            });
        }
    }

    None
}

fn inflight_recent_output(snapshot: &InflightTurnSnapshot) -> Option<String> {
    let mut sections = Vec::new();
    if let Some(tool_line) = snapshot
        .current_tool_line
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        sections.push(tool_line.to_string());
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

fn find_agent_turn_session(
    conn: &rusqlite::Connection,
    agent_id: &str,
) -> Result<Option<AgentTurnSession>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT COALESCE(session_key, ''), provider, status, active_dispatch_id,
                last_heartbeat, created_at, thread_channel_id
         FROM sessions
         WHERE agent_id = ?1
         ORDER BY last_heartbeat DESC, created_at DESC, id DESC",
    )?;

    let rows = stmt.query_map([agent_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, Option<String>>(6)?,
        ))
    })?;

    let mut resolver = SessionActivityResolver::new();
    let mut latest = None;

    for row in rows {
        let (
            session_key,
            provider,
            raw_status,
            active_dispatch_id,
            last_heartbeat,
            created_at,
            thread_channel_id,
        ) = row?;
        let session_key_ref = (!session_key.trim().is_empty()).then_some(session_key.as_str());
        let effective = resolver.resolve(
            session_key_ref,
            raw_status.as_deref(),
            active_dispatch_id.as_deref(),
            last_heartbeat.as_deref(),
        );
        let candidate = AgentTurnSession {
            session_key,
            provider,
            last_heartbeat,
            created_at,
            thread_channel_id,
            effective_status: effective.status,
            effective_active_dispatch_id: effective.active_dispatch_id,
            is_working: effective.is_working,
        };
        if latest.is_none() {
            latest = Some(candidate.clone());
        }
        if candidate.is_working {
            return Ok(Some(candidate));
        }
    }

    Ok(latest)
}

// ── Handlers ─────────────────────────────────────────────────

/// GET /api/agents/:id/offices
pub async fn agent_offices(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    // Check agent exists
    let exists: bool = conn
        .query_row("SELECT COUNT(*) FROM agents WHERE id = ?1", [&id], |row| {
            row.get::<_, i64>(0)
        })
        .map(|c| c > 0)
        .unwrap_or(false);

    if !exists {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "agent not found"})),
        );
    }

    let mut stmt = match conn.prepare(
        "SELECT o.id, o.name, o.layout, oa.department_id, oa.joined_at
         FROM office_agents oa
         INNER JOIN offices o ON o.id = oa.office_id
         WHERE oa.agent_id = ?1
         ORDER BY o.id",
    ) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("prepare: {e}")})),
            );
        }
    };

    let rows = stmt
        .query_map([&id], |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, Option<String>>(1)?,
                "layout": row.get::<_, Option<String>>(2)?,
                "assigned": true,
                "office_department_id": row.get::<_, Option<String>>(3)?,
                "joined_at": row.get::<_, Option<String>>(4)?,
            }))
        })
        .ok();

    let offices: Vec<serde_json::Value> = match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect(),
        None => Vec::new(),
    };

    (StatusCode::OK, Json(json!({"offices": offices})))
}

/// GET /api/agents/:id/cron
#[allow(dead_code)]
pub async fn agent_cron(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    // Check agent exists
    let exists: bool = conn
        .query_row("SELECT COUNT(*) FROM agents WHERE id = ?1", [&id], |row| {
            row.get::<_, i64>(0)
        })
        .map(|c| c > 0)
        .unwrap_or(false);

    if !exists {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "agent not found"})),
        );
    }

    // Stub: no cron table yet
    (StatusCode::OK, Json(json!({"jobs": []})))
}

/// GET /api/agents/:id/skills
pub async fn agent_skills(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    // Check agent exists
    let exists: bool = conn
        .query_row("SELECT COUNT(*) FROM agents WHERE id = ?1", [&id], |row| {
            row.get::<_, i64>(0)
        })
        .map(|c| c > 0)
        .unwrap_or(false);

    if !exists {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "agent not found"})),
        );
    }

    // Query skills used by this agent (via skill_usage join)
    let mut stmt = match conn.prepare(
        "SELECT DISTINCT s.id, s.name, s.description, s.source_path, s.trigger_patterns, s.updated_at
         FROM skills s
         INNER JOIN skill_usage su ON su.skill_id = s.id
         WHERE su.agent_id = ?1
         ORDER BY s.id",
    ) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("prepare: {e}")})),
            )
        }
    };

    let rows = stmt
        .query_map([&id], |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, Option<String>>(1)?,
                "description": row.get::<_, Option<String>>(2)?,
                "source_path": row.get::<_, Option<String>>(3)?,
                "trigger_patterns": row.get::<_, Option<String>>(4)?,
                "updated_at": row.get::<_, Option<String>>(5)?,
            }))
        })
        .ok();

    let skills: Vec<serde_json::Value> = match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect(),
        None => Vec::new(),
    };

    let total_count = skills.len();

    (
        StatusCode::OK,
        Json(json!({
            "skills": skills,
            "sharedSkills": [],
            "totalCount": total_count,
        })),
    )
}

/// GET /api/agents/:id/dispatched-sessions
pub async fn agent_dispatched_sessions(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    if !agent_exists(&conn, &id) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "agent not found"})),
        );
    }

    let mut stmt = match conn.prepare(
        "SELECT id, session_key, agent_id, provider, status, active_dispatch_id,
                model, tokens, cwd, last_heartbeat, thread_channel_id
         FROM sessions
         WHERE agent_id = ?1
         ORDER BY id",
    ) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("prepare: {e}")})),
            );
        }
    };

    let rows = stmt
        .query_map([&id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<String>>(10)?,
            ))
        })
        .ok();

    let mut resolver = SessionActivityResolver::new();
    let sessions: Vec<serde_json::Value> = match rows {
        Some(iter) => iter
            .filter_map(|r| r.ok())
            .map(
                |(
                    session_id,
                    session_key,
                    agent_id,
                    provider,
                    status,
                    active_dispatch_id,
                    model,
                    tokens,
                    cwd,
                    last_heartbeat,
                    thread_channel_id,
                )| {
                    let effective = resolver.resolve(
                        session_key.as_deref(),
                        status.as_deref(),
                        active_dispatch_id.as_deref(),
                        last_heartbeat.as_deref(),
                    );
                    json!({
                        "id": session_id,
                        "session_key": session_key,
                        "agent_id": agent_id,
                        "provider": provider,
                        "status": effective.status,
                        "active_dispatch_id": effective.active_dispatch_id,
                        "model": model,
                        "tokens": tokens,
                        "cwd": cwd,
                        "last_heartbeat": last_heartbeat,
                        "thread_channel_id": thread_channel_id,
                    })
                },
            )
            .collect(),
        None => Vec::new(),
    };

    (StatusCode::OK, Json(json!({"sessions": sessions})))
}

/// GET /api/agents/:id/turn
pub async fn agent_turn(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let session = {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };

        if !agent_exists(&conn, &id) {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "agent not found"})),
            );
        }

        match find_agent_turn_session(&conn, &id) {
            Ok(session) => session,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("query: {e}")})),
                );
            }
        }
    };

    let Some(session) = session else {
        return (
            StatusCode::OK,
            Json(json!({
                "agent_id": id,
                "status": "idle",
                "started_at": serde_json::Value::Null,
                "recent_output": serde_json::Value::Null,
                "session_key": serde_json::Value::Null,
                "tmux_session": serde_json::Value::Null,
                "provider": serde_json::Value::Null,
                "thread_channel_id": serde_json::Value::Null,
                "active_dispatch_id": serde_json::Value::Null,
                "last_heartbeat": serde_json::Value::Null,
            })),
        );
    };

    if !session.is_working {
        return (
            StatusCode::OK,
            Json(json!({
                "agent_id": id,
                "status": "idle",
                "started_at": serde_json::Value::Null,
                "recent_output": serde_json::Value::Null,
                "session_key": session.session_key,
                "tmux_session": extract_tmux_name(&session.session_key),
                "provider": session.provider,
                "thread_channel_id": session.thread_channel_id,
                "active_dispatch_id": serde_json::Value::Null,
                "last_heartbeat": session.last_heartbeat,
            })),
        );
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

    (
        StatusCode::OK,
        Json(json!({
            "agent_id": id,
            "status": session.effective_status,
            "started_at": started_at,
            "recent_output": recent_output,
            "recent_output_source": recent_output_source,
            "session_key": session.session_key,
            "tmux_session": tmux_name,
            "provider": session.provider,
            "thread_channel_id": session.thread_channel_id,
            "active_dispatch_id": session.effective_active_dispatch_id,
            "last_heartbeat": session.last_heartbeat,
        })),
    )
}

/// POST /api/agents/:id/turn/stop
pub async fn stop_agent_turn(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let session = {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };

        if !agent_exists(&conn, &id) {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "agent not found"})),
            );
        }

        match find_agent_turn_session(&conn, &id) {
            Ok(session) => session,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("query: {e}")})),
                );
            }
        }
    };

    let Some(session) = session.filter(|candidate| candidate.is_working) else {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "no active turn found for agent",
                "agent_id": id,
                "status": "idle",
            })),
        );
    };

    if session.session_key.trim().is_empty() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "active session is missing session_key"})),
        );
    }

    let session_key = session.session_key.clone();
    let (status, Json(mut body)) =
        super::dispatched_sessions::force_kill_session_impl(&state, &session_key, false).await;
    body["agent_id"] = json!(id);
    body["session_key"] = json!(session_key);
    body["status"] = json!(if status == StatusCode::OK {
        "stopped"
    } else {
        "error"
    });
    (status, Json(body))
}

/// GET /api/agents/:id/timeline?limit=30
pub async fn agent_timeline(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<TimelineQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    // Check agent exists
    let exists: bool = conn
        .query_row("SELECT COUNT(*) FROM agents WHERE id = ?1", [&id], |row| {
            row.get::<_, i64>(0)
        })
        .map(|c| c > 0)
        .unwrap_or(false);

    if !exists {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "agent not found"})),
        );
    }

    let limit = params.limit.unwrap_or(30);

    let sql = "
        SELECT id, source, type, title, status, timestamp, duration_ms FROM (
            SELECT
                id,
                'dispatch' AS source,
                COALESCE(dispatch_type, 'task') AS type,
                title,
                status,
                CAST(strftime('%s', created_at) AS INTEGER) * 1000 AS timestamp,
                CASE
                    WHEN updated_at IS NOT NULL AND created_at IS NOT NULL
                    THEN (CAST(strftime('%s', updated_at) AS INTEGER) - CAST(strftime('%s', created_at) AS INTEGER)) * 1000
                    ELSE NULL
                END AS duration_ms
            FROM task_dispatches
            WHERE to_agent_id = ?1 OR from_agent_id = ?1

            UNION ALL

            SELECT
                CAST(id AS TEXT),
                'session' AS source,
                'session' AS type,
                COALESCE(session_key, 'session') AS title,
                status,
                CAST(strftime('%s', created_at) AS INTEGER) * 1000 AS timestamp,
                CASE
                    WHEN last_heartbeat IS NOT NULL AND created_at IS NOT NULL
                    THEN (CAST(strftime('%s', last_heartbeat) AS INTEGER) - CAST(strftime('%s', created_at) AS INTEGER)) * 1000
                    ELSE NULL
                END AS duration_ms
            FROM sessions
            WHERE agent_id = ?1

            UNION ALL

            SELECT
                id,
                'kanban' AS source,
                'card' AS type,
                title,
                status,
                CAST(strftime('%s', created_at) AS INTEGER) * 1000 AS timestamp,
                CASE
                    WHEN updated_at IS NOT NULL AND created_at IS NOT NULL
                    THEN (CAST(strftime('%s', updated_at) AS INTEGER) - CAST(strftime('%s', created_at) AS INTEGER)) * 1000
                    ELSE NULL
                END AS duration_ms
            FROM kanban_cards
            WHERE assigned_agent_id = ?1
        )
        ORDER BY timestamp DESC
        LIMIT ?2
    ";

    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("prepare: {e}")})),
            );
        }
    };

    let rows = stmt
        .query_map(rusqlite::params![id, limit], |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "source": row.get::<_, String>(1)?,
                "type": row.get::<_, String>(2)?,
                "title": row.get::<_, Option<String>>(3)?,
                "status": row.get::<_, Option<String>>(4)?,
                "timestamp": row.get::<_, Option<i64>>(5)?,
                "duration_ms": row.get::<_, Option<i64>>(6)?,
            }))
        })
        .ok();

    let events: Vec<serde_json::Value> = match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect(),
        None => Vec::new(),
    };

    (StatusCode::OK, Json(json!({"events": events})))
}

/// POST /api/agents/:id/signal
/// Agent sends a status signal (e.g., "blocked" with reason).
pub async fn agent_signal(
    State(state): State<super::AppState>,
    axum::extract::Path(agent_id): axum::extract::Path<String>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let signal = body.get("signal").and_then(|v| v.as_str()).unwrap_or("");
    let reason = body.get("reason").and_then(|v| v.as_str()).unwrap_or("");

    if signal != "blocked" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("unknown signal: {signal}. supported: blocked")})),
        );
    }

    // Find active card for this agent
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let card_id: Option<String> = conn
        .query_row(
            "SELECT id FROM kanban_cards WHERE assigned_agent_id = ?1 AND status = 'in_progress' ORDER BY updated_at DESC LIMIT 1",
            [&agent_id],
            |row| row.get(0),
        )
        .ok();

    let Some(card_id) = card_id else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no active card for agent"})),
        );
    };

    conn.execute(
        "UPDATE kanban_cards SET blocked_reason = ?1 WHERE id = ?2",
        rusqlite::params![reason, card_id],
    )
    .ok();
    drop(conn);

    let _ = crate::kanban::transition_status_with_opts(
        &state.db,
        &state.engine,
        &card_id,
        "blocked",
        "agent-signal",
        true,
    );

    (
        StatusCode::OK,
        Json(json!({"ok": true, "card_id": card_id, "signal": signal})),
    )
}

/// GET /api/agent-channels
/// Returns agent ID → Discord channel mapping.
pub async fn agent_channels(
    State(state): State<super::AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let mut stmt = conn
        .prepare(
            "SELECT id, name, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
             FROM agents ORDER BY id",
        )
        .unwrap();

    let channels: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            Ok(json!({
                "agent_id": row.get::<_, String>(0)?,
                "name": row.get::<_, Option<String>>(1)?,
                "channel_id": row.get::<_, Option<String>>(2)?,
                "channel_alt": row.get::<_, Option<String>>(3)?,
                "channel_cc": row.get::<_, Option<String>>(4)?,
                "channel_cdx": row.get::<_, Option<String>>(5)?,
            }))
        })
        .ok()
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();

    (StatusCode::OK, Json(json!({"channels": channels})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::engine::PolicyEngine;

    fn test_db() -> Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    fn test_engine(db: &Db) -> PolicyEngine {
        let config = crate::config::Config::default();
        PolicyEngine::new(&config, db.clone()).unwrap()
    }

    #[tokio::test]
    async fn agent_dispatched_sessions_include_thread_channel_id() {
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db.clone(), engine);

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, provider, status, xp) VALUES ('project-agentdesk', 'AgentDesk', 'codex', 'idle', 0)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO sessions (session_key, agent_id, provider, status, active_dispatch_id, thread_channel_id, last_heartbeat)
                 VALUES (?1, 'project-agentdesk', 'codex', 'working', 'dispatch-1', '1485506232256168011', datetime('now'))",
                ["mac-mini:AgentDesk-codex-adk-cdx-t1485506232256168011"],
            )
            .unwrap();
        }

        let (status, Json(body)) =
            agent_dispatched_sessions(State(state), Path("project-agentdesk".to_string())).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["sessions"][0]["thread_channel_id"],
            serde_json::Value::String("1485506232256168011".to_string())
        );
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
}
