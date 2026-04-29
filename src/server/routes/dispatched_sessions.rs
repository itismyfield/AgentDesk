use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{PgPool, Row};

use super::AppState;
use super::session_activity::SessionActivityResolver;
use crate::db::agents::resolve_agent_channel_for_provider_pg;
#[cfg(test)]
use crate::db::session_agent_resolution::resolve_agent_id_for_session;
use crate::db::session_agent_resolution::{
    normalize_thread_channel_id, parse_thread_channel_id_from_session_key,
    parse_thread_channel_name, resolve_agent_id_for_session_pg,
};
use crate::db::session_status::{
    is_live_status, is_user_wait_status, normalize_incoming_session_status,
};
use crate::services::message_outbox::enqueue_lifecycle_notification_pg;
use crate::services::provider::ProviderKind;
use crate::services::turn_lifecycle::{TurnLifecycleTarget, force_kill_turn};

async fn load_dispatch_thread_id_pg(pool: &PgPool, dispatch_id: &str) -> Option<String> {
    let thread_id = sqlx::query_scalar::<_, Option<String>>(
        "SELECT thread_id FROM task_dispatches WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .flatten();
    normalize_thread_channel_id(thread_id.as_deref())
}

#[cfg(test)]
fn load_dispatch_thread_id_sqlite(
    conn: &rusqlite::Connection,
    dispatch_id: &str,
) -> Option<String> {
    let thread_id: Option<String> = conn
        .query_row(
            "SELECT thread_id FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| row.get(0),
        )
        .ok()
        .flatten();
    normalize_thread_channel_id(thread_id.as_deref())
}

#[derive(Debug)]
struct RetryDispatchMeta {
    card_id: String,
    to_agent_id: Option<String>,
    dispatch_type: Option<String>,
    title: Option<String>,
    context: Option<String>,
    retry_count: i64,
}

async fn load_force_kill_session_pg(
    pool: &PgPool,
    session_key: &str,
    provider_name: Option<&str>,
) -> Result<
    Option<(
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    )>,
    String,
> {
    let row = sqlx::query(
        "SELECT active_dispatch_id, agent_id, thread_channel_id, provider
         FROM sessions
         WHERE session_key = $1",
    )
    .bind(session_key)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres session {session_key}: {error}"))?;

    let Some(row) = row else {
        return Ok(None);
    };

    let active_dispatch_id: Option<String> = row
        .try_get("active_dispatch_id")
        .map_err(|error| format!("decode active_dispatch_id for {session_key}: {error}"))?;
    let agent_id: Option<String> = row
        .try_get("agent_id")
        .map_err(|error| format!("decode agent_id for {session_key}: {error}"))?;
    let thread_channel_id: Option<String> = row
        .try_get("thread_channel_id")
        .map_err(|error| format!("decode thread_channel_id for {session_key}: {error}"))?;
    let session_provider: Option<String> = row
        .try_get("provider")
        .map_err(|error| format!("decode provider for {session_key}: {error}"))?;

    let effective_provider = provider_name.or(session_provider.as_deref());
    let runtime_channel_id =
        if let Some(channel_id) = normalize_thread_channel_id(thread_channel_id.as_deref()) {
            Some(channel_id)
        } else if let Some(agent_id) = agent_id.as_deref() {
            resolve_agent_channel_for_provider_pg(pool, agent_id, effective_provider)
            .await
            .map_err(|error| {
                format!(
                    "resolve postgres channel for session {session_key} / agent {agent_id}: {error}"
                )
            })?
            .and_then(|channel| normalize_thread_channel_id(Some(channel.as_str())))
        } else {
            None
        };

    Ok(Some((
        active_dispatch_id,
        agent_id,
        runtime_channel_id,
        session_provider,
    )))
}

async fn disconnect_session_and_prepare_retry_pg(
    pool: &PgPool,
    session_key: &str,
    active_dispatch_id: Option<&str>,
    retry: bool,
) -> Result<Option<RetryDispatchMeta>, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres force-kill transaction: {error}"))?;

    sqlx::query(
        "UPDATE sessions
         SET status = 'disconnected',
             active_dispatch_id = NULL,
             claude_session_id = NULL,
             raw_provider_session_id = NULL
         WHERE session_key = $1",
    )
    .bind(session_key)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("disconnect postgres session {session_key}: {error}"))?;

    let mut retry_meta = None;
    if let Some(dispatch_id) = active_dispatch_id {
        let current_status = sqlx::query_scalar::<_, Option<String>>(
            "SELECT status
             FROM task_dispatches
             WHERE id = $1",
        )
        .bind(dispatch_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| format!("load postgres dispatch status {dispatch_id}: {error}"))?
        .flatten();

        if current_status.as_deref() != Some("completed") {
            sqlx::query(
                "UPDATE task_dispatches
                 SET status = 'failed',
                     updated_at = NOW(),
                     completed_at = COALESCE(completed_at, NOW())
                 WHERE id = $1",
            )
            .bind(dispatch_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("mark postgres dispatch {dispatch_id} failed: {error}"))?;
        }

        if retry {
            retry_meta = sqlx::query(
                "SELECT
                    kanban_card_id,
                    to_agent_id,
                    dispatch_type,
                    title,
                    context,
                    COALESCE(retry_count, 0)::BIGINT AS retry_count
                 FROM task_dispatches
                 WHERE id = $1",
            )
            .bind(dispatch_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| format!("load postgres retry metadata {dispatch_id}: {error}"))?
            .map(|row| {
                Ok(RetryDispatchMeta {
                    card_id: row.try_get("kanban_card_id")?,
                    to_agent_id: row.try_get("to_agent_id")?,
                    dispatch_type: row.try_get("dispatch_type")?,
                    title: row.try_get("title")?,
                    context: row.try_get("context")?,
                    retry_count: row.try_get("retry_count")?,
                })
            })
            .transpose()
            .map_err(|error: sqlx::Error| {
                format!("decode postgres retry metadata {dispatch_id}: {error}")
            })?;
        }
    }

    tx.commit()
        .await
        .map_err(|error| format!("commit postgres force-kill transaction: {error}"))?;

    Ok(retry_meta)
}

async fn create_retry_dispatch_pg(
    pool: &PgPool,
    meta: &RetryDispatchMeta,
) -> Result<String, String> {
    let dispatch_id = uuid::Uuid::new_v4().to_string();
    let dispatch_type = meta
        .dispatch_type
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("implementation");
    let title = meta
        .title
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("retry dispatch");

    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres retry dispatch transaction: {error}"))?;

    sqlx::query(
        "INSERT INTO task_dispatches (
            id,
            kanban_card_id,
            to_agent_id,
            dispatch_type,
            status,
            title,
            context,
            retry_count,
            created_at,
            updated_at
        ) VALUES (
            $1, $2, $3, $4, 'pending', $5, $6, $7, NOW(), NOW()
        )",
    )
    .bind(&dispatch_id)
    .bind(&meta.card_id)
    .bind(meta.to_agent_id.as_deref())
    .bind(dispatch_type)
    .bind(title)
    .bind(meta.context.as_deref().unwrap_or("{}"))
    .bind(meta.retry_count + 1)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("insert postgres retry dispatch {dispatch_id}: {error}"))?;

    sqlx::query(
        "INSERT INTO dispatch_events (
            dispatch_id,
            kanban_card_id,
            dispatch_type,
            from_status,
            to_status,
            transition_source,
            payload_json
        ) VALUES (
            $1, $2, $3, NULL, 'pending', 'force_kill_session_retry', NULL
        )",
    )
    .bind(&dispatch_id)
    .bind(&meta.card_id)
    .bind(dispatch_type)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("insert postgres retry dispatch event {dispatch_id}: {error}"))?;

    sqlx::query(
        "INSERT INTO dispatch_outbox (dispatch_id, action, agent_id, card_id, title)
         VALUES ($1, 'notify', $2, $3, $4)
         ON CONFLICT DO NOTHING",
    )
    .bind(&dispatch_id)
    .bind(meta.to_agent_id.as_deref())
    .bind(&meta.card_id)
    .bind(title)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("insert postgres retry dispatch outbox {dispatch_id}: {error}"))?;

    sqlx::query(
        "UPDATE kanban_cards
         SET latest_dispatch_id = $1,
             updated_at = NOW()
         WHERE id = $2",
    )
    .bind(&dispatch_id)
    .bind(&meta.card_id)
    .execute(&mut *tx)
    .await
    .map_err(|error| {
        format!(
            "update postgres card latest_dispatch_id for {}: {error}",
            meta.card_id
        )
    })?;

    tx.commit()
        .await
        .map_err(|error| format!("commit postgres retry dispatch {dispatch_id}: {error}"))?;

    Ok(dispatch_id)
}

async fn list_dispatched_sessions_pg(
    pool: &PgPool,
    include_all: bool,
) -> Result<Vec<serde_json::Value>, String> {
    let sql = if include_all {
        "SELECT
            s.id,
            s.session_key,
            s.agent_id,
            s.provider,
            s.status,
            s.active_dispatch_id,
            s.model,
            s.tokens,
            s.cwd,
            s.last_heartbeat,
            s.session_info,
            a.department,
            a.sprite_number,
            a.avatar_emoji,
            COALESCE(a.xp, 0)::BIGINT AS stats_xp,
            d.name AS department_name,
            d.name_ko AS department_name_ko,
            d.color AS department_color,
            s.thread_channel_id
         FROM sessions s
         LEFT JOIN agents a ON s.agent_id = a.id
         LEFT JOIN departments d ON a.department = d.id
         ORDER BY s.id"
    } else {
        "SELECT
            s.id,
            s.session_key,
            s.agent_id,
            s.provider,
            s.status,
            s.active_dispatch_id,
            s.model,
            s.tokens,
            s.cwd,
            s.last_heartbeat,
            s.session_info,
            a.department,
            a.sprite_number,
            a.avatar_emoji,
            COALESCE(a.xp, 0)::BIGINT AS stats_xp,
            d.name AS department_name,
            d.name_ko AS department_name_ko,
            d.color AS department_color,
            s.thread_channel_id
         FROM sessions s
         LEFT JOIN agents a ON s.agent_id = a.id
         LEFT JOIN departments d ON a.department = d.id
         WHERE s.active_dispatch_id IS NOT NULL
         ORDER BY s.id"
    };

    let rows = sqlx::query(sql)
        .fetch_all(pool)
        .await
        .map_err(|error| format!("list postgres sessions: {error}"))?;

    let mut resolver = SessionActivityResolver::new();
    let mut sessions = Vec::with_capacity(rows.len());

    for row in rows {
        let id: i64 = row
            .try_get("id")
            .map_err(|error| format!("decode postgres session id: {error}"))?;
        let session_key: Option<String> = row
            .try_get("session_key")
            .map_err(|error| format!("decode postgres session_key for session {id}: {error}"))?;
        let agent_id: Option<String> = row
            .try_get("agent_id")
            .map_err(|error| format!("decode postgres agent_id for session {id}: {error}"))?;
        let provider: Option<String> = row
            .try_get("provider")
            .map_err(|error| format!("decode postgres provider for session {id}: {error}"))?;
        let status: Option<String> = row
            .try_get("status")
            .map_err(|error| format!("decode postgres status for session {id}: {error}"))?;
        let active_dispatch_id: Option<String> =
            row.try_get("active_dispatch_id").map_err(|error| {
                format!("decode postgres active_dispatch_id for session {id}: {error}")
            })?;
        let model: Option<String> = row
            .try_get("model")
            .map_err(|error| format!("decode postgres model for session {id}: {error}"))?;
        let tokens: i64 = row
            .try_get("tokens")
            .map_err(|error| format!("decode postgres tokens for session {id}: {error}"))?;
        let cwd: Option<String> = row
            .try_get("cwd")
            .map_err(|error| format!("decode postgres cwd for session {id}: {error}"))?;
        let last_heartbeat: Option<chrono::DateTime<chrono::Utc>> =
            row.try_get("last_heartbeat").map_err(|error| {
                format!("decode postgres last_heartbeat for session {id}: {error}")
            })?;
        let last_heartbeat = last_heartbeat.map(|value| value.to_rfc3339());
        let session_info: Option<String> = row
            .try_get("session_info")
            .map_err(|error| format!("decode postgres session_info for session {id}: {error}"))?;
        let department_id: Option<String> = row
            .try_get("department")
            .map_err(|error| format!("decode postgres department for session {id}: {error}"))?;
        let sprite_number: Option<i64> = row
            .try_get("sprite_number")
            .map_err(|error| format!("decode postgres sprite_number for session {id}: {error}"))?;
        let avatar_emoji: Option<String> = row
            .try_get("avatar_emoji")
            .map_err(|error| format!("decode postgres avatar_emoji for session {id}: {error}"))?;
        let stats_xp: i64 = row
            .try_get("stats_xp")
            .map_err(|error| format!("decode postgres stats_xp for session {id}: {error}"))?;
        let department_name: Option<String> = row.try_get("department_name").map_err(|error| {
            format!("decode postgres department_name for session {id}: {error}")
        })?;
        let department_name_ko: Option<String> =
            row.try_get("department_name_ko").map_err(|error| {
                format!("decode postgres department_name_ko for session {id}: {error}")
            })?;
        let department_color: Option<String> =
            row.try_get("department_color").map_err(|error| {
                format!("decode postgres department_color for session {id}: {error}")
            })?;
        let thread_channel_id: Option<String> =
            row.try_get("thread_channel_id").map_err(|error| {
                format!("decode postgres thread_channel_id for session {id}: {error}")
            })?;

        let effective = resolver.resolve(
            session_key.as_deref(),
            status.as_deref(),
            active_dispatch_id.as_deref(),
            last_heartbeat.as_deref(),
        );
        if !include_all && !effective.is_working && effective.active_dispatch_id.is_none() {
            continue;
        }
        if !include_all && thread_channel_id.is_some() && !effective.is_working {
            continue;
        }

        sessions.push(json!({
            "id": id.to_string(),
            "session_key": session_key,
            "agent_id": agent_id,
            "provider": provider,
            "status": effective.status,
            "active_dispatch_id": effective.active_dispatch_id,
            "model": model,
            "tokens": tokens,
            "cwd": cwd,
            "last_heartbeat": last_heartbeat,
            "session_info": session_info,
            "linked_agent_id": agent_id,
            "last_seen_at": last_heartbeat,
            "name": session_key,
            "department_id": department_id,
            "sprite_number": sprite_number,
            "avatar_emoji": avatar_emoji.unwrap_or_else(|| "\u{1F916}".to_string()),
            "stats_xp": stats_xp,
            "connected_at": null,
            "department_name": department_name,
            "department_name_ko": department_name_ko,
            "department_color": department_color,
            "thread_channel_id": thread_channel_id,
        }));
    }

    Ok(sessions)
}

