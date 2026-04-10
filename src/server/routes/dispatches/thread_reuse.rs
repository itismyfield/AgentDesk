use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;

use crate::db::agents::{
    resolve_agent_counter_model_channel_on_conn, resolve_agent_primary_channel_on_conn,
};
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

pub(crate) async fn validate_channel_thread_maps_on_startup(
    db: &crate::db::Db,
    token: &str,
) -> (usize, usize) {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            tracing::warn!("[dispatch] failed to build startup thread-map validator client: {e}");
            return (0, 0);
        }
    };

    validate_channel_thread_maps_on_startup_with_base_url(
        db,
        &client,
        token,
        "https://discord.com/api/v10",
    )
    .await
}

async fn validate_channel_thread_maps_on_startup_with_base_url(
    db: &crate::db::Db,
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
) -> (usize, usize) {
    let rows: Vec<(String, String, Option<String>)> = match db.lock() {
        Ok(conn) => conn
            .prepare(
                "SELECT id, channel_thread_map, active_thread_id
                 FROM kanban_cards
                 WHERE channel_thread_map IS NOT NULL
                   AND TRIM(channel_thread_map) != ''
                   AND TRIM(channel_thread_map) != '{}'",
            )
            .and_then(|mut stmt| {
                stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                    .map(|rows| rows.filter_map(|row| row.ok()).collect())
            })
            .unwrap_or_default(),
        Err(e) => {
            tracing::warn!("[dispatch] startup thread-map validation skipped (db lock): {e}");
            return (0, 0);
        }
    };

    let mut checked = 0usize;
    let mut cleared = 0usize;

    for (card_id, map_json, active_thread_id) in rows {
        let Ok(mut map) =
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&map_json)
        else {
            if let Ok(conn) = db.lock() {
                conn.execute(
                    "UPDATE kanban_cards SET channel_thread_map = NULL WHERE id = ?1",
                    [&card_id],
                )
                .ok();
            }
            cleared += 1;
            continue;
        };

        let mut changed = false;
        let mut removed_active = false;
        let snapshot: Vec<(String, Option<String>)> = map
            .iter()
            .map(|(channel_id, thread_id)| {
                (
                    channel_id.clone(),
                    thread_id.as_str().map(std::string::ToString::to_string),
                )
            })
            .collect();

        for (channel_id_raw, thread_id) in snapshot {
            checked += 1;
            let Some(thread_id) = thread_id else {
                map.remove(&channel_id_raw);
                changed = true;
                cleared += 1;
                continue;
            };
            let Ok(expected_parent) = channel_id_raw.parse::<u64>() else {
                map.remove(&channel_id_raw);
                changed = true;
                cleared += 1;
                if active_thread_id.as_deref() == Some(thread_id.as_str()) {
                    removed_active = true;
                }
                continue;
            };

            let thread_info_url =
                format!("{}/channels/{}", base_url.trim_end_matches('/'), thread_id);
            let response = match client
                .get(&thread_info_url)
                .header("Authorization", format!("Bot {}", token))
                .send()
                .await
            {
                Ok(response) => response,
                Err(e) => {
                    tracing::info!(
                        "[dispatch] startup thread-map validation clearing {} -> {} after request error: {}",
                        card_id,
                        thread_id,
                        e
                    );
                    map.remove(&channel_id_raw);
                    changed = true;
                    cleared += 1;
                    if active_thread_id.as_deref() == Some(thread_id.as_str()) {
                        removed_active = true;
                    }
                    continue;
                }
            };

            if !response.status().is_success() {
                tracing::info!(
                    "[dispatch] startup thread-map validation clearing {} -> {} after {}",
                    card_id,
                    thread_id,
                    response.status()
                );
                map.remove(&channel_id_raw);
                changed = true;
                cleared += 1;
                if active_thread_id.as_deref() == Some(thread_id.as_str()) {
                    removed_active = true;
                }
                continue;
            }

            let body: serde_json::Value = match response.json().await {
                Ok(body) => body,
                Err(e) => {
                    tracing::info!(
                        "[dispatch] startup thread-map validation clearing {} -> {} after json error: {}",
                        card_id,
                        thread_id,
                        e
                    );
                    map.remove(&channel_id_raw);
                    changed = true;
                    cleared += 1;
                    if active_thread_id.as_deref() == Some(thread_id.as_str()) {
                        removed_active = true;
                    }
                    continue;
                }
            };

            let parent_id = body
                .get("parent_id")
                .and_then(|value| value.as_str())
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or_default();

            if parent_id != expected_parent {
                tracing::info!(
                    "[dispatch] startup thread-map validation clearing {} -> {} (parent {} != {})",
                    card_id,
                    thread_id,
                    parent_id,
                    expected_parent
                );
                map.remove(&channel_id_raw);
                changed = true;
                cleared += 1;
                if active_thread_id.as_deref() == Some(thread_id.as_str()) {
                    removed_active = true;
                }
            }
        }

        if !changed {
            continue;
        }

        let new_map = if map.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&map).unwrap_or_else(|_| "{}".to_string()))
        };
        let new_active_thread_id = if removed_active {
            map.values()
                .find_map(|value| value.as_str())
                .map(std::string::ToString::to_string)
        } else {
            active_thread_id.clone()
        };

        if let Ok(conn) = db.lock() {
            conn.execute(
                "UPDATE kanban_cards
                 SET channel_thread_map = ?1,
                     active_thread_id = ?2
                 WHERE id = ?3",
                rusqlite::params![new_map, new_active_thread_id, card_id],
            )
            .ok();
        }
    }

    (checked, cleared)
}

