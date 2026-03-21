use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;

use super::AppState;

// ── Query types ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct TimelineQuery {
    pub limit: Option<i64>,
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
        "SELECT id, session_key, agent_id, provider, status, active_dispatch_id,
                model, tokens, cwd, last_heartbeat
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
            Ok(json!({
                "id": row.get::<_, i64>(0)?,
                "session_key": row.get::<_, Option<String>>(1)?,
                "agent_id": row.get::<_, Option<String>>(2)?,
                "provider": row.get::<_, Option<String>>(3)?,
                "status": row.get::<_, Option<String>>(4)?,
                "active_dispatch_id": row.get::<_, Option<String>>(5)?,
                "model": row.get::<_, Option<String>>(6)?,
                "tokens": row.get::<_, i64>(7)?,
                "cwd": row.get::<_, Option<String>>(8)?,
                "last_heartbeat": row.get::<_, Option<String>>(9)?,
            }))
        })
        .ok();

    let sessions: Vec<serde_json::Value> = match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect(),
        None => Vec::new(),
    };

    (StatusCode::OK, Json(json!({"sessions": sessions})))
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
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": format!("{e}")}))),
    };

    let card_id: Option<String> = conn
        .query_row(
            "SELECT id FROM kanban_cards WHERE assigned_agent_id = ?1 AND status = 'in_progress' ORDER BY updated_at DESC LIMIT 1",
            [&agent_id],
            |row| row.get(0),
        )
        .ok();

    let Some(card_id) = card_id else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "no active card for agent"})));
    };

    conn.execute(
        "UPDATE kanban_cards SET blocked_reason = ?1 WHERE id = ?2",
        rusqlite::params![reason, card_id],
    ).ok();
    drop(conn);

    let _ = crate::kanban::transition_status(&state.db, &state.engine, &card_id, "blocked");

    (StatusCode::OK, Json(json!({"ok": true, "card_id": card_id, "signal": signal})))
}

/// GET /api/agent-channels
/// Returns agent ID → Discord channel mapping.
pub async fn agent_channels(
    State(state): State<super::AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": format!("{e}")}))),
    };

    let mut stmt = conn
        .prepare("SELECT id, name, discord_channel_id, discord_channel_alt FROM agents ORDER BY id")
        .unwrap();

    let channels: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            Ok(json!({
                "agent_id": row.get::<_, String>(0)?,
                "name": row.get::<_, Option<String>>(1)?,
                "channel_id": row.get::<_, Option<String>>(2)?,
                "channel_alt": row.get::<_, Option<String>>(3)?,
            }))
        })
        .ok()
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();

    (StatusCode::OK, Json(json!({"channels": channels})))
}