async fn load_session_event_payload_pg(
    pool: &PgPool,
    session_key: &str,
) -> Result<Option<serde_json::Value>, String> {
    let row = sqlx::query(
        "SELECT
            s.id,
            s.session_key,
            s.agent_id,
            s.provider,
            s.status,
            s.active_dispatch_id,
            s.model,
            s.tokens,
            s.cwd,
            s.last_heartbeat,
            s.session_info,
            a.department,
            a.sprite_number,
            a.avatar_emoji,
            COALESCE(a.xp, 0)::BIGINT AS stats_xp,
            s.thread_channel_id,
            d.name AS department_name,
            d.name_ko AS department_name_ko,
            d.color AS department_color
         FROM sessions s
         LEFT JOIN agents a ON s.agent_id = a.id
         LEFT JOIN departments d ON a.department = d.id
         WHERE s.session_key = $1",
    )
    .bind(session_key)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres session event payload for {session_key}: {error}"))?;

    let Some(row) = row else {
        return Ok(None);
    };

    let id: i64 = row
        .try_get("id")
        .map_err(|error| format!("decode postgres session event id for {session_key}: {error}"))?;
    let session_key_value: Option<String> = row.try_get("session_key").map_err(|error| {
        format!("decode postgres session_key for session event {session_key}: {error}")
    })?;
    let last_seen_at: Option<chrono::DateTime<chrono::Utc>> =
        row.try_get("last_heartbeat").map_err(|error| {
            format!("decode postgres last_heartbeat for session event {session_key}: {error}")
        })?;

    Ok(Some(json!({
        "id": id.to_string(),
        "session_key": session_key_value,
        "name": session_key_value,
        "linked_agent_id": row.try_get::<Option<String>, _>("agent_id").map_err(|error| format!("decode postgres agent_id for session event {session_key}: {error}"))?,
        "provider": row.try_get::<Option<String>, _>("provider").map_err(|error| format!("decode postgres provider for session event {session_key}: {error}"))?,
        "status": row.try_get::<Option<String>, _>("status").map_err(|error| format!("decode postgres status for session event {session_key}: {error}"))?,
        "active_dispatch_id": row.try_get::<Option<String>, _>("active_dispatch_id").map_err(|error| format!("decode postgres active_dispatch_id for session event {session_key}: {error}"))?,
        "model": row.try_get::<Option<String>, _>("model").map_err(|error| format!("decode postgres model for session event {session_key}: {error}"))?,
        "tokens": row.try_get::<i64, _>("tokens").map_err(|error| format!("decode postgres tokens for session event {session_key}: {error}"))?,
        "cwd": row.try_get::<Option<String>, _>("cwd").map_err(|error| format!("decode postgres cwd for session event {session_key}: {error}"))?,
        "last_seen_at": last_seen_at.map(|value| value.to_rfc3339()),
        "session_info": row.try_get::<Option<String>, _>("session_info").map_err(|error| format!("decode postgres session_info for session event {session_key}: {error}"))?,
        "department_id": row.try_get::<Option<String>, _>("department").map_err(|error| format!("decode postgres department for session event {session_key}: {error}"))?,
        "sprite_number": row.try_get::<Option<i64>, _>("sprite_number").map_err(|error| format!("decode postgres sprite_number for session event {session_key}: {error}"))?,
        "avatar_emoji": row.try_get::<Option<String>, _>("avatar_emoji").map_err(|error| format!("decode postgres avatar_emoji for session event {session_key}: {error}"))?.unwrap_or_else(|| "\u{1F916}".to_string()),
        "stats_xp": row.try_get::<i64, _>("stats_xp").map_err(|error| format!("decode postgres stats_xp for session event {session_key}: {error}"))?,
        "thread_channel_id": row.try_get::<Option<String>, _>("thread_channel_id").map_err(|error| format!("decode postgres thread_channel_id for session event {session_key}: {error}"))?,
        "department_name": row.try_get::<Option<String>, _>("department_name").map_err(|error| format!("decode postgres department_name for session event {session_key}: {error}"))?,
        "department_name_ko": row.try_get::<Option<String>, _>("department_name_ko").map_err(|error| format!("decode postgres department_name_ko for session event {session_key}: {error}"))?,
        "department_color": row.try_get::<Option<String>, _>("department_color").map_err(|error| format!("decode postgres department_color for session event {session_key}: {error}"))?,
        "connected_at": null,
    })))
}

async fn load_agent_status_payload_pg(
    pool: &PgPool,
    agent_id: &str,
    session_key: &str,
) -> Result<Option<serde_json::Value>, String> {
    let row = sqlx::query(
        "SELECT
            a.id,
            a.name,
            a.name_ko,
            s.status,
            s.session_info,
            a.provider AS cli_provider,
            a.avatar_emoji,
            a.department,
            a.discord_channel_id,
            a.discord_channel_alt,
            a.discord_channel_cc,
            a.discord_channel_cdx
         FROM agents a
         LEFT JOIN sessions s
           ON s.agent_id = a.id
          AND s.session_key = $2
         WHERE a.id = $1",
    )
    .bind(agent_id)
    .bind(session_key)
    .fetch_optional(pool)
    .await
    .map_err(|error| {
        format!("load postgres agent status payload for {agent_id}/{session_key}: {error}")
    })?;

    let Some(row) = row else {
        return Ok(None);
    };

    Ok(Some(json!({
        "id": row.try_get::<String, _>("id").map_err(|error| format!("decode postgres agent id for {agent_id}: {error}"))?,
        "name": row.try_get::<String, _>("name").map_err(|error| format!("decode postgres agent name for {agent_id}: {error}"))?,
        "name_ko": row.try_get::<Option<String>, _>("name_ko").map_err(|error| format!("decode postgres agent name_ko for {agent_id}: {error}"))?,
        "status": row.try_get::<Option<String>, _>("status").map_err(|error| format!("decode postgres agent status for {agent_id}: {error}"))?,
        "session_info": row.try_get::<Option<String>, _>("session_info").map_err(|error| format!("decode postgres agent session_info for {agent_id}: {error}"))?,
        "cli_provider": row.try_get::<Option<String>, _>("cli_provider").map_err(|error| format!("decode postgres cli_provider for {agent_id}: {error}"))?,
        "avatar_emoji": row.try_get::<Option<String>, _>("avatar_emoji").map_err(|error| format!("decode postgres avatar_emoji for {agent_id}: {error}"))?,
        "department": row.try_get::<Option<String>, _>("department").map_err(|error| format!("decode postgres department for {agent_id}: {error}"))?,
        "discord_channel_id": row.try_get::<Option<String>, _>("discord_channel_id").map_err(|error| format!("decode postgres discord_channel_id for {agent_id}: {error}"))?,
        "discord_channel_alt": row.try_get::<Option<String>, _>("discord_channel_alt").map_err(|error| format!("decode postgres discord_channel_alt for {agent_id}: {error}"))?,
        "discord_channel_cc": row.try_get::<Option<String>, _>("discord_channel_cc").map_err(|error| format!("decode postgres discord_channel_cc for {agent_id}: {error}"))?,
        "discord_channel_cdx": row.try_get::<Option<String>, _>("discord_channel_cdx").map_err(|error| format!("decode postgres discord_channel_cdx for {agent_id}: {error}"))?,
        "discord_channel_id_codex": row.try_get::<Option<String>, _>("discord_channel_cdx").map_err(|error| format!("decode postgres discord_channel_id_codex for {agent_id}: {error}"))?,
    })))
}

