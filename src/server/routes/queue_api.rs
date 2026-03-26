//! #138: Channel queue management + dispatch cancel API.
//!
//! Provides operational endpoints for pipeline incident recovery.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;

use super::AppState;

// ── GET /api/channels/:id/queue ─────────────────────────────────

/// List intervention queue for a channel.
pub async fn list_channel_queue(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
) -> Json<serde_json::Value> {
    let channel_id_num: u64 = match channel_id.parse() {
        Ok(n) => n,
        Err(_) => return Json(json!({"error": "invalid channel_id", "queue": []})),
    };
    let cid = serenity::model::id::ChannelId::new(channel_id_num);

    // Access SharedData via engine's shared_data (not available here)
    // Instead, read from the DB-backed pending queue files
    // For now, return dispatches as a proxy for queue state
    let dispatches = match state.db.lock() {
        Ok(conn) => {
            let mut stmt = conn
                .prepare(
                    "SELECT td.id, td.dispatch_type, td.status, td.title, td.created_at, kc.github_issue_number \
                     FROM task_dispatches td \
                     JOIN kanban_cards kc ON td.kanban_card_id = kc.id \
                     JOIN agents a ON td.to_agent_id = a.id \
                     WHERE (a.discord_channel_id = ?1 OR a.discord_channel_alt = ?1) \
                     AND td.status IN ('pending', 'dispatched') \
                     ORDER BY td.created_at DESC",
                )
                .ok();
            stmt.as_mut()
                .and_then(|s| {
                    s.query_map([&channel_id], |row| {
                        Ok(json!({
                            "dispatch_id": row.get::<_, String>(0)?,
                            "dispatch_type": row.get::<_, String>(1)?,
                            "status": row.get::<_, String>(2)?,
                            "title": row.get::<_, Option<String>>(3)?,
                            "created_at": row.get::<_, Option<String>>(4)?,
                            "github_issue_number": row.get::<_, Option<i64>>(5)?,
                        }))
                    })
                    .ok()
                })
                .map(|rows| rows.filter_map(|r| r.ok()).collect::<Vec<_>>())
                .unwrap_or_default()
        }
        Err(_) => Vec::new(),
    };

    Json(json!({"channel_id": channel_id, "dispatches": dispatches}))
}

// ── GET /api/dispatches/pending ─────────────────────────────────

/// List all pending dispatches across all agents.
pub async fn list_pending_dispatches(
    State(state): State<AppState>,
) -> Json<serde_json::Value> {
    let dispatches = match state.db.lock() {
        Ok(conn) => {
            let mut stmt = conn
                .prepare(
                    "SELECT td.id, td.kanban_card_id, td.to_agent_id, td.dispatch_type, td.status, \
                            td.title, td.thread_id, td.created_at, td.retry_count, \
                            kc.github_issue_number, kc.status as card_status \
                     FROM task_dispatches td \
                     JOIN kanban_cards kc ON td.kanban_card_id = kc.id \
                     WHERE td.status IN ('pending', 'dispatched') \
                     ORDER BY td.created_at DESC",
                )
                .ok();
            stmt.as_mut()
                .and_then(|s| {
                    s.query_map([], |row| {
                        Ok(json!({
                            "id": row.get::<_, String>(0)?,
                            "kanban_card_id": row.get::<_, String>(1)?,
                            "to_agent_id": row.get::<_, String>(2)?,
                            "dispatch_type": row.get::<_, String>(3)?,
                            "status": row.get::<_, String>(4)?,
                            "title": row.get::<_, Option<String>>(5)?,
                            "thread_id": row.get::<_, Option<String>>(6)?,
                            "created_at": row.get::<_, Option<String>>(7)?,
                            "retry_count": row.get::<_, i64>(8)?,
                            "github_issue_number": row.get::<_, Option<i64>>(9)?,
                            "card_status": row.get::<_, String>(10)?,
                        }))
                    })
                    .ok()
                })
                .map(|rows| rows.filter_map(|r| r.ok()).collect::<Vec<_>>())
                .unwrap_or_default()
        }
        Err(_) => Vec::new(),
    };

    Json(json!({"dispatches": dispatches, "count": dispatches.len()}))
}

// ── POST /api/dispatches/:id/cancel ─────────────────────────────

/// Cancel a specific dispatch.
pub async fn cancel_dispatch(
    State(state): State<AppState>,
    Path(dispatch_id): Path<String>,
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

    // Check current status
    let current_status: Option<String> = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = ?1",
            [&dispatch_id],
            |row| row.get(0),
        )
        .ok();

    match current_status.as_deref() {
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "dispatch not found"})),
        ),
        Some("completed") | Some("cancelled") | Some("failed") => (
            StatusCode::CONFLICT,
            Json(json!({"error": format!("dispatch already in terminal state: {}", current_status.unwrap())})),
        ),
        Some(_) => {
            conn.execute(
                "UPDATE task_dispatches SET status = 'cancelled', updated_at = datetime('now') WHERE id = ?1",
                [&dispatch_id],
            )
            .ok();
            // Also clean up notification marker
            conn.execute(
                "DELETE FROM kv_meta WHERE key = ?1",
                [&format!("dispatch_notified:{dispatch_id}")],
            )
            .ok();

            tracing::info!("[queue-api] Cancelled dispatch {dispatch_id}");
            (StatusCode::OK, Json(json!({"ok": true, "dispatch_id": dispatch_id})))
        }
    }
}

// ── POST /api/dispatches/cancel-all ─────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CancelAllBody {
    pub kanban_card_id: Option<String>,
    pub agent_id: Option<String>,
}

/// Cancel all pending/dispatched dispatches matching filters.
pub async fn cancel_all_dispatches(
    State(state): State<AppState>,
    Json(body): Json<CancelAllBody>,
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

    let mut conditions = vec!["status IN ('pending', 'dispatched')".to_string()];
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if let Some(ref card_id) = body.kanban_card_id {
        params.push(Box::new(card_id.clone()));
        conditions.push(format!("kanban_card_id = ?{}", params.len()));
    }
    if let Some(ref agent_id) = body.agent_id {
        params.push(Box::new(agent_id.clone()));
        conditions.push(format!("to_agent_id = ?{}", params.len()));
    }

    let sql = format!(
        "UPDATE task_dispatches SET status = 'cancelled', updated_at = datetime('now') WHERE {}",
        conditions.join(" AND ")
    );
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let count = conn.execute(&sql, param_refs.as_slice()).unwrap_or(0);

    tracing::info!("[queue-api] Cancelled {count} dispatches (card={:?}, agent={:?})", body.kanban_card_id, body.agent_id);
    (StatusCode::OK, Json(json!({"ok": true, "cancelled": count})))
}
