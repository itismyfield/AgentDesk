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
use crate::services::observability::session_inventory::{
    derive_visual_status, load_child_inventory_by_parent_key_pg,
};
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

fn pg_required_response() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": "postgres pool unavailable"})),
    )
}

/// GET /api/agents/{id}/quality
pub async fn agent_quality(
    Path(id): Path<String>,
    Query(query): Query<AgentQualityQuery>,
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    match crate::services::observability::query_agent_quality_summary(
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

#[derive(Debug, Clone)]
struct AgentDiagSession {
    session_key: String,
    agent_id: Option<String>,
    agent_name: Option<String>,
    provider: Option<String>,
    status: Option<String>,
    last_tool_at: Option<DateTime<Utc>>,
    active_children: i32,
    thread_channel_id: Option<String>,
    created_at: Option<DateTime<Utc>>,
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

async fn find_diag_session_pg(
    pool: &sqlx::PgPool,
    identifier: &str,
) -> Result<Option<AgentDiagSession>, sqlx::Error> {
    let identifier = identifier.trim();
    if identifier.is_empty() {
        return Ok(None);
    }

    let row = sqlx::query(
        "SELECT COALESCE(s.session_key, '') AS session_key,
                s.agent_id,
                a.name AS agent_name,
                s.provider,
                s.status,
                s.last_tool_at,
                COALESCE(s.active_children, 0) AS active_children,
                s.thread_channel_id::TEXT AS thread_channel_id,
                s.created_at
           FROM sessions s
           LEFT JOIN agents a ON a.id = s.agent_id
          WHERE s.agent_id = $1
             OR s.thread_channel_id::TEXT = $1
             OR a.discord_channel_id = $1
             OR a.discord_channel_alt = $1
             OR a.discord_channel_cc = $1
             OR a.discord_channel_cdx = $1
          ORDER BY CASE WHEN s.status = 'working' THEN 0 ELSE 1 END,
                   s.last_heartbeat DESC NULLS LAST,
                   s.last_tool_at DESC NULLS LAST,
                   s.created_at DESC NULLS LAST,
                   s.id DESC
          LIMIT 1",
    )
    .bind(identifier)
    .fetch_optional(pool)
    .await?;

    row.map(|row| {
        Ok(AgentDiagSession {
            session_key: row.try_get("session_key")?,
            agent_id: row.try_get("agent_id").ok().flatten(),
            agent_name: row.try_get("agent_name").ok().flatten(),
            provider: row.try_get("provider").ok().flatten(),
            status: row.try_get("status").ok().flatten(),
            last_tool_at: row.try_get("last_tool_at").ok().flatten(),
            active_children: row.try_get("active_children").unwrap_or(0),
            thread_channel_id: row.try_get("thread_channel_id").ok().flatten(),
            created_at: row.try_get("created_at").ok().flatten(),
        })
    })
    .transpose()
}

fn loop_suspicion(events: &[TurnToolEvent]) -> serde_json::Value {
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

// ── Handlers ─────────────────────────────────────────────────

/// GET /api/agents/diag/:identifier
pub async fn agent_diag(
    State(state): State<AppState>,
    Path(identifier): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_required_response();
    };

    let session = match find_diag_session_pg(pool, &identifier).await {
        Ok(Some(session)) => session,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "agent/channel session not found"})),
            );
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("query diag session: {error}")})),
            );
        }
    };

    let now = Utc::now();
    let visual = derive_visual_status(
        session.status.as_deref(),
        session.last_tool_at,
        session.active_children,
        now,
    );
    let last_tool_elapsed_secs = session
        .last_tool_at
        .map(|last| now.signed_duration_since(last).num_seconds().max(0));

    let tmux_name = extract_tmux_name(&session.session_key);
    let inflight = load_inflight_snapshot(session.provider.as_deref(), tmux_name.as_deref());
    let recent_output = tmux_name
        .as_deref()
        .and_then(capture_recent_tmux_output)
        .or_else(|| inflight.as_ref().and_then(inflight_recent_output));
    let events = collect_turn_tool_events(recent_output.as_deref(), inflight.as_ref());
    let last_tool = events.iter().rev().find(|event| event.kind == "tool");
    let child_inventory = load_child_inventory_by_parent_key_pg(pool, &session.session_key)
        .await
        .unwrap_or_default();
    let oldest_child_spawned_at = child_inventory
        .alive
        .iter()
        .filter_map(|child| child.spawned_at)
        .min()
        .map(|value| value.to_rfc3339());

    (
        StatusCode::OK,
        Json(json!({
            "target": identifier,
            "agent_id": session.agent_id,
            "agent_name": session.agent_name,
            "provider": session.provider,
            "session_key": session.session_key,
            "status": session.status,
            "visual_status": visual.display(),
            "visual_status_emoji": visual.emoji(),
            "visual_status_code": visual.code(),
            "thread_channel_id": session.thread_channel_id,
            "created_at": session.created_at.map(|value| value.to_rfc3339()),
            "last_tool_at": session.last_tool_at.map(|value| value.to_rfc3339()),
            "last_tool_elapsed_secs": last_tool_elapsed_secs,
            "active_children": session.active_children,
            "oldest_child_spawned_at": oldest_child_spawned_at,
            "children": child_inventory,
            "last_tool": last_tool.map(|event| json!({
                "tool_name": event.tool_name,
                "summary": event.summary,
                "status": event.status,
                "line": event.line,
            })),
            "recent_loop_suspicion": loop_suspicion(&events),
        })),
    )
}

