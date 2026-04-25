use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Row;
use std::sync::OnceLock;

use super::AppState;
use super::session_activity::SessionActivityResolver;
use crate::services::provider::ProviderKind;
use crate::services::turn_lifecycle::{TurnLifecycleTarget, stop_turn_preserving_queue};

// ── Query types ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct TimelineQuery {
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct TranscriptQuery {
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct AgentQualityQuery {
    pub days: Option<i64>,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct AgentQualityRankingQuery {
    pub limit: Option<usize>,
    /// Which metric to rank by. One of `turn_success_rate` (default) or
    /// `review_pass_rate`.
    pub metric: Option<String>,
    /// Which rolling window to use. One of `7d` (default) or `30d`.
    pub window: Option<String>,
    /// Override the minimum sample_size threshold. Defaults to 5
    /// (`QUALITY_SAMPLE_GUARD`).
    pub min_sample_size: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct StartAgentTurnBody {
    pub prompt: String,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub source: Option<String>,
    /// Optional provider override: "claude" or "codex".
    /// When set, the turn runs on that provider's channel binding instead
    /// of the agent's primary channel — lets external babysitters drive
    /// either side without going through the command bot.
    #[serde(default)]
    pub provider: Option<String>,
    /// Optional explicit channel override (Discord channel id or alias).
    /// Takes precedence over `provider` when both are set.
    #[serde(default)]
    pub channel_id: Option<String>,
}

const TURN_CAPTURE_SCROLLBACK_LINES: i32 = -80;
const TURN_CAPTURE_TAIL_LINES: usize = 60;
const TURN_OUTPUT_MAX_CHARS: usize = 4000;

/// GET /api/agents/{id}/quality
pub async fn agent_quality(
    Path(id): Path<String>,
    Query(query): Query<AgentQualityQuery>,
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    match crate::services::observability::query_agent_quality_summary(
        &state.db,
        state.pg_pool_ref(),
        &id,
        query.days.unwrap_or(30),
        query.limit.unwrap_or(60),
    )
    .await
    {
        Ok(summary) => (StatusCode::OK, Json(json!(summary))),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("query agent quality summary: {error}")})),
        ),
    }
}

/// GET /api/agents/quality/ranking
pub async fn agents_quality_ranking(
    Query(query): Query<AgentQualityRankingQuery>,
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    use crate::services::observability::{QualityRankingMetric, QualityRankingWindow};
    let metric = QualityRankingMetric::parse(query.metric.as_deref());
    let window = QualityRankingWindow::parse(query.window.as_deref());
    let min_sample_size = query.min_sample_size.unwrap_or(5);
    match crate::services::observability::query_agent_quality_ranking_with(
        &state.db,
        state.pg_pool_ref(),
        query.limit.unwrap_or(50),
        metric,
        window,
        min_sample_size,
    )
    .await
    {
        Ok(ranking) => (StatusCode::OK, Json(json!(ranking))),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("query agent quality ranking: {error}")})),
        ),
    }
}

#[derive(Debug, Clone)]
struct AgentTurnSession {
    session_key: String,
    provider: Option<String>,
    last_heartbeat: Option<String>,
    created_at: Option<String>,
    thread_channel_id: Option<String>,
    runtime_channel_id: Option<String>,
    effective_status: &'static str,
    effective_active_dispatch_id: Option<String>,
    is_working: bool,
}

