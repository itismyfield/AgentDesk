use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::json;

use super::AppState;

// ── Body types ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SkillUsageBody {
    pub skill_id: String,
    pub agent_id: Option<String>,
    pub role_id: Option<String>,
    pub session_key: Option<String>,
}

// ── Handlers ─────────────────────────────────────────────────

/// POST /api/hook/reset-status
pub async fn reset_status(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            )
        }
    };

    match conn.execute(
        "UPDATE agents SET status = 'idle' WHERE status = 'working'",
        [],
    ) {
        Ok(count) => (StatusCode::OK, Json(json!({"ok": true, "updated": count}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// POST /api/hook/skill-usage
pub async fn skill_usage(
    State(state): State<AppState>,
    Json(body): Json<SkillUsageBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            )
        }
    };

    // Resolve agent_id: use provided value, or look up by role_id
    let agent_id = body.agent_id.clone().or_else(|| {
        body.role_id.as_ref().and_then(|rid| {
            conn.query_row(
                "SELECT id FROM agents WHERE role_id = ?1",
                [rid],
                |row| row.get(0),
            )
            .ok()
        })
    });

    match conn.execute(
        "INSERT INTO skill_usage (skill_id, agent_id, session_key) VALUES (?1, ?2, ?3)",
        libsql_rusqlite::params![body.skill_id, agent_id, body.session_key],
    ) {
        Ok(_) => {
            let id = conn.last_insert_rowid();
            (StatusCode::OK, Json(json!({"ok": true, "id": id})))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// DELETE /api/hook/session/{sessionKey}
pub async fn disconnect_session(
    State(state): State<AppState>,
    Path(session_key): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            )
        }
    };

    match conn.execute(
        "UPDATE sessions SET status = 'disconnected' WHERE session_key = ?1",
        [&session_key],
    ) {
        Ok(0) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "session not found"})),
        ),
        Ok(_) => (
            StatusCode::OK,
            Json(json!({"ok": true, "session_key": session_key})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}