async fn load_session_update_payload_pg(
    pool: &PgPool,
    id: i64,
) -> Result<Option<serde_json::Value>, String> {
    let row = sqlx::query(
        "SELECT
            id,
            session_key,
            agent_id,
            status,
            provider,
            session_info,
            model,
            tokens,
            cwd,
            active_dispatch_id,
            last_heartbeat
         FROM sessions
         WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres session update payload for {id}: {error}"))?;

    let Some(row) = row else {
        return Ok(None);
    };

    let last_heartbeat: Option<chrono::DateTime<chrono::Utc>> =
        row.try_get("last_heartbeat").map_err(|error| {
            format!("decode postgres last_heartbeat for session update {id}: {error}")
        })?;

    Ok(Some(json!({
        "id": row.try_get::<i64, _>("id").map_err(|error| format!("decode postgres session id for update {id}: {error}"))?.to_string(),
        "session_key": row.try_get::<String, _>("session_key").map_err(|error| format!("decode postgres session_key for update {id}: {error}"))?,
        "agent_id": row.try_get::<Option<String>, _>("agent_id").map_err(|error| format!("decode postgres agent_id for update {id}: {error}"))?,
        "status": row.try_get::<Option<String>, _>("status").map_err(|error| format!("decode postgres status for update {id}: {error}"))?,
        "provider": row.try_get::<Option<String>, _>("provider").map_err(|error| format!("decode postgres provider for update {id}: {error}"))?,
        "session_info": row.try_get::<Option<String>, _>("session_info").map_err(|error| format!("decode postgres session_info for update {id}: {error}"))?,
        "model": row.try_get::<Option<String>, _>("model").map_err(|error| format!("decode postgres model for update {id}: {error}"))?,
        "tokens": row.try_get::<i64, _>("tokens").map_err(|error| format!("decode postgres tokens for update {id}: {error}"))?,
        "cwd": row.try_get::<Option<String>, _>("cwd").map_err(|error| format!("decode postgres cwd for update {id}: {error}"))?,
        "active_dispatch_id": row.try_get::<Option<String>, _>("active_dispatch_id").map_err(|error| format!("decode postgres active_dispatch_id for update {id}: {error}"))?,
        "last_heartbeat": last_heartbeat.map(|value| value.to_rfc3339()),
    })))
}

async fn hook_session_pg(
    state: &AppState,
    pool: &PgPool,
    body: HookSessionBody,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut thread_channel_id = normalize_thread_channel_id(body.thread_channel_id.as_deref())
        .or_else(|| {
            body.name
                .as_deref()
                .and_then(parse_thread_channel_name)
                .map(|(_, tid)| tid.to_string())
        })
        .or_else(|| parse_thread_channel_id_from_session_key(&body.session_key));
    if thread_channel_id.is_none()
        && let Some(dispatch_id) = body.dispatch_id.as_deref()
    {
        thread_channel_id = load_dispatch_thread_id_pg(pool, dispatch_id).await;
    }

    let agent_id = resolve_agent_id_for_session_pg(
        pool,
        None,
        Some(&body.session_key),
        body.name.as_deref(),
        thread_channel_id.as_deref(),
        body.dispatch_id.as_deref(),
    )
    .await;

    let status = normalize_incoming_session_status(body.status.as_deref());
    let provider = body.provider.as_deref().unwrap_or("claude");
    let tokens = body.tokens.unwrap_or(0) as i64;
    let active_dispatch_id = normalize_hook_active_dispatch_id(status, body.dispatch_id.as_deref());
    let claude_session_id = body.claude_session_id.as_deref().filter(|s| !s.is_empty());
    let raw_provider_session_id = body.session_id.as_deref().filter(|s| !s.is_empty());

    let is_new_session = match sqlx::query("SELECT 1 FROM sessions WHERE session_key = $1 LIMIT 1")
        .bind(&body.session_key)
        .fetch_optional(pool)
        .await
    {
        Ok(row) => row.is_none(),
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("load postgres session existence for {}: {error}", body.session_key)}),
                ),
            );
        }
    };

    let result = sqlx::query(
        "INSERT INTO sessions (
            session_key,
            agent_id,
            provider,
            status,
            session_info,
            model,
            tokens,
            cwd,
            active_dispatch_id,
            thread_channel_id,
            claude_session_id,
            raw_provider_session_id,
            last_heartbeat
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, NOW()
         )
         ON CONFLICT(session_key) DO UPDATE SET
            status = EXCLUDED.status,
            provider = EXCLUDED.provider,
            session_info = COALESCE(EXCLUDED.session_info, sessions.session_info),
            model = COALESCE(EXCLUDED.model, sessions.model),
            tokens = EXCLUDED.tokens,
            cwd = COALESCE(EXCLUDED.cwd, sessions.cwd),
            active_dispatch_id = CASE
              WHEN lower(EXCLUDED.status) IN ('disconnected', 'aborted') THEN NULL
              WHEN EXCLUDED.active_dispatch_id IS NOT NULL THEN EXCLUDED.active_dispatch_id
              ELSE sessions.active_dispatch_id
            END,
            agent_id = COALESCE(NULLIF(BTRIM(EXCLUDED.agent_id), ''), NULLIF(BTRIM(sessions.agent_id), '')),
            thread_channel_id = COALESCE(EXCLUDED.thread_channel_id, sessions.thread_channel_id),
            claude_session_id = COALESCE(EXCLUDED.claude_session_id, sessions.claude_session_id),
            raw_provider_session_id = COALESCE(EXCLUDED.raw_provider_session_id, sessions.raw_provider_session_id),
            last_heartbeat = NOW()",
    )
    .bind(&body.session_key)
    .bind(agent_id.as_deref())
    .bind(provider)
    .bind(status)
    .bind(body.session_info.as_deref())
    .bind(body.model.as_deref())
    .bind(tokens)
    .bind(body.cwd.as_deref())
    .bind(active_dispatch_id.as_deref())
    .bind(thread_channel_id.as_deref())
    .bind(claude_session_id)
    .bind(raw_provider_session_id)
    .execute(pool)
    .await;

    match result {
        Ok(_) => {
            let dispatch_id = body.dispatch_id.clone();

            crate::kanban::fire_event_hooks_with_backends(
                None,
                &state.engine,
                "on_session_status_change",
                "OnSessionStatusChange",
                json!({
                    "session_key": body.session_key,
                    "status": status,
                    "agent_id": agent_id,
                    "dispatch_id": dispatch_id,
                    "provider": provider,
                }),
            );

            if is_user_wait_status(status)
                && let Some(aid) = agent_id.as_ref()
            {
                spawn_auto_queue_activate_for_agent(state.clone(), aid.clone());
            }

            match load_session_event_payload_pg(pool, &body.session_key).await {
                Ok(Some(payload)) => {
                    if is_new_session {
                        crate::server::ws::emit_event(
                            &state.broadcast_tx,
                            "dispatched_session_new",
                            payload,
                        );
                    } else {
                        crate::server::ws::emit_batched_event(
                            &state.batch_buffer,
                            "dispatched_session_update",
                            &body.session_key,
                            payload,
                        );
                    }
                }
                Ok(None) => {}
                Err(error) => tracing::warn!(
                    "[dispatched-sessions] hook_session_pg: failed to load session payload for {}: {}",
                    body.session_key,
                    error
                ),
            }

            if let Some(aid) = agent_id.as_deref() {
                match load_agent_status_payload_pg(pool, aid, &body.session_key).await {
                    Ok(Some(agent)) => {
                        crate::server::ws::emit_batched_event(
                            &state.batch_buffer,
                            "agent_status",
                            aid,
                            agent,
                        );
                    }
                    Ok(None) => {}
                    Err(error) => tracing::warn!(
                        "[dispatched-sessions] hook_session_pg: failed to load agent payload for {} / {}: {}",
                        aid,
                        body.session_key,
                        error
                    ),
                }
            }

            (StatusCode::OK, Json(json!({"ok": true})))
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(
                json!({"error": format!("upsert postgres session {}: {error}", body.session_key)}),
            ),
        ),
    }
}

fn spawn_auto_queue_activate_for_agent(state: AppState, agent_id: String) {
    tokio::spawn(async move {
        // Let the session/dispatch cleanup commit before queue activation probes.
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        let _ = super::auto_queue::activate(
            State(state),
            Json(super::auto_queue::ActivateBody {
                run_id: None,
                repo: None,
                agent_id: Some(agent_id),
                thread_group: None,
                unified_thread: None,
                active_only: Some(true),
            }),
        )
        .await;
    });
}

fn normalize_hook_active_dispatch_id(status: &str, dispatch_id: Option<&str>) -> Option<String> {
    if !is_live_status(status) {
        return None;
    }

    dispatch_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

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

#[derive(Debug, Deserialize, Serialize)]
#[allow(dead_code)]
pub struct HookSessionBody {
    pub session_key: String,
    pub agent_id: Option<String>,
    pub status: Option<String>,
    pub provider: Option<String>,
    pub session_info: Option<String>,
    pub name: Option<String>,
    pub model: Option<String>,
    pub tokens: Option<u64>,
    pub cwd: Option<String>,
    pub dispatch_id: Option<String>,
    pub thread_channel_id: Option<String>,
    pub claude_session_id: Option<String>,
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct DeleteSessionQuery {
    pub session_key: String,
    pub provider: Option<String>,
}

// ── Handlers ──────────────────────────────────────────────────

/// GET /api/dispatched-sessions
pub async fn list_dispatched_sessions(
    State(state): State<AppState>,
    Query(params): Query<ListDispatchedSessionsQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let include_all = params.include_merged.as_deref() == Some("1");
    if let Some(pool) = state.pg_pool_ref() {
        return match list_dispatched_sessions_pg(pool, include_all).await {
            Ok(sessions) => (StatusCode::OK, Json(json!({"sessions": sessions}))),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            ),
        };
    }

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "postgres pool unavailable"})),
    )
}

/// POST /api/dispatched-sessions/webhook — upsert session from dcserver
pub async fn hook_session(
    State(state): State<AppState>,
    Json(body): Json<HookSessionBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        return hook_session_pg(&state, pool, body).await;
    }

    #[cfg(test)]
    if let Some(db) = state.legacy_db().cloned() {
        return hook_session_sqlite_for_tests(&state, db, body).await;
    }

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "postgres pool unavailable"})),
    )
}

#[cfg(test)]
async fn hook_session_sqlite_for_tests(
    state: &AppState,
    db: crate::db::Db,
    body: HookSessionBody,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match db.lock() {
        Ok(conn) => conn,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            );
        }
    };

    let thread_channel_id = normalize_thread_channel_id(body.thread_channel_id.as_deref())
        .or_else(|| {
            body.name
                .as_deref()
                .and_then(parse_thread_channel_name)
                .map(|(_, tid)| tid.to_string())
        })
        .or_else(|| parse_thread_channel_id_from_session_key(&body.session_key))
        .or_else(|| {
            body.dispatch_id
                .as_deref()
                .and_then(|dispatch_id| load_dispatch_thread_id_sqlite(&conn, dispatch_id))
        });

    let agent_id = resolve_agent_id_for_session(
        &conn,
        None,
        Some(&body.session_key),
        body.name.as_deref(),
        thread_channel_id.as_deref(),
        body.dispatch_id.as_deref(),
    );

    let status = normalize_incoming_session_status(body.status.as_deref());
    let provider = body.provider.as_deref().unwrap_or("claude");
    let tokens = body.tokens.unwrap_or(0) as i64;
    let active_dispatch_id = normalize_hook_active_dispatch_id(status, body.dispatch_id.as_deref());
    let claude_session_id = body.claude_session_id.as_deref().filter(|s| !s.is_empty());
    let raw_provider_session_id = body.session_id.as_deref().filter(|s| !s.is_empty());

    let result = conn.execute(
        "INSERT INTO sessions (
            session_key,
            agent_id,
            provider,
            status,
            session_info,
            model,
            tokens,
            cwd,
            active_dispatch_id,
            thread_channel_id,
            claude_session_id,
            raw_provider_session_id,
            last_heartbeat
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, datetime('now'))
         ON CONFLICT(session_key) DO UPDATE SET
            status = excluded.status,
            provider = excluded.provider,
            session_info = COALESCE(excluded.session_info, sessions.session_info),
            model = COALESCE(excluded.model, sessions.model),
            tokens = excluded.tokens,
            cwd = COALESCE(excluded.cwd, sessions.cwd),
            active_dispatch_id = CASE
              WHEN lower(excluded.status) IN ('disconnected', 'aborted') THEN NULL
              WHEN excluded.active_dispatch_id IS NOT NULL THEN excluded.active_dispatch_id
              ELSE sessions.active_dispatch_id
            END,
            agent_id = COALESCE(NULLIF(TRIM(excluded.agent_id), ''), NULLIF(TRIM(sessions.agent_id), '')),
            thread_channel_id = COALESCE(excluded.thread_channel_id, sessions.thread_channel_id),
            claude_session_id = COALESCE(excluded.claude_session_id, sessions.claude_session_id),
            raw_provider_session_id = COALESCE(excluded.raw_provider_session_id, sessions.raw_provider_session_id),
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
            active_dispatch_id,
            thread_channel_id,
            claude_session_id,
            raw_provider_session_id,
        ],
    );

    match result {
        Ok(_) => {
            let dispatch_id = body.dispatch_id.clone();
            drop(conn);

            crate::kanban::fire_event_hooks(
                &db,
                &state.engine,
                "on_session_status_change",
                "OnSessionStatusChange",
                json!({
                    "session_key": body.session_key,
                    "status": status,
                    "agent_id": agent_id,
                    "dispatch_id": dispatch_id,
                    "provider": provider,
                }),
            );

            if is_user_wait_status(status) {
                if let Some(aid) = agent_id {
                    spawn_auto_queue_activate_for_agent(state.clone(), aid);
                }
            }

            (StatusCode::OK, Json(json!({"ok": true})))
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{error}")})),
        ),
    }
}

/// DELETE /api/dispatched-sessions/cleanup — manual: delete disconnected sessions
pub async fn cleanup_sessions(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query("DELETE FROM sessions WHERE status = 'disconnected'")
            .execute(pool)
            .await
        {
            Ok(result) => (
                StatusCode::OK,
                Json(json!({"ok": true, "deleted": result.rows_affected()})),
            ),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            ),
        };
    }

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "postgres pool unavailable"})),
    )
}

/// DELETE /api/dispatched-sessions/gc-threads — periodic: delete stale thread sessions
pub async fn gc_thread_sessions(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        let deleted = gc_stale_thread_sessions_pg(pool).await;
        return (
            StatusCode::OK,
            Json(json!({"ok": true, "gc_threads": deleted})),
        );
    }

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "postgres pool unavailable"})),
    )
}

/// DELETE /api/dispatched-sessions/webhook — delete a session by session_key
pub async fn delete_session(
    State(state): State<AppState>,
    Query(params): Query<DeleteSessionQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        let session_id =
            match sqlx::query_scalar::<_, i64>("SELECT id FROM sessions WHERE session_key = $1")
                .bind(&params.session_key)
                .fetch_optional(pool)
                .await
            {
                Ok(session_id) => session_id,
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": format!("{error}")})),
                    );
                }
            };

        return match sqlx::query("DELETE FROM sessions WHERE session_key = $1")
            .bind(&params.session_key)
            .execute(pool)
            .await
        {
            Ok(result) => {
                if let Some(session_id) = session_id {
                    crate::server::ws::emit_event(
                        &state.broadcast_tx,
                        "dispatched_session_disconnect",
                        json!({"id": session_id.to_string()}),
                    );
                }
                (
                    StatusCode::OK,
                    Json(json!({"ok": true, "deleted": result.rows_affected()})),
                )
            }
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            ),
        };
    }

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "postgres pool unavailable"})),
    )
}