#[derive(Debug, Clone, Default)]
struct InflightTurnSnapshot {
    started_at: Option<String>,
    updated_at: Option<String>,
    current_tool_line: Option<String>,
    prev_tool_status: Option<String>,
    full_response: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct TurnToolEvent {
    kind: &'static str,
    status: &'static str,
    tool_name: Option<String>,
    summary: String,
    line: String,
}

#[derive(Debug, Clone)]
struct ParsedTurnToolEvent {
    event: TurnToolEvent,
    identity_kind: &'static str,
    identity_value: String,
}

fn agent_exists(conn: &libsql_rusqlite::Connection, id: &str) -> bool {
    conn.query_row("SELECT COUNT(*) FROM agents WHERE id = ?1", [id], |row| {
        row.get::<_, i64>(0)
    })
    .map(|count| count > 0)
    .unwrap_or(false)
}

fn resolve_channel_identifier(value: &str) -> Option<u64> {
    super::dispatches::resolve_channel_alias_pub(value).or_else(|| value.trim().parse::<u64>().ok())
}

fn channel_identifier_matches(left: &str, right: &str) -> bool {
    let left_trimmed = left.trim();
    let right_trimmed = right.trim();
    if left_trimmed.eq_ignore_ascii_case(right_trimmed) {
        return true;
    }

    match (
        resolve_channel_identifier(left_trimmed),
        resolve_channel_identifier(right_trimmed),
    ) {
        (Some(left_id), Some(right_id)) => left_id == right_id,
        _ => false,
    }
}

fn channel_override_is_allowed(
    override_channel: &str,
    bindings: &crate::db::agents::AgentChannelBindings,
) -> bool {
    bindings
        .all_channels()
        .into_iter()
        .any(|channel| channel_identifier_matches(&channel, override_channel))
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

fn sanitize_status_line(text: &str) -> Option<String> {
    let stripped = strip_ansi(text);
    let sanitized = sanitize_sensitive_text(stripped.trim());
    let normalized = sanitized.trim();
    (!normalized.is_empty()).then(|| normalized.to_string())
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
            });
        }
    }

    None
}

