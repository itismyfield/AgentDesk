use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;

use super::AppState;

// ── Query / Body types ────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ListDispatchedSessionsQuery {
    #[serde(rename = "includeMerged")]
    pub include_merged: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateDispatchedSessionBody {
    pub status: Option<String>,
    pub active_dispatch_id: Option<String>,
    pub model: Option<String>,
    pub tokens: Option<i64>,
    pub cwd: Option<String>,
    pub session_info: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct HookSessionBody {
    pub session_key: String,
    pub status: Option<String>,
    pub provider: Option<String>,
    pub session_info: Option<String>,
    pub name: Option<String>,
    pub model: Option<String>,
    pub tokens: Option<u64>,
    pub cwd: Option<String>,
    pub dispatch_id: Option<String>,
}

// ── Handlers ──────────────────────────────────────────────────

/// GET /api/dispatched-sessions
pub async fn list_dispatched_sessions(
    State(state): State<AppState>,
    Query(params): Query<ListDispatchedSessionsQuery>,
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

    let include_all = params.include_merged.as_deref() == Some("1");

    let sql = if include_all {
        "SELECT s.id, s.session_key, s.agent_id, s.provider, s.status, s.active_dispatch_id,
                s.model, s.tokens, s.cwd, s.last_heartbeat, s.session_info,
                a.department, a.sprite_number, a.avatar_emoji, a.xp,
                d.name AS department_name, d.name_ko AS department_name_ko, d.color AS department_color
         FROM sessions s
         LEFT JOIN agents a ON s.agent_id = a.id
         LEFT JOIN departments d ON a.department = d.id
         ORDER BY s.id"
    } else {
        "SELECT s.id, s.session_key, s.agent_id, s.provider, s.status, s.active_dispatch_id,
                s.model, s.tokens, s.cwd, s.last_heartbeat, s.session_info,
                a.department, a.sprite_number, a.avatar_emoji, a.xp,
                d.name AS department_name, d.name_ko AS department_name_ko, d.color AS department_color
         FROM sessions s
         LEFT JOIN agents a ON s.agent_id = a.id
         LEFT JOIN departments d ON a.department = d.id
         WHERE s.active_dispatch_id IS NOT NULL
         ORDER BY s.id"
    };

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
        .query_map([], |row| {
            let agent_id = row.get::<_, Option<String>>(2)?;
            let session_key = row.get::<_, Option<String>>(1)?;
            let last_heartbeat = row.get::<_, Option<String>>(9)?;
            Ok(json!({
                "id": row.get::<_, i64>(0)?,
                "session_key": session_key,
                "agent_id": agent_id,
                "provider": row.get::<_, Option<String>>(3)?,
                "status": row.get::<_, Option<String>>(4)?,
                "active_dispatch_id": row.get::<_, Option<String>>(5)?,
                "model": row.get::<_, Option<String>>(6)?,
                "tokens": row.get::<_, i64>(7)?,
                "cwd": row.get::<_, Option<String>>(8)?,
                "last_heartbeat": last_heartbeat,
                "session_info": row.get::<_, Option<String>>(10)?,
                // alias fields for frontend compatibility
                "linked_agent_id": agent_id,
                "last_seen_at": last_heartbeat,
                "name": session_key,
                // joined agent fields
                "department_id": row.get::<_, Option<String>>(11)?,
                "sprite_number": row.get::<_, Option<i64>>(12)?,
                "avatar_emoji": row.get::<_, Option<String>>(13).ok().flatten().unwrap_or_else(|| "\u{1F916}".to_string()),
                "stats_xp": row.get::<_, i64>(14).unwrap_or(0),
                "connected_at": null,
                // joined department fields
                "department_name": row.get::<_, Option<String>>(15)?,
                "department_name_ko": row.get::<_, Option<String>>(16)?,
                "department_color": row.get::<_, Option<String>>(17)?,
            }))
        })
        .ok();

    let sessions: Vec<serde_json::Value> = match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect(),
        None => Vec::new(),
    };

    (StatusCode::OK, Json(json!({"sessions": sessions})))
}