/// Try to reuse an existing Discord thread for a dispatch.
/// Returns `Some(true)` if reuse succeeded, `Some(false)` if the thread exists but is locked,
/// or `None` if the thread couldn't be accessed (deleted, wrong parent, etc.).
pub(super) async fn try_reuse_thread(
    client: &reqwest::Client,
    token: &str,
    thread_id: &str,
    expected_parent: u64,
    desired_thread_name: &str,
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

    // Keep slot thread names in sync with the currently assigned issue set.
    let current_thread_name = body.get("name").and_then(|v| v.as_str()).unwrap_or("");
    if !desired_thread_name.is_empty() && current_thread_name != desired_thread_name {
        let _ = client
            .patch(&thread_info_url)
            .header("Authorization", format!("Bot {}", token))
            .json(&serde_json::json!({"name": desired_thread_name}))
            .send()
            .await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router, extract::Path, http::StatusCode, response::IntoResponse, routing::get,
    };
    use serde_json::json;

    fn test_db() -> crate::db::Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    async fn spawn_thread_info_server() -> (String, tokio::task::JoinHandle<()>) {
        async fn channel(Path(thread_id): Path<String>) -> impl IntoResponse {
            match thread_id.as_str() {
                "thread-valid" => (
                    StatusCode::OK,
                    Json(json!({"id":"thread-valid","parent_id":"111"})),
                )
                    .into_response(),
                "thread-wrong-parent" => (
                    StatusCode::OK,
                    Json(json!({"id":"thread-wrong-parent","parent_id":"999"})),
                )
                    .into_response(),
                _ => StatusCode::NOT_FOUND.into_response(),
            }
        }

        let app = Router::new().route("/channels/{thread_id}", get(channel));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn startup_validation_clears_missing_and_mismatched_thread_bindings() {
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, priority, channel_thread_map, active_thread_id,
                    created_at, updated_at
                ) VALUES (
                    'card-thread-map-startup', 'Issue #335', 'review', 'medium',
                    '{\"111\":\"thread-valid\",\"222\":\"thread-missing\",\"333\":\"thread-wrong-parent\"}',
                    'thread-missing',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
        }

        let client = reqwest::Client::builder().build().unwrap();
        let (base_url, server_handle) = spawn_thread_info_server().await;
        let (checked, cleared) = validate_channel_thread_maps_on_startup_with_base_url(
            &db,
            &client,
            "test-token",
            &base_url,
        )
        .await;
        server_handle.abort();

        assert_eq!(checked, 3);
        assert_eq!(cleared, 2);

        let conn = db.lock().unwrap();
        let (map_json, active_thread_id): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT channel_thread_map, active_thread_id
                 FROM kanban_cards
                 WHERE id = 'card-thread-map-startup'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        let map: serde_json::Value =
            serde_json::from_str(map_json.as_deref().unwrap_or("{}")).unwrap();
        assert_eq!(map["111"], "thread-valid");
        assert!(map.get("222").is_none());
        assert!(map.get("333").is_none());
        assert_eq!(active_thread_id.as_deref(), Some("thread-valid"));
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
        String,
        Option<String>,
    )> = conn
        .query_row(
            "SELECT kc.id, kc.active_thread_id, td.dispatch_type, \
                    td.to_agent_id, \
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
                ))
            },
        )
        .ok();

    match result {
        Some((card_id, _legacy_thread_id, dispatch_type, to_agent_id, dispatch_context)) => {
            let primary_channel = resolve_agent_primary_channel_on_conn(&conn, &to_agent_id)
                .ok()
                .flatten();
            let counter_model_channel =
                resolve_agent_counter_model_channel_on_conn(&conn, &to_agent_id)
                    .ok()
                    .flatten();
            // Determine target channel for this dispatch type
            let use_alt = super::use_counter_model_channel(dispatch_type.as_deref());
            let target_channel = if use_alt {
                counter_model_channel.as_deref()
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
                    "discord_channel_id": primary_channel,
                    "discord_channel_alt": counter_model_channel,
                    "discord_channel_target": target_channel,
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
    // thread via thread_id (direct match), active_thread_id, or
    // channel_thread_map JSON values. This must cover review/review-decision
    // as well as work dispatches because reused threads often omit a fresh
    // DISPATCH: prefix.
    //
    // #355: Use td.thread_id as primary match, and json_each for
    // channel_thread_map to avoid matching JSON keys (parent channel IDs).
    // INSTR was matching parent channel IDs, causing thread dispatch
    // reminders to leak into parent channel turns.
    let dispatch_id: Option<String> = conn
        .query_row(
            "SELECT td.id FROM task_dispatches td \
             JOIN kanban_cards kc ON kc.id = td.kanban_card_id \
             WHERE td.status IN ('pending', 'dispatched') \
             AND (td.thread_id = ?1 \
                  OR kc.active_thread_id = ?1 \
                  OR EXISTS(SELECT 1 FROM json_each(kc.channel_thread_map) \
                            WHERE json_each.value = ?1)) \
             ORDER BY td.created_at DESC LIMIT 1",
            [thread_id],
            |row| row.get(0),
        )
        .ok();

    match dispatch_id {
        Some(id) => (StatusCode::OK, Json(json!({"dispatch_id": id}))),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no pending dispatch for thread"})),
        ),
    }
}