fn inflight_recent_output(snapshot: &InflightTurnSnapshot) -> Option<String> {
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

fn collect_turn_tool_events(
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

fn find_agent_turn_session(
    conn: &libsql_rusqlite::Connection,
    agent_id: &str,
) -> Result<Option<AgentTurnSession>, libsql_rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT COALESCE(s.session_key, ''), s.provider, s.status, s.active_dispatch_id,
                s.last_heartbeat, s.created_at, s.thread_channel_id,
                COALESCE(
                    s.thread_channel_id,
                    a.discord_channel_id,
                    a.discord_channel_alt,
                    a.discord_channel_cc,
                    a.discord_channel_cdx
                ) AS runtime_channel_id
         FROM sessions s
         LEFT JOIN agents a ON a.id = s.agent_id
         WHERE s.agent_id = ?1
         ORDER BY s.last_heartbeat DESC, s.created_at DESC, s.id DESC",
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
            row.get::<_, Option<String>>(7)?,
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
            runtime_channel_id,
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
            runtime_channel_id,
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

async fn agent_exists_pg(pool: &sqlx::PgPool, id: &str) -> Result<bool, sqlx::Error> {
    let row = sqlx::query("SELECT COUNT(*)::BIGINT AS count FROM agents WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await?;
    Ok(row.try_get::<i64, _>("count").unwrap_or(0) > 0)
}

fn pg_timestamp_to_rfc3339(value: Option<DateTime<Utc>>) -> Option<String> {
    value.map(|value| value.to_rfc3339())
}

async fn find_agent_turn_session_pg(
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

    let mut resolver = SessionActivityResolver::new();
    let mut latest = None;

    for row in rows {
        let session_key: String = row.try_get("session_key")?;
        let provider: Option<String> = row.try_get("provider")?;
        let raw_status: Option<String> = row.try_get("status")?;
        let active_dispatch_id: Option<String> = row.try_get("active_dispatch_id")?;
        let last_heartbeat = pg_timestamp_to_rfc3339(row.try_get("last_heartbeat")?);
        let created_at = pg_timestamp_to_rfc3339(row.try_get("created_at")?);
        let thread_channel_id: Option<String> = row.try_get("thread_channel_id")?;
        let runtime_channel_id: Option<String> = row.try_get("runtime_channel_id")?;
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
            runtime_channel_id,
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
    let conn = match state.sqlite_db().lock() {
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
    let conn = match state.sqlite_db().lock() {
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
    let conn = match state.sqlite_db().lock() {
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
    let conn = match state.sqlite_db().lock() {
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
        let conn = match state.sqlite_db().lock() {
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
    let tool_events = collect_turn_tool_events(recent_output.as_deref(), inflight.as_ref());
    let tool_count = tool_events
        .iter()
        .filter(|event| event.kind == "tool")
        .count();

    (
        StatusCode::OK,
        Json(json!({
            "agent_id": id,
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
        })),
    )
}

/// POST /api/agents/:id/turn/start
pub async fn start_agent_turn(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<StartAgentTurnBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let prompt = body.prompt.trim();
    if prompt.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "prompt is required"})),
        );
    }

    let provider_override = body
        .provider
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let channel_override = body
        .channel_id
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let (provider, primary_channel) = {
        let conn = match state.sqlite_db().lock() {
            Ok(conn) => conn,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"ok": false, "error": format!("{error}")})),
                );
            }
        };

        if !agent_exists(&conn, &id) {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"ok": false, "error": "agent not found"})),
            );
        }

        let Some(bindings) = crate::db::agents::load_agent_channel_bindings(&conn, &id)
            .map_err(|error| error.to_string())
            .ok()
            .flatten()
        else {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"ok": false, "error": "agent channel binding not found"})),
            );
        };

        if let Some(channel_override) = channel_override.as_deref()
            && !channel_override_is_allowed(channel_override, &bindings)
        {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "ok": false,
                    "error": format!(
                        "channel override {} is not allowed for agent {}",
                        channel_override,
                        id
                    ),
                })),
            );
        }

        let provider = match provider_override.as_deref() {
            Some(raw) => match ProviderKind::from_str(raw) {
                Some(kind) => kind,
                None => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({
                            "ok": false,
                            "error": format!("unsupported provider override: {raw}"),
                        })),
                    );
                }
            },
            None => {
                let Some(kind) = bindings.resolved_primary_provider_kind() else {
                    return (
                        StatusCode::CONFLICT,
                        Json(
                            json!({"ok": false, "error": "agent primary provider is not configured"}),
                        ),
                    );
                };
                kind
            }
        };

        let primary_channel = if let Some(chan) = channel_override.clone() {
            chan
        } else if provider_override.is_some() {
            let Some(chan) = bindings.channel_for_provider(provider_override.as_deref()) else {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "ok": false,
                        "error": format!(
                            "agent has no channel bound for provider {}",
                            provider_override.as_deref().unwrap_or("")
                        ),
                    })),
                );
            };
            chan
        } else {
            let Some(chan) = bindings.primary_channel() else {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({"ok": false, "error": "agent primary channel is not configured"})),
                );
            };
            chan
        };

        (provider, primary_channel)
    };

    let Some(channel_id_num) = super::dispatches::resolve_channel_alias_pub(&primary_channel)
        .or_else(|| primary_channel.parse::<u64>().ok())
    else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "ok": false,
                "error": format!("agent primary channel is invalid: {}", primary_channel),
            })),
        );
    };

    let Some(registry) = state.health_registry.as_deref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok": false, "error": "discord runtime health registry unavailable"})),
        );
    };

    let channel_name_hint = primary_channel
        .chars()
        .all(|ch| ch.is_ascii_digit())
        .then_some(None)
        .unwrap_or_else(|| Some(primary_channel.clone()));

    match crate::services::discord::health::start_headless_agent_turn(
        registry,
        poise::serenity_prelude::ChannelId::new(channel_id_num),
        provider,
        prompt.to_string(),
        body.source,
        body.metadata,
        channel_name_hint,
    )
    .await
    {
        Ok(outcome) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "turn_id": outcome.turn_id,
                "status": "started",
            })),
        ),
        Err(crate::services::discord::HeadlessTurnStartError::Conflict(error)) => (
            StatusCode::CONFLICT,
            Json(json!({
                "ok": false,
                "error": error,
                "status": "conflict",
            })),
        ),
        Err(crate::services::discord::HeadlessTurnStartError::Internal(error)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "ok": false,
                "error": error,
            })),
        ),
    }
}