/// GET /api/dispatched-sessions/claude-session-id?session_key=...
/// Returns the stored provider session_id for the given session_key.
pub async fn get_claude_session_id(
    State(state): State<AppState>,
    Query(params): Query<DeleteSessionQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        let _ = disconnect_stale_fixed_session_by_key_pg(pool, &params.session_key).await;

        let provider = params.provider.as_deref().filter(|s| !s.is_empty());
        let result = if let Some(provider) = provider {
            sqlx::query(
                "SELECT claude_session_id, raw_provider_session_id
                 FROM sessions
                 WHERE session_key = $1 AND provider = $2",
            )
            .bind(&params.session_key)
            .bind(provider)
            .fetch_optional(pool)
            .await
        } else {
            sqlx::query(
                "SELECT claude_session_id, raw_provider_session_id
                 FROM sessions
                 WHERE session_key = $1",
            )
            .bind(&params.session_key)
            .fetch_optional(pool)
            .await
        };

        return match result {
            Ok(Some(row)) => {
                let claude_session_id: Option<String> = match row.try_get("claude_session_id") {
                    Ok(value) => value,
                    Err(error) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": format!("{error}")})),
                        );
                    }
                };
                let raw_provider_session_id: Option<String> =
                    match row.try_get("raw_provider_session_id") {
                        Ok(value) => value,
                        Err(error) => {
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(json!({"error": format!("{error}")})),
                            );
                        }
                    };
                (
                    StatusCode::OK,
                    Json(json!({
                        "claude_session_id": claude_session_id,
                        "session_id": claude_session_id,
                        "raw_provider_session_id": raw_provider_session_id,
                    })),
                )
            }
            Ok(None) => (
                StatusCode::OK,
                Json(json!({
                    "claude_session_id": null,
                    "session_id": null,
                    "raw_provider_session_id": null,
                })),
            ),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            ),
        };
    }

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "postgres pool unavailable"})),
    )
}

/// POST /api/dispatched-sessions/clear-stale-session-id
/// Clears provider session_id from ALL sessions that have the given stale ID.
pub async fn clear_stale_session_id(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(sid) = body
        .get("session_id")
        .and_then(|v| v.as_str())
        .or_else(|| body.get("claude_session_id").and_then(|v| v.as_str()))
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "session_id required"})),
        );
    };
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query(
            "UPDATE sessions
             SET claude_session_id = NULL,
                 raw_provider_session_id = NULL
             WHERE claude_session_id = $1
                OR raw_provider_session_id = $1",
        )
        .bind(sid)
        .execute(pool)
        .await
        {
            Ok(result) => (
                StatusCode::OK,
                Json(json!({"cleared": result.rows_affected()})),
            ),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            ),
        };
    }

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "postgres pool unavailable"})),
    )
}

/// POST /api/dispatched-sessions/clear-session-id
/// Clears claude_session_id for a specific session_key.
/// Used when /clear is called so the next turn doesn't resume a dead session.
pub async fn clear_session_id_by_key(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(key) = body.get("session_key").and_then(|v| v.as_str()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "session_key required"})),
        );
    };
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query(
            "UPDATE sessions
             SET claude_session_id = NULL,
                 raw_provider_session_id = NULL
             WHERE session_key = $1",
        )
        .bind(key)
        .execute(pool)
        .await
        {
            Ok(result) => (
                StatusCode::OK,
                Json(json!({"cleared": result.rows_affected()})),
            ),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            ),
        };
    }

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "postgres pool unavailable"})),
    )
}

async fn backfill_legacy_thread_channel_ids_pg(pool: &PgPool) -> usize {
    let session_keys = match sqlx::query_scalar::<_, String>(
        "SELECT session_key
         FROM sessions
         WHERE thread_channel_id IS NULL",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(
                "[dispatched-sessions] backfill_legacy_thread_channel_ids_pg: failed to load session keys: {error}"
            );
            return 0;
        }
    };

    let mut updated = 0usize;
    for session_key in session_keys {
        let Some(thread_channel_id) = parse_thread_channel_id_from_session_key(&session_key) else {
            continue;
        };

        match sqlx::query(
            "UPDATE sessions
             SET thread_channel_id = $1
             WHERE session_key = $2
               AND thread_channel_id IS NULL",
        )
        .bind(&thread_channel_id)
        .bind(&session_key)
        .execute(pool)
        .await
        {
            Ok(result) => updated += result.rows_affected() as usize,
            Err(error) => tracing::warn!(
                "[dispatched-sessions] backfill_legacy_thread_channel_ids_pg: failed to update {}: {}",
                session_key,
                error
            ),
        }
    }

    updated
}

pub async fn gc_stale_thread_sessions_pg(pool: &PgPool) -> usize {
    let _ = backfill_legacy_thread_channel_ids_pg(pool).await;
    match sqlx::query(
        "DELETE FROM sessions
         WHERE thread_channel_id IS NOT NULL
           AND status IN ('idle', 'awaiting_user', 'disconnected', 'aborted')
           AND (
             (active_dispatch_id IS NULL
               AND COALESCE(last_heartbeat, created_at) < NOW() - INTERVAL '1 hour')
             OR COALESCE(last_heartbeat, created_at) < NOW() - INTERVAL '3 hours'
           )",
    )
    .execute(pool)
    .await
    {
        Ok(result) => result.rows_affected() as usize,
        Err(error) => {
            tracing::warn!(
                "[dispatched-sessions] gc_stale_thread_sessions_pg: failed to delete stale sessions: {error}"
            );
            0
        }
    }
}

/// Mark stale fixed-channel working sessions as disconnected so they cannot
/// keep restoring dead provider session IDs after restart.
pub async fn gc_stale_fixed_working_sessions_db_pg(pool: &PgPool) -> usize {
    let stale_dispatches = match sqlx::query_scalar::<_, String>(
        "SELECT active_dispatch_id
         FROM sessions
         WHERE thread_channel_id IS NULL
           AND status IN ('working', 'turn_active')
           AND active_dispatch_id IS NOT NULL
           AND COALESCE(last_heartbeat, created_at) < NOW() - INTERVAL '6 hours'",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(
                "[dispatched-sessions] gc_stale_fixed_working_sessions_db_pg: failed to load stale dispatches: {error}"
            );
            return 0;
        }
    };

    for dispatch_id in stale_dispatches {
        if let Err(error) = sqlx::query(
            "UPDATE task_dispatches
             SET status = 'failed',
                 updated_at = NOW(),
                 completed_at = COALESCE(completed_at, NOW())
             WHERE id = $1
               AND status IN ('pending', 'dispatched')",
        )
        .bind(&dispatch_id)
        .execute(pool)
        .await
        {
            tracing::warn!(
                "[dispatched-sessions] gc_stale_fixed_working_sessions_db_pg: failed to mark stale dispatch {} as failed: {}",
                dispatch_id,
                error
            );
        }
    }

    match sqlx::query(
        "UPDATE sessions
         SET status = 'disconnected',
             active_dispatch_id = NULL,
             claude_session_id = NULL,
             raw_provider_session_id = NULL
         WHERE thread_channel_id IS NULL
           AND status IN ('working', 'turn_active')
           AND COALESCE(last_heartbeat, created_at) < NOW() - INTERVAL '6 hours'",
    )
    .execute(pool)
    .await
    {
        Ok(result) => result.rows_affected() as usize,
        Err(error) => {
            tracing::warn!(
                "[dispatched-sessions] gc_stale_fixed_working_sessions_db_pg: failed to disconnect stale sessions: {error}"
            );
            0
        }
    }
}

async fn disconnect_stale_fixed_session_by_key_pg(pool: &PgPool, session_key: &str) -> usize {
    let stale_dispatches = match sqlx::query_scalar::<_, String>(
        "SELECT active_dispatch_id
         FROM sessions
         WHERE session_key = $1
           AND thread_channel_id IS NULL
           AND status IN ('working', 'turn_active')
           AND active_dispatch_id IS NOT NULL
           AND COALESCE(last_heartbeat, created_at) < NOW() - INTERVAL '6 hours'",
    )
    .bind(session_key)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(
                "[dispatched-sessions] disconnect_stale_fixed_session_by_key_pg: failed to load stale dispatches for {}: {}",
                session_key,
                error
            );
            return 0;
        }
    };

    for dispatch_id in stale_dispatches {
        if let Err(error) = sqlx::query(
            "UPDATE task_dispatches
             SET status = 'failed',
                 updated_at = NOW(),
                 completed_at = COALESCE(completed_at, NOW())
             WHERE id = $1
               AND status IN ('pending', 'dispatched')",
        )
        .bind(&dispatch_id)
        .execute(pool)
        .await
        {
            tracing::warn!(
                "[dispatched-sessions] disconnect_stale_fixed_session_by_key_pg: failed to mark stale dispatch {} as failed: {}",
                dispatch_id,
                error
            );
        }
    }

    match sqlx::query(
        "UPDATE sessions
         SET status = 'disconnected',
             active_dispatch_id = NULL,
             claude_session_id = NULL,
             raw_provider_session_id = NULL
         WHERE session_key = $1
           AND thread_channel_id IS NULL
           AND status IN ('working', 'turn_active')
           AND COALESCE(last_heartbeat, created_at) < NOW() - INTERVAL '6 hours'",
    )
    .bind(session_key)
    .execute(pool)
    .await
    {
        Ok(result) => result.rows_affected() as usize,
        Err(error) => {
            tracing::warn!(
                "[dispatched-sessions] disconnect_stale_fixed_session_by_key_pg: failed to disconnect stale session {}: {}",
                session_key,
                error
            );
            0
        }
    }
}

/// PATCH /api/dispatched-sessions/:id
pub async fn update_dispatched_session(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateDispatchedSessionBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        if body.status.is_none()
            && body.active_dispatch_id.is_none()
            && body.model.is_none()
            && body.tokens.is_none()
            && body.cwd.is_none()
            && body.session_info.is_none()
        {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "no fields to update"})),
            );
        }

        return match sqlx::query(
            "UPDATE sessions
             SET status = COALESCE($1, status),
                 active_dispatch_id = COALESCE($2, active_dispatch_id),
                 model = COALESCE($3, model),
                 tokens = COALESCE($4, tokens),
                 cwd = COALESCE($5, cwd),
                 session_info = COALESCE($6, session_info)
             WHERE id = $7",
        )
        .bind(
            body.status
                .as_deref()
                .map(|status| normalize_incoming_session_status(Some(status))),
        )
        .bind(body.active_dispatch_id.as_deref())
        .bind(body.model.as_deref())
        .bind(body.tokens)
        .bind(body.cwd.as_deref())
        .bind(body.session_info.as_deref())
        .bind(id)
        .execute(pool)
        .await
        {
            Ok(result) if result.rows_affected() == 0 => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "session not found"})),
            ),
            Ok(_) => {
                match load_session_update_payload_pg(pool, id).await {
                    Ok(Some(session)) => {
                        crate::server::ws::emit_batched_event(
                            &state.batch_buffer,
                            "dispatched_session_update",
                            &id.to_string(),
                            session,
                        );
                    }
                    Ok(None) => {}
                    Err(error) => tracing::warn!(
                        "[dispatched-sessions] update_dispatched_session: failed to load postgres session payload {}: {}",
                        id,
                        error
                    ),
                }
                (StatusCode::OK, Json(json!({"ok": true})))
            }
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            ),
        };
    }

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "postgres pool unavailable"})),
    )
}

#[derive(Deserialize)]
pub struct ForceKillBody {
    pub session_key: String,
    /// If true, mark the dispatch as 'failed' and create a retry dispatch.
    #[serde(default)]
    pub retry: bool,
}

#[derive(Deserialize)]
pub struct ForceKillOptions {
    /// If true, mark the dispatch as 'failed' and create a retry dispatch.
    #[serde(default)]
    pub retry: bool,
    /// Human-readable reason for the kill (e.g. "idle timeout", "slot reclaim").
    #[serde(default)]
    pub reason: Option<String>,
}

pub(crate) async fn force_kill_session_impl(
    state: &AppState,
    session_key: &str,
    retry: bool,
) -> (StatusCode, Json<serde_json::Value>) {
    force_kill_session_impl_with_reason(
        state,
        session_key,
        retry,
        "force-kill API 직접 호출 (호출자 미상)",
    )
    .await
}

