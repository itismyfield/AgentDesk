use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;

use crate::dispatch;
use crate::server::routes::AppState;

use super::outbox::queue_dispatch_followup;

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
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let mut sql = String::from(
        "SELECT id, kanban_card_id, from_agent_id, to_agent_id, dispatch_type, status, title, context, result, parent_dispatch_id, chain_depth, created_at, updated_at FROM task_dispatches WHERE 1=1",
    );
    let mut bind_values: Vec<String> = Vec::new();

    if let Some(ref status) = params.status {
        bind_values.push(status.clone());
        sql.push_str(&format!(" AND status = ?{}", bind_values.len()));
    }
    if let Some(ref card_id) = params.kanban_card_id {
        bind_values.push(card_id.clone());
        sql.push_str(&format!(" AND kanban_card_id = ?{}", bind_values.len()));
    }

    sql.push_str(" ORDER BY created_at DESC");

    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("prepare: {e}")})),
            );
        }
    };

    let params_ref: Vec<&dyn rusqlite::types::ToSql> = bind_values
        .iter()
        .map(|v| v as &dyn rusqlite::types::ToSql)
        .collect();

    let rows = stmt
        .query_map(params_ref.as_slice(), |row| dispatch_row_to_json(row))
        .ok();

    let dispatches: Vec<serde_json::Value> = match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect(),
        None => Vec::new(),
    };

    (StatusCode::OK, Json(json!({"dispatches": dispatches})))
}

/// GET /api/dispatches/:id
pub async fn get_dispatch(
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

    match conn.query_row(
        "SELECT id, kanban_card_id, from_agent_id, to_agent_id, dispatch_type, status, title, context, result, parent_dispatch_id, chain_depth, created_at, updated_at FROM task_dispatches WHERE id = ?1",
        [&id],
        |row| dispatch_row_to_json(row),
    ) {
        Ok(d) => (StatusCode::OK, Json(json!({"dispatch": d}))),
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            (StatusCode::NOT_FOUND, Json(json!({"error": "dispatch not found"})))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
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
    let dispatch_type = body
        .dispatch_type
        .unwrap_or_else(|| "implementation".to_string());
    let context = body.context.unwrap_or(json!({}));
    let options = dispatch::DispatchCreateOptions {
        skip_outbox: body.skip_outbox.unwrap_or(false),
    };

    match dispatch::create_dispatch_with_options(
        &state.db,
        &state.engine,
        &body.kanban_card_id,
        &body.to_agent_id,
        &dispatch_type,
        &body.title,
        &context,
        options,
    ) {
        Ok(d) => {
            let was_reused = d.get("__reused").and_then(|v| v.as_bool()).unwrap_or(false);
            let status_code = if was_reused {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            };
            (status_code, Json(json!({"dispatch": d})))
        }
        Err(e) => {
            let msg = format!("{e}");
            if msg.contains("not found") {
                (StatusCode::NOT_FOUND, Json(json!({"error": msg})))
            } else {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": msg})),
                )
            }
        }
    }
}