/// POST /api/agents/:id/turn/stop
pub async fn stop_agent_turn(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let session = if let Some(pool) = state.pg_pool.as_ref() {
        match agent_exists_pg(pool, &id).await {
            Ok(true) => {}
            Ok(false) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "agent not found"})),
                );
            }
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("query: {e}")})),
                );
            }
        }

        match find_agent_turn_session_pg(pool, &id).await {
            Ok(session) => session,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("query: {e}")})),
                );
            }
        }
    } else {
        let conn = match state.sqlite_db().lock() {
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
    let tmux_name = session_key.split(':').next_back().unwrap_or(&session_key);
    let lifecycle = stop_turn_preserving_queue(
        state.health_registry.as_deref(),
        &TurnLifecycleTarget {
            provider: session.provider.as_deref().and_then(ProviderKind::from_str),
            channel_id: session
                .runtime_channel_id
                .as_deref()
                .and_then(|value| value.parse::<u64>().ok())
                .map(poise::serenity_prelude::ChannelId::new),
            tmux_name: tmux_name.to_string(),
        },
        &format!("사용자가 {id} 에이전트 턴 수동 중단 (POST /api/agents/{id}/turn/stop)"),
    )
    .await;

    if let Some(pool) = state.pg_pool.as_ref() {
        sqlx::query(
            "UPDATE sessions
             SET status = 'disconnected',
                 active_dispatch_id = NULL,
                 claude_session_id = NULL,
                 raw_provider_session_id = NULL
             WHERE session_key = $1",
        )
        .bind(&session_key)
        .execute(pool)
        .await
        .ok();
    } else if let Ok(conn) = state.sqlite_db().lock() {
        conn.execute(
            "UPDATE sessions
             SET status = 'disconnected',
                 active_dispatch_id = NULL,
                 claude_session_id = NULL,
                 raw_provider_session_id = NULL
             WHERE session_key = ?1",
            [&session_key],
        )
        .ok();
    }

    let status = StatusCode::OK;
    let Json(mut body) = Json(json!({
        "ok": true,
        "session_key": session_key,
        "tmux_session": tmux_name,
        "tmux_killed": lifecycle.tmux_killed,
        "lifecycle_path": lifecycle.lifecycle_path,
        "queued_remaining": lifecycle.queue_depth,
        "queue_preserved": lifecycle.queue_preserved,
    }));
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
    let conn = match state.sqlite_db().lock() {
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
        .query_map(libsql_rusqlite::params![id, limit], |row| {
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

/// GET /api/agents/:id/transcripts?limit=10
pub async fn agent_transcripts(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<TranscriptQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let agent_exists = if let Some(pool) = state.pg_pool_ref() {
        match sqlx::query("SELECT COUNT(*)::BIGINT AS count FROM agents WHERE id = $1")
            .bind(&id)
            .fetch_one(pool)
            .await
        {
            Ok(row) => row.try_get::<i64, _>("count").unwrap_or(0) > 0,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("query: {e}")})),
                );
            }
        }
    } else {
        let conn = match state.sqlite_db().read_conn() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };
        agent_exists(&conn, &id)
    };

    if !agent_exists {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "agent not found"})),
        );
    }

    match crate::db::session_transcripts::list_transcripts_for_agent_db(
        state.sqlite_db(),
        state.pg_pool_ref(),
        &id,
        params.limit.unwrap_or(8),
    )
    .await
    {
        Ok(transcripts) => (
            StatusCode::OK,
            Json(json!({
                "agent_id": id,
                "transcripts": transcripts,
            })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("transcripts: {e}")})),
        ),
    }
}

