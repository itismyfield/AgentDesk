use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::dispatch;

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
}

#[derive(Debug, Deserialize)]
pub struct UpdateDispatchBody {
    pub status: Option<String>,
    pub result: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct LinkDispatchThreadBody {
    pub dispatch_id: String,
    pub thread_id: String,
    pub channel_id: Option<String>,
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

/// POST /api/dispatches
pub async fn create_dispatch(
    State(state): State<AppState>,
    Json(body): Json<CreateDispatchBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let dispatch_type = body
        .dispatch_type
        .unwrap_or_else(|| "implementation".to_string());
    let context = body.context.unwrap_or(json!({}));

    match dispatch::create_dispatch(
        &state.db,
        &state.engine,
        &body.kanban_card_id,
        &body.to_agent_id,
        &dispatch_type,
        &body.title,
        &context,
    ) {
        Ok(d) => {
            // Send dispatch message to the target agent's Discord channel
            let to_agent_id = body.to_agent_id.clone();
            let title = body.title.clone();
            let card_id = body.kanban_card_id.clone();
            let dispatch_id = d["id"].as_str().unwrap_or("").to_string();
            let db = state.db.clone();
            tokio::spawn(async move {
                send_dispatch_to_discord(&db, &to_agent_id, &title, &card_id, &dispatch_id).await;
            });
            (StatusCode::CREATED, Json(json!({"dispatch": d})))
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
    // If status is "completed", use the dispatch engine's complete_dispatch
    if body.status.as_deref() == Some("completed") {
        let result = body.result.unwrap_or(json!({}));
        match dispatch::complete_dispatch(&state.db, &state.engine, &id, &result) {
            Ok(d) => {
                let db_clone = state.db.clone();
                let dispatch_id = id.clone();
                tokio::spawn(async move {
                    handle_completed_dispatch_followups(&db_clone, &dispatch_id).await;
                });
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

fn dispatch_row_to_json(row: &rusqlite::Row) -> rusqlite::Result<serde_json::Value> {
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

// ── Channel-thread map helpers ────────────────────────────────

/// Look up the thread_id for a specific channel from channel_thread_map.
/// Falls back to active_thread_id for backward compatibility.
fn get_thread_for_channel(
    conn: &rusqlite::Connection,
    card_id: &str,
    channel_id: u64,
) -> Option<String> {
    let map_json: Option<String> = conn
        .query_row(
            "SELECT channel_thread_map FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| row.get(0),
        )
        .ok()
        .flatten();

    if let Some(ref json_str) = map_json {
        if let Ok(map) =
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(json_str)
        {
            let key = channel_id.to_string();
            if let Some(tid) = map.get(&key).and_then(|v| v.as_str()) {
                return Some(tid.to_string());
            }
        }
    }

    // Fallback: legacy active_thread_id (no channel distinction)
    conn.query_row(
        "SELECT active_thread_id FROM kanban_cards WHERE id = ?1 AND active_thread_id IS NOT NULL",
        [card_id],
        |row| row.get(0),
    )
    .ok()
}

/// Set the thread_id for a specific channel in channel_thread_map.
/// Also updates active_thread_id for backward compatibility.
fn set_thread_for_channel(
    conn: &rusqlite::Connection,
    card_id: &str,
    channel_id: u64,
    thread_id: &str,
) {
    let existing: Option<String> = conn
        .query_row(
            "SELECT channel_thread_map FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| row.get(0),
        )
        .ok()
        .flatten();

    let mut map: serde_json::Map<String, serde_json::Value> = existing
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    map.insert(
        channel_id.to_string(),
        serde_json::Value::String(thread_id.to_string()),
    );

    let json_str = serde_json::to_string(&map).unwrap_or_default();
    conn.execute(
        "UPDATE kanban_cards SET channel_thread_map = ?1, active_thread_id = ?2 WHERE id = ?3",
        rusqlite::params![json_str, thread_id, card_id],
    )
    .ok();
}

/// Clear thread mapping for a specific channel.
fn clear_thread_for_channel(conn: &rusqlite::Connection, card_id: &str, channel_id: u64) {
    let existing: Option<String> = conn
        .query_row(
            "SELECT channel_thread_map FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| row.get(0),
        )
        .ok()
        .flatten();

    if let Some(json_str) = existing {
        if let Ok(mut map) =
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&json_str)
        {
            map.remove(&channel_id.to_string());
            let new_json = serde_json::to_string(&map).unwrap_or_default();
            conn.execute(
                "UPDATE kanban_cards SET channel_thread_map = ?1 WHERE id = ?2",
                rusqlite::params![new_json, card_id],
            )
            .ok();
        }
    }
}

/// Parse a channel identifier (numeric ID or alias like "adk-cc") to u64.
fn parse_channel_id(channel: &str) -> Option<u64> {
    channel
        .parse::<u64>()
        .ok()
        .or_else(|| resolve_channel_alias(channel))
}

/// Clear ALL thread mappings (card done).
pub(super) fn clear_all_threads(conn: &rusqlite::Connection, card_id: &str) {
    conn.execute(
        "UPDATE kanban_cards SET channel_thread_map = NULL, active_thread_id = NULL WHERE id = ?1",
        [card_id],
    )
    .ok();
}

/// Send a dispatch notification to the target agent's Discord channel.
/// Message format: `DISPATCH:<dispatch_id> - <title>\n<issue_url>`
/// The `DISPATCH:<uuid>` prefix is required for the dcserver to link the
/// resulting Claude session back to the kanban card (via `parse_dispatch_id`).
pub(crate) async fn send_dispatch_to_discord(
    db: &crate::db::Db,
    agent_id: &str,
    title: &str,
    card_id: &str,
    dispatch_id: &str,
) {
    // Guard: atomic reservation — exactly one caller wins the INSERT
    {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        let inserted = conn
            .execute(
                "INSERT OR IGNORE INTO kv_meta (key, value) VALUES (?1, ?2)",
                rusqlite::params![format!("dispatch_notified:{dispatch_id}"), dispatch_id],
            )
            .unwrap_or(0);
        if inserted == 0 {
            // Already reserved by another caller — skip
            return;
        }
    }

    // Determine dispatch type to choose the right channel
    let dispatch_type: Option<String> = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        conn.query_row(
            "SELECT dispatch_type FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| row.get(0),
        )
        .ok()
        .flatten()
    };

    // For review dispatches, use the alternate channel (counter-model)
    let mut use_alt = use_counter_model_channel(dispatch_type.as_deref());

    // #145: Check if this dispatch is in a unified thread auto-queue run (dispatch_id path)
    let is_unified_run: bool = db
        .lock()
        .ok()
        .and_then(|conn| {
            conn.query_row(
                "SELECT COUNT(*) > 0 FROM auto_queue_runs r \
                 JOIN auto_queue_entries e ON e.run_id = r.id \
                 WHERE e.dispatch_id = ?1 AND r.unified_thread = 1 AND r.status = 'active'",
                [dispatch_id],
                |row| row.get(0),
            )
            .ok()
        })
        .unwrap_or(false);
    // Each channel (primary/alt) gets its own unified thread — don't override use_alt

    // Look up agent's discord channel
    let channel_id: Option<String> = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        let col = if use_alt {
            "discord_channel_alt"
        } else {
            "discord_channel_id"
        };
        conn.query_row(
            &format!("SELECT {col} FROM agents WHERE id = ?1"),
            [agent_id],
            |row| row.get(0),
        )
        .ok()
    };

    let channel_id = match channel_id {
        Some(id) if !id.is_empty() => id,
        _ => {
            tracing::warn!(
                "[dispatch] No discord_channel_id for agent {agent_id}, skipping message"
            );
            return;
        }
    };

    // Parse channel ID as u64, or resolve alias via role_map.json
    let channel_id_num: u64 = match channel_id.parse() {
        Ok(n) => n,
        Err(_) => {
            // Try resolving channel name alias from role_map.json
            match resolve_channel_alias(&channel_id) {
                Some(n) => n,
                None => {
                    tracing::warn!(
                        "[dispatch] Cannot resolve channel '{channel_id}' for agent {agent_id}"
                    );
                    return;
                }
            }
        }
    };

    // Look up the issue URL and number for context
    let (issue_url, issue_number): (Option<String>, Option<i64>) = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        conn.query_row(
            "SELECT github_issue_url, github_issue_number FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap_or_default()
    };

    // For review dispatches, look up reviewed commit SHA and target provider from context
    let (reviewed_commit, target_provider): (Option<String>, Option<String>) = if use_alt {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        let ctx: Option<String> = conn
            .query_row(
                "SELECT context FROM task_dispatches WHERE id = ?1",
                [dispatch_id],
                |row| row.get(0),
            )
            .ok()
            .flatten();
        let ctx_val: serde_json::Value = ctx
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(serde_json::json!({}));
        (
            ctx_val
                .get("reviewed_commit")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            ctx_val
                .get("target_provider")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        )
    } else {
        (None, None)
    };

    let message = format_dispatch_message(
        dispatch_id,
        title,
        issue_url.as_deref(),
        issue_number,
        use_alt,
        reviewed_commit.as_deref(),
        target_provider.as_deref(),
    );

    // Send via Discord HTTP API using the announce bot
    let token = match crate::credential::read_bot_token("announce") {
        Some(t) => t,
        None => {
            tracing::warn!(
                "[dispatch] No announce bot token (missing credential/announce_bot_token)"
            );
            return;
        }
    };

    // ── Thread reuse: check if card already has an active thread ──
    let client = reqwest::Client::new();
    let dispatch_type_label = dispatch_type.as_deref().unwrap_or("implementation");

    // #145: Look up per-channel unified thread via dispatch_id path
    let mut unified_thread_id: Option<String> = db.lock().ok().and_then(|conn| {
        let map_json: Option<String> = conn
            .query_row(
                "SELECT r.unified_thread_id FROM auto_queue_runs r \
                     JOIN auto_queue_entries e ON e.run_id = r.id \
                     WHERE e.dispatch_id = ?1 AND r.unified_thread = 1 AND r.status = 'active' \
                     AND r.unified_thread_id IS NOT NULL",
                [dispatch_id],
                |row| row.get::<_, String>(0),
            )
            .ok();
        map_json.and_then(|json_str| {
            // Try parsing as JSON map {"channel_id": "thread_id", ...}
            if let Ok(map) = serde_json::from_str::<serde_json::Value>(&json_str) {
                if map.is_object() {
                    map.get(&channel_id_num.to_string())
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                } else {
                    // Legacy: parsed as number/string, not a JSON object — skip
                    // A new JSON map will be created when a thread is made for this channel
                    None
                }
            } else {
                // Unparseable — skip, will be overwritten with proper JSON map
                None
            }
        })
    });

    // Try to reuse existing thread for this card (channel-specific)
    let existing_thread_id: Option<String> = if unified_thread_id.is_some() {
        unified_thread_id.clone()
    } else {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        get_thread_for_channel(&conn, card_id, channel_id_num)
    };

    if let Some(ref existing_tid) = existing_thread_id {
        // Try to unarchive and reuse the existing thread
        if let Some(reused) = try_reuse_thread(
            &client,
            &token,
            existing_tid,
            channel_id_num,
            dispatch_type_label,
            &message,
            dispatch_id,
            card_id,
            db,
        )
        .await
        {
            if reused {
                return;
            }
        }
    }

    // #145: If unified thread reuse failed, remove this channel from JSON map (dispatch_id path)
    if unified_thread_id.is_some() {
        if let Ok(conn) = db.lock() {
            let existing: String = conn
                .query_row(
                    "SELECT COALESCE(unified_thread_id, '{}') FROM auto_queue_runs \
                     WHERE id IN (SELECT run_id FROM auto_queue_entries WHERE dispatch_id = ?1) \
                     AND status IN ('active', 'paused')",
                    [dispatch_id],
                    |row| row.get(0),
                )
                .unwrap_or_else(|_| "{}".to_string());
            if let Ok(mut map) = serde_json::from_str::<serde_json::Value>(&existing) {
                if let Some(obj) = map.as_object_mut() {
                    obj.remove(&channel_id_num.to_string());
                }
                conn.execute(
                    "UPDATE auto_queue_runs SET unified_thread_id = ?1 \
                     WHERE id IN (SELECT run_id FROM auto_queue_entries WHERE dispatch_id = ?2) \
                     AND status IN ('active', 'paused')",
                    rusqlite::params![map.to_string(), dispatch_id],
                )
                .ok();
            }
        }
        unified_thread_id = None; // Reset local so new thread creation saves to run below
    }

    // No existing thread or reuse failed — create a new thread
    // #137: For unified thread, build name from all queued issue numbers
    let thread_name = if unified_thread_id.is_none() {
        // First dispatch in unified run — check if we should use a combined name
        let unified_issues: Option<String> = db
            .lock()
            .ok()
            .and_then(|conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT kc.github_issue_number FROM auto_queue_entries e \
                         JOIN auto_queue_runs r ON e.run_id = r.id \
                         JOIN kanban_cards kc ON e.kanban_card_id = kc.id \
                         WHERE r.unified_thread = 1 \
                         AND e.kanban_card_id = ?1 AND kc.github_issue_number IS NOT NULL \
                         LIMIT 1",
                    )
                    .ok()?;
                // If this card is in a unified run, gather all issue numbers
                let is_unified: bool = stmt
                    .query_map([card_id], |row| row.get::<_, i64>(0))
                    .ok()
                    .map(|rows| rows.count() > 0)
                    .unwrap_or(false);
                if !is_unified {
                    return None;
                }
                drop(stmt);
                let mut stmt2 = conn
                    .prepare(
                        "SELECT kc.github_issue_number FROM auto_queue_entries e \
                         JOIN auto_queue_runs r ON e.run_id = r.id \
                         JOIN kanban_cards kc ON e.kanban_card_id = kc.id \
                         WHERE r.unified_thread = 1 \
                         AND e.run_id IN (SELECT run_id FROM auto_queue_entries WHERE kanban_card_id = ?1) \
                         AND kc.github_issue_number IS NOT NULL \
                         ORDER BY e.priority_rank ASC",
                    )
                    .ok()?;
                // Get current card's issue number for highlighting
                let current_issue: Option<i64> = conn
                    .query_row(
                        "SELECT github_issue_number FROM kanban_cards WHERE id = ?1",
                        [card_id],
                        |row| row.get(0),
                    )
                    .ok();
                let nums: Vec<String> = stmt2
                    .query_map([card_id], |row| row.get::<_, i64>(0))
                    .ok()?
                    .filter_map(|r| r.ok())
                    .map(|n| {
                        if Some(n) == current_issue {
                            format!("▸{}", n)
                        } else {
                            format!("#{}", n)
                        }
                    })
                    .collect();
                if nums.is_empty() {
                    None
                } else {
                    Some(nums.join(" "))
                }
            });

        if let Some(name) = unified_issues {
            // Discord thread name max 100 chars
            name.chars().take(100).collect()
        } else if let Some(num) = issue_number {
            let short: String = title.chars().take(90).collect();
            format!("#{} {}", num, short)
        } else {
            title.chars().take(100).collect()
        }
    } else if let Some(num) = issue_number {
        let short: String = title.chars().take(90).collect();
        format!("#{} {}", num, short)
    } else {
        title.chars().take(100).collect()
    };

    let thread_url = format!(
        "https://discord.com/api/v10/channels/{}/threads",
        channel_id_num
    );
    let thread_resp = client
        .post(&thread_url)
        .header("Authorization", format!("Bot {}", token))
        .json(&serde_json::json!({
            "name": thread_name,
            "type": 11, // PUBLIC_THREAD
            "auto_archive_duration": 1440, // 24h
        }))
        .send()
        .await;

    match thread_resp {
        Ok(tr) if tr.status().is_success() => {
            if let Ok(thread_body) = tr.json::<serde_json::Value>().await {
                let thread_id = thread_body.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if !thread_id.is_empty() {
                    // Send dispatch message into the thread BEFORE persisting thread_id.
                    // If the POST fails, we rollback (don't save thread_id) so that
                    // [I-0] recovery sends to the channel and future dispatches won't
                    // reuse an empty thread.
                    let thread_msg_url = format!(
                        "https://discord.com/api/v10/channels/{}/messages",
                        thread_id
                    );
                    let thread_msg_ok = client
                        .post(&thread_msg_url)
                        .header("Authorization", format!("Bot {}", token))
                        .json(&serde_json::json!({"content": message}))
                        .send()
                        .await
                        .map(|r| r.status().is_success())
                        .unwrap_or(false);
                    if thread_msg_ok {
                        // Persist thread_id on success (notified marker already set atomically)
                        if let Ok(conn) = db.lock() {
                            conn.execute(
                                "UPDATE task_dispatches SET thread_id = ?1 WHERE id = ?2",
                                rusqlite::params![thread_id, dispatch_id],
                            )
                            .ok();
                            set_thread_for_channel(&conn, card_id, channel_id_num, thread_id);
                            // #141: Store unified thread per channel in JSON map
                            // Save when: no existing thread for this channel (unified_thread_id is None)
                            // AND this card belongs to a unified run
                            if unified_thread_id.is_none() && is_unified_run {
                                // Read existing map, add this channel's thread
                                let existing: String = conn
                                    .query_row(
                                        "SELECT COALESCE(unified_thread_id, '{}') FROM auto_queue_runs \
                                         WHERE id IN (SELECT run_id FROM auto_queue_entries WHERE dispatch_id = ?1) \
                                         AND status IN ('active', 'paused')",
                                        [dispatch_id],
                                        |row| row.get(0),
                                    )
                                    .unwrap_or_else(|_| "{}".to_string());
                                // #141: Ensure we have a proper JSON object — legacy plain
                                // string/number values get upgraded to a map, preserving
                                // the legacy thread_id under the primary channel key
                                let mut map: serde_json::Value = serde_json::from_str::<
                                    serde_json::Value,
                                >(
                                    &existing
                                )
                                .ok()
                                .filter(|v: &serde_json::Value| v.is_object())
                                .unwrap_or_else(|| {
                                    // Legacy: plain thread_id — promote to JSON map
                                    // Preserve it under primary channel key if available
                                    let legacy_tid = existing.trim().to_string();
                                    if legacy_tid.is_empty() || legacy_tid == "{}" {
                                        return serde_json::json!({});
                                    }
                                    let primary_ch: Option<String> = conn
                                        .query_row(
                                            "SELECT discord_channel_id FROM agents WHERE id = ?1",
                                            [agent_id],
                                            |row| row.get(0),
                                        )
                                        .ok();
                                    let primary_num: Option<u64> = primary_ch.and_then(|ch| {
                                        ch.parse().ok().or_else(|| resolve_channel_alias(&ch))
                                    });
                                    if let Some(pch) = primary_num {
                                        // Backfill channel_thread_map with the legacy primary mapping
                                        // so get_thread_for_channel() resolves correctly for primary channel
                                        set_thread_for_channel(&conn, card_id, pch, &legacy_tid);
                                        serde_json::json!({ pch.to_string(): legacy_tid })
                                    } else {
                                        serde_json::json!({})
                                    }
                                });
                                map[channel_id_num.to_string()] = serde_json::json!(thread_id);
                                // #145: Direct dispatch→entry→run association via dispatch_id.
                                // Eliminates card_id ambiguity when same card is re-queued.
                                conn.execute(
                                    "UPDATE auto_queue_runs SET unified_thread_id = ?1, unified_thread_channel_id = ?2 \
                                     WHERE id IN (SELECT run_id FROM auto_queue_entries WHERE dispatch_id = ?3) \
                                     AND status IN ('active', 'paused')",
                                    rusqlite::params![map.to_string(), thread_id, dispatch_id],
                                )
                                .ok();
                            }
                        }
                        tracing::info!(
                            "[dispatch] Created thread {thread_id} and sent dispatch {dispatch_id} to {agent_id}"
                        );
                    } else {
                        // Rollback atomic reservation so retry can succeed
                        if let Ok(conn) = db.lock() {
                            conn.execute(
                                "DELETE FROM kv_meta WHERE key = ?1",
                                [&format!("dispatch_notified:{dispatch_id}")],
                            )
                            .ok();
                        }
                        tracing::warn!(
                            "[dispatch] Thread message POST failed for dispatch {dispatch_id}, rolled back notified marker"
                        );
                    }
                }
            }
        }
        Ok(tr) => {
            // Thread creation failed — fall back to sending directly to the channel
            let status = tr.status();
            tracing::warn!(
                "[dispatch] Thread creation failed ({status}), falling back to channel message"
            );
            let url = format!(
                "https://discord.com/api/v10/channels/{}/messages",
                channel_id_num
            );
            match client
                .post(&url)
                .header("Authorization", format!("Bot {}", token))
                .json(&serde_json::json!({"content": message}))
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => {
                    // notified marker already set atomically at function entry
                    tracing::info!(
                        "[dispatch] Sent fallback message to {agent_id} (channel {channel_id})"
                    );
                }
                Ok(r) => {
                    let st = r.status();
                    let body = r.text().await.unwrap_or_default();
                    // Rollback atomic reservation so retry can succeed
                    if let Ok(conn) = db.lock() {
                        conn.execute(
                            "DELETE FROM kv_meta WHERE key = ?1",
                            [&format!("dispatch_notified:{dispatch_id}")],
                        )
                        .ok();
                    }
                    tracing::warn!("[dispatch] Discord API error {st}: {body}");
                }
                Err(e) => {
                    // Rollback atomic reservation so retry can succeed
                    if let Ok(conn) = db.lock() {
                        conn.execute(
                            "DELETE FROM kv_meta WHERE key = ?1",
                            [&format!("dispatch_notified:{dispatch_id}")],
                        )
                        .ok();
                    }
                    tracing::warn!("[dispatch] Request failed: {e}");
                }
            }
        }
        Err(e) => {
            // Rollback atomic reservation so retry can succeed
            if let Ok(conn) = db.lock() {
                conn.execute(
                    "DELETE FROM kv_meta WHERE key = ?1",
                    [&format!("dispatch_notified:{dispatch_id}")],
                )
                .ok();
            }
            tracing::warn!("[dispatch] Thread creation request failed: {e}");
        }
    }
}

/// Try to reuse an existing Discord thread for a dispatch.
/// Returns `Some(true)` if reuse succeeded, `Some(false)` if the thread exists but is locked,
/// or `None` if the thread couldn't be accessed (deleted, wrong parent, etc.).
async fn try_reuse_thread(
    client: &reqwest::Client,
    token: &str,
    thread_id: &str,
    expected_parent: u64,
    dispatch_type: &str,
    message: &str,
    dispatch_id: &str,
    card_id: &str,
    db: &crate::db::Db,
) -> Option<bool> {
    // 1. Fetch thread info to verify it exists and belongs to the right parent channel
    let thread_info_url = format!("https://discord.com/api/v10/channels/{}", thread_id);
    let resp = client
        .get(&thread_info_url)
        .header("Authorization", format!("Bot {}", token))
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        tracing::info!("[dispatch] Thread {thread_id} no longer accessible, will create new");
        // Clear stale thread for this channel
        if let Ok(conn) = db.lock() {
            clear_thread_for_channel(&conn, card_id, expected_parent);
        }
        return None;
    }

    let body: serde_json::Value = resp.json().await.ok()?;

    // Check parent_id — only reuse threads from the same channel.
    // Each channel independently manages its own thread per card.
    let parent_id = body
        .get("parent_id")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    if parent_id != expected_parent {
        tracing::info!(
            "[dispatch] Thread {thread_id} belongs to channel {parent_id}, expected {expected_parent}, skipping reuse"
        );
        // Clear stale cross-channel thread references so retries don't keep
        // probing the wrong thread via active_thread_id fallback
        if let Ok(conn) = db.lock() {
            clear_thread_for_channel(&conn, card_id, expected_parent);
            // Also clear active_thread_id if it points to the mismatched thread,
            // preventing get_thread_for_channel() fallback from re-selecting it
            conn.execute(
                "UPDATE kanban_cards SET active_thread_id = NULL \
                 WHERE id = ?1 AND active_thread_id = ?2",
                rusqlite::params![card_id, thread_id],
            )
            .ok();
        }
        return None;
    }

    // Check if thread is locked — locked threads cannot be reused
    let metadata = body.get("thread_metadata");
    let is_locked = metadata
        .and_then(|m| m.get("locked"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if is_locked {
        tracing::info!("[dispatch] Thread {thread_id} is locked, will create new");
        // Clear stale thread for this channel
        if let Ok(conn) = db.lock() {
            clear_thread_for_channel(&conn, card_id, expected_parent);
        }
        return Some(false);
    }

    // Unarchive if needed
    let is_archived = metadata
        .and_then(|m| m.get("archived"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if is_archived {
        let unarchive_resp = client
            .patch(&thread_info_url)
            .header("Authorization", format!("Bot {}", token))
            .json(&serde_json::json!({"archived": false}))
            .send()
            .await;
        match unarchive_resp {
            Ok(r) if r.status().is_success() => {
                tracing::info!("[dispatch] Unarchived thread {thread_id} for reuse");
            }
            _ => {
                tracing::warn!(
                    "[dispatch] Failed to unarchive thread {thread_id}, will create new"
                );
                return None;
            }
        }
    }

    // 2a. Update thread name — for unified threads, move ▸ marker to current issue
    let current_thread_name = body.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let has_marker = current_thread_name.contains('▸');
    let new_name: Option<String> = if has_marker {
        // Unified thread — update ▸ marker position
        let current_issue: Option<i64> = db.lock().ok().and_then(|conn| {
            conn.query_row(
                "SELECT github_issue_number FROM kanban_cards WHERE id = ?1",
                [card_id],
                |row| row.get(0),
            )
            .ok()
        });
        current_issue.map(|cur| {
            // Replace all ▸N with #N, then set ▸ on current
            let mut name = current_thread_name.replace('▸', "#");
            let target = format!("#{}", cur);
            let replacement = format!("▸{}", cur);
            name = name.replacen(&target, &replacement, 1);
            name
        })
    } else {
        // Single-card thread — update to current issue
        db.lock().ok().and_then(|conn| {
            conn.query_row(
                "SELECT kc.github_issue_number, kc.title FROM kanban_cards kc WHERE kc.id = ?1",
                [card_id],
                |row| {
                    let num: Option<i64> = row.get(0)?;
                    let title: String = row.get(1)?;
                    Ok(num.map(|n| {
                        let short: String = title.chars().take(85).collect();
                        format!("#{} {}", n, short)
                    }))
                },
            )
            .ok()
            .flatten()
        })
    };
    {
        if let Some(ref name) = new_name {
            let _ = client
                .patch(&thread_info_url)
                .header("Authorization", format!("Bot {}", token))
                .json(&serde_json::json!({"name": name}))
                .send()
                .await;
        }
    }

    // 2b. Send separator message to visually distinguish dispatch phases
    let separator = format!("── {} dispatch ──", dispatch_type);
    let msg_url = format!(
        "https://discord.com/api/v10/channels/{}/messages",
        thread_id
    );
    let _ = client
        .post(&msg_url)
        .header("Authorization", format!("Bot {}", token))
        .json(&serde_json::json!({"content": separator}))
        .send()
        .await;

    // 3. Send the dispatch message
    let msg_ok = client
        .post(&msg_url)
        .header("Authorization", format!("Bot {}", token))
        .json(&serde_json::json!({"content": message}))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);

    if msg_ok {
        // Update dispatch thread_id and mark as notified
        if let Ok(conn) = db.lock() {
            conn.execute(
                "UPDATE task_dispatches SET thread_id = ?1 WHERE id = ?2",
                rusqlite::params![thread_id, dispatch_id],
            )
            .ok();
            conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                rusqlite::params![format!("dispatch_notified:{}", dispatch_id), dispatch_id],
            )
            .ok();
        }
        tracing::info!("[dispatch] Reused thread {thread_id} for dispatch {dispatch_id}");
        Some(true)
    } else {
        tracing::warn!("[dispatch] Failed to send message to reused thread {thread_id}");
        None
    }
}

/// Send review result notification to the agent's PRIMARY channel.
/// Called after a counter-model review dispatch completes.
pub(super) async fn send_review_result_to_primary(
    db: &crate::db::Db,
    card_id: &str,
    verdict: &str,
) {
    // Look up card info
    let (agent_id, title, issue_url, channel_id): (String, String, Option<String>, String) = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        let result = conn.query_row(
            "SELECT kc.assigned_agent_id, kc.title, kc.github_issue_url, a.discord_channel_id \
             FROM kanban_cards kc \
             JOIN agents a ON kc.assigned_agent_id = a.id \
             WHERE kc.id = ?1",
            [card_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        );
        match result {
            Ok(r) => r,
            Err(_) => return,
        }
    };

    // Resolve channel ID (may be a name alias)
    let channel_id_num: u64 = match channel_id.parse() {
        Ok(n) => n,
        Err(_) => match resolve_channel_alias(&channel_id) {
            Some(n) => n,
            None => return,
        },
    };

    let token = match crate::credential::read_bot_token("announce") {
        Some(t) => t,
        None => return,
    };
    let client = reqwest::Client::new();

    // Look up thread for primary channel (review results go to primary)
    // channel_id_num (u64) was already resolved above from the alias
    let active_thread_id: Option<String> = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        get_thread_for_channel(&conn, card_id, channel_id_num)
    };
    // Use resolved numeric channel ID for Discord API calls
    let channel_id = channel_id_num.to_string();

    // Determine target: existing thread from primary channel (if valid) or main channel.
    let target_channel = if let Some(ref tid) = active_thread_id {
        let info_url = format!("https://discord.com/api/v10/channels/{}", tid);
        let valid = match client
            .get(&info_url)
            .header("Authorization", format!("Bot {}", &token))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(body) = resp.json::<serde_json::Value>().await {
                    let locked = body
                        .get("thread_metadata")
                        .and_then(|m| m.get("locked"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    !locked
                } else {
                    false
                }
            }
            _ => false,
        };
        if valid {
            // Unarchive if needed — check result and fallback to channel on failure
            let unarchive_ok = match client
                .patch(&info_url)
                .header("Authorization", format!("Bot {}", &token))
                .json(&serde_json::json!({"archived": false}))
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => true,
                Ok(r) => {
                    tracing::warn!(
                        "[review] Failed to unarchive thread {tid}: HTTP {}",
                        r.status()
                    );
                    false
                }
                Err(e) => {
                    tracing::warn!("[review] Failed to unarchive thread {tid}: {e}");
                    false
                }
            };
            if unarchive_ok {
                tid.clone()
            } else {
                // Unarchive failed — clear stale channel-thread mapping and fall back to channel
                if let Ok(conn) = db.lock() {
                    clear_thread_for_channel(&conn, card_id, channel_id_num);
                }
                channel_id.clone()
            }
        } else {
            // Thread is locked or inaccessible — clear stale channel-thread mapping and fall back to channel
            if let Ok(conn) = db.lock() {
                clear_thread_for_channel(&conn, card_id, channel_id_num);
            }
            channel_id.clone()
        }
    } else {
        channel_id.clone()
    };
    let sending_to_thread = active_thread_id
        .as_ref()
        .map(|t| *t == target_channel)
        .unwrap_or(false);

    // For pass/approved verdict, just send a simple notification (no action needed).
    // #116: accept is NOT a counter-model verdict — it's a review-decision action.
    if verdict == "pass" || verdict == "approved" {
        let url_line = issue_url.map(|u| format!("\n{u}")).unwrap_or_default();
        let message = format!("✅ [리뷰 통과] {title} — done으로 이동{url_line}");

        let url = format!(
            "https://discord.com/api/v10/channels/{}/messages",
            target_channel
        );
        let _ = client
            .post(&url)
            .header("Authorization", format!("Bot {}", token))
            .json(&serde_json::json!({"content": message}))
            .send()
            .await;
        return;
    }

    // For unknown verdict (e.g. session idle auto-completed without verdict submission),
    // notify the original agent to check GitHub comments and decide.
    if verdict == "unknown" {
        let url_line = issue_url.map(|u| format!("\n{u}")).unwrap_or_default();
        let message = format!(
            "⚠️ [리뷰 verdict 미제출] {title}\n\
             카운터모델이 verdict를 제출하지 않고 세션이 종료됐습니다.\n\
             GitHub 이슈 코멘트를 확인하고 리뷰 내용이 있으면 반영해주세요.{url_line}"
        );

        let url = format!(
            "https://discord.com/api/v10/channels/{}/messages",
            target_channel
        );
        let _ = client
            .post(&url)
            .header("Authorization", format!("Bot {}", token))
            .json(&serde_json::json!({"content": message}))
            .send()
            .await;
        return;
    }

    // #118: If approach-change already created a rework dispatch (review_status = rework_pending),
    // skip creating the review-decision dispatch to avoid double dispatch.
    {
        let skip = db
            .lock()
            .ok()
            .and_then(|conn| {
                conn.query_row(
                    "SELECT review_status FROM kanban_cards WHERE id = ?1",
                    [card_id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .ok()
                .flatten()
            })
            .map(|s| s == "rework_pending")
            .unwrap_or(false);
        if skip {
            tracing::info!(
                "[review-followup] #118 skipping review-decision for {card_id} — approach-change rework already dispatched"
            );
            return;
        }
    }

    // For improve/rework/reject: create a review-decision dispatch via central create_dispatch_core
    // to enforce the done terminal guard (prevents review-decision on done cards).
    let dispatch_id = match crate::dispatch::create_dispatch_core(
        db,
        card_id,
        &agent_id,
        "review-decision",
        &format!("[리뷰 검토] {title}"),
        &serde_json::json!({"verdict": verdict}),
    ) {
        Ok((id, _old_status)) => {
            // #117: Update canonical card_review_state with pending_dispatch_id
            if let Ok(conn) = db.lock() {
                conn.execute(
                    "INSERT INTO card_review_state (card_id, state, pending_dispatch_id, last_verdict, updated_at) \
                     VALUES (?1, 'suggestion_pending', ?2, ?3, datetime('now')) \
                     ON CONFLICT(card_id) DO UPDATE SET \
                       pending_dispatch_id = ?2, last_verdict = ?3, updated_at = datetime('now')",
                    rusqlite::params![card_id, id, verdict],
                ).ok();
            }
            id
        }
        Err(e) => {
            tracing::warn!(
                "[review-followup] skipping review-decision dispatch for card {card_id}: {e}"
            );
            return;
        }
    };

    let url_line = issue_url.map(|u| format!("\n{u}")).unwrap_or_default();
    let message = format!(
        "DISPATCH:{dispatch_id} - [리뷰 검토] {title}\n\
         📝 카운터모델 리뷰 결과: **{verdict}**\n\
         GitHub 이슈 코멘트를 확인하고 다음 중 하나를 선택하세요:\n\
         • 수용 → 리뷰 반영 수정 후 review-decision API에 accept 호출\n\
         • 반론 → GitHub 코멘트로 이의 제기 후 review-decision API에 dispute 호출\n\
         • 불수용 → review-decision API에 dismiss 호출{url_line}"
    );

    // Send separator + dispatch to existing thread, or just to channel
    if sending_to_thread {
        let msg_url = format!(
            "https://discord.com/api/v10/channels/{}/messages",
            target_channel
        );
        // Separator
        let _ = client
            .post(&msg_url)
            .header("Authorization", format!("Bot {}", token))
            .json(&serde_json::json!({"content": "── review-decision dispatch ──"}))
            .send()
            .await;
        // Dispatch message
        let ok = client
            .post(&msg_url)
            .header("Authorization", format!("Bot {}", token))
            .json(&serde_json::json!({"content": message}))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        if ok {
            if let Ok(conn) = db.lock() {
                conn.execute(
                    "UPDATE task_dispatches SET thread_id = ?1 WHERE id = ?2",
                    rusqlite::params![target_channel, dispatch_id],
                )
                .ok();
                // Mark as notified so timeouts.js [I-0] won't resend
                conn.execute(
                    "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                    rusqlite::params![format!("dispatch_notified:{}", dispatch_id), dispatch_id],
                )
                .ok();
            }
            tracing::info!(
                "[review] Sent review-decision to existing thread {target_channel} for {agent_id}"
            );
        }
    } else {
        let url = format!(
            "https://discord.com/api/v10/channels/{}/messages",
            target_channel
        );
        match client
            .post(&url)
            .header("Authorization", format!("Bot {}", token))
            .json(&serde_json::json!({"content": message}))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                // Mark as notified so timeouts.js [I-0] won't resend
                if let Ok(conn) = db.lock() {
                    conn.execute(
                        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                        rusqlite::params![
                            format!("dispatch_notified:{}", dispatch_id),
                            dispatch_id
                        ],
                    )
                    .ok();
                }
                tracing::info!("[review] Sent review result to {agent_id} (channel {channel_id})");
            }
            Ok(resp) => {
                let status = resp.status();
                tracing::warn!("[review] Discord API error {status}");
            }
            Err(e) => {
                tracing::warn!("[review] Request failed: {e}");
            }
        }
    }
}

fn extract_review_verdict(result_json: Option<&str>) -> String {
    result_json
        .and_then(|r| serde_json::from_str::<serde_json::Value>(r).ok())
        .and_then(|v| {
            v.get("verdict")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    v.get("decision")
                        .and_then(|s| s.as_str())
                        .map(|s| s.to_string())
                })
        })
        // NEVER default to "pass" — missing verdict means the review agent
        // did not submit a verdict (e.g. session idle auto-complete).
        // Returning "unknown" forces the followup path to request human/agent review.
        .unwrap_or_else(|| "unknown".to_string())
}

