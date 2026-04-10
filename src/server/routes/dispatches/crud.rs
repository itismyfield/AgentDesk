use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;

use crate::server::routes::AppState;

// ── Query / Body types ─────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ListDispatchesQuery {
    pub status: Option<String>,
    pub kanban_card_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateDispatchBody {
    pub kanban_card_id: String,
    pub to_agent_id: String,
    pub dispatch_type: Option<String>,
    pub title: String,
    pub context: Option<serde_json::Value>,
    pub skip_outbox: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateDispatchBody {
    pub status: Option<String>,
    pub result: Option<serde_json::Value>,
}

// ── Handlers ───────────────────────────────────────────────────

/// GET /api/dispatches
pub async fn list_dispatches(
    State(state): State<AppState>,
    Query(params): Query<ListDispatchesQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state
        .dispatch_service()
        .list_dispatches(params.status.as_deref(), params.kanban_card_id.as_deref())
    {
        Ok(dispatches) => (StatusCode::OK, Json(json!({"dispatches": dispatches}))),
        Err(error) => error.into_json_response(),
    }
}

/// GET /api/dispatches/:id
pub async fn get_dispatch(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.dispatch_service().get_dispatch(&id) {
        Ok(dispatch) => (StatusCode::OK, Json(json!({"dispatch": dispatch}))),
        Err(error) => error.into_json_response(),
    }
}

/// POST /api/dispatches/:id/cancel
///
/// Cancel a dispatch AND kill the associated agent session/turn.
/// Ensures the agent stops working on the cancelled dispatch.
pub async fn cancel_dispatch(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    // 1. Get dispatch info before cancelling
    let dispatch_info: Option<(String, String, Option<String>)> =
        state.db.lock().ok().and_then(|conn| {
            conn.query_row(
                "SELECT to_agent_id, status, thread_id FROM task_dispatches WHERE id = ?1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .ok()
        });

    let (agent_id, status, thread_id) = match dispatch_info {
        Some(info) => info,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "dispatch not found"})),
            );
        }
    };

    if status == "completed" || status == "cancelled" {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": format!("dispatch already {status}"), "status": status})),
        );
    }

    // 2. Cancel the dispatch in DB
    {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };
        if let Err(e) = crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_conn(
            &conn,
            &id,
            Some("api_cancel"),
        ) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    }

    // 3. Kill the agent's tmux session for this thread (if any)
    let mut session_killed = false;
    if let Some(ref tid) = thread_id {
        // Find tmux session name from thread_id
        let session_name: Option<String> = state
            .db
            .lock()
            .ok()
            .and_then(|conn| {
                conn.query_row(
                    "SELECT session_key FROM sessions WHERE session_key LIKE ?1 AND status IN ('working', 'idle')",
                    [format!("%t{tid}%")],
                    |row| row.get::<_, String>(0),
                )
                .ok()
            });

        if let Some(ref key) = session_name {
            let tmux_name = key.split(':').last().unwrap_or(key);
            // Kill tmux session
            let _ = std::process::Command::new("tmux")
                .args(["kill-session", "-t", tmux_name])
                .output();
            // Mark session as idle
            if let Ok(conn) = state.db.lock() {
                conn.execute(
                    "UPDATE sessions SET status = 'idle', active_dispatch_id = NULL WHERE session_key = ?1",
                    [key.as_str()],
                )
                .ok();
            }
            session_killed = true;
        }
    }

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "dispatch_id": id,
            "agent_id": agent_id,
            "session_killed": session_killed,
        })),
    )
}

/// POST /api/dispatches
pub async fn create_dispatch(
    State(state): State<AppState>,
    Json(body): Json<CreateDispatchBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let input = crate::services::dispatches::CreateDispatchInput {
        kanban_card_id: body.kanban_card_id,
        to_agent_id: body.to_agent_id,
        dispatch_type: body.dispatch_type,
        title: body.title,
        context: body.context,
        skip_outbox: body.skip_outbox,
    };

    match state.dispatch_service().create_dispatch(input) {
        Ok(result) => (result.status, Json(json!({"dispatch": result.dispatch}))),
        Err(error) => error.into_json_response(),
    }
}

/// PATCH /api/dispatches/:id
pub async fn update_dispatch(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateDispatchBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let input = crate::services::dispatches::UpdateDispatchInput {
        status: body.status,
        result: body.result,
    };

    match state.dispatch_service().update_dispatch(&id, input) {
        Ok(dispatch) => (StatusCode::OK, Json(json!({"dispatch": dispatch}))),
        Err(error) => error.into_json_response(),
    }
}