/// POST /api/agents/:id/signal
/// Agent sends an operational signal (e.g., "blocked" with reason).
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
    let conn = match state.sqlite_db().lock() {
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
        "UPDATE kanban_cards SET blocked_reason = ?1, updated_at = datetime('now') WHERE id = ?2",
        libsql_rusqlite::params![reason, card_id],
    )
    .ok();

    (
        StatusCode::OK,
        Json(json!({"ok": true, "card_id": card_id, "signal": signal})),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::db::session_transcripts::{
        PersistSessionTranscript, SessionTranscriptEvent, SessionTranscriptEventKind,
        persist_turn_on_conn,
    };
    use crate::engine::PolicyEngine;

    fn test_db() -> Db {
        let conn = libsql_rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    fn test_engine(db: &Db) -> PolicyEngine {
        let config = crate::config::Config::default();
        PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap()
    }

    #[test]
    fn pg_timestamp_to_rfc3339_keeps_timezone_marker_for_activity_resolution() {
        let timestamp = chrono::DateTime::parse_from_rfc3339("2026-04-24T10:15:30+09:00")
            .unwrap()
            .with_timezone(&Utc);

        let formatted = pg_timestamp_to_rfc3339(Some(timestamp)).unwrap();

        assert_eq!(formatted, "2026-04-24T01:15:30+00:00");
        assert!(chrono::DateTime::parse_from_rfc3339(&formatted).is_ok());
        assert_ne!(formatted, "2026-04-24 01:15:30");
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

    #[tokio::test]
    async fn agent_transcripts_returns_structured_events() {
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db.clone(), engine);

        {
            let mut conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, provider, status, xp) VALUES ('agent-transcript', 'Transcript Agent', 'codex', 'idle', 0)",
                [],
            )
            .unwrap();

            let events = vec![SessionTranscriptEvent {
                kind: SessionTranscriptEventKind::ToolUse,
                tool_name: Some("Bash".to_string()),
                summary: Some("cargo test".to_string()),
                content: "cargo test --no-run".to_string(),
                status: Some("success".to_string()),
                is_error: false,
            }];
            persist_turn_on_conn(
                &mut conn,
                PersistSessionTranscript {
                    turn_id: "discord:agent-transcript:1",
                    session_key: Some("host:agent-transcript"),
                    channel_id: Some("chan-1"),
                    agent_id: Some("agent-transcript"),
                    provider: Some("codex"),
                    dispatch_id: None,
                    user_message: "verify build",
                    assistant_message: "build verified",
                    events: &events,
                    duration_ms: Some(4200),
                },
            )
            .unwrap();
        }

        let (status, Json(body)) = agent_transcripts(
            State(state),
            Path("agent-transcript".to_string()),
            Query(TranscriptQuery { limit: Some(5) }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["agent_id"],
            serde_json::Value::String("agent-transcript".to_string())
        );
        assert_eq!(
            body["transcripts"][0]["turn_id"],
            "discord:agent-transcript:1"
        );
        assert!(body["transcripts"][0]["card_title"].is_null());
        assert!(body["transcripts"][0]["github_issue_number"].is_null());
        assert_eq!(body["transcripts"][0]["duration_ms"], 4200);
        assert_eq!(body["transcripts"][0]["events"][0]["kind"], "tool_use");
        assert_eq!(body["transcripts"][0]["events"][0]["tool_name"], "Bash");
    }

    #[tokio::test]
    async fn agent_transcripts_falls_back_to_session_agent_for_legacy_rows() {
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db.clone(), engine);

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, provider, status, xp) VALUES ('agent-transcript-fallback', 'Transcript Fallback', 'codex', 'idle', 0)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO sessions (session_key, agent_id, provider, status, last_heartbeat)
                 VALUES ('host:agent-transcript-fallback', 'agent-transcript-fallback', 'codex', 'idle', datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO session_transcripts (
                    turn_id, session_key, channel_id, agent_id, provider, dispatch_id, user_message, assistant_message, events_json
                 ) VALUES (
                    'discord:agent-transcript-fallback:1',
                    'host:agent-transcript-fallback',
                    'chan-fallback',
                    NULL,
                    'codex',
                    NULL,
                    'legacy question',
                    'legacy answer',
                    '[]'
                 )",
                [],
            )
            .unwrap();
        }

        let (status, Json(body)) = agent_transcripts(
            State(state),
            Path("agent-transcript-fallback".to_string()),
            Query(TranscriptQuery { limit: Some(5) }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["transcripts"].as_array().map(Vec::len), Some(1));
        assert_eq!(
            body["transcripts"][0]["turn_id"],
            "discord:agent-transcript-fallback:1"
        );
        assert_eq!(body["transcripts"][0]["agent_id"], serde_json::Value::Null);
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
