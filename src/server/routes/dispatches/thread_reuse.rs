use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{PgPool, Row as SqlxRow};

use crate::db::agents::{resolve_agent_counter_model_channel_pg, resolve_agent_primary_channel_pg};
use crate::server::routes::AppState;

use super::parse_channel_id;

// ── Body types ─────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize)]
pub struct LinkDispatchThreadBody {
    pub dispatch_id: String,
    pub thread_id: String,
    pub channel_id: Option<String>,
}

// ── Channel-thread map helpers ────────────────────────────────

fn parse_channel_thread_map(
    raw: Option<&str>,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    raw.and_then(|value| {
        serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(value).ok()
    })
}

fn lookup_thread_for_channel_from_map(map_json: Option<&str>, channel_id: u64) -> Option<String> {
    parse_channel_thread_map(map_json).and_then(|map| {
        map.get(&channel_id.to_string())
            .and_then(|value| value.as_str())
            .map(|value| value.to_string())
    })
}

fn json_value_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn lookup_thread_for_channel_from_map_pg(
    map_json: Option<&str>,
    card_id: &str,
    channel_id: u64,
) -> Option<String> {
    let Some(raw) = map_json
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "null")
    else {
        return None;
    };

    let value = match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(
                card_id,
                channel_id,
                %error,
                "[dispatch] invalid postgres channel_thread_map JSON; skipping thread reuse"
            );
            return None;
        }
    };

    let Some(map) = value.as_object() else {
        tracing::warn!(
            card_id,
            channel_id,
            json_type = json_value_kind(&value),
            "[dispatch] postgres channel_thread_map is not an object; skipping thread reuse"
        );
        return None;
    };

    match map.get(&channel_id.to_string()) {
        Some(serde_json::Value::String(thread_id)) => Some(thread_id.to_string()),
        Some(other) => {
            tracing::warn!(
                card_id,
                channel_id,
                json_type = json_value_kind(other),
                "[dispatch] postgres channel_thread_map entry is not a string; skipping thread reuse"
            );
            None
        }
        None => None,
    }
}

/// Look up the thread_id for a specific channel from channel_thread_map.
/// Falls back to active_thread_id for backward compatibility.
pub(super) fn get_thread_for_channel(
    conn: &libsql_rusqlite::Connection,
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

    if let Some(thread_id) = lookup_thread_for_channel_from_map(map_json.as_deref(), channel_id) {
        return Some(thread_id);
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

pub(super) async fn get_thread_for_channel_pg(
    pool: &PgPool,
    card_id: &str,
    channel_id: u64,
) -> Result<Option<String>, String> {
    let row = sqlx::query(
        "SELECT channel_thread_map::text AS channel_thread_map, active_thread_id
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres thread map for {card_id}: {error}"))?;

    let Some(row) = row else {
        return Ok(None);
    };

    let map_json: Option<String> = row
        .try_get("channel_thread_map")
        .map_err(|error| format!("read postgres channel_thread_map for {card_id}: {error}"))?;
    let active_thread_id: Option<String> = row
        .try_get("active_thread_id")
        .map_err(|error| format!("read postgres active_thread_id for {card_id}: {error}"))?;

    if let Some(thread_id) =
        lookup_thread_for_channel_from_map_pg(map_json.as_deref(), card_id, channel_id)
    {
        return Ok(Some(thread_id));
    }

    if map_json
        .as_deref()
        .map_or(true, |value| value.is_empty() || value == "{}")
    {
        return Ok(active_thread_id.filter(|value| !value.trim().is_empty()));
    }

    Ok(None)
}

/// Set the thread_id for a specific channel in channel_thread_map.
/// Also updates active_thread_id for backward compatibility.
pub(super) fn set_thread_for_channel(
    conn: &libsql_rusqlite::Connection,
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
        libsql_rusqlite::params![json_str, thread_id, card_id],
    )
    .ok();
}

pub(super) async fn set_thread_for_channel_pg(
    pool: &PgPool,
    card_id: &str,
    channel_id: u64,
    thread_id: &str,
) -> Result<(), String> {
    let existing: Option<String> = sqlx::query_scalar::<_, Option<String>>(
        "SELECT channel_thread_map::text
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres thread map for {card_id}: {error}"))?
    .flatten();

    let mut map: serde_json::Map<String, serde_json::Value> = existing
        .and_then(|value| serde_json::from_str(&value).ok())
        .unwrap_or_default();
    map.insert(
        channel_id.to_string(),
        serde_json::Value::String(thread_id.to_string()),
    );
    let json_str = serde_json::to_string(&map)
        .map_err(|error| format!("serialize postgres thread map for {card_id}: {error}"))?;

    sqlx::query(
        "UPDATE kanban_cards
         SET channel_thread_map = $1::jsonb,
             active_thread_id = $2,
             updated_at = NOW()
         WHERE id = $3",
    )
    .bind(json_str)
    .bind(thread_id)
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| format!("save postgres thread map for {card_id}: {error}"))?;
    Ok(())
}

/// Clear thread mapping for a specific channel.
pub(super) fn clear_thread_for_channel(
    conn: &libsql_rusqlite::Connection,
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
                libsql_rusqlite::params![new_json, card_id],
            )
            .ok();
        }
    }
}

