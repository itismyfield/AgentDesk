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
    if channel_id.parse::<u64>().is_err() {
        return Json(json!({"error": "invalid channel_id", "queue": []}));
    }

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
                     WHERE (
                         a.discord_channel_id = ?1 OR a.discord_channel_alt = ?1 OR
                         a.discord_channel_cc = ?1 OR a.discord_channel_cdx = ?1
                     ) \
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
pub async fn list_pending_dispatches(State(state): State<AppState>) -> Json<serde_json::Value> {
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
    match state.queue_service().cancel_dispatch(&dispatch_id) {
        Ok(body) => (StatusCode::OK, Json(body)),
        Err(error) => error.into_json_response(),
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
    match state
        .queue_service()
        .cancel_all_dispatches(body.kanban_card_id.as_deref(), body.agent_id.as_deref())
    {
        Ok(response) => (StatusCode::OK, Json(response)),
        Err(error) => error.into_json_response(),
    }
}

// ── POST /api/turns/:channel_id/cancel ──────────────────────────

/// Cancel the active turn in a channel by killing its tmux session.
/// This is the hard-stop equivalent — the turn will not complete gracefully.
pub async fn cancel_turn(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.queue_service().cancel_turn(&channel_id) {
        Ok(response) => (StatusCode::OK, Json(response)),
        Err(error) => error.into_json_response(),
    }
}

// ── POST /api/turns/:channel_id/extend-timeout ───────────────────

#[derive(Deserialize)]
pub struct ExtendTimeoutBody {
    /// Seconds to extend. Default: 1800 (30 min).
    #[serde(default = "default_extend_secs")]
    pub extend_secs: u64,
}

fn default_extend_secs() -> u64 {
    1800
}

/// Extend the watchdog timeout for an active turn in a channel.
/// The deadline will be clamped to 3 hours from the original turn start.
pub async fn extend_turn_timeout(
    Path(channel_id): Path<String>,
    Json(body): Json<ExtendTimeoutBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let channel_num: u64 = match channel_id.parse() {
        Ok(n) => n,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "channel_id must be a numeric Discord channel ID"})),
            );
        }
    };

    match crate::services::discord::extend_watchdog_deadline(channel_num, body.extend_secs).await {
        Some(new_deadline_ms) => {
            let remaining_min =
                (new_deadline_ms - chrono::Utc::now().timestamp_millis()) / 1000 / 60;
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "channel_id": channel_id,
                    "new_deadline_ms": new_deadline_ms,
                    "remaining_minutes": remaining_min,
                })),
            )
        }
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "failed to extend watchdog deadline"})),
        ),
    }
}