/// PATCH /api/dispatches/:id
pub async fn update_dispatch(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateDispatchBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    // #143: Route all API-driven completions through finalize_dispatch
    if body.status.as_deref() == Some("completed") {
        let context = body.result.as_ref();
        match dispatch::finalize_dispatch(&state.db, &state.engine, &id, "api", context) {
            Ok(d) => {
                queue_dispatch_followup(&state.db, &id);
                return (StatusCode::OK, Json(json!({"dispatch": d})));
            }
            Err(e) => {
                let msg = format!("{e}");
                if msg.contains("not found") {
                    return (StatusCode::NOT_FOUND, Json(json!({"error": msg})));
                }
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": msg})),
                );
            }
        }
    }

    // #265: Validate status before applying generic update.
    // Only whitelisted values are accepted. `completed` is included for documentation
    // but is already routed through finalize_dispatch() above and won't reach here.
    //
    // NOTE: `pending`, `dispatched`, `cancelled`, `failed` are set as raw DB updates
    // in this generic path — they do NOT fire lifecycle hooks (e.g. OnDispatchCompleted).
    // If hook-driven side effects are needed for these statuses in the future, they
    // must be routed explicitly like `completed` is above.
    const VALID_DISPATCH_STATUSES: &[&str] =
        &["pending", "dispatched", "completed", "cancelled", "failed"];

    if let Some(ref status) = body.status {
        if !VALID_DISPATCH_STATUSES.contains(&status.as_str()) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!(
                        "invalid dispatch status '{}' — allowed values: {}",
                        status,
                        VALID_DISPATCH_STATUSES.join(", ")
                    )
                })),
            );
        }
    }

    // Generic status update
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let mut sets: Vec<String> = Vec::new();
    let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    let mut idx = 1;

    if let Some(ref status) = body.status {
        sets.push(format!("status = ?{}", idx));
        values.push(Box::new(status.clone()));
        idx += 1;
    }

    if let Some(ref result) = body.result {
        let result_str = serde_json::to_string(result).unwrap_or_default();
        sets.push(format!("result = ?{}", idx));
        values.push(Box::new(result_str));
        idx += 1;
    }

    if sets.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no fields to update"})),
        );
    }

    sets.push("updated_at = datetime('now')".to_string());

    let sql = format!(
        "UPDATE task_dispatches SET {} WHERE id = ?{}",
        sets.join(", "),
        idx
    );
    values.push(Box::new(id.clone()));

    let params_ref: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|v| v.as_ref()).collect();
    match conn.execute(&sql, params_ref.as_slice()) {
        Ok(0) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "dispatch not found"})),
            );
        }
        Ok(_) => {}
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    }

    // If the new status is "completed" (edge case: should have been caught above), fire hook
    if body.status.as_deref() == Some("completed") {
        let kanban_card_id: Option<String> = conn
            .query_row(
                "SELECT kanban_card_id FROM task_dispatches WHERE id = ?1",
                [&id],
                |row| row.get(0),
            )
            .ok();
        drop(conn);

        crate::kanban::fire_event_hooks(
            &state.db,
            &state.engine,
            "on_dispatch_completed",
            "OnDispatchCompleted",
            json!({
                "dispatch_id": id,
                "kanban_card_id": kanban_card_id,
            }),
        );

        // Drain pending transitions: onDispatchCompleted may call setStatus (review, etc.)
        loop {
            let transitions = state.engine.drain_pending_transitions();
            if transitions.is_empty() {
                break;
            }
            for (t_card_id, old_s, new_s) in &transitions {
                crate::kanban::fire_transition_hooks(
                    &state.db,
                    &state.engine,
                    t_card_id,
                    old_s,
                    new_s,
                );
            }
        }
    } else {
        drop(conn);
    }

    // Read back
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    match conn.query_row(
        "SELECT id, kanban_card_id, from_agent_id, to_agent_id, dispatch_type, status, title, context, result, parent_dispatch_id, chain_depth, created_at, updated_at FROM task_dispatches WHERE id = ?1",
        [&id],
        |row| dispatch_row_to_json(row),
    ) {
        Ok(d) => (StatusCode::OK, Json(json!({"dispatch": d}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

// ── Helpers ────────────────────────────────────────────────────

pub(super) fn dispatch_row_to_json(row: &rusqlite::Row) -> rusqlite::Result<serde_json::Value> {
    let status = row.get::<_, String>(5)?;
    let created_at = row.get::<_, Option<String>>(11).ok().flatten().or_else(|| {
        row.get::<_, Option<i64>>(11)
            .ok()
            .flatten()
            .map(|v| v.to_string())
    });
    let updated_at = row.get::<_, Option<String>>(12).ok().flatten().or_else(|| {
        row.get::<_, Option<i64>>(12)
            .ok()
            .flatten()
            .map(|v| v.to_string())
    });
    let completed_at = if status == "completed" {
        updated_at.clone()
    } else {
        None
    };
    Ok(json!({
        "id": row.get::<_, String>(0)?,
        "kanban_card_id": row.get::<_, Option<String>>(1)?,
        "from_agent_id": row.get::<_, Option<String>>(2)?,
        "to_agent_id": row.get::<_, Option<String>>(3)?,
        "dispatch_type": row.get::<_, Option<String>>(4)?,
        "status": status,
        "title": row.get::<_, Option<String>>(6)?,
        "context": row.get::<_, Option<String>>(7)?.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()),
        "result": row.get::<_, Option<String>>(8)?.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()),
        "context_file": serde_json::Value::Null,
        "result_file": serde_json::Value::Null,
        "result_summary": serde_json::Value::Null,
        "parent_dispatch_id": row.get::<_, Option<String>>(9)?,
        "chain_depth": row.get::<_, i64>(10).unwrap_or(0),
        "created_at": created_at,
        "dispatched_at": row.get::<_, Option<String>>(11).ok().flatten().or_else(|| row.get::<_, Option<i64>>(11).ok().flatten().map(|v| v.to_string())),
        "updated_at": updated_at,
        "completed_at": completed_at,
    }))
}