pub(super) async fn handle_completed_dispatch_followups(db: &crate::db::Db, dispatch_id: &str) {
    let info: Option<(String, String, String, String, String, Option<String>)> = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        conn.query_row(
            "SELECT td.dispatch_type, td.status, kc.id, COALESCE(kc.assigned_agent_id, ''), kc.title, td.result \
             FROM task_dispatches td \
             JOIN kanban_cards kc ON kc.id = td.kanban_card_id \
             WHERE td.id = ?1",
            [dispatch_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .ok()
    };

    let Some((dispatch_type, status, card_id, _agent_id, _title, result_json)) = info else {
        return;
    };
    if status != "completed" {
        return;
    }

    if dispatch_type == "review" {
        let verdict = extract_review_verdict(result_json.as_deref());
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!(
            "  [{ts}] 🔍 REVIEW-FOLLOWUP: dispatch={dispatch_id} verdict={verdict} result={:?}",
            result_json.as_deref().unwrap_or("NULL")
        );
        // Skip Discord notification for auto-completed reviews without an explicit verdict.
        // The policy engine's onDispatchCompleted hook handles those (review-automation.js).
        // Only send_review_result_to_primary for explicit verdicts (pass/improve/reject)
        // submitted via the verdict API — these have a real "verdict" field in the result.
        if verdict != "unknown" {
            send_review_result_to_primary(db, &card_id, &verdict).await;
        } else {
            println!(
                "  [{ts}] ⏭ REVIEW-FOLLOWUP: skipping send_review_result_to_primary (verdict=unknown)"
            );
        }
    }

    // Archive thread on dispatch completion — but only if the card is done.
    // When the card has an active lifecycle (not done), keep the thread open for reuse
    // by subsequent dispatches (rework, review-decision, etc.).
    let card_status: Option<String> = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        conn.query_row(
            "SELECT status FROM kanban_cards WHERE id = ?1",
            [&card_id],
            |row| row.get(0),
        )
        .ok()
    };
    let should_archive = card_status.as_deref() == Some("done");

    if should_archive {
        let thread_id: Option<String> = {
            let conn = match db.lock() {
                Ok(c) => c,
                Err(_) => return,
            };
            conn.query_row(
                "SELECT COALESCE(thread_id, json_extract(context, '$.thread_id')) FROM task_dispatches WHERE id = ?1",
                [dispatch_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
        };
        if let Some(ref tid) = thread_id {
            if let Some(token) = crate::credential::read_bot_token("announce") {
                let archive_url = format!("https://discord.com/api/v10/channels/{}", tid);
                let client = reqwest::Client::new();
                let _ = client
                    .patch(&archive_url)
                    .header("Authorization", format!("Bot {}", token))
                    .json(&serde_json::json!({"archived": true}))
                    .send()
                    .await;
                tracing::info!(
                    "[dispatch] Archived thread {tid} for completed dispatch {dispatch_id} (card done)"
                );
            }
        }
        // Clear all thread mappings when card is done
        if let Ok(conn) = db.lock() {
            clear_all_threads(&conn, &card_id);
        }
    }

    // Generic resend removed — dispatch Discord notification is handled by:
    // 1. kanban.rs fire_transition_hooks → onCardTransition → send_dispatch_to_discord
    // 2. timeouts.js [I-0] recovery for unnotified dispatches
    // 3. send_dispatch_to_discord has a dispatch_notified guard to prevent duplicates
    // Previously this generic resend caused 2-3x duplicate messages for every dispatch.
}

/// Resolve a channel name alias (e.g. "adk-cc") to a numeric channel ID
/// by reading role_map.json's byChannelName section.
pub fn resolve_channel_alias_pub(alias: &str) -> Option<u64> {
    resolve_channel_alias(alias)
}

fn use_counter_model_channel(dispatch_type: Option<&str>) -> bool {
    // Only "review" goes to the counter-model channel.
    // "review-decision" is sent to the original agent's primary channel
    // so it reuses the implementation thread.
    matches!(dispatch_type, Some("review"))
}

fn format_dispatch_message(
    dispatch_id: &str,
    title: &str,
    issue_url: Option<&str>,
    issue_number: Option<i64>,
    use_alt: bool,
    reviewed_commit: Option<&str>,
    target_provider: Option<&str>,
) -> String {
    // Format issue link as markdown hyperlink with angle brackets to suppress embed
    let issue_link = match (issue_url, issue_number) {
        (Some(url), Some(num)) => format!("[{title} #{num}](<{url}>)"),
        (Some(url), None) => format!("[{title}](<{url}>)"),
        _ => String::new(),
    };

    if use_alt {
        let mut message = format!(
            "DISPATCH:{dispatch_id} - {title}\n\
             ⚠️ 검토 전용 — 작업 착수 금지\n\
             코드 리뷰만 수행하고 GitHub 이슈에 코멘트로 피드백해주세요."
        );
        if !issue_link.is_empty() {
            message.push('\n');
            message.push_str(&issue_link);
        }
        // Append verdict API call instructions for the counter-model reviewer
        let commit_arg = reviewed_commit
            .map(|c| format!(r#","commit":"{}""#, c))
            .unwrap_or_default();
        let provider_arg = target_provider
            .map(|p| format!(r#","provider":"{}""#, p))
            .unwrap_or_default();
        let base_url = crate::config::local_api_url(crate::config::load_graceful().server.port, "");
        message.push_str(&format!(
            "\n---\n\
             응답 첫 줄에 반드시 `VERDICT: pass|improve|reject|rework` 중 하나를 적으세요.\n\
             verdict API가 200 OK로 호출되기 전까지 리뷰는 완료로 간주되지 않습니다.\n\
             리뷰 완료 후 verdict API를 호출하세요:\n\
             `curl -sf -X POST {base_url}/api/review-verdict \
             -H \"Content-Type: application/json\" \
             -d '{{\"dispatch_id\":\"{dispatch_id}\",\"overall\":\"pass|improve|reject|rework\"{commit_arg}{provider_arg}}}'`"
        ));
        message
    } else if !issue_link.is_empty() {
        format!("DISPATCH:{dispatch_id} - {title}\n{issue_link}")
    } else {
        format!("DISPATCH:{dispatch_id} - {title}")
    }
}

fn resolve_channel_alias(alias: &str) -> Option<u64> {
    let root = crate::cli::agentdesk_runtime_root()?;
    let path = root.join("config/role_map.json");
    let content = std::fs::read_to_string(&path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;

    // Strategy 1: Direct lookup in byChannelName → channelId field
    let by_name = json.get("byChannelName")?.as_object()?;
    if let Some(entry) = by_name.get(alias) {
        // If byChannelName entry has a channelId field, use it directly (most reliable)
        if let Some(id) = entry.get("channelId").and_then(|v| v.as_str()) {
            return id.parse().ok();
        }
        if let Some(id) = entry.get("channelId").and_then(|v| v.as_u64()) {
            return Some(id);
        }
    }

    // Strategy 2: Search byChannelId for entries whose channel name matches the alias
    // Each byChannelId entry may have been registered with a channel name
    let by_id = json.get("byChannelId")?.as_object()?;
    for (ch_id, ch_entry) in by_id {
        // Check if this entry's associated channel name matches our alias
        if let Some(ch_name) = ch_entry.get("channelName").and_then(|v| v.as_str()) {
            if ch_name == alias {
                return ch_id.parse().ok();
            }
        }
    }

    // Strategy 3: Fallback — roleId matching (original approach)
    if let Some(entry) = by_name.get(alias) {
        let role_id = entry.get("roleId").and_then(|v| v.as_str())?;
        let provider = entry.get("provider").and_then(|v| v.as_str());
        for (ch_id, ch_entry) in by_id {
            let entry_role = ch_entry.get("roleId").and_then(|v| v.as_str());
            let entry_provider = ch_entry.get("provider").and_then(|v| v.as_str());
            if entry_role == Some(role_id) {
                // If both have provider, must match. If either is missing, accept the match.
                if let (Some(p1), Some(p2)) = (provider, entry_provider) {
                    if p1 == p2 {
                        return ch_id.parse().ok();
                    }
                } else {
                    return ch_id.parse().ok();
                }
            }
        }
    }

    None
}

/// POST /api/internal/link-dispatch-thread
/// Links a dispatch's kanban card to a Discord thread (sets active_thread_id).
/// Called by dcserver router.rs when it creates a thread as fallback.
pub async fn link_dispatch_thread(
    State(state): State<AppState>,
    Json(body): Json<LinkDispatchThreadBody>,
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

    // Look up card_id from the dispatch, then set channel-thread mapping
    let card_id: Option<String> = conn
        .query_row(
            "SELECT kanban_card_id FROM task_dispatches WHERE id = ?1",
            [&body.dispatch_id],
            |row| row.get(0),
        )
        .ok();

    match card_id {
        Some(cid) => {
            conn.execute(
                "UPDATE task_dispatches SET thread_id = ?1 WHERE id = ?2",
                rusqlite::params![body.thread_id, body.dispatch_id],
            )
            .ok();
            if let Some(ref ch_id) = body.channel_id {
                if let Ok(ch_num) = ch_id.parse::<u64>() {
                    set_thread_for_channel(&conn, &cid, ch_num, &body.thread_id);
                } else {
                    // Fallback: legacy active_thread_id
                    conn.execute(
                        "UPDATE kanban_cards SET active_thread_id = ?1 WHERE id = ?2",
                        rusqlite::params![body.thread_id, cid],
                    )
                    .ok();
                }
            } else {
                // No channel_id provided — legacy path
                conn.execute(
                    "UPDATE kanban_cards SET active_thread_id = ?1 WHERE id = ?2",
                    rusqlite::params![body.thread_id, cid],
                )
                .ok();
            }
            (StatusCode::OK, Json(json!({"ok": true, "card_id": cid})))
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "dispatch not found"})),
        ),
    }
}

/// GET /api/internal/card-thread?dispatch_id=xxx
/// Returns the active_thread_id for a dispatch's card (if any).
pub async fn get_card_thread(
    State(state): State<AppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let dispatch_id = match params.get("dispatch_id") {
        Some(id) => id,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "dispatch_id required"})),
            );
        }
    };

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let result: Option<(
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    )> = conn
        .query_row(
            "SELECT kc.id, kc.active_thread_id, td.dispatch_type, \
                    (SELECT a.discord_channel_alt FROM agents a WHERE a.id = td.to_agent_id), \
                    (SELECT a.discord_channel_id FROM agents a WHERE a.id = td.to_agent_id) \
             FROM task_dispatches td \
             JOIN kanban_cards kc ON kc.id = td.kanban_card_id \
             WHERE td.id = ?1",
            [dispatch_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .ok();

    match result {
        Some((card_id, _legacy_thread_id, dispatch_type, alt_channel, primary_channel)) => {
            // Determine target channel for this dispatch type
            let use_alt = matches!(dispatch_type.as_deref(), Some("review"));
            let target_channel = if use_alt {
                alt_channel.as_deref()
            } else {
                primary_channel.as_deref()
            };
            // Look up channel-specific thread
            let thread_id = target_channel
                .and_then(|ch| parse_channel_id(ch))
                .and_then(|ch_num| get_thread_for_channel(&conn, &card_id, ch_num));

            (
                StatusCode::OK,
                Json(json!({
                    "card_id": card_id,
                    "active_thread_id": thread_id,
                    "dispatch_type": dispatch_type,
                    "discord_channel_alt": alt_channel,
                })),
            )
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "dispatch not found"})),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        extract_review_verdict, format_dispatch_message, handle_completed_dispatch_followups,
        use_counter_model_channel,
    };
    use crate::db::Db;
    use std::sync::{Arc, Mutex};

    fn test_db() -> Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    #[test]
    fn review_dispatch_uses_counter_model_channel() {
        assert!(use_counter_model_channel(Some("review")));
        // review-decision goes to the original agent's primary channel,
        // not the counter-model channel, to reuse the implementation thread
        assert!(!use_counter_model_channel(Some("review-decision")));
        assert!(!use_counter_model_channel(Some("implementation")));
        assert!(!use_counter_model_channel(Some("rework")));
        assert!(!use_counter_model_channel(None));
    }

    #[test]
    fn review_dispatch_message_includes_review_only_banner() {
        let message = format_dispatch_message(
            "dispatch-1",
            "[Review R1] card-1",
            Some("https://github.com/itismyfield/AgentDesk/issues/19"),
            Some(19),
            true,
            Some("abc123"),
            Some("codex"),
        );

        assert!(message.starts_with("DISPATCH:dispatch-1 - [Review R1] card-1"));
        assert!(message.contains("⚠️ 검토 전용"));
        assert!(message.contains("코드 리뷰만 수행하고 GitHub 이슈에 코멘트로 피드백해주세요."));
        assert!(message.contains(
            "[Review R1] card-1 #19](<https://github.com/itismyfield/AgentDesk/issues/19>)"
        ));
        // Verdict API instructions must be present for counter-model reviewers
        assert!(message.contains("review-verdict"));
        assert!(message.contains("VERDICT: pass|improve|reject|rework"));
        assert!(message.contains("dispatch-1"));
        assert!(message.contains("abc123"));
        // Provider must be included in the curl example
        assert!(message.contains(r#""provider":"codex""#));
    }

    #[test]
    fn review_dispatch_message_without_commit() {
        let message = format_dispatch_message(
            "dispatch-no-commit",
            "[Review R1] card-1",
            None,
            None,
            true,
            None,
            None,
        );

        assert!(message.contains("review-verdict"));
        assert!(message.contains("dispatch-no-commit"));
        // No commit arg in the curl command
        assert!(!message.contains(r#""commit""#));
    }

    #[test]
    fn implementation_dispatch_message_stays_compact() {
        let message = format_dispatch_message(
            "dispatch-2",
            "Implement feature",
            Some("https://github.com/itismyfield/AgentDesk/issues/24"),
            Some(24),
            false,
            None,
            None,
        );

        assert!(message.contains(
            "[Implement feature #24](<https://github.com/itismyfield/AgentDesk/issues/24>)"
        ));
        assert!(!message.contains("검토 전용"));
        // Implementation dispatches should NOT include verdict instructions
        assert!(!message.contains("review-verdict"));
    }

    #[test]
    fn review_verdict_extraction_defaults_to_unknown() {
        // Missing verdict must NOT default to "pass" — that caused false review passes
        assert_eq!(extract_review_verdict(None), "unknown");
        assert_eq!(
            extract_review_verdict(Some(r#"{"auto_completed":true}"#)),
            "unknown"
        );
        assert_eq!(
            extract_review_verdict(Some(r#"{"decision":"dismiss"}"#)),
            "dismiss"
        );
        assert_eq!(
            extract_review_verdict(Some(r#"{"verdict":"improve"}"#)),
            "improve"
        );
        assert_eq!(
            extract_review_verdict(Some(r#"{"verdict":"pass"}"#)),
            "pass"
        );
    }

    #[tokio::test]
    #[ignore] // CI: send_review_result_to_primary early-returns without local ADK runtime (channel resolution)
    async fn completed_review_dispatch_with_explicit_verdict_creates_followup() {
        // When a review dispatch has an explicit verdict (e.g. "improve"),
        // Rust creates a review-decision dispatch for the original agent.
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at)
                 VALUES ('card-1', 'Needs follow-up', 'review', 'agent-1', 'dispatch-review', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, result, created_at, updated_at)
                 VALUES ('dispatch-review', 'card-1', 'agent-1', 'review', 'completed', '[Review R1] card-1', '{\"verdict\":\"improve\"}', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
        }

        handle_completed_dispatch_followups(&db, "dispatch-review").await;

        let conn = db.lock().unwrap();
        let latest_dispatch_id: String = conn
            .query_row(
                "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_ne!(latest_dispatch_id, "dispatch-review");
        let (dispatch_type, dispatch_status): (String, String) = conn
            .query_row(
                "SELECT dispatch_type, status FROM task_dispatches WHERE id = ?1",
                [&latest_dispatch_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(dispatch_type, "review-decision");
        assert_eq!(dispatch_status, "pending");
    }

    #[tokio::test]
    async fn auto_completed_review_dispatch_skips_rust_followup() {
        // When a review dispatch is auto-completed without a verdict,
        // Rust should NOT create a followup (policy engine handles it).
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at)
                 VALUES ('card-1', 'Auto test', 'review', 'agent-1', 'dispatch-auto', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, result, created_at, updated_at)
                 VALUES ('dispatch-auto', 'card-1', 'agent-1', 'review', 'completed', '[Review R1] card-1', '{\"auto_completed\":true,\"completion_source\":\"session_idle\"}', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
        }

        handle_completed_dispatch_followups(&db, "dispatch-auto").await;

        let conn = db.lock().unwrap();
        let latest_dispatch_id: String = conn
            .query_row(
                "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        // latest_dispatch_id should remain unchanged — auto-complete with "unknown" verdict skips Rust followup
        assert_eq!(latest_dispatch_id, "dispatch-auto");
    }

    /// After an implementation dispatch completes, if hooks created a review dispatch
    /// (latest_dispatch_id changed), handle_completed_dispatch_followups should detect it
    /// and attempt to send it to Discord. This test verifies the detection logic without
    /// actually hitting Discord (send_dispatch_to_discord will no-op without a bot token).
    #[tokio::test]
    async fn impl_dispatch_followup_detects_new_review_dispatch() {
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at)
                 VALUES ('card-1', 'Impl card', 'review', 'agent-1', 'dispatch-review', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            // The completed implementation dispatch
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, result, created_at, updated_at)
                 VALUES ('dispatch-impl', 'card-1', 'agent-1', 'implementation', 'completed', 'Impl card', '{\"auto_completed\":true}', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            // The review dispatch created by hooks after implementation completion
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
                 VALUES ('dispatch-review', 'card-1', 'agent-1', 'review', 'pending', '[Review R1] card-1', '{}', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
        }

        // handle_completed_dispatch_followups should detect that latest_dispatch_id
        // ('dispatch-review') differs from the completed dispatch ('dispatch-impl')
        // and attempt send_dispatch_to_discord (which no-ops without bot token).
        // The key assertion: no panic, no error, and the review dispatch stays pending.
        handle_completed_dispatch_followups(&db, "dispatch-impl").await;

        let conn = db.lock().unwrap();
        // latest_dispatch_id should still point to the review dispatch
        let latest: String = conn
            .query_row(
                "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(latest, "dispatch-review");

        // Review dispatch should remain pending (not modified by followup handler)
        let review_status: String = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = 'dispatch-review'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(review_status, "pending");
    }

    #[tokio::test]
    async fn thread_not_archived_when_card_not_done() {
        // When an implementation dispatch completes but card is in "review" (not done),
        // the thread should NOT be archived — it may be reused for rework/review-decision.
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, active_thread_id)
                 VALUES ('card-1', 'In Review', 'review', 'agent-1', 'dispatch-impl', '999888777')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, thread_id, created_at, updated_at)
                 VALUES ('dispatch-impl', 'card-1', 'agent-1', 'implementation', 'completed', 'card-1', '999888777', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
        }

        handle_completed_dispatch_followups(&db, "dispatch-impl").await;

        // active_thread_id should still be set (NOT cleared) because card is not done
        let conn = db.lock().unwrap();
        let active_thread: Option<String> = conn
            .query_row(
                "SELECT active_thread_id FROM kanban_cards WHERE id = 'card-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(active_thread, Some("999888777".to_string()));
    }

    #[tokio::test]
    async fn thread_archived_and_cleared_when_card_done() {
        // When a card reaches "done", active_thread_id should be cleared.
        // (Thread archiving requires Discord API call, but we verify the DB cleanup.)
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, active_thread_id)
                 VALUES ('card-1', 'Done Card', 'done', 'agent-1', 'dispatch-final', '999888777')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, thread_id, created_at, updated_at)
                 VALUES ('dispatch-final', 'card-1', 'agent-1', 'implementation', 'completed', 'card-1', '999888777', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
        }

        handle_completed_dispatch_followups(&db, "dispatch-final").await;

        // active_thread_id should be cleared when card is done
        let conn = db.lock().unwrap();
        let active_thread: Option<String> = conn
            .query_row(
                "SELECT active_thread_id FROM kanban_cards WHERE id = 'card-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(active_thread.is_none());
    }

    /// When an explicit review verdict (improve/rework/reject) completes,
    /// send_review_result_to_primary creates the review-decision dispatch
    /// and sets review_followup_handled=true, preventing duplicate resend
    /// via the generic latest_dispatch_id check.
    #[tokio::test]
    #[ignore] // CI: send_review_result_to_primary early-returns without local ADK runtime
    async fn review_followup_skips_generic_resend_for_explicit_verdict() {
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at)
                 VALUES ('card-1', 'Review test', 'review', 'agent-1', 'dispatch-review', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, result, created_at, updated_at)
                 VALUES ('dispatch-review', 'card-1', 'agent-1', 'review', 'completed', '[Review R1] card-1', '{\"verdict\":\"rework\"}', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
        }

        handle_completed_dispatch_followups(&db, "dispatch-review").await;

        let conn = db.lock().unwrap();
        // A review-decision dispatch should have been created
        let latest_dispatch_id: String = conn
            .query_row(
                "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_ne!(latest_dispatch_id, "dispatch-review");

        // Count total dispatches — should be exactly 2 (original review + one review-decision).
        // Before this fix, the generic latest_dispatch_id check would call send_dispatch_to_discord
        // again, potentially creating duplicate notifications.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 2,
            "should have exactly 2 dispatches (review + review-decision), not more"
        );

        let (dt, ds): (String, String) = conn
            .query_row(
                "SELECT dispatch_type, status FROM task_dispatches WHERE id = ?1",
                [&latest_dispatch_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(dt, "review-decision");
        assert_eq!(ds, "pending");
    }

    /// When the agent's discord_channel_id points to a non-existent channel,
    /// send_dispatch_to_discord must NOT write the notified marker.
    /// This ensures that Discord send failures leave the dispatch recoverable
    /// by timeouts.js [I-0].
    #[tokio::test]
    #[ignore] // CI: send_dispatch_to_discord early-returns without local ADK runtime
    async fn no_notified_marker_when_discord_send_fails() {
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            // Use a bogus numeric channel ID that will fail at Discord API
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '1')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at)
                 VALUES ('card-1', 'Test card', 'requested', 'agent-1', 'dispatch-1', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
                 VALUES ('dispatch-1', 'card-1', 'agent-1', 'implementation', 'pending', 'Test card', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
        }

        // Channel ID "1" is a valid u64 but not a real Discord channel.
        // Thread creation and fallback will both fail with Discord API errors.
        // No notified marker should be written.
        super::send_dispatch_to_discord(&db, "agent-1", "Test card", "card-1", "dispatch-1").await;

        let conn = db.lock().unwrap();
        let marker_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM kv_meta WHERE key = 'dispatch_notified:dispatch-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            marker_count, 0,
            "notified marker must not be written when Discord send fails"
        );

        // thread_id should also NOT be saved (rollback on failure)
        let thread_id: Option<String> = conn
            .query_row(
                "SELECT thread_id FROM task_dispatches WHERE id = 'dispatch-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            thread_id.is_none(),
            "thread_id must not be saved when thread message POST fails"
        );
    }

    /// send_review_result_to_primary must not create a review-decision dispatch
    /// for done cards — the central create_dispatch_core done guard blocks it.
    #[tokio::test]
    async fn review_followup_does_not_create_dispatch_for_done_card() {
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at)
                 VALUES ('card-done', 'Done Card', 'done', 'agent-1', 'dispatch-review', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, result, created_at, updated_at)
                 VALUES ('dispatch-review', 'card-done', 'agent-1', 'review', 'completed', '[Review R1]', '{\"verdict\":\"rework\"}', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
        }

        // This triggers send_review_result_to_primary for a done card
        handle_completed_dispatch_followups(&db, "dispatch-review").await;

        let conn = db.lock().unwrap();
        // latest_dispatch_id should NOT have changed (no new dispatch created)
        let latest: String = conn
            .query_row(
                "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-done'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            latest, "dispatch-review",
            "done card latest_dispatch_id must not be overwritten"
        );

        // Only the original dispatch should exist — no review-decision was created
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-done'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "no review-decision dispatch should be created for done card"
        );
    }
}
