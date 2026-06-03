use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde_json::json;
use sqlx::{PgPool, Row as SqlxRow};

use crate::db::agents::{resolve_agent_counter_model_channel_pg, resolve_agent_primary_channel_pg};
use crate::server::routes::AppState;
use crate::services::dispatches::LinkDispatchThreadBody;

use super::parse_channel_id;

// ── Channel-thread map helpers ────────────────────────────────

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

pub(crate) async fn get_thread_for_channel_pg(
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

pub(crate) async fn get_mapped_thread_for_channel_pg(
    pool: &PgPool,
    card_id: &str,
    channel_id: u64,
) -> Result<Option<String>, String> {
    let row = sqlx::query(
        "SELECT channel_thread_map::text AS channel_thread_map
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

    Ok(lookup_thread_for_channel_from_map_pg(
        map_json.as_deref(),
        card_id,
        channel_id,
    ))
}

pub(crate) async fn set_thread_for_channel_pg(
    pool: &PgPool,
    card_id: &str,
    channel_id: u64,
    thread_id: &str,
) -> Result<(), String> {
    set_thread_for_channel_pg_with_active(pool, card_id, channel_id, thread_id, true).await
}

pub(crate) async fn set_thread_for_channel_map_only_pg(
    pool: &PgPool,
    card_id: &str,
    channel_id: u64,
    thread_id: &str,
) -> Result<(), String> {
    set_thread_for_channel_pg_with_active(pool, card_id, channel_id, thread_id, false).await
}

async fn set_thread_for_channel_pg_with_active(
    pool: &PgPool,
    card_id: &str,
    channel_id: u64,
    thread_id: &str,
    update_active_thread: bool,
) -> Result<(), String> {
    sqlx::query(
        "UPDATE kanban_cards
         SET channel_thread_map = COALESCE(channel_thread_map, '{}'::jsonb)
                                  || jsonb_build_object($1::text, $2::text),
             active_thread_id = CASE WHEN $4::boolean THEN $2 ELSE active_thread_id END,
             updated_at = NOW()
         WHERE id = $3",
    )
    .bind(channel_id.to_string())
    .bind(thread_id)
    .bind(card_id)
    .bind(update_active_thread)
    .execute(pool)
    .await
    .map_err(|error| format!("save postgres thread map for {card_id}: {error}"))?;
    Ok(())
}

pub(crate) async fn clear_thread_for_channel_pg(
    pool: &PgPool,
    card_id: &str,
    channel_id: u64,
) -> Result<(), String> {
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
        return Ok(());
    };
    let existing: Option<String> = row
        .try_get("channel_thread_map")
        .map_err(|error| format!("read postgres thread map for {card_id}: {error}"))?;
    let active_thread_id: Option<String> = row
        .try_get("active_thread_id")
        .map_err(|error| format!("read postgres active thread for {card_id}: {error}"))?;
    let Some(existing) = existing else {
        return Ok(());
    };

    let Ok(mut map) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&existing)
    else {
        return Ok(());
    };

    let removed_thread_id = map
        .remove(&channel_id.to_string())
        .and_then(|value| value.as_str().map(std::string::ToString::to_string));
    let new_json = if map.is_empty() {
        None
    } else {
        Some(
            serde_json::to_string(&map)
                .map_err(|error| format!("serialize thread map: {error}"))?,
        )
    };
    let new_active_thread_id = if removed_thread_id.as_deref() == active_thread_id.as_deref() {
        map.values()
            .find_map(|value| value.as_str())
            .map(std::string::ToString::to_string)
    } else {
        active_thread_id
    };
    sqlx::query(
        "UPDATE kanban_cards
         SET channel_thread_map = $1::jsonb,
             active_thread_id = $2,
             updated_at = NOW()
         WHERE id = $3",
    )
    .bind(new_json)
    .bind(new_active_thread_id)
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| format!("clear postgres thread map for {card_id}: {error}"))?;
    Ok(())
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

pub(crate) async fn validate_channel_thread_maps_on_startup_with_backends(
    _db: Option<&crate::db::Db>,
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

    let Some(pool) = pg_pool else {
        // PG-only after #843 / #1239. Without a pool there is nothing to validate.
        return (0, 0);
    };

    validate_channel_thread_maps_on_startup_with_base_url_pg(
        pool,
        &client,
        token,
        "https://discord.com/api/v10",
    )
    .await
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
pub(crate) async fn try_reuse_thread(
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
    // #1968: Refuse to reuse a thread that already has a *different* active
    // dispatch. Two dispatches assigned to the same Discord thread results in
    // the second never receiving turn_started — its session_key/started_at
    // stay null forever. Force the caller to create a fresh thread instead.
    if let Some(pool) = pg_pool {
        let active_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM task_dispatches
             WHERE thread_id = $1
               AND id <> $2
               AND status IN ('pending','dispatched')",
        )
        .bind(thread_id)
        .bind(dispatch_id)
        .fetch_one(pool)
        .await
        .unwrap_or(0);
        if active_count > 0 {
            tracing::info!(
                "[dispatch] Thread {thread_id} has {active_count} active dispatch(es); refusing reuse for {dispatch_id} and forcing fresh thread"
            );
            // Clear so subsequent retries don't keep probing this busy thread.
            clear_thread_for_channel_pg(pool, card_id, expected_parent)
                .await
                .ok();
            return Ok(None);
        }
    }

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
        // Clear stale thread for this channel (PG-only after #843 / #1239).
        if let Some(pool) = pg_pool {
            clear_thread_for_channel_pg(pool, card_id, expected_parent)
                .await
                .ok();
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
        // probing the wrong thread via active_thread_id fallback (PG-only).
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
        // Clear stale thread for this channel (PG-only after #843 / #1239).
        if let Some(pool) = pg_pool {
            clear_thread_for_channel_pg(pool, card_id, expected_parent)
                .await
                .ok();
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
            // Update dispatch thread_id and mark as notified (PG-only).
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

    let dispatch_row = match sqlx::query(
        "SELECT kanban_card_id, dispatch_type, to_agent_id
         FROM task_dispatches
         WHERE id = $1",
    )
    .bind(&body.dispatch_id)
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

    match dispatch_row {
        Some(row) => {
            let cid: String = row.try_get("kanban_card_id").unwrap_or_default();
            let dispatch_type: Option<String> = row.try_get("dispatch_type").ok().flatten();
            let to_agent_id: String = row.try_get("to_agent_id").unwrap_or_default();

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
                    let counter_channel =
                        resolve_agent_counter_model_channel_pg(pool, &to_agent_id)
                            .await
                            .ok()
                            .flatten()
                            .and_then(|value| parse_channel_id(&value));
                    let is_counter_model_thread =
                        super::use_counter_model_channel(dispatch_type.as_deref())
                            && counter_channel == Some(ch_num);
                    if is_counter_model_thread {
                        set_thread_for_channel_map_only_pg(pool, &cid, ch_num, &body.thread_id)
                            .await
                    } else {
                        set_thread_for_channel_pg(pool, &cid, ch_num, &body.thread_id).await
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
                let lookup_result = if use_alt {
                    get_mapped_thread_for_channel_pg(pool, &card_id, ch_num).await
                } else {
                    get_thread_for_channel_pg(pool, &card_id, ch_num).await
                };
                match lookup_result {
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