/// GET /api/agents/:id/offices
pub async fn agent_offices(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_required_response();
    };
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

    let rows = match sqlx::query(
        "SELECT o.id, o.name, o.layout, oa.department_id, oa.joined_at
         FROM office_agents oa
         INNER JOIN offices o ON o.id = oa.office_id
         WHERE oa.agent_id = $1
         ORDER BY o.id",
    )
    .bind(&id)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("query: {e}")})),
            );
        }
    };

    let offices: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            json!({
                "id": row.try_get::<String, _>("id").unwrap_or_default(),
                "name": row.try_get::<Option<String>, _>("name").ok().flatten(),
                "layout": row.try_get::<Option<String>, _>("layout").ok().flatten(),
                "assigned": true,
                "office_department_id": row.try_get::<Option<String>, _>("department_id").ok().flatten(),
                "joined_at": pg_timestamp_to_rfc3339(row.try_get("joined_at").ok().flatten()),
            })
        })
        .collect();

    (StatusCode::OK, Json(json!({"offices": offices})))
}

/// GET /api/agents/:id/cron
#[allow(dead_code)]
pub async fn agent_cron(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_required_response();
    };
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

    // Stub: no cron table yet
    (StatusCode::OK, Json(json!({"jobs": []})))
}

/// GET /api/agents/:id/skills
pub async fn agent_skills(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_required_response();
    };
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

    let rows = match sqlx::query(
        "SELECT DISTINCT s.id, s.name, s.description, s.source_path, s.trigger_patterns, s.updated_at
         FROM skills s
         INNER JOIN skill_usage su ON su.skill_id = s.id
         WHERE su.agent_id = $1
         ORDER BY s.id",
    )
    .bind(&id)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("query: {e}")})),
            )
        }
    };

    let skills: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            json!({
                "id": row.try_get::<String, _>("id").unwrap_or_default(),
                "name": row.try_get::<Option<String>, _>("name").ok().flatten(),
                "description": row.try_get::<Option<String>, _>("description").ok().flatten(),
                "source_path": row.try_get::<Option<String>, _>("source_path").ok().flatten(),
                "trigger_patterns": row.try_get::<Option<String>, _>("trigger_patterns").ok().flatten(),
                "updated_at": pg_timestamp_to_rfc3339(row.try_get("updated_at").ok().flatten()),
            })
        })
        .collect();

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
    let Some(pool) = state.pg_pool_ref() else {
        return pg_required_response();
    };
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

    // SQL only orders by recency now. Dedupe + activity-aware ranking are
    // both done in application code below using SessionActivityResolver,
    // because raw status='working' can lag behind the resolver's view
    // (Codex review PR #1258, 9th pass).
    // LEFT JOIN task_dispatches via active_dispatch_id so the response can
    // expose the kanban_card_id this session is currently working on. The
    // dashboard's restored "감사 / Audit" panel uses that mapping to
    // deeplink each audit row to the most recent Discord turn for the same
    // card without a separate API round-trip.
    let rows = match sqlx::query(
        "SELECT s.id, s.session_key, s.agent_id, s.provider, s.status, s.active_dispatch_id,
                s.model, s.tokens, s.cwd, s.last_heartbeat, s.thread_channel_id,
                td.kanban_card_id AS kanban_card_id
         FROM sessions s
         LEFT JOIN task_dispatches td ON td.id = s.active_dispatch_id
         WHERE s.agent_id = $1
         ORDER BY COALESCE(s.last_heartbeat, s.created_at) DESC NULLS LAST, s.id DESC",
    )
    .bind(&id)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("query: {e}")})),
            );
        }
    };

    let guild_id = state
        .config
        .discord
        .guild_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    // Resolve every row first — SessionActivityResolver translates raw
    // status + active_dispatch_id + heartbeat freshness into the effective
    // status the dashboard renders. We need that view *before* the dedupe
    // so the live row beats a stale 'working' sibling for the same
    // (thread_channel_id, provider).
    let mut resolver = SessionActivityResolver::new();
    let resolved: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            let session_key = row
                .try_get::<Option<String>, _>("session_key")
                .ok()
                .flatten();
            let status = row.try_get::<Option<String>, _>("status").ok().flatten();
            let active_dispatch_id = row
                .try_get::<Option<String>, _>("active_dispatch_id")
                .ok()
                .flatten();
            let last_heartbeat =
                pg_timestamp_to_rfc3339(row.try_get("last_heartbeat").ok().flatten());
            let provider = row.try_get::<Option<String>, _>("provider").ok().flatten();
            let thread_channel_id = row
                .try_get::<Option<String>, _>("thread_channel_id")
                .ok()
                .flatten();

            let effective = resolver.resolve(
                session_key.as_deref(),
                status.as_deref(),
                active_dispatch_id.as_deref(),
                last_heartbeat.as_deref(),
            );

            let (channel_web_url, channel_deeplink_url) =
                build_channel_deeplinks(thread_channel_id.as_deref(), guild_id.as_deref());
            let kanban_card_id = row
                .try_get::<Option<String>, _>("kanban_card_id")
                .ok()
                .flatten();

            // Issue #1241: expose canonical {channel_id, deeplink_url, thread_id,
            // thread_deeplink_url} so the dashboard can drop the field straight
            // into an anchor `href` without rebuilding Discord URLs client-side.
            // Legacy thread_channel_id / channel_web_url / channel_deeplink_url
            // remain for backwards compatibility — every dispatched session
            // lives inside its agent thread, so channel_id === thread_id and
            // deeplink_url === thread web URL.
            json!({
                "id": row.try_get::<i64, _>("id").unwrap_or(0),
                "session_key": session_key,
                "agent_id": row.try_get::<Option<String>, _>("agent_id").ok().flatten(),
                "provider": provider,
                "status": effective.status,
                "active_dispatch_id": effective.active_dispatch_id,
                "model": row.try_get::<Option<String>, _>("model").ok().flatten(),
                "tokens": row.try_get::<i64, _>("tokens").unwrap_or(0),
                "cwd": row.try_get::<Option<String>, _>("cwd").ok().flatten(),
                "last_heartbeat": last_heartbeat,
                "thread_channel_id": thread_channel_id.clone(),
                "channel_id": thread_channel_id.clone(),
                "thread_id": thread_channel_id,
                "guild_id": guild_id.clone(),
                "channel_web_url": channel_web_url.clone(),
                "channel_deeplink_url": channel_deeplink_url.clone(),
                "deeplink_url": channel_web_url,
                "thread_deeplink_url": channel_deeplink_url,
                "kanban_card_id": kanban_card_id,
            })
        })
        .collect();

    let sessions = dedup_dispatched_sessions(resolved);

    (StatusCode::OK, Json(json!({"sessions": sessions})))
}