pub(super) async fn clear_thread_for_channel_pg(
    pool: &PgPool,
    card_id: &str,
    channel_id: u64,
) -> Result<(), String> {
    let existing: Option<String> = sqlx::query_scalar::<_, Option<String>>(
        "SELECT channel_thread_map::text
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres thread map for {card_id}: {error}"))?
    .flatten();

    let Some(existing) = existing else {
        return Ok(());
    };

    let Ok(mut map) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&existing)
    else {
        return Ok(());
    };

    map.remove(&channel_id.to_string());
    let new_json =
        serde_json::to_string(&map).map_err(|error| format!("serialize thread map: {error}"))?;
    sqlx::query(
        "UPDATE kanban_cards
         SET channel_thread_map = $1::jsonb,
             updated_at = NOW()
         WHERE id = $2",
    )
    .bind(new_json)
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| format!("clear postgres thread map for {card_id}: {error}"))?;
    Ok(())
}

/// Clear ALL thread mappings (card done).
pub(in crate::server::routes) fn clear_all_threads(
    conn: &libsql_rusqlite::Connection,
    card_id: &str,
) {
    conn.execute(
        "UPDATE kanban_cards SET channel_thread_map = NULL, active_thread_id = NULL WHERE id = ?1",
        [card_id],
    )
    .ok();
}

#[derive(Debug)]
struct ThreadMapValidationRow {
    card_id: String,
    map_json: String,
    active_thread_id: Option<String>,
}

#[derive(Debug)]
struct ThreadMapValidationOutcome {
    checked: usize,
    cleared: usize,
    persist: bool,
    new_map: Option<String>,
    new_active_thread_id: Option<String>,
}

pub(crate) async fn validate_channel_thread_maps_on_startup(
    db: &crate::db::Db,
    token: &str,
) -> (usize, usize) {
    validate_channel_thread_maps_on_startup_with_backends(Some(db), None, token).await
}