/// POST /api/hook/session — upsert session from dcserver
pub async fn hook_session(
    State(state): State<AppState>,
    Json(body): Json<HookSessionBody>,
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

    // Resolve agent_id from channel name: check discord_channel_id or discord_channel_alt
    let agent_id: Option<String> = body.name.as_ref().and_then(|channel_name| {
        // Try exact match first, then suffix match (e.g. "td-cc" in "cookingheart-td-cc")
        conn.query_row(
            "SELECT id FROM agents WHERE discord_channel_id = ?1 OR discord_channel_alt = ?1",
            [channel_name],
            |row| row.get(0),
        )
        .ok()
        .or_else(|| {
            let mut stmt = conn
                .prepare("SELECT id, discord_channel_id FROM agents")
                .ok()?;
            let mut rows = stmt.query([]).ok()?;
            while let Ok(Some(row)) = rows.next() {
                let id: String = row.get(0).ok()?;
                let ch_id: String = row.get::<_, Option<String>>(1).ok()?.unwrap_or_default();
                if !ch_id.is_empty() && channel_name.contains(&ch_id) {
                    return Some(id);
                }
            }
            None
        })
    });

    let status = body.status.as_deref().unwrap_or("working");
    let provider = body.provider.as_deref().unwrap_or("claude");
    let tokens = body.tokens.unwrap_or(0) as i64;

    let result = conn.execute(
        "INSERT INTO sessions (session_key, agent_id, provider, status, session_info, model, tokens, cwd, active_dispatch_id, last_heartbeat)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, datetime('now'))
         ON CONFLICT(session_key) DO UPDATE SET
           status = excluded.status,
           provider = excluded.provider,
           session_info = COALESCE(excluded.session_info, sessions.session_info),
           model = COALESCE(excluded.model, sessions.model),
           tokens = CASE WHEN excluded.tokens > 0 THEN excluded.tokens ELSE sessions.tokens END,
           cwd = COALESCE(excluded.cwd, sessions.cwd),
           active_dispatch_id = COALESCE(excluded.active_dispatch_id, sessions.active_dispatch_id),
           agent_id = COALESCE(excluded.agent_id, sessions.agent_id),
           last_heartbeat = datetime('now')",
        rusqlite::params![
            body.session_key,
            agent_id,
            provider,
            status,
            body.session_info,
            body.model,
            tokens,
            body.cwd,
            body.dispatch_id,
        ],
    );

    match result {
        Ok(_) => {
            // Fire OnSessionStatusChange hook for policy engines
            let dispatch_id = body.dispatch_id.clone();
            drop(conn);
            let _ = state.engine.fire_hook(
                crate::engine::hooks::Hook::OnSessionStatusChange,
                json!({
                    "session_key": body.session_key,
                    "status": status,
                    "agent_id": agent_id,
                    "dispatch_id": dispatch_id,
                    "provider": provider,
                }),
            );

            // After the hook fires, check if the policy auto-completed a review dispatch.
            // If so, fire OnDispatchCompleted so review-automation.js can process the verdict.
            if status == "idle" {
                if let Some(ref did) = dispatch_id {
                    let conn = state.db.lock().ok();
                    if let Some(conn) = conn {
                        let dispatch_status: Option<(String, String)> = conn
                            .query_row(
                                "SELECT dispatch_type, status FROM task_dispatches WHERE id = ?1",
                                [did],
                                |row| Ok((row.get(0)?, row.get(1)?)),
                            )
                            .ok();
                        drop(conn);
                        if let Some((dtype, dstatus)) = dispatch_status {
                            if (dtype == "review" || dtype == "review-decision")
                                && dstatus == "completed"
                            {
                                // Policy auto-completed this review dispatch — fire OnDispatchCompleted
                                let _ = state.engine.fire_hook(
                                    crate::engine::hooks::Hook::OnDispatchCompleted,
                                    json!({
                                        "dispatch_id": did,
                                    }),
                                );
                            }
                        }
                    }
                }
            }

            (StatusCode::OK, Json(json!({"ok": true})))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// DELETE /api/dispatched-sessions/cleanup
pub async fn cleanup_sessions(
    State(state): State<AppState>,
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

    match conn.execute(
        "DELETE FROM dispatched_sessions WHERE status = 'disconnected'",
        [],
    ) {
        Ok(n) => (StatusCode::OK, Json(json!({"ok": true, "deleted": n}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// PATCH /api/dispatched-sessions/:id
pub async fn update_dispatched_session(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateDispatchedSessionBody>,
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

    let mut sets: Vec<String> = Vec::new();
    let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    let mut idx = 1;

    if let Some(ref status) = body.status {
        sets.push(format!("status = ?{}", idx));
        values.push(Box::new(status.clone()));
        idx += 1;
    }
    if let Some(ref dispatch_id) = body.active_dispatch_id {
        sets.push(format!("active_dispatch_id = ?{}", idx));
        values.push(Box::new(dispatch_id.clone()));
        idx += 1;
    }
    if let Some(ref model) = body.model {
        sets.push(format!("model = ?{}", idx));
        values.push(Box::new(model.clone()));
        idx += 1;
    }
    if let Some(tokens) = body.tokens {
        sets.push(format!("tokens = ?{}", idx));
        values.push(Box::new(tokens));
        idx += 1;
    }
    if let Some(ref cwd) = body.cwd {
        sets.push(format!("cwd = ?{}", idx));
        values.push(Box::new(cwd.clone()));
        idx += 1;
    }
    if let Some(ref session_info) = body.session_info {
        sets.push(format!("session_info = ?{}", idx));
        values.push(Box::new(session_info.clone()));
        idx += 1;
    }

    if sets.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no fields to update"})),
        );
    }

    let sql = format!(
        "UPDATE sessions SET {} WHERE id = ?{}",
        sets.join(", "),
        idx
    );
    values.push(Box::new(id));

    let params_ref: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|v| v.as_ref()).collect();
    match conn.execute(&sql, params_ref.as_slice()) {
        Ok(0) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "session not found"})),
        ),
        Ok(_) => (StatusCode::OK, Json(json!({"ok": true}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}