/// Issue #1241: dedupe dispatched-session rows by `(channel_id, agent_id)`.
///
/// The previous key was `(channel_id, provider)`; that let two rows for the
/// same agent in the same Discord channel survive whenever a stale alt-provider
/// session lingered, which surfaced as the duplicated "#agent-manager
/// #agent-manager" labels the issue calls out. Using `(channel_id, agent_id)`
/// collapses each agent ↔ channel pairing to a single canonical row even when
/// the legacy session row carries a different provider snapshot.
///
/// Within each (channel, agent) bucket we keep the row with the highest
/// effective priority — Codex review (PR #1258, 9th pass): dedupe AFTER the
/// SessionActivityResolver translation so the resolved 'working' row outranks
/// a stale sibling that still has raw status='working' but no fresh heartbeat
/// / no active dispatch.
fn dedup_dispatched_sessions(resolved: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    fn effective_priority(value: &serde_json::Value) -> u8 {
        let status = value.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let has_dispatch = value
            .get("active_dispatch_id")
            .map(|v| !v.is_null())
            .unwrap_or(false);
        match status {
            "working" => 0,
            _ if has_dispatch => 1,
            "idle" => 2,
            _ => 3,
        }
    }

    let mut best_index_for_key: std::collections::HashMap<(String, String), usize> =
        std::collections::HashMap::new();
    let mut keep: Vec<bool> = vec![true; resolved.len()];
    for (idx, value) in resolved.iter().enumerate() {
        let channel = value
            .get("channel_id")
            .and_then(|v| v.as_str())
            .or_else(|| value.get("thread_channel_id").and_then(|v| v.as_str()));
        let agent_id = value.get("agent_id").and_then(|v| v.as_str());
        if let (Some(cid), Some(aid)) = (channel, agent_id) {
            let key = (cid.to_string(), aid.to_string());
            match best_index_for_key.get(&key) {
                None => {
                    best_index_for_key.insert(key, idx);
                }
                Some(&prev_idx) => {
                    let prev_priority = effective_priority(&resolved[prev_idx]);
                    let curr_priority = effective_priority(value);
                    if curr_priority < prev_priority {
                        keep[prev_idx] = false;
                        best_index_for_key.insert(key, idx);
                    } else {
                        keep[idx] = false;
                    }
                }
            }
        }
    }

    resolved
        .into_iter()
        .enumerate()
        .filter_map(|(idx, value)| if keep[idx] { Some(value) } else { None })
        .collect()
}

