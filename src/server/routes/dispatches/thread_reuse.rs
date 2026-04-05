use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;

use crate::server::routes::AppState;

use super::parse_channel_id;

// ── Body types ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct LinkDispatchThreadBody {
    pub dispatch_id: String,
    pub thread_id: String,
    pub channel_id: Option<String>,
}

// ── Channel-thread map helpers ────────────────────────────────

/// Look up the thread_id for a specific channel from channel_thread_map.
/// Falls back to active_thread_id for backward compatibility.
pub(super) fn get_thread_for_channel(
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

    // Fallback: legacy active_thread_id — only if channel_thread_map is empty/absent.
    // When the map exists but doesn't contain this channel, the thread belongs to a
    // different channel (e.g. CDX review thread) and must NOT be reused for the
    // primary channel's review-decision message.
    if map_json
        .as_deref()
        .map_or(true, |s| s.is_empty() || s == "{}")
    {
        return conn
            .query_row(
                "SELECT active_thread_id FROM kanban_cards WHERE id = ?1 AND active_thread_id IS NOT NULL",
                [card_id],
                |row| row.get(0),
            )
            .ok();
    }
    None
}

/// Set the thread_id for a specific channel in channel_thread_map.
/// Also updates active_thread_id for backward compatibility.
pub(super) fn set_thread_for_channel(
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
pub(super) fn clear_thread_for_channel(
    conn: &rusqlite::Connection,
    card_id: &str,
    channel_id: u64,
) {
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

/// Clear ALL thread mappings (card done).
pub(in crate::server::routes) fn clear_all_threads(conn: &rusqlite::Connection, card_id: &str) {
    conn.execute(
        "UPDATE kanban_cards SET channel_thread_map = NULL, active_thread_id = NULL WHERE id = ?1",
        [card_id],
    )
    .ok();
}

/// Try to reuse an existing Discord thread for a dispatch.
/// Returns `Some(true)` if reuse succeeded, `Some(false)` if the thread exists but is locked,
/// or `None` if the thread couldn't be accessed (deleted, wrong parent, etc.).
pub(super) async fn try_reuse_thread(
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
        let _ = dispatch_type; // suppress unused warning — dispatch_type used in caller context
        if let Some(ref name) = new_name {
            let _ = client
                .patch(&thread_info_url)
                .header("Authorization", format!("Bot {}", token))
                .json(&serde_json::json!({"name": name}))
                .send()
                .await;
        }
    }

    let msg_url = format!(
        "https://discord.com/api/v10/channels/{}/messages",
        thread_id
    );
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

// ── Route handlers ────────────────────────────────────────────

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
        Option<String>,
    )> = conn
        .query_row(
            "SELECT kc.id, kc.active_thread_id, td.dispatch_type, \
                    (SELECT a.discord_channel_alt FROM agents a WHERE a.id = td.to_agent_id), \
                    (SELECT a.discord_channel_id FROM agents a WHERE a.id = td.to_agent_id), \
                    td.context \
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
        .ok();

    match result {
        Some((
            card_id,
            _legacy_thread_id,
            dispatch_type,
            alt_channel,
            primary_channel,
            dispatch_context,
        )) => {
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
                    "dispatch_context": dispatch_context,
                })),
            )
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "dispatch not found"})),
        ),
    }
}

/// GET /api/internal/pending-dispatch-for-thread?thread_id=xxx
///
/// #222: Look up the latest pending/dispatched dispatch whose kanban card is
/// linked to the given thread channel. Used by turn_bridge as fallback when
/// parse_dispatch_id(user_text) fails in unified/reused threads, including
/// review and review-decision flows.
pub async fn get_pending_dispatch_for_thread(
    State(state): State<AppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let thread_id = match params.get("thread_id") {
        Some(id) => id,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "thread_id required"})),
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

    // Find the latest pending/dispatched dispatch whose card is linked to this
    // thread via channel_thread_map (JSON contains the thread_id) or
    // active_thread_id. This must cover review/review-decision as well as work
    // dispatches because reused threads often omit a fresh DISPATCH: prefix.
    let dispatch_id: Option<String> = conn
        .query_row(
            "SELECT td.id FROM task_dispatches td \
             JOIN kanban_cards kc ON kc.id = td.kanban_card_id \
             WHERE td.status IN ('pending', 'dispatched') \
             AND (kc.active_thread_id = ?1 \
                  OR INSTR(COALESCE(kc.channel_thread_map, ''), ?1) > 0) \
             ORDER BY td.created_at DESC LIMIT 1",
            [thread_id],
            |row| row.get(0),
        )
        .ok();

    // Fallback: check unified_thread_id / unified_thread_channel_id in
    // auto_queue_runs. These runs only own work dispatches, so keep the
    // explicit implementation/rework filter here.
    let dispatch_id = dispatch_id.or_else(|| {
        conn.query_row(
            "SELECT td.id FROM task_dispatches td \
             JOIN auto_queue_entries e ON e.kanban_card_id = td.kanban_card_id \
             JOIN auto_queue_runs r ON r.id = e.run_id \
             WHERE td.status IN ('pending', 'dispatched') \
             AND td.dispatch_type IN ('implementation', 'rework') \
             AND r.unified_thread = 1 AND r.status = 'active' \
             AND (r.unified_thread_channel_id = ?1 \
                  OR INSTR(COALESCE(r.unified_thread_id, ''), ?1) > 0) \
             ORDER BY td.created_at DESC LIMIT 1",
            [thread_id],
            |row| row.get(0),
        )
        .ok()
    });

    match dispatch_id {
        Some(id) => (StatusCode::OK, Json(json!({"dispatch_id": id}))),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no pending dispatch for thread"})),
        ),
    }
}