pub(crate) async fn validate_channel_thread_maps_on_startup_with_backends(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&PgPool>,
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

    if let Some(pool) = pg_pool {
        return validate_channel_thread_maps_on_startup_with_base_url_pg(
            pool,
            &client,
            token,
            "https://discord.com/api/v10",
        )
        .await;
    }

    let Some(db) = db else {
        return (0, 0);
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
    let rows: Vec<ThreadMapValidationRow> = match db.lock() {
        Ok(conn) => conn
            .prepare(
                "SELECT id, channel_thread_map, active_thread_id
                 FROM kanban_cards
                 WHERE channel_thread_map IS NOT NULL
                   AND TRIM(channel_thread_map) != ''
                   AND TRIM(channel_thread_map) != '{}'",
            )
            .and_then(|mut stmt| {
                stmt.query_map([], |row| {
                    Ok(ThreadMapValidationRow {
                        card_id: row.get(0)?,
                        map_json: row.get(1)?,
                        active_thread_id: row.get(2)?,
                    })
                })
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

    for row in rows {
        let outcome = validate_thread_map_validation_row(client, token, base_url, &row).await;
        checked += outcome.checked;
        cleared += outcome.cleared;

        if !outcome.persist {
            continue;
        }

        if let Ok(conn) = db.lock() {
            conn.execute(
                "UPDATE kanban_cards
                 SET channel_thread_map = ?1,
                     active_thread_id = ?2
                 WHERE id = ?3",
                libsql_rusqlite::params![
                    outcome.new_map,
                    outcome.new_active_thread_id,
                    row.card_id
                ],
            )
            .ok();
        }
    }

    (checked, cleared)
}

async fn validate_channel_thread_maps_on_startup_with_base_url_pg(
    pool: &PgPool,
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
) -> (usize, usize) {
    let rows: Vec<ThreadMapValidationRow> = match sqlx::query(
        "SELECT id, channel_thread_map::text AS channel_thread_map, active_thread_id
         FROM kanban_cards
         WHERE channel_thread_map IS NOT NULL
           AND BTRIM(channel_thread_map::text) != ''
           AND BTRIM(channel_thread_map::text) != '{}'",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows
            .into_iter()
            .filter_map(|row| {
                Some(ThreadMapValidationRow {
                    card_id: row.try_get::<String, _>("id").ok()?,
                    map_json: row.try_get::<String, _>("channel_thread_map").ok()?,
                    active_thread_id: row.try_get::<Option<String>, _>("active_thread_id").ok()?,
                })
            })
            .collect(),
        Err(error) => {
            tracing::warn!(
                "[dispatch] startup thread-map validation skipped (postgres query): {error}"
            );
            return (0, 0);
        }
    };

    let mut checked = 0usize;
    let mut cleared = 0usize;

    for row in rows {
        let outcome = validate_thread_map_validation_row(client, token, base_url, &row).await;
        checked += outcome.checked;
        cleared += outcome.cleared;

        if !outcome.persist {
            continue;
        }

        if let Err(error) = sqlx::query(
            "UPDATE kanban_cards
             SET channel_thread_map = $1::jsonb,
                 active_thread_id = $2,
                 updated_at = NOW()
             WHERE id = $3",
        )
        .bind(outcome.new_map)
        .bind(outcome.new_active_thread_id)
        .bind(&row.card_id)
        .execute(pool)
        .await
        {
            tracing::warn!(
                "[dispatch] failed to persist postgres startup thread-map cleanup for {}: {}",
                row.card_id,
                error
            );
        }
    }

    (checked, cleared)
}

async fn validate_thread_map_validation_row(
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
    row: &ThreadMapValidationRow,
) -> ThreadMapValidationOutcome {
    let Ok(mut map) =
        serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&row.map_json)
    else {
        return ThreadMapValidationOutcome {
            checked: 0,
            cleared: 1,
            persist: true,
            new_map: None,
            new_active_thread_id: row.active_thread_id.clone(),
        };
    };

    let mut checked = 0usize;
    let mut cleared = 0usize;
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
            if row.active_thread_id.as_deref() == Some(thread_id.as_str()) {
                removed_active = true;
            }
            continue;
        };

        let thread_info_url = format!("{}/channels/{}", base_url.trim_end_matches('/'), thread_id);
        let response = match client
            .get(&thread_info_url)
            .header("Authorization", format!("Bot {}", token))
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                tracing::info!(
                    "[dispatch] startup thread-map validation clearing {} -> {} after request error: {}",
                    row.card_id,
                    thread_id,
                    error
                );
                map.remove(&channel_id_raw);
                changed = true;
                cleared += 1;
                if row.active_thread_id.as_deref() == Some(thread_id.as_str()) {
                    removed_active = true;
                }
                continue;
            }
        };

        if !response.status().is_success() {
            tracing::info!(
                "[dispatch] startup thread-map validation clearing {} -> {} after {}",
                row.card_id,
                thread_id,
                response.status()
            );
            map.remove(&channel_id_raw);
            changed = true;
            cleared += 1;
            if row.active_thread_id.as_deref() == Some(thread_id.as_str()) {
                removed_active = true;
            }
            continue;
        }

        let body: serde_json::Value = match response.json().await {
            Ok(body) => body,
            Err(error) => {
                tracing::info!(
                    "[dispatch] startup thread-map validation clearing {} -> {} after json error: {}",
                    row.card_id,
                    thread_id,
                    error
                );
                map.remove(&channel_id_raw);
                changed = true;
                cleared += 1;
                if row.active_thread_id.as_deref() == Some(thread_id.as_str()) {
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
                row.card_id,
                thread_id,
                parent_id,
                expected_parent
            );
            map.remove(&channel_id_raw);
            changed = true;
            cleared += 1;
            if row.active_thread_id.as_deref() == Some(thread_id.as_str()) {
                removed_active = true;
            }
        }
    }

    if !changed {
        return ThreadMapValidationOutcome {
            checked,
            cleared,
            persist: false,
            new_map: None,
            new_active_thread_id: row.active_thread_id.clone(),
        };
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
        row.active_thread_id.clone()
    };

    ThreadMapValidationOutcome {
        checked,
        cleared,
        persist: true,
        new_map,
        new_active_thread_id,
    }
}