pub(crate) async fn force_kill_session_impl_with_reason(
    state: &AppState,
    session_key: &str,
    retry: bool,
    reason: &str,
) -> (StatusCode, Json<serde_json::Value>) {
    let session_key = session_key;

    // Parse tmux session name from session_key (format: "hostname:tmux_name")
    let tmux_name = match session_key.split_once(':') {
        Some((_, name)) => name.to_string(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "invalid session_key format — expected hostname:tmux_name"})),
            );
        }
    };

    // Parse provider from tmux name
    let provider_info =
        crate::services::provider::parse_provider_and_channel_from_tmux_name(&tmux_name);

    // Query session from the authoritative store.
    let provider_name = provider_info
        .as_ref()
        .map(|(provider, _)| provider.as_str());
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "postgres pool unavailable"})),
        );
    };
    let (active_dispatch_id, agent_id, runtime_channel_id, session_provider) =
        match load_force_kill_session_pg(pool, session_key, provider_name).await {
            Ok(Some(tuple)) => tuple,
            Ok(None) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "session not found"})),
                );
            }
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }
        };

    let (termination_reason_code, lifecycle_reason_code) =
        classify_session_termination_reason(reason);

    let lifecycle = force_kill_turn(
        state.health_registry.as_deref(),
        &TurnLifecycleTarget {
            provider: provider_info
                .as_ref()
                .map(|(provider, _)| provider.clone())
                .or_else(|| session_provider.as_deref().and_then(ProviderKind::from_str)),
            channel_id: runtime_channel_id
                .as_deref()
                .and_then(|channel_id| channel_id.parse::<u64>().ok())
                .map(poise::serenity_prelude::ChannelId::new),
            tmux_name: tmux_name.clone(),
        },
        reason,
        termination_reason_code,
    )
    .await;

    // 1. Kill tmux session (or confirm the runtime path already stopped it).
    let tmux_killed = lifecycle.tmux_killed;

    // 2. Clear persistent inflight state by matching tmux_session_name/channel_id.
    let inflight_cleared = lifecycle.inflight_cleared;

    // 3. Update session → disconnected, clear active fields
    // 4. Mark dispatch → failed
    // 5. Optionally create retry dispatch via central path (#108)
    let mut retry_dispatch_id: Option<String> = None;
    let retry_meta = match disconnect_session_and_prepare_retry_pg(
        pool,
        session_key,
        active_dispatch_id.as_deref(),
        retry,
    )
    .await
    {
        Ok(meta) => meta.map(|meta| {
            (
                meta.card_id,
                meta.to_agent_id,
                meta.dispatch_type,
                meta.title,
                meta.context,
                meta.retry_count,
            )
        }),
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            );
        }
    };

    // Create retry dispatch via central authoritative path (#108)
    if let Some((card_id, to_agent_id, dispatch_type, title, context, retry_count)) = retry_meta {
        let ctx: serde_json::Value = context
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(|| json!({}));

        let meta = RetryDispatchMeta {
            card_id,
            to_agent_id,
            dispatch_type,
            title,
            context: Some(ctx.to_string()),
            retry_count,
        };
        match create_retry_dispatch_pg(pool, &meta).await {
            Ok(new_id) => {
                retry_dispatch_id = Some(new_id);
            }
            Err(e) => {
                tracing::warn!(
                    "[force-kill] retry dispatch creation via postgres path failed for card {}: {e}",
                    meta.card_id
                );
            }
        }
    }

    let queue_activation_requested = if retry_dispatch_id.is_none() {
        if let Some(ref aid) = agent_id {
            spawn_auto_queue_activate_for_agent(state.clone(), aid.clone());
            true
        } else {
            false
        }
    } else {
        false
    };

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] ⚡ force-kill: session={}, tmux_killed={}, inflight_cleared={}, dispatch_failed={:?}, lifecycle={}",
        session_key,
        tmux_killed,
        inflight_cleared,
        active_dispatch_id,
        lifecycle.lifecycle_path
    );

    if tmux_killed && !lifecycle.termination_recorded {
        crate::services::termination_audit::record_termination_with_handles(
            None,
            state.pg_pool_ref(),
            session_key,
            active_dispatch_id.as_deref(),
            "force_kill_api",
            termination_reason_code,
            Some(reason),
            None,
            None,
            Some(false),
        );
    }

    // Notify bot message for force-kill visibility
    if tmux_killed && let Some(ref channel_id_str) = runtime_channel_id {
        // Build human-readable message: agent name + reason from tmux exit file
        let agent_label = agent_id.as_deref().unwrap_or("unknown");
        let exit_reason = crate::services::tmux_diagnostics::read_tmux_exit_reason(&tmux_name)
            .map(|r| {
                // Strip timestamp prefix "[2026-...] " if present
                let trimmed = if let Some(idx) = r.find("] ") {
                    &r[idx + 2..]
                } else {
                    &r
                };
                let s = trimmed.trim();
                if s.len() > 80 {
                    format!("{}…", &s[..80])
                } else {
                    s.to_string()
                }
            })
            .unwrap_or_else(|| lifecycle.lifecycle_path.to_string());
        let _ = enqueue_lifecycle_notification_pg(
            pool,
            &format!("channel:{channel_id_str}"),
            Some(session_key),
            lifecycle_reason_code,
            &format!("🔴 세션 종료: {agent_label}\n사유: {exit_reason}"),
        )
        .await;
    }

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "tmux_killed": tmux_killed,
            "inflight_cleared": inflight_cleared,
            "lifecycle_path": lifecycle.lifecycle_path,
            "queued_remaining": lifecycle.queue_depth,
            "queue_preserved": lifecycle.queue_preserved,
            "dispatch_failed": active_dispatch_id,
            "retry_dispatch_id": retry_dispatch_id,
            "queue_activation_requested": queue_activation_requested,
        })),
    )
}

fn classify_session_termination_reason(reason: &str) -> (&'static str, &'static str) {
    let lower = reason.to_ascii_lowercase();
    if lower.contains("idle")
        || lower.contains("auto cleanup")
        || lower.contains("자동 정리")
        || lower.contains("turn cap")
        || lower.contains("cleanup")
    {
        ("auto_cleanup", "lifecycle.auto_cleanup")
    } else {
        ("force_kill", "lifecycle.force_kill")
    }
}

/// Query parameters for the tmux-output endpoint.
#[derive(Debug, Deserialize, Default)]
pub struct TmuxOutputQuery {
    /// Number of trailing tmux pane lines to capture. Default: 80. Clamped to
    /// the inclusive range [1, 2000] to avoid accidental giant captures.
    pub lines: Option<i32>,
}

const TMUX_OUTPUT_DEFAULT_LINES: i32 = 80;
const TMUX_OUTPUT_MAX_LINES: i32 = 2000;

async fn load_session_by_id_pg(
    pool: &PgPool,
    id: i64,
) -> Result<Option<(String, Option<String>, Option<String>, Option<String>)>, String> {
    let row = sqlx::query(
        "SELECT session_key, agent_id, provider, status
         FROM sessions
         WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres session #{id}: {error}"))?;
    let Some(row) = row else {
        return Ok(None);
    };
    let session_key: Option<String> = row
        .try_get("session_key")
        .map_err(|error| format!("decode session_key for #{id}: {error}"))?;
    let agent_id: Option<String> = row
        .try_get("agent_id")
        .map_err(|error| format!("decode agent_id for #{id}: {error}"))?;
    let provider: Option<String> = row
        .try_get("provider")
        .map_err(|error| format!("decode provider for #{id}: {error}"))?;
    let status: Option<String> = row
        .try_get("status")
        .map_err(|error| format!("decode status for #{id}: {error}"))?;
    let Some(session_key) = session_key else {
        return Ok(None);
    };
    Ok(Some((session_key, agent_id, provider, status)))
}

/// GET /api/sessions/{id}/tmux-output?lines=N
///
/// #1067: Skill promotion for watch-agent-turn. Returns the latest N lines of
/// the tmux pane bound to the session identified by the numeric session id
/// (`sessions.id`). Reads the session row to derive `hostname:tmux_name` from
/// `session_key`, then shells out via [`crate::services::platform::tmux::capture_pane`].
pub async fn tmux_output(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(params): Query<TmuxOutputQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let requested_lines = params.lines.unwrap_or(TMUX_OUTPUT_DEFAULT_LINES);
    let effective_lines = requested_lines.max(1).min(TMUX_OUTPUT_MAX_LINES);

    // Lookup session row. Prefer Postgres (authoritative) when available.
    let session_row = if let Some(pool) = state.pg_pool_ref() {
        match load_session_by_id_pg(pool, id).await {
            Ok(value) => value,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }
        }
    } else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "postgres pool unavailable"})),
        );
    };

    let Some((session_key, agent_id, provider, status)) = session_row else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": format!("session #{id} not found"),
                "session_id": id,
            })),
        );
    };

    // session_key format: "hostname:tmux_name"
    let tmux_name = match session_key.split_once(':') {
        Some((_, name)) if !name.is_empty() => name.to_string(),
        _ => {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": format!(
                        "session #{id} session_key does not follow hostname:tmux_name format"
                    ),
                    "session_id": id,
                    "session_key": session_key,
                })),
            );
        }
    };

    let captured_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis() as i64)
        .unwrap_or(0);

    // capture_pane takes scroll_back as a negative offset from the pane bottom.
    let recent_output = crate::services::platform::tmux::capture_pane(&tmux_name, -effective_lines);
    let tmux_alive = recent_output.is_some();

    (
        StatusCode::OK,
        Json(json!({
            "session_id": id,
            "session_key": session_key,
            "tmux_name": tmux_name,
            "tmux_alive": tmux_alive,
            "agent_id": agent_id,
            "provider": provider,
            "status": status,
            "lines_requested": requested_lines,
            "lines_effective": effective_lines,
            "recent_output": recent_output.unwrap_or_default(),
            "captured_at_ms": captured_at_ms,
        })),
    )
}

/// POST /api/sessions/{session_key}/force-kill
///
/// Atomically: kill tmux session + clear inflight file + set session disconnected
/// + mark active dispatch failed. Optionally creates a retry dispatch.
pub async fn force_kill_session(
    State(state): State<AppState>,
    Path(session_key): Path<String>,
    Json(body): Json<ForceKillOptions>,
) -> (StatusCode, Json<serde_json::Value>) {
    let reason = body.reason.as_deref().unwrap_or("force-kill API invoked");
    force_kill_session_impl_with_reason(&state, &session_key, body.retry, reason).await
}