/// Build Discord web + deep-link URLs for a channel. Returns (None, None) when either
/// channel_id or guild_id is missing so the caller can render plain text fallback.
fn build_channel_deeplinks(
    channel_id: Option<&str>,
    guild_id: Option<&str>,
) -> (Option<String>, Option<String>) {
    let channel = channel_id.map(str::trim).filter(|s| !s.is_empty());
    let guild = guild_id.map(str::trim).filter(|s| !s.is_empty());
    match (channel, guild) {
        (Some(c), Some(g)) => (
            Some(format!("https://discord.com/channels/{g}/{c}")),
            Some(format!("discord://discord.com/channels/{g}/{c}")),
        ),
        _ => (None, None),
    }
}

/// GET /api/agents/:id/turn
pub async fn agent_turn(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_required_response();
    };
    let session = {
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

    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok": false, "error": "postgres pool unavailable"})),
        );
    };
    let (provider, primary_channel) = {
        match agent_exists_pg(pool, &id).await {
            Ok(true) => {}
            Ok(false) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"ok": false, "error": "agent not found"})),
                );
            }
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"ok": false, "error": format!("query: {error}")})),
                );
            }
        }

        let Some(bindings) = crate::db::agents::load_agent_channel_bindings_pg(pool, &id)
            .await
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
    let Some(pool) = state.pg_pool_ref() else {
        return pg_required_response();
    };
    let session = {
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
    let Some(pool) = state.pg_pool_ref() else {
        return pg_required_response();
    };
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

    let limit = params.limit.unwrap_or(30);

    let sql = "
        SELECT id, source, type, title, status, timestamp, duration_ms FROM (
            SELECT
                id,
                'dispatch' AS source,
                COALESCE(dispatch_type, 'task') AS type,
                title,
                status,
                (EXTRACT(EPOCH FROM created_at) * 1000)::BIGINT AS timestamp,
                CASE
                    WHEN updated_at IS NOT NULL AND created_at IS NOT NULL
                    THEN ((EXTRACT(EPOCH FROM updated_at) - EXTRACT(EPOCH FROM created_at)) * 1000)::BIGINT
                    ELSE NULL
                END AS duration_ms
            FROM task_dispatches
            WHERE to_agent_id = $1 OR from_agent_id = $1

            UNION ALL

            SELECT
                id::TEXT,
                'session' AS source,
                'session' AS type,
                COALESCE(session_key, 'session') AS title,
                status,
                (EXTRACT(EPOCH FROM created_at) * 1000)::BIGINT AS timestamp,
                CASE
                    WHEN last_heartbeat IS NOT NULL AND created_at IS NOT NULL
                    THEN ((EXTRACT(EPOCH FROM last_heartbeat) - EXTRACT(EPOCH FROM created_at)) * 1000)::BIGINT
                    ELSE NULL
                END AS duration_ms
            FROM sessions
            WHERE agent_id = $1

            UNION ALL

            SELECT
                id,
                'kanban' AS source,
                'card' AS type,
                title,
                status,
                (EXTRACT(EPOCH FROM created_at) * 1000)::BIGINT AS timestamp,
                CASE
                    WHEN updated_at IS NOT NULL AND created_at IS NOT NULL
                    THEN ((EXTRACT(EPOCH FROM updated_at) - EXTRACT(EPOCH FROM created_at)) * 1000)::BIGINT
                    ELSE NULL
                END AS duration_ms
            FROM kanban_cards
            WHERE assigned_agent_id = $1
        )
        ORDER BY timestamp DESC
        LIMIT $2
    ";

    let rows = match sqlx::query(sql).bind(&id).bind(limit).fetch_all(pool).await {
        Ok(rows) => rows,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("query: {e}")})),
            );
        }
    };

    let events: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            json!({
                "id": row.try_get::<String, _>("id").unwrap_or_default(),
                "source": row.try_get::<String, _>("source").unwrap_or_default(),
                "type": row.try_get::<String, _>("type").unwrap_or_default(),
                "title": row.try_get::<Option<String>, _>("title").ok().flatten(),
                "status": row.try_get::<Option<String>, _>("status").ok().flatten(),
                "timestamp": row.try_get::<Option<i64>, _>("timestamp").ok().flatten(),
                "duration_ms": row.try_get::<Option<i64>, _>("duration_ms").ok().flatten(),
            })
        })
        .collect();

    (StatusCode::OK, Json(json!({"events": events})))
}