/// Try to reuse an existing Discord thread for a dispatch.
/// Returns `Some(true)` if reuse succeeded, `Some(false)` if the thread exists but is locked,
/// or `None` if the thread couldn't be accessed (deleted, wrong parent, etc.).
pub(super) async fn try_reuse_thread(
    client: &reqwest::Client,
    token: &str,
    discord_api_base: &str,
    thread_id: &str,
    expected_parent: u64,
    desired_thread_name: &str,
    message: &str,
    minimal_message: &str,
    dispatch_id: &str,
    card_id: &str,
    db: Option<&crate::db::Db>,
    pg_pool: Option<&PgPool>,
) -> Result<
    Option<(
        bool,
        Option<super::discord_delivery::DispatchMessagePostOutcome>,
    )>,
    super::discord_delivery::DispatchMessagePostError,
> {
    // 1. Fetch thread info to verify it exists and belongs to the right parent channel
    let thread_info_url = format!(
        "{}/channels/{}",
        discord_api_base.trim_end_matches('/'),
        thread_id
    );
    let resp = client
        .get(&thread_info_url)
        .header("Authorization", format!("Bot {}", token))
        .send()
        .await
        .map_err(|error| {
            super::discord_delivery::DispatchMessagePostError::new(
                super::discord_delivery::DispatchMessagePostErrorKind::Other,
                format!("failed to inspect reusable thread {thread_id}: {error}"),
            )
        })?;

    if !resp.status().is_success() {
        tracing::info!("[dispatch] Thread {thread_id} no longer accessible, will create new");
        // Clear stale thread for this channel
        if let Some(pool) = pg_pool {
            clear_thread_for_channel_pg(pool, card_id, expected_parent)
                .await
                .ok();
        } else if let Some(db) = db {
            if let Ok(conn) = db.lock() {
                clear_thread_for_channel(&conn, card_id, expected_parent);
            }
        }
        return Ok(None);
    }

    let body: serde_json::Value = resp.json().await.map_err(|error| {
        super::discord_delivery::DispatchMessagePostError::new(
            super::discord_delivery::DispatchMessagePostErrorKind::Other,
            format!("failed to parse reusable thread {thread_id}: {error}"),
        )
    })?;

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
        if let Some(pool) = pg_pool {
            clear_thread_for_channel_pg(pool, card_id, expected_parent)
                .await
                .ok();
            sqlx::query(
                "UPDATE kanban_cards
                 SET active_thread_id = NULL,
                     updated_at = NOW()
                 WHERE id = $1 AND active_thread_id = $2",
            )
            .bind(card_id)
            .bind(thread_id)
            .execute(pool)
            .await
            .ok();
        } else if let Some(db) = db {
            if let Ok(conn) = db.lock() {
                clear_thread_for_channel(&conn, card_id, expected_parent);
                // Also clear active_thread_id if it points to the mismatched thread,
                // preventing get_thread_for_channel() fallback from re-selecting it
                conn.execute(
                    "UPDATE kanban_cards SET active_thread_id = NULL \
                     WHERE id = ?1 AND active_thread_id = ?2",
                    libsql_rusqlite::params![card_id, thread_id],
                )
                .ok();
            }
        }
        return Ok(None);
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
        if let Some(pool) = pg_pool {
            clear_thread_for_channel_pg(pool, card_id, expected_parent)
                .await
                .ok();
        } else if let Some(db) = db {
            if let Ok(conn) = db.lock() {
                clear_thread_for_channel(&conn, card_id, expected_parent);
            }
        }
        return Ok(Some((false, None)));
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
                return Ok(None);
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

    match super::discord_delivery::post_dispatch_message_to_channel_with_delivery(
        client,
        token,
        discord_api_base,
        thread_id,
        message,
        minimal_message,
        Some(dispatch_id),
    )
    .await
    {
        Ok(outcome) => {
            // Update dispatch thread_id and mark as notified
            if let Some(pool) = pg_pool {
                sqlx::query(
                    "UPDATE task_dispatches
                     SET thread_id = $1,
                         updated_at = NOW()
                     WHERE id = $2",
                )
                .bind(thread_id)
                .bind(dispatch_id)
                .execute(pool)
                .await
                .ok();
                sqlx::query(
                    "INSERT INTO kv_meta (key, value)
                     VALUES ($1, $2)
                     ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
                )
                .bind(format!("dispatch_notified:{dispatch_id}"))
                .bind(dispatch_id)
                .execute(pool)
                .await
                .ok();
            } else if let Some(db) = db {
                if let Ok(conn) = db.lock() {
                    conn.execute(
                        "UPDATE task_dispatches SET thread_id = ?1 WHERE id = ?2",
                        libsql_rusqlite::params![thread_id, dispatch_id],
                    )
                    .ok();
                    conn.execute(
                        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                        libsql_rusqlite::params![
                            format!("dispatch_notified:{}", dispatch_id),
                            dispatch_id
                        ],
                    )
                    .ok();
                }
            }
            if let Err(error) =
                super::discord_delivery::persist_dispatch_message_target_and_add_pending_reaction_with_pg(
                    db,
                    client,
                    token,
                    discord_api_base,
                    dispatch_id,
                    thread_id,
                    &outcome.message_id,
                    pg_pool,
                )
                .await
            {
                tracing::warn!(
                    "[dispatch] Failed to persist reused thread message target for {}: {}",
                    dispatch_id,
                    error
                );
            }
            tracing::info!("[dispatch] Reused thread {thread_id} for dispatch {dispatch_id}");
            Ok(Some((true, Some(outcome))))
        }
        Err(error) => {
            tracing::warn!(
                "[dispatch] Failed to send message to reused thread {thread_id}: {}",
                error
            );
            if error.is_length_error() {
                return Err(error);
            }
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router, extract::Path, http::StatusCode, response::IntoResponse, routing::get,
    };
    use serde_json::json;
    use std::{
        future::Future,
        io::{self, Write},
        sync::{Arc, Mutex},
    };

    struct TestLogWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    fn test_db() -> crate::db::Db {
        let conn = libsql_rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    impl Write for TestLogWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buffer.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    async fn capture_logs_async<T, F>(run: impl FnOnce() -> F) -> (T, String)
    where
        F: Future<Output = T>,
    {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let log_buffer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .without_time()
            .with_writer(move || TestLogWriter {
                buffer: log_buffer.clone(),
            })
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let result = run().await;
        let captured = buffer.lock().unwrap().clone();
        (result, String::from_utf8_lossy(&captured).to_string())
    }

    struct TestPostgresDb {
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestPostgresDb {
        async fn create() -> Option<Self> {
            let admin_url = postgres_admin_database_url();
            let database_name = format!("agentdesk_thread_reuse_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            let admin_pool = match sqlx::PgPool::connect(&admin_url).await {
                Ok(pool) => pool,
                Err(error) => {
                    eprintln!("skipping postgres thread_reuse test: admin connect failed: {error}");
                    return None;
                }
            };
            if let Err(error) = sqlx::query(&format!("CREATE DATABASE \"{database_name}\""))
                .execute(&admin_pool)
                .await
            {
                eprintln!("skipping postgres thread_reuse test: create database failed: {error}");
                admin_pool.close().await;
                return None;
            }
            admin_pool.close().await;
            Some(Self {
                admin_url,
                database_name,
                database_url,
            })
        }

        async fn migrate(&self) -> Option<sqlx::PgPool> {
            let pool = match sqlx::PgPool::connect(&self.database_url).await {
                Ok(pool) => pool,
                Err(error) => {
                    eprintln!("skipping postgres thread_reuse test: db connect failed: {error}");
                    return None;
                }
            };
            if let Err(error) = crate::db::postgres::migrate(&pool).await {
                eprintln!("skipping postgres thread_reuse test: migrate failed: {error}");
                pool.close().await;
                return None;
            }
            Some(pool)
        }

        async fn drop(self) {
            let Ok(admin_pool) = sqlx::PgPool::connect(&self.admin_url).await else {
                return;
            };
            let _ = sqlx::query(
                "SELECT pg_terminate_backend(pid)
                 FROM pg_stat_activity
                 WHERE datname = $1
                   AND pid <> pg_backend_pid()",
            )
            .bind(&self.database_name)
            .execute(&admin_pool)
            .await;
            let _ = sqlx::query(&format!(
                "DROP DATABASE IF EXISTS \"{}\"",
                self.database_name
            ))
            .execute(&admin_pool)
            .await;
            admin_pool.close().await;
        }
    }

    fn postgres_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }

        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "127.0.0.1".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgres://{user}:{password}@{host}:{port}"),
            None => format!("postgres://{user}@{host}:{port}"),
        }
    }

    fn postgres_admin_database_url() -> String {
        if let Ok(url) = std::env::var("POSTGRES_TEST_ADMIN_URL") {
            let trimmed = url.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        format!("{}/postgres", postgres_base_database_url())
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

    #[tokio::test(flavor = "current_thread")]
    async fn get_thread_for_channel_pg_warns_on_non_object_thread_map() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, channel_thread_map)
             VALUES ($1, 'Test Card', 'review', $2::jsonb)",
        )
        .bind("card-bad-thread-map")
        .bind("\"bad-thread-map\"")
        .execute(&pool)
        .await
        .expect("seed malformed thread map");

        let (result, logs) = capture_logs_async(|| async {
            get_thread_for_channel_pg(&pool, "card-bad-thread-map", 111).await
        })
        .await;

        assert_eq!(result.expect("thread lookup should not fail"), None);
        assert!(
            logs.contains("postgres channel_thread_map is not an object"),
            "expected warn log for malformed channel_thread_map, got: {logs}"
        );

        pool.close().await;
        pg_db.drop().await;
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
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "postgres pool unavailable"})),
        );
    };

    let card_id = match sqlx::query_scalar::<_, String>(
        "SELECT kanban_card_id FROM task_dispatches WHERE id = $1",
    )
    .bind(&body.dispatch_id)
    .fetch_optional(pool)
    .await
    {
        Ok(card_id) => card_id,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            );
        }
    };

    match card_id {
        Some(cid) => {
            if let Err(error) = sqlx::query(
                "UPDATE task_dispatches
                 SET thread_id = $1,
                     updated_at = NOW()
                 WHERE id = $2",
            )
            .bind(&body.thread_id)
            .bind(&body.dispatch_id)
            .execute(pool)
            .await
            {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
            }

            let save_result = if let Some(ref ch_id) = body.channel_id {
                if let Ok(ch_num) = ch_id.parse::<u64>() {
                    set_thread_for_channel_pg(pool, &cid, ch_num, &body.thread_id).await
                } else {
                    sqlx::query(
                        "UPDATE kanban_cards
                         SET active_thread_id = $1,
                             updated_at = NOW()
                         WHERE id = $2",
                    )
                    .bind(&body.thread_id)
                    .bind(&cid)
                    .execute(pool)
                    .await
                    .map(|_| ())
                    .map_err(|error| format!("{error}"))
                }
            } else {
                sqlx::query(
                    "UPDATE kanban_cards
                     SET active_thread_id = $1,
                         updated_at = NOW()
                     WHERE id = $2",
                )
                .bind(&body.thread_id)
                .bind(&cid)
                .execute(pool)
                .await
                .map(|_| ())
                .map_err(|error| format!("{error}"))
            };

            match save_result {
                Ok(()) => (StatusCode::OK, Json(json!({"ok": true, "card_id": cid}))),
                Err(error) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                ),
            }
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

    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "postgres pool unavailable"})),
        );
    };

    let result = match sqlx::query(
        "SELECT kc.id AS card_id,
                kc.title AS card_title,
                kc.github_issue_url AS github_issue_url,
                kc.github_issue_number::BIGINT AS github_issue_number,
                kc.description AS issue_body,
                kc.deferred_dod_json::text AS deferred_dod_json,
                kc.active_thread_id AS legacy_thread_id,
                td.dispatch_type AS dispatch_type,
                td.title AS dispatch_title,
                td.to_agent_id AS dispatch_agent_id,
                td.context AS dispatch_context
         FROM task_dispatches td
         JOIN kanban_cards kc ON kc.id = td.kanban_card_id
         WHERE td.id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(pool)
    .await
    {
        Ok(row) => row,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            );
        }
    };

    match result {
        Some(row) => {
            let card_id: String = row.try_get("card_id").unwrap_or_default();
            let card_title: String = row.try_get("card_title").unwrap_or_default();
            let github_issue_url: Option<String> = row.try_get("github_issue_url").ok().flatten();
            let github_issue_number: Option<i64> =
                row.try_get("github_issue_number").ok().flatten();
            let issue_body: Option<String> = row.try_get("issue_body").ok().flatten();
            let deferred_dod_json: Option<String> = row.try_get("deferred_dod_json").ok().flatten();
            let dispatch_type: Option<String> = row.try_get("dispatch_type").ok().flatten();
            let dispatch_title: Option<String> = row.try_get("dispatch_title").ok().flatten();
            let to_agent_id: String = row.try_get("dispatch_agent_id").unwrap_or_default();
            let dispatch_context: Option<String> = row.try_get("dispatch_context").ok().flatten();

            let primary_channel = resolve_agent_primary_channel_pg(pool, &to_agent_id)
                .await
                .ok()
                .flatten();
            let counter_model_channel = resolve_agent_counter_model_channel_pg(pool, &to_agent_id)
                .await
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
            let thread_id = if let Some(ch_num) = target_channel.and_then(parse_channel_id) {
                match get_thread_for_channel_pg(pool, &card_id, ch_num).await {
                    Ok(thread_id) => thread_id,
                    Err(error) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": error})),
                        );
                    }
                }
            } else {
                None
            };
            let deferred_dod = deferred_dod_json
                .as_deref()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());

            (
                StatusCode::OK,
                Json(json!({
                    "card_id": card_id,
                    "card_title": card_title,
                    "github_issue_url": github_issue_url,
                    "github_issue_number": github_issue_number,
                    "issue_body": issue_body,
                    "deferred_dod": deferred_dod,
                    "active_thread_id": thread_id,
                    "dispatch_type": dispatch_type,
                    "dispatch_title": dispatch_title,
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

    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "postgres pool unavailable"})),
        );
    };

    let dispatch_id = match sqlx::query_scalar::<_, String>(
        "SELECT td.id
         FROM task_dispatches td
         JOIN kanban_cards kc ON kc.id = td.kanban_card_id
         WHERE td.status IN ('pending', 'dispatched')
           AND (
             td.thread_id = $1
             OR kc.active_thread_id = $1
             OR EXISTS(
               SELECT 1
               FROM jsonb_each_text(COALESCE(kc.channel_thread_map, '{}'::jsonb)) AS entry(key, value)
               WHERE entry.value = $1
             )
           )
         ORDER BY td.created_at DESC
         LIMIT 1",
    )
    .bind(thread_id)
    .fetch_optional(pool)
    .await
    {
        Ok(dispatch_id) => dispatch_id,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            );
        }
    };

    match dispatch_id {
        Some(id) => (StatusCode::OK, Json(json!({"dispatch_id": id}))),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no pending dispatch for thread"})),
        ),
    }
}