/// Legacy body-based wrapper retained for compatibility tests and direct callers.
///
/// This helper is no longer exposed as an HTTP route; use
/// `POST /api/sessions/{session_key}/force-kill` instead.
pub async fn force_kill_session_legacy(
    State(state): State<AppState>,
    Json(body): Json<ForceKillBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    force_kill_session_impl(&state, &body.session_key, body.retry).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::engine::PolicyEngine;
    use serde_json::Value;
    use std::ffi::OsString;
    use std::process::Command;
    use std::sync::MutexGuard;

    fn test_db() -> Db {
        crate::db::test_db()
    }

    fn test_engine(db: &Db) -> PolicyEngine {
        let config = crate::config::Config::default();
        PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap()
    }

    fn env_lock() -> MutexGuard<'static, ()> {
        crate::services::discord::runtime_store::lock_test_env()
    }

    struct TestPostgresDb {
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestPostgresDb {
        async fn create() -> Self {
            let admin_url = postgres_admin_database_url();
            let database_name = format!(
                "agentdesk_dispatched_sessions_{}",
                uuid::Uuid::new_v4().simple()
            );
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "dispatched sessions tests",
            )
            .await
            .expect("create postgres test db");

            Self {
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn connect_and_migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "dispatched sessions tests",
            )
            .await
            .expect("apply postgres migration")
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "dispatched sessions tests",
            )
            .await
            .expect("drop postgres test db");
        }
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set_path(key: &'static str, value: &std::path::Path) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    fn response_json(resp: Json<Value>) -> Value {
        resp.0
    }

    async fn seed_agent_pg(pool: &sqlx::PgPool, agent_id: &str) {
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id, created_at, updated_at)
             VALUES ($1, $2, 'codex', $3, NOW(), NOW())",
        )
        .bind(agent_id)
        .bind(format!("Agent {agent_id}"))
        .bind("123456789012345678")
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_card_pg(pool: &sqlx::PgPool, card_id: &str, dispatch_id: &str, status: &str) {
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, latest_dispatch_id, created_at, updated_at)
             VALUES ($1, 'Force Kill Card', $2, $3, NOW(), NOW())",
        )
        .bind(card_id)
        .bind(status)
        .bind(dispatch_id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_dispatch_pg(
        pool: &sqlx::PgPool,
        dispatch_id: &str,
        card_id: &str,
        agent_id: &str,
    ) {
        sqlx::query(
            "INSERT INTO task_dispatches
             (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, retry_count, created_at, updated_at)
             VALUES ($1, $2, $3, 'implementation', 'pending', 'Recover me', '{}', 0, NOW(), NOW())",
        )
        .bind(dispatch_id)
        .bind(card_id)
        .bind(agent_id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_session_pg(
        pool: &sqlx::PgPool,
        session_key: &str,
        agent_id: &str,
        dispatch_id: &str,
    ) {
        sqlx::query(
            "INSERT INTO sessions
             (session_key, agent_id, status, active_dispatch_id, provider, last_heartbeat, created_at)
             VALUES ($1, $2, 'turn_active', $3, 'codex', NOW(), NOW())",
        )
        .bind(session_key)
        .bind(agent_id)
        .bind(dispatch_id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_session_without_dispatch_pg(
        pool: &sqlx::PgPool,
        session_key: &str,
        agent_id: &str,
    ) {
        sqlx::query(
            "INSERT INTO sessions
             (session_key, agent_id, status, provider, last_heartbeat, created_at)
             VALUES ($1, $2, 'turn_active', 'codex', NOW(), NOW())",
        )
        .bind(session_key)
        .bind(agent_id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn count_message_outbox_rows_pg(pool: &sqlx::PgPool) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM message_outbox")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn count_termination_events_pg(pool: &sqlx::PgPool, session_key: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM session_termination_events WHERE session_key = $1",
        )
        .bind(session_key)
        .fetch_one(pool)
        .await
        .unwrap()
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
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn postgres_admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", postgres_base_database_url(), admin_db)
    }

    #[tokio::test]
    async fn force_kill_session_path_route_retries_active_dispatch_pg_path() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let db = test_db();
        let engine = test_engine(&db);
        let mut state = AppState::test_state(db, engine);
        state.pg_pool = Some(pool.clone());

        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id, created_at, updated_at)
             VALUES ($1, $2, 'codex', $3, NOW(), NOW())",
        )
        .bind("agent-force-pg")
        .bind("Agent agent-force-pg")
        .bind("123456789012345678")
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, latest_dispatch_id, created_at, updated_at)
             VALUES ($1, 'Force Kill Card', 'requested', $2, NOW(), NOW())",
        )
        .bind("card-force-pg")
        .bind("dispatch-force-pg")
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO task_dispatches (
                id,
                kanban_card_id,
                to_agent_id,
                dispatch_type,
                status,
                title,
                context,
                retry_count,
                created_at,
                updated_at
            ) VALUES ($1, $2, $3, 'implementation', 'pending', 'Recover me', '{}', 0, NOW(), NOW())",
        )
        .bind("dispatch-force-pg")
        .bind("card-force-pg")
        .bind("agent-force-pg")
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO sessions (
                session_key,
                agent_id,
                status,
                active_dispatch_id,
                provider,
                last_heartbeat,
                created_at
            ) VALUES ($1, $2, 'turn_active', $3, 'codex', NOW(), NOW())",
        )
        .bind("host:codex-agent-force-pg")
        .bind("agent-force-pg")
        .bind("dispatch-force-pg")
        .execute(&pool)
        .await
        .unwrap();

        let (status, body) = force_kill_session(
            State(state),
            Path("host:codex-agent-force-pg".to_string()),
            Json(ForceKillOptions {
                retry: true,
                reason: None,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let body = response_json(body);
        let retry_dispatch_id = body["retry_dispatch_id"].as_str().unwrap().to_string();
        assert!(!retry_dispatch_id.is_empty());
        assert_eq!(body["queue_activation_requested"], false);

        let session_state = sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT status, active_dispatch_id
             FROM sessions
             WHERE session_key = $1",
        )
        .bind("host:codex-agent-force-pg")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(session_state.0, "disconnected");
        assert!(session_state.1.is_none());

        let old_dispatch_status =
            sqlx::query_scalar::<_, String>("SELECT status FROM task_dispatches WHERE id = $1")
                .bind("dispatch-force-pg")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(old_dispatch_status, "failed");

        let new_dispatch = sqlx::query_as::<_, (String, i64)>(
            "SELECT status, retry_count::BIGINT FROM task_dispatches WHERE id = $1",
        )
        .bind(&retry_dispatch_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(new_dispatch.0, "pending");
        assert_eq!(new_dispatch.1, 1);

        let latest_dispatch_id = sqlx::query_scalar::<_, Option<String>>(
            "SELECT latest_dispatch_id FROM kanban_cards WHERE id = $1",
        )
        .bind("card-force-pg")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            latest_dispatch_id.as_deref(),
            Some(retry_dispatch_id.as_str())
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn force_kill_session_legacy_wrapper_pg_uses_same_core_without_retry() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());

        seed_agent_pg(&pool, "agent-force-legacy").await;
        seed_card_pg(
            &pool,
            "card-force-legacy",
            "dispatch-force-legacy",
            "requested",
        )
        .await;
        seed_dispatch_pg(
            &pool,
            "dispatch-force-legacy",
            "card-force-legacy",
            "agent-force-legacy",
        )
        .await;
        seed_session_pg(
            &pool,
            "host:claude-agent-force-legacy",
            "agent-force-legacy",
            "dispatch-force-legacy",
        )
        .await;

        let (status, body) = force_kill_session_legacy(
            State(state),
            Json(ForceKillBody {
                session_key: "host:claude-agent-force-legacy".to_string(),
                retry: false,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let body = response_json(body);
        assert_eq!(body["lifecycle_path"], "direct-fallback");
        assert!(body["retry_dispatch_id"].is_null());
        assert_eq!(body["queue_activation_requested"], true);

        let dispatch_status =
            sqlx::query_scalar::<_, String>("SELECT status FROM task_dispatches WHERE id = $1")
                .bind("dispatch-force-legacy")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(dispatch_status, "failed");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn force_kill_session_pg_clears_matching_inflight_and_live_tmux() {
        let _env_lock = env_lock();
        if Command::new("tmux").arg("-V").output().is_err() {
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
        let tmux_name = format!("AgentDesk-codex-force-kill-{}", std::process::id());
        let session_key = format!("host:{tmux_name}");
        let inflight_dir = temp
            .path()
            .join("runtime")
            .join("discord_inflight")
            .join("codex");
        std::fs::create_dir_all(&inflight_dir).unwrap();
        let inflight_path = inflight_dir.join("force-kill.json");
        std::fs::write(
            &inflight_path,
            serde_json::to_string(&json!({
                "version": 1,
                "provider": "codex",
                "channel_id": 123456789012345678u64,
                "channel_name": "force-kill",
                "request_owner_user_id": 1u64,
                "user_msg_id": 2u64,
                "current_msg_id": 3u64,
                "current_msg_len": 0,
                "user_text": "kill this",
                "session_id": null,
                "tmux_session_name": tmux_name,
                "output_path": null,
                "input_fifo_path": null,
                "last_offset": 0u64,
                "full_response": "",
                "response_sent_offset": 0,
                "started_at": "2026-04-06 10:20:00",
                "updated_at": "2026-04-06 10:20:01"
            }))
            .unwrap(),
        )
        .unwrap();

        let tmux_started = Command::new("tmux")
            .args(["new-session", "-d", "-s", &tmux_name, "sleep 30"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !tmux_started {
            return;
        }

        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());

        seed_agent_pg(&pool, "agent-force-live").await;
        seed_session_without_dispatch_pg(&pool, &session_key, "agent-force-live").await;

        let (status, body) = force_kill_session(
            State(state),
            Path(session_key.clone()),
            Json(ForceKillOptions {
                retry: false,
                reason: None,
            }),
        )
        .await;

        let body = response_json(body);
        let tmux_still_alive = Command::new("tmux")
            .args(["has-session", "-t", &tmux_name])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if tmux_still_alive {
            let _ = Command::new("tmux")
                .args(["kill-session", "-t", &tmux_name])
                .status();
        }

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["tmux_killed"], true);
        assert_eq!(body["inflight_cleared"], true);
        assert_eq!(body["lifecycle_path"], "direct-fallback");
        assert_eq!(body["queue_activation_requested"], true);
        assert!(
            !tmux_still_alive,
            "tmux session should be gone after force-kill"
        );
        assert!(
            !inflight_path.exists(),
            "matching inflight file should be deleted"
        );

        let session_status =
            sqlx::query_scalar::<_, String>("SELECT status FROM sessions WHERE session_key = $1")
                .bind(&session_key)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(session_status, "disconnected");
        assert_eq!(count_message_outbox_rows_pg(&pool).await, 1);
        assert_eq!(count_termination_events_pg(&pool, &session_key).await, 1);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn force_kill_session_pg_skips_notify_and_audit_when_tmux_is_already_gone() {
        let _env_lock = env_lock();
        let temp = tempfile::tempdir().unwrap();
        let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());

        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());
        let session_key = format!(
            "host:AgentDesk-codex-force-kill-dead-{}",
            std::process::id()
        );

        seed_agent_pg(&pool, "agent-force-dead").await;
        seed_session_without_dispatch_pg(&pool, &session_key, "agent-force-dead").await;

        let (status, body) = force_kill_session(
            State(state),
            Path(session_key.clone()),
            Json(ForceKillOptions {
                retry: false,
                reason: Some("idle 60분 초과 — 자동 정리".to_string()),
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let body = response_json(body);
        assert_eq!(body["tmux_killed"], false);
        assert_eq!(body["inflight_cleared"], false);
        assert_eq!(body["queue_activation_requested"], true);

        let session_status =
            sqlx::query_scalar::<_, String>("SELECT status FROM sessions WHERE session_key = $1")
                .bind(&session_key)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(session_status, "disconnected");
        assert_eq!(count_message_outbox_rows_pg(&pool).await, 0);
        assert_eq!(count_termination_events_pg(&pool, &session_key).await, 0);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn idle_hook_pg_does_not_auto_complete_implementation_dispatch() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());

        let card_id = "card-1";
        let dispatch_id = "dispatch-1";
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, latest_dispatch_id, created_at, updated_at)
             VALUES ($1, 'Test Card', 'requested', $2, NOW(), NOW())",
        )
        .bind(card_id)
        .bind(dispatch_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
             VALUES ($1, $2, 'ch-td', 'implementation', 'pending', 'Test Card', '{}', NOW(), NOW())",
        )
        .bind(dispatch_id)
        .bind(card_id)
        .execute(&pool)
        .await
        .unwrap();

        let (working_status, _) = hook_session(
            State(state.clone()),
            Json(HookSessionBody {
                session_key: "session-1".to_string(),
                agent_id: None,
                status: Some("working".to_string()),
                provider: Some("claude".to_string()),
                session_info: Some("working".to_string()),
                name: None,
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: Some(dispatch_id.to_string()),
                claude_session_id: None,
                thread_channel_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(working_status, StatusCode::OK);

        let (idle_status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: "session-1".to_string(),
                agent_id: None,
                status: Some("idle".to_string()),
                provider: Some("claude".to_string()),
                session_info: Some("idle".to_string()),
                name: None,
                model: None,
                tokens: Some(42),
                cwd: None,
                dispatch_id: Some(dispatch_id.to_string()),
                claude_session_id: None,
                thread_channel_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(idle_status, StatusCode::OK);

        // implementation dispatches must NOT be auto-completed on idle —
        // they require explicit completion from turn_bridge
        let card_status =
            sqlx::query_scalar::<_, String>("SELECT status FROM kanban_cards WHERE id = $1")
                .bind(card_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let dispatch_status =
            sqlx::query_scalar::<_, String>("SELECT status FROM task_dispatches WHERE id = $1")
                .bind(dispatch_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let active_dispatch_id = sqlx::query_scalar::<_, Option<String>>(
            "SELECT active_dispatch_id FROM sessions WHERE session_key = 'session-1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        // Card may move to in_progress via kanban-rules policy when session reports working,
        // but must NOT advance to review (which would happen if idle auto-completed the dispatch).
        assert!(
            card_status == "requested" || card_status == "in_progress",
            "card should not advance past in_progress, got: {card_status}"
        );
        assert_eq!(
            dispatch_status, "pending",
            "implementation dispatch should stay pending on idle"
        );
        assert_eq!(
            active_dispatch_id.as_deref(),
            Some(dispatch_id),
            "idle dispatch sessions must keep sticky active_dispatch_id for 180-minute TTL cleanup"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn idle_hook_pg_does_not_auto_complete_rework_dispatch() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());

        let card_id = "card-rework";
        let dispatch_id = "dispatch-rework";
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, latest_dispatch_id, created_at, updated_at)
             VALUES ($1, 'Rework Card', 'rework', $2, NOW(), NOW())",
        )
        .bind(card_id)
        .bind(dispatch_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
             VALUES ($1, $2, 'ch-td', 'rework', 'pending', 'Rework Card', '{}', NOW(), NOW())",
        )
        .bind(dispatch_id)
        .bind(card_id)
        .execute(&pool)
        .await
        .unwrap();

        let (working_status, _) = hook_session(
            State(state.clone()),
            Json(HookSessionBody {
                session_key: "session-rework".to_string(),
                agent_id: None,
                status: Some("working".to_string()),
                provider: Some("claude".to_string()),
                session_info: Some("working".to_string()),
                name: None,
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: Some(dispatch_id.to_string()),
                claude_session_id: None,
                thread_channel_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(working_status, StatusCode::OK);

        let (idle_status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: "session-rework".to_string(),
                agent_id: None,
                status: Some("idle".to_string()),
                provider: Some("claude".to_string()),
                session_info: Some("idle".to_string()),
                name: None,
                model: None,
                tokens: Some(10),
                cwd: None,
                dispatch_id: Some(dispatch_id.to_string()),
                claude_session_id: None,
                thread_channel_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(idle_status, StatusCode::OK);

        // rework dispatches must NOT be auto-completed on idle —
        // they require explicit completion from turn_bridge
        let card_status =
            sqlx::query_scalar::<_, String>("SELECT status FROM kanban_cards WHERE id = $1")
                .bind(card_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let dispatch_status =
            sqlx::query_scalar::<_, String>("SELECT status FROM task_dispatches WHERE id = $1")
                .bind(dispatch_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let active_dispatch_id = sqlx::query_scalar::<_, Option<String>>(
            "SELECT active_dispatch_id FROM sessions WHERE session_key = 'session-rework'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        // Card stays in rework — must NOT advance to review (which would happen
        // if idle auto-completed the rework dispatch).
        assert_eq!(card_status, "rework", "card should not advance past rework");
        assert_eq!(
            dispatch_status, "pending",
            "rework dispatch should stay pending on idle"
        );
        assert_eq!(
            active_dispatch_id.as_deref(),
            Some(dispatch_id),
            "idle rework sessions must keep sticky active_dispatch_id for 180-minute TTL cleanup"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn idle_hook_pg_does_not_auto_complete_pending_review_dispatch() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());

        let card_id = "card-review";
        let dispatch_id = "dispatch-review";
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, latest_dispatch_id, created_at, updated_at)
             VALUES ($1, 'Review Card', 'review', $2, NOW(), NOW())",
        )
        .bind(card_id)
        .bind(dispatch_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
             VALUES ($1, $2, 'project-agentdesk', 'review', 'pending', '[Review R1] Review Card', '{}', NOW(), NOW())",
        )
        .bind(dispatch_id)
        .bind(card_id)
        .execute(&pool)
        .await
        .unwrap();

        let (working_status, _) = hook_session(
            State(state.clone()),
            Json(HookSessionBody {
                session_key: "session-review".to_string(),
                agent_id: None,
                status: Some("working".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("working".to_string()),
                name: None,
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: Some(dispatch_id.to_string()),
                claude_session_id: None,
                thread_channel_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(working_status, StatusCode::OK);

        let (idle_status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: "session-review".to_string(),
                agent_id: None,
                status: Some("idle".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("idle".to_string()),
                name: None,
                model: None,
                tokens: Some(11),
                cwd: None,
                dispatch_id: Some(dispatch_id.to_string()),
                claude_session_id: None,
                thread_channel_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(idle_status, StatusCode::OK);

        let dispatch_status =
            sqlx::query_scalar::<_, String>("SELECT status FROM task_dispatches WHERE id = $1")
                .bind(dispatch_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let dispatch_result = sqlx::query_scalar::<_, Option<String>>(
            "SELECT result FROM task_dispatches WHERE id = $1",
        )
        .bind(dispatch_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let active_dispatch_id = sqlx::query_scalar::<_, Option<String>>(
            "SELECT active_dispatch_id FROM sessions WHERE session_key = 'session-review'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        // review dispatches must stay pending until an explicit review-verdict arrives
        assert_eq!(dispatch_status, "pending");
        assert!(dispatch_result.is_none());
        assert_eq!(
            active_dispatch_id.as_deref(),
            Some(dispatch_id),
            "idle review sessions must keep sticky active_dispatch_id for 180-minute TTL cleanup"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn idle_hook_without_dispatch_id_pg_preserves_existing_dispatch_binding() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());

        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, latest_dispatch_id, created_at, updated_at)
             VALUES ('card-sticky', 'Sticky Card', 'in_progress', 'dispatch-sticky', NOW(), NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
             VALUES ('dispatch-sticky', 'card-sticky', 'project-agentdesk', 'implementation', 'completed', 'Sticky', '{}', NOW(), NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();

        let (working_status, _) = hook_session(
            State(state.clone()),
            Json(HookSessionBody {
                session_key: "session-sticky".to_string(),
                agent_id: None,
                status: Some("working".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("working".to_string()),
                name: None,
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: Some("dispatch-sticky".to_string()),
                claude_session_id: None,
                thread_channel_id: Some("1485506232256168011".to_string()),
                session_id: None,
            }),
        )
        .await;
        assert_eq!(working_status, StatusCode::OK);

        let (working_refresh_status, _) = hook_session(
            State(state.clone()),
            Json(HookSessionBody {
                session_key: "session-sticky".to_string(),
                agent_id: None,
                status: Some("working".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("working".to_string()),
                name: None,
                model: None,
                tokens: Some(9),
                cwd: None,
                dispatch_id: None,
                claude_session_id: None,
                thread_channel_id: Some("1485506232256168011".to_string()),
                session_id: None,
            }),
        )
        .await;
        assert_eq!(working_refresh_status, StatusCode::OK);
        let (idle_status, _) = hook_session(
            State(state.clone()),
            Json(HookSessionBody {
                session_key: "session-sticky".to_string(),
                agent_id: None,
                status: Some("idle".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("idle".to_string()),
                name: None,
                model: None,
                tokens: Some(17),
                cwd: None,
                dispatch_id: Some("dispatch-sticky".to_string()),
                claude_session_id: None,
                thread_channel_id: Some("1485506232256168011".to_string()),
                session_id: None,
            }),
        )
        .await;
        assert_eq!(idle_status, StatusCode::OK);

        let (idle_refresh_status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: "session-sticky".to_string(),
                agent_id: None,
                status: Some("idle".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("idle".to_string()),
                name: None,
                model: None,
                tokens: Some(33),
                cwd: None,
                dispatch_id: None,
                claude_session_id: None,
                thread_channel_id: Some("1485506232256168011".to_string()),
                session_id: None,
            }),
        )
        .await;
        assert_eq!(idle_refresh_status, StatusCode::OK);

        let active_dispatch_id = sqlx::query_scalar::<_, Option<String>>(
            "SELECT active_dispatch_id FROM sessions WHERE session_key = 'session-sticky'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(active_dispatch_id.as_deref(), Some("dispatch-sticky"));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn heartbeat_without_dispatch_id_pg_does_not_resurrect_cleared_binding() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());

        sqlx::query(
            "INSERT INTO sessions
             (session_key, provider, status, active_dispatch_id, last_heartbeat, created_at)
             VALUES ('session-cleared', 'codex', 'turn_active', 'dispatch-cleared', NOW(), NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE sessions SET active_dispatch_id = NULL WHERE session_key = 'session-cleared'",
        )
        .execute(&pool)
        .await
        .unwrap();

        let (status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: "session-cleared".to_string(),
                agent_id: None,
                status: Some("working".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("working".to_string()),
                name: None,
                model: None,
                tokens: Some(21),
                cwd: None,
                dispatch_id: None,
                claude_session_id: None,
                thread_channel_id: Some("1485506232256168011".to_string()),
                session_id: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let active_dispatch_id = sqlx::query_scalar::<_, Option<String>>(
            "SELECT active_dispatch_id FROM sessions WHERE session_key = 'session-cleared'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(active_dispatch_id, None);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn hook_session_turn_activity_pg_refreshes_last_heartbeat_from_created_at() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());

        let seeded_created_at: chrono::DateTime<chrono::Utc> = "2026-04-09T01:02:03Z"
            .parse::<chrono::DateTime<chrono::Utc>>()
            .unwrap();
        sqlx::query(
            "INSERT INTO sessions
             (session_key, provider, status, created_at, last_heartbeat)
             VALUES ('session-heartbeat', 'codex', 'idle', $1, NULL)",
        )
        .bind(seeded_created_at)
        .execute(&pool)
        .await
        .unwrap();

        let (status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: "session-heartbeat".to_string(),
                agent_id: None,
                status: Some("working".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("working".to_string()),
                name: None,
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: None,
                claude_session_id: None,
                thread_channel_id: Some("1485506232256168011".to_string()),
                session_id: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (created_at, last_heartbeat) = sqlx::query_as::<
            _,
            (
                chrono::DateTime<chrono::Utc>,
                Option<chrono::DateTime<chrono::Utc>>,
            ),
        >(
            "SELECT created_at, last_heartbeat
             FROM sessions
             WHERE session_key = 'session-heartbeat'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(created_at, seeded_created_at);
        assert!(
            last_heartbeat.is_some_and(|value| value > created_at),
            "turn activity must refresh last_heartbeat beyond the original created_at"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[test]
    fn parse_thread_channel_name_extracts_parent_and_thread_id() {
        let result = parse_thread_channel_name("adk-cc-t1485400795435372796");
        assert_eq!(result, Some(("adk-cc", "1485400795435372796")));
    }

    #[test]
    fn parse_thread_channel_name_with_complex_parent() {
        let result = parse_thread_channel_name("cookingheart-dev-cc-t1485503849761607815");
        assert_eq!(result, Some(("cookingheart-dev-cc", "1485503849761607815")));
    }

    #[test]
    fn parse_thread_channel_name_returns_none_for_regular_channel() {
        assert_eq!(parse_thread_channel_name("adk-cc"), None);
        assert_eq!(parse_thread_channel_name("cookingheart-dev-cc"), None);
    }

    #[test]
    fn parse_thread_channel_name_returns_none_for_short_suffix() {
        // "-t" followed by less than 15 digits is not a thread ID
        assert_eq!(parse_thread_channel_name("test-t123"), None);
    }

    #[test]
    fn parse_thread_channel_id_from_session_key_extracts_thread_id() {
        assert_eq!(
            parse_thread_channel_id_from_session_key(
                "mac-mini:AgentDesk-codex-adk-cdx-t1485506232256168011"
            )
            .as_deref(),
            Some("1485506232256168011")
        );
    }

    #[test]
    fn parse_thread_channel_id_from_session_key_rejects_non_thread_suffix() {
        assert_eq!(
            parse_thread_channel_id_from_session_key("mac-mini:AgentDesk-claude-adk-cc-token-test"),
            None
        );
    }

    #[tokio::test]
    async fn gc_thread_sessions_handler_pg_reports_deleted_legacy_thread_rows() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());

        sqlx::query(
            "INSERT INTO sessions
             (session_key, provider, status, last_heartbeat, created_at)
             VALUES ($1, 'codex', 'idle', NOW() - INTERVAL '2 hours', NOW() - INTERVAL '2 hours')",
        )
        .bind("mac-mini:AgentDesk-codex-adk-cdx-t1490653467734446120")
        .execute(&pool)
        .await
        .unwrap();

        let (status, body) = gc_thread_sessions(State(state)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response_json(body)["gc_threads"], 1);

        let remaining = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sessions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, 0);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn thread_session_pg_resolves_agent_from_parent_channel() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());

        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
             VALUES ('project-agentdesk', 'AgentDesk', 'adk-cc', 'adk-cdx')",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Post session with thread channel name
        let (status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: "mac-mini:AgentDesk-claude-adk-cc-t1485400795435372796".to_string(),
                agent_id: None,
                status: Some("working".to_string()),
                provider: Some("claude".to_string()),
                session_info: Some("thread work".to_string()),
                name: Some("adk-cc-t1485400795435372796".to_string()),
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: None,
                claude_session_id: None,
                thread_channel_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (agent_id, thread_channel_id) = sqlx::query_as::<_, (Option<String>, Option<String>)>(
            "SELECT agent_id, thread_channel_id FROM sessions WHERE session_key = $1",
        )
        .bind("mac-mini:AgentDesk-claude-adk-cc-t1485400795435372796")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(agent_id.as_deref(), Some("project-agentdesk"));
        assert_eq!(thread_channel_id.as_deref(), Some("1485400795435372796"));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn thread_session_pg_resolves_alt_channel_agent_from_session_key_fallback() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());

        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
             VALUES ('project-agentdesk', 'AgentDesk', 'adk-cc', 'adk-cdx')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let session_key = "mac-mini:AgentDesk-codex-adk-cdx-t1485506232256168011";
        let (status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: session_key.to_string(),
                agent_id: None,
                status: Some("working".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("thread work".to_string()),
                name: None,
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: Some("dispatch-1".to_string()),
                claude_session_id: None,
                thread_channel_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (agent_id, thread_channel_id) = sqlx::query_as::<_, (Option<String>, Option<String>)>(
            "SELECT agent_id, thread_channel_id FROM sessions WHERE session_key = $1",
        )
        .bind(session_key)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(agent_id.as_deref(), Some("project-agentdesk"));
        assert_eq!(thread_channel_id.as_deref(), Some("1485506232256168011"));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn direct_session_pg_resolves_agent_from_dispatch_when_tmux_channel_is_truncated() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());
        let long_channel = "project-skillmanager-extremely-verbose-channel-cdx";
        let tmux_name = ProviderKind::Codex.build_tmux_session_name(long_channel);
        let session_key = format!("mac-mini:{tmux_name}");

        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_alt)
             VALUES ('project-skillmanager', 'SkillManager', $1)",
        )
        .bind(long_channel)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, created_at, updated_at)
             VALUES ('card-dispatch-fallback', 'Dispatch Fallback', 'in_progress', NOW(), NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches
             (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
             VALUES ('dispatch-dispatch-fallback', 'card-dispatch-fallback', 'project-skillmanager', 'implementation', 'dispatched', 'Dispatch fallback', NOW(), NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();

        let (status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: session_key.clone(),
                agent_id: None,
                status: Some("working".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("dispatch fallback".to_string()),
                name: None,
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: Some("dispatch-dispatch-fallback".to_string()),
                claude_session_id: None,
                thread_channel_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let agent_id = sqlx::query_scalar::<_, Option<String>>(
            "SELECT agent_id FROM sessions WHERE session_key = $1",
        )
        .bind(&session_key)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(agent_id.as_deref(), Some("project-skillmanager"));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn direct_session_pg_ignores_missing_agent_from_dispatch_fallback() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());
        let long_channel = "project-skillmanager-extremely-verbose-channel-cdx";
        let tmux_name = ProviderKind::Codex.build_tmux_session_name(long_channel);
        let session_key = format!("mac-mini:{tmux_name}");

        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, created_at, updated_at)
             VALUES ('card-missing-dispatch-agent', 'Missing Dispatch Agent', 'in_progress', NOW(), NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches
             (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
             VALUES ('dispatch-missing-dispatch-agent', 'card-missing-dispatch-agent', 'project-missing-agent', 'implementation', 'dispatched', 'Dispatch fallback', NOW(), NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();

        let (status, body) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: session_key.clone(),
                agent_id: None,
                status: Some("working".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("dispatch fallback".to_string()),
                name: None,
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: Some("dispatch-missing-dispatch-agent".to_string()),
                claude_session_id: None,
                thread_channel_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:?}");

        let agent_id = sqlx::query_scalar::<_, Option<String>>(
            "SELECT agent_id FROM sessions WHERE session_key = $1",
        )
        .bind(&session_key)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(agent_id, None);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn direct_session_pg_ignores_explicit_agent_id_without_other_resolution_context() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());
        let tmux_name = ProviderKind::Codex
            .build_tmux_session_name("project-skillmanager-extremely-verbose-channel-cdx");
        let session_key = format!("codex/hash123/mac-mini:{tmux_name}");

        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_alt)
             VALUES ('project-spoofed', 'Spoofed Agent', 'spoofed-channel')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let (status, body) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: session_key.clone(),
                agent_id: Some("project-spoofed".to_string()),
                status: Some("working".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("explicit agent".to_string()),
                name: None,
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: None,
                claude_session_id: None,
                thread_channel_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:?}");

        let agent_id = sqlx::query_scalar::<_, Option<String>>(
            "SELECT agent_id FROM sessions WHERE session_key = $1",
        )
        .bind(&session_key)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(agent_id, None);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn thread_session_pg_resolves_agent_from_thread_id_when_parent_channel_is_truncated() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());
        let thread_id = "1487044675541012490";
        let long_parent_channel = "project-skillmanager-extremely-verbose-channel-cdx";
        let tmux_name = ProviderKind::Codex
            .build_tmux_session_name(&format!("{long_parent_channel}-t{thread_id}"));
        let session_key = format!("mac-mini:{tmux_name}");

        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_alt)
             VALUES ('project-skillmanager', 'SkillManager', $1)",
        )
        .bind(long_parent_channel)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, created_at, updated_at)
             VALUES ('card-thread-fallback', 'Thread Fallback', 'in_progress', NOW(), NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches
             (id, kanban_card_id, to_agent_id, dispatch_type, status, title, thread_id, created_at, updated_at)
             VALUES ('dispatch-thread-fallback', 'card-thread-fallback', 'project-skillmanager', 'implementation', 'dispatched', 'Thread fallback', $1, NOW(), NOW())",
        )
        .bind(thread_id)
        .execute(&pool)
        .await
        .unwrap();

        let (status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: session_key.clone(),
                agent_id: None,
                status: Some("working".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("thread fallback".to_string()),
                name: None,
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: None,
                claude_session_id: None,
                thread_channel_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (agent_id, stored_thread_id) = sqlx::query_as::<_, (Option<String>, Option<String>)>(
            "SELECT agent_id, thread_channel_id FROM sessions WHERE session_key = $1",
        )
        .bind(&session_key)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(agent_id.as_deref(), Some("project-skillmanager"));
        assert_eq!(stored_thread_id.as_deref(), Some(thread_id));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn thread_session_pg_accepts_explicit_thread_channel_id_without_thread_name() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());

        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
             VALUES ('project-agentdesk', 'AgentDesk', 'adk-cc', 'adk-cdx')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let session_key = "mac-mini:AgentDesk-codex-adk-cdx";
        let (status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: session_key.to_string(),
                agent_id: None,
                status: Some("working".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("thread work".to_string()),
                name: Some("adk-cdx".to_string()),
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: None,
                claude_session_id: None,
                thread_channel_id: Some("1485506232256168011".to_string()),
                session_id: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (agent_id, thread_channel_id) = sqlx::query_as::<_, (Option<String>, Option<String>)>(
            "SELECT agent_id, thread_channel_id FROM sessions WHERE session_key = $1",
        )
        .bind(session_key)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(agent_id.as_deref(), Some("project-agentdesk"));
        assert_eq!(thread_channel_id.as_deref(), Some("1485506232256168011"));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn direct_channel_session_pg_keeps_agent_mapping_without_thread_id() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());

        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
             VALUES ('project-agentdesk', 'AgentDesk', 'adk-cc', 'adk-cdx')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let session_key = "mac-mini:AgentDesk-codex-adk-cdx";
        let (status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: session_key.to_string(),
                agent_id: None,
                status: Some("working".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("direct channel work".to_string()),
                name: Some("adk-cdx".to_string()),
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: None,
                claude_session_id: None,
                thread_channel_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (agent_id, thread_channel_id) = sqlx::query_as::<_, (Option<String>, Option<String>)>(
            "SELECT agent_id, thread_channel_id FROM sessions WHERE session_key = $1",
        )
        .bind(session_key)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(agent_id.as_deref(), Some("project-agentdesk"));
        assert_eq!(thread_channel_id, None);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn stale_local_tmux_session_pg_is_filtered_from_active_dispatch_list() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());

        let hostname = crate::services::platform::hostname_short();
        let session_key = format!("{hostname}:AgentDesk-stale-test-{}", std::process::id());

        sqlx::query(
            "INSERT INTO agents (id, name, name_ko, provider, avatar_emoji, status, created_at)
             VALUES ('ch-ad', 'AD', 'AD', 'claude', '🤖', 'idle', NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sessions (session_key, agent_id, provider, status, session_info, active_dispatch_id, last_heartbeat)
             VALUES ($1, 'ch-ad', 'claude', 'turn_active', 'stale session', 'dispatch-stale', NOW())",
        )
        .bind(&session_key)
        .execute(&pool)
        .await
        .unwrap();

        let (status, Json(body)) = list_dispatched_sessions(
            State(state),
            Query(ListDispatchedSessionsQuery {
                include_merged: None,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["sessions"].as_array().unwrap().len(), 0);

        pool.close().await;
        pg_db.drop().await;
    }

    // #1067: sessions_tmux_output tests — watch-agent-turn skill promotion.
    // #1238: Migrated to PG fixtures. `tmux_output` now requires `pg_pool_ref()`
    // — without a populated pool the route returns 500 ("postgres pool unavailable")
    // and the 404 assertion fails.
    #[tokio::test]
    async fn sessions_tmux_output_pg_returns_404_for_unknown_session_id() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let db = test_db();
        let engine = test_engine(&db);
        let mut state = AppState::test_state(db, engine);
        state.pg_pool = Some(pool.clone());

        let (status, body) = tmux_output(
            State(state),
            Path(999_999),
            Query(TmuxOutputQuery { lines: None }),
        )
        .await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        let body: Value = response_json(body);
        assert_eq!(body["session_id"], 999_999);
        assert!(
            body["error"]
                .as_str()
                .map(|s| s.contains("not found"))
                .unwrap_or(false)
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn sessions_tmux_output_pg_shape_for_seeded_session_without_live_tmux() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let tmux_name = format!("AgentDesk-codex-1067-output-{}", std::process::id());
        let session_key = format!("mac-mini:{tmux_name}");

        seed_agent_pg(&pool, "agent-1067").await;
        sqlx::query(
            "INSERT INTO sessions
             (session_key, agent_id, provider, status, last_heartbeat, created_at)
             VALUES ($1, 'agent-1067', 'codex', 'turn_active', NOW(), NOW())",
        )
        .bind(&session_key)
        .execute(&pool)
        .await
        .unwrap();
        let session_id =
            sqlx::query_scalar::<_, i64>("SELECT id FROM sessions WHERE session_key = $1")
                .bind(&session_key)
                .fetch_one(&pool)
                .await
                .unwrap();
        let state = AppState::test_state_with_pg(db, engine, pool.clone());

        let (status, body) = tmux_output(
            State(state),
            Path(session_id),
            Query(TmuxOutputQuery { lines: Some(20) }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let body: Value = response_json(body);
        assert_eq!(body["session_id"], session_id);
        assert_eq!(body["session_key"], session_key);
        assert_eq!(body["tmux_name"], tmux_name);
        assert_eq!(body["agent_id"], "agent-1067");
        assert_eq!(body["provider"], "codex");
        assert_eq!(body["status"], "turn_active");
        assert_eq!(body["lines_requested"], 20);
        assert_eq!(body["lines_effective"], 20);
        // tmux session was never created, so capture_pane returns None → empty string + alive=false.
        assert_eq!(body["tmux_alive"], false);
        assert_eq!(body["recent_output"], "");
        assert!(body["captured_at_ms"].as_i64().unwrap() > 0);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn sessions_tmux_output_pg_clamps_lines_to_allowed_range() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let session_key = format!("mac-mini:AgentDesk-codex-1067-clamp-{}", std::process::id());

        seed_agent_pg(&pool, "agent-1067-clamp").await;
        sqlx::query(
            "INSERT INTO sessions
             (session_key, agent_id, provider, status, last_heartbeat, created_at)
             VALUES ($1, 'agent-1067-clamp', 'codex', 'idle', NOW(), NOW())",
        )
        .bind(&session_key)
        .execute(&pool)
        .await
        .unwrap();
        let session_id =
            sqlx::query_scalar::<_, i64>("SELECT id FROM sessions WHERE session_key = $1")
                .bind(&session_key)
                .fetch_one(&pool)
                .await
                .unwrap();
        let state = AppState::test_state_with_pg(db, engine, pool.clone());

        let (status_hi, body_hi) = tmux_output(
            State(state.clone()),
            Path(session_id),
            Query(TmuxOutputQuery { lines: Some(9_999) }),
        )
        .await;
        assert_eq!(status_hi, StatusCode::OK);
        let body_hi: Value = response_json(body_hi);
        assert_eq!(body_hi["lines_requested"], 9_999);
        assert_eq!(body_hi["lines_effective"], 2_000);

        let (status_lo, body_lo) = tmux_output(
            State(state),
            Path(session_id),
            Query(TmuxOutputQuery { lines: Some(-42) }),
        )
        .await;
        assert_eq!(status_lo, StatusCode::OK);
        let body_lo: Value = response_json(body_lo);
        assert_eq!(body_lo["lines_requested"], -42);
        assert_eq!(body_lo["lines_effective"], 1);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn sessions_tmux_output_pg_rejects_malformed_session_key() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        // session_key without ":" — conflicts with hostname:tmux_name format.
        let bad_session_key = "no-colon-here".to_string();

        seed_agent_pg(&pool, "agent-1067-bad").await;
        sqlx::query(
            "INSERT INTO sessions
             (session_key, agent_id, provider, status, last_heartbeat, created_at)
             VALUES ($1, 'agent-1067-bad', 'codex', 'idle', NOW(), NOW())",
        )
        .bind(&bad_session_key)
        .execute(&pool)
        .await
        .unwrap();
        let session_id =
            sqlx::query_scalar::<_, i64>("SELECT id FROM sessions WHERE session_key = $1")
                .bind(&bad_session_key)
                .fetch_one(&pool)
                .await
                .unwrap();
        let state = AppState::test_state_with_pg(db, engine, pool.clone());

        let (status, body) = tmux_output(
            State(state),
            Path(session_id),
            Query(TmuxOutputQuery { lines: None }),
        )
        .await;

        assert_eq!(status, StatusCode::CONFLICT);
        let body: Value = response_json(body);
        assert_eq!(body["session_id"], session_id);
        assert_eq!(body["session_key"], bad_session_key);

        pool.close().await;
        pg_db.drop().await;
    }
}
