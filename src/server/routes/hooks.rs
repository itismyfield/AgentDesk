use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::PgPool;

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
pub async fn reset_status(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool.as_ref() {
        return match reset_status_pg(pool).await {
            Ok(updated) => (
                StatusCode::OK,
                Json(json!({"ok": true, "updated": updated})),
            ),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            ),
        };
    }

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
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
    if let Some(pool) = state.pg_pool.as_ref() {
        return match skill_usage_pg(pool, &body).await {
            Ok(id) => (StatusCode::OK, Json(json!({"ok": true, "id": id}))),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            ),
        };
    }

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    // Resolve agent_id: use provided value, or look up by role_id
    let agent_id = body.agent_id.clone().or_else(|| {
        body.role_id.as_ref().and_then(|rid| {
            conn.query_row("SELECT id FROM agents WHERE id = ?1", [rid], |row| {
                row.get(0)
            })
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
    if let Some(pool) = state.pg_pool.as_ref() {
        return match disconnect_session_pg(pool, &session_key).await {
            Ok(false) => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "session not found"})),
            ),
            Ok(true) => (
                StatusCode::OK,
                Json(json!({"ok": true, "session_key": session_key})),
            ),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            ),
        };
    }

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
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

async fn reset_status_pg(pool: &PgPool) -> Result<u64, sqlx::Error> {
    sqlx::query("UPDATE agents SET status = 'idle' WHERE status = 'working'")
        .execute(pool)
        .await
        .map(|result| result.rows_affected())
}

async fn skill_usage_pg(pool: &PgPool, body: &SkillUsageBody) -> Result<i64, sqlx::Error> {
    let agent_id = match body.agent_id.as_deref() {
        Some(agent_id) => Some(agent_id.to_string()),
        None => {
            if let Some(role_id) = body.role_id.as_deref() {
                sqlx::query_scalar("SELECT id FROM agents WHERE id = $1")
                    .bind(role_id)
                    .fetch_optional(pool)
                    .await?
            } else {
                None
            }
        }
    };

    sqlx::query_scalar(
        "INSERT INTO skill_usage (skill_id, agent_id, session_key, used_at)
         VALUES ($1, $2, $3, NOW())
         RETURNING id::BIGINT",
    )
    .bind(body.skill_id.as_str())
    .bind(agent_id.as_deref())
    .bind(body.session_key.as_deref())
    .fetch_one(pool)
    .await
}

async fn disconnect_session_pg(pool: &PgPool, session_key: &str) -> Result<bool, sqlx::Error> {
    sqlx::query("UPDATE sessions SET status = 'disconnected' WHERE session_key = $1")
        .bind(session_key)
        .execute(pool)
        .await
        .map(|result| result.rows_affected() > 0)
}