/// GET /api/agents/:id/transcripts?limit=10
pub async fn agent_transcripts(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<TranscriptQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_required_response();
    };
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

    match list_agent_transcripts_pg_json(pool, &id, params.limit.unwrap_or(8)).await {
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

async fn list_agent_transcripts_pg_json(
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
    .bind(limit.clamp(1, 100) as i64)
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
                .unwrap_or_else(|| json!([]));
            json!({
                "id": row.try_get::<i64, _>("id").unwrap_or(0),
                "turn_id": row.try_get::<String, _>("turn_id").unwrap_or_default(),
                "session_key": row.try_get::<Option<String>, _>("session_key").ok().flatten(),
                "channel_id": row.try_get::<Option<String>, _>("channel_id").ok().flatten(),
                "agent_id": row.try_get::<Option<String>, _>("agent_id").ok().flatten(),
                "provider": row.try_get::<Option<String>, _>("provider").ok().flatten(),
                "dispatch_id": row.try_get::<Option<String>, _>("dispatch_id").ok().flatten(),
                "kanban_card_id": row.try_get::<Option<String>, _>("kanban_card_id").ok().flatten(),
                "dispatch_title": row.try_get::<Option<String>, _>("dispatch_title").ok().flatten(),
                "card_title": row.try_get::<Option<String>, _>("card_title").ok().flatten(),
                "github_issue_number": row.try_get::<Option<i64>, _>("github_issue_number").ok().flatten(),
                "user_message": row.try_get::<String, _>("user_message").unwrap_or_default(),
                "assistant_message": row.try_get::<String, _>("assistant_message").unwrap_or_default(),
                "events": events,
                "duration_ms": row.try_get::<Option<i64>, _>("duration_ms").ok().flatten(),
                "created_at": row.try_get::<String, _>("created_at").unwrap_or_default(),
            })
        })
        .collect())
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

    let Some(pool) = state.pg_pool_ref() else {
        return pg_required_response();
    };

    let card_id: Option<String> = match sqlx::query_scalar(
        "SELECT id
         FROM kanban_cards
         WHERE assigned_agent_id = $1 AND status = 'in_progress'
         ORDER BY updated_at DESC
         LIMIT 1",
    )
    .bind(&agent_id)
    .fetch_optional(pool)
    .await
    {
        Ok(card_id) => card_id,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("query: {error}")})),
            );
        }
    };

    let Some(card_id) = card_id else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no active card for agent"})),
        );
    };

    sqlx::query("UPDATE kanban_cards SET blocked_reason = $1, updated_at = NOW() WHERE id = $2")
        .bind(reason)
        .bind(&card_id)
        .execute(pool)
        .await
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
    use crate::db::session_transcripts::{SessionTranscriptEvent, SessionTranscriptEventKind};
    use crate::engine::PolicyEngine;

    fn test_db() -> Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    fn test_engine(db: &Db) -> PolicyEngine {
        let config = crate::config::Config::default();
        PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap()
    }

    fn test_engine_with_pg(pg_pool: sqlx::PgPool) -> PolicyEngine {
        let mut config = crate::config::Config::default();
        config.policies.dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
        config.policies.hot_reload = false;
        PolicyEngine::new_with_pg(&config, Some(pg_pool)).unwrap()
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
    fn loop_suspicion_keeps_json_shape_for_empty_events() {
        let value = loop_suspicion(&[]);
        assert_eq!(value["suspected"], false);
        assert_eq!(value["reason"], serde_json::Value::Null);
        assert_eq!(value["repeat_count"], 0);
        assert_eq!(value["tool"], serde_json::Value::Null);
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

    /// Per-test Postgres database lifecycle for the #1238 migration of
    /// agents handler tests, which now require a PG pool.
    struct AgentsPgDatabase {
        _lifecycle: crate::db::postgres::PostgresTestLifecycleGuard,
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl AgentsPgDatabase {
        async fn create() -> Self {
            let lifecycle = crate::db::postgres::lock_test_lifecycle();
            let admin_url = pg_test_admin_database_url();
            let database_name = format!("agentdesk_agents_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", pg_test_base_database_url(), database_name);
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "agents handler pg",
            )
            .await
            .expect("create agents postgres test db");

            Self {
                _lifecycle: lifecycle,
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "agents handler pg",
            )
            .await
            .expect("connect + migrate agents postgres test db")
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "agents handler pg",
            )
            .await
            .expect("drop agents postgres test db");
        }
    }

    fn pg_test_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }
        let user = std::env::var("PGUSER")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| std::env::var("USER").ok().filter(|v| !v.trim().is_empty()))
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|v| !v.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());
        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn pg_test_admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", pg_test_base_database_url(), admin_db)
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
    async fn agent_dispatched_sessions_returns_503_when_pg_pool_missing() {
        // The endpoint requires the Postgres pool — sqlite shim alone is not
        // enough — so without pg_pool we expect a clean 503 instead of a
        // panic. Pre-#1241 this test asserted a 200 OK happy path with the
        // sqlite shim alone, which always failed because the route bails out
        // early without pg_pool. Use the new dedup_dispatched_sessions unit
        // tests below for the response-shape contract.
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db.clone(), engine);

        let (status, _) =
            agent_dispatched_sessions(State(state), Path("project-agentdesk".to_string())).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    /// Issue #1241: the dashboard surfaced the same `#agent-manager` channel
    /// twice because the dedup key was `(channel_id, provider)` — a stale
    /// codex row alongside a fresh claude row both survived. The new key
    /// `(channel_id, agent_id)` collapses any (agent, channel) pair to one
    /// canonical row regardless of provider snapshot drift.
    #[test]
    fn dedup_dispatched_sessions_collapses_same_agent_channel_across_providers() {
        let stale = json!({
            "agent_id": "project-agentdesk",
            "provider": "codex",
            "status": "idle",
            "active_dispatch_id": null,
            "thread_channel_id": "1485506232256168011",
            "channel_id": "1485506232256168011",
        });
        let fresh = json!({
            "agent_id": "project-agentdesk",
            "provider": "claude",
            "status": "working",
            "active_dispatch_id": "dispatch-1",
            "thread_channel_id": "1485506232256168011",
            "channel_id": "1485506232256168011",
        });

        let result = dedup_dispatched_sessions(vec![stale, fresh]);
        assert_eq!(result.len(), 1, "duplicates should collapse to one row");
        assert_eq!(result[0]["status"], "working");
        assert_eq!(result[0]["provider"], "claude");
    }

    /// Issue #1241: distinct agents in the same Discord channel must NOT be
    /// merged. Pre-#1241 the (channel, provider) key would collapse them
    /// whenever they shared a provider snapshot.
    #[test]
    fn dedup_dispatched_sessions_keeps_distinct_agents_in_same_channel() {
        let alpha = json!({
            "agent_id": "agent-alpha",
            "provider": "claude",
            "status": "working",
            "active_dispatch_id": "dispatch-a",
            "thread_channel_id": "1485506232256168011",
            "channel_id": "1485506232256168011",
        });
        let beta = json!({
            "agent_id": "agent-beta",
            "provider": "claude",
            "status": "idle",
            "active_dispatch_id": null,
            "thread_channel_id": "1485506232256168011",
            "channel_id": "1485506232256168011",
        });

        let result = dedup_dispatched_sessions(vec![alpha, beta]);
        assert_eq!(result.len(), 2, "different agents must each survive");
    }

    /// Issue #1241: build_channel_deeplinks must mint the canonical
    /// {web,deeplink} pair the dashboard renders straight into anchor `href`s.
    /// Web URL is `https://discord.com/channels/{guild}/{channel}`; deeplink
    /// uses the `discord://` scheme so Discord's app handler picks it up.
    #[test]
    fn build_channel_deeplinks_emits_https_and_discord_scheme_pair() {
        let (web, deep) =
            build_channel_deeplinks(Some("1485506232256168011"), Some("1490141479707086938"));
        assert_eq!(
            web.as_deref(),
            Some("https://discord.com/channels/1490141479707086938/1485506232256168011"),
        );
        assert_eq!(
            deep.as_deref(),
            Some("discord://discord.com/channels/1490141479707086938/1485506232256168011"),
        );

        // Missing guild → both fall back to None so callers render plain text.
        let (web_none, deep_none) = build_channel_deeplinks(Some("1485506232256168011"), None);
        assert!(web_none.is_none());
        assert!(deep_none.is_none());
    }

    #[tokio::test]
    async fn agent_transcripts_pg_returns_structured_events() {
        let pg_db = AgentsPgDatabase::create().await;
        let pool = pg_db.migrate().await;
        let db = test_db();
        let state = AppState::test_state_with_pg(
            db.clone(),
            test_engine_with_pg(pool.clone()),
            pool.clone(),
        );

        sqlx::query(
            "INSERT INTO agents (id, name, provider, status, xp) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind("agent-transcript")
        .bind("Transcript Agent")
        .bind("codex")
        .bind("idle")
        .bind(0_i32)
        .execute(&pool)
        .await
        .unwrap();

        let events = vec![SessionTranscriptEvent {
            kind: SessionTranscriptEventKind::ToolUse,
            tool_name: Some("Bash".to_string()),
            summary: Some("cargo test".to_string()),
            content: "cargo test --no-run".to_string(),
            status: Some("success".to_string()),
            is_error: false,
        }];
        let events_json = serde_json::to_string(&events).unwrap();
        sqlx::query(
            "INSERT INTO session_transcripts (
                turn_id, session_key, channel_id, agent_id, provider, dispatch_id,
                user_message, assistant_message, events_json, duration_ms
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, CAST($9 AS jsonb), $10)",
        )
        .bind("discord:agent-transcript:1")
        .bind("host:agent-transcript")
        .bind("chan-1")
        .bind("agent-transcript")
        .bind("codex")
        .bind(Option::<String>::None)
        .bind("verify build")
        .bind("build verified")
        .bind(events_json)
        .bind(4200_i32)
        .execute(&pool)
        .await
        .unwrap();

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

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn agent_transcripts_pg_falls_back_to_session_agent_for_legacy_rows() {
        let pg_db = AgentsPgDatabase::create().await;
        let pool = pg_db.migrate().await;
        let db = test_db();
        let state = AppState::test_state_with_pg(
            db.clone(),
            test_engine_with_pg(pool.clone()),
            pool.clone(),
        );

        sqlx::query(
            "INSERT INTO agents (id, name, provider, status, xp) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind("agent-transcript-fallback")
        .bind("Transcript Fallback")
        .bind("codex")
        .bind("idle")
        .bind(0_i32)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sessions (session_key, agent_id, provider, status, last_heartbeat)
             VALUES ($1, $2, $3, $4, NOW())",
        )
        .bind("host:agent-transcript-fallback")
        .bind("agent-transcript-fallback")
        .bind("codex")
        .bind("idle")
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO session_transcripts (
                turn_id, session_key, channel_id, agent_id, provider, dispatch_id,
                user_message, assistant_message, events_json
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, CAST($9 AS jsonb))",
        )
        .bind("discord:agent-transcript-fallback:1")
        .bind("host:agent-transcript-fallback")
        .bind("chan-fallback")
        .bind(Option::<String>::None)
        .bind("codex")
        .bind(Option::<String>::None)
        .bind("legacy question")
        .bind("legacy answer")
        .bind("[]")
        .execute(&pool)
        .await
        .unwrap();

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

        pool.close().await;
        pg_db.drop().await;
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
