use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::db::agents::{AgentChannelBindings, load_all_agent_channel_bindings_pg};

// ── Handlers ───────────────────────────────────────────────────

/// GET /api/discord/bindings
/// (Legacy alias: /api/discord-bindings — kept for backward-compat, deprecated via #1065.)
///
/// Reads agent channel bindings, preferring Postgres when available
/// (codex P2 round 2 on #1306). Falls back to the legacy `Db` for older
/// deployments still on SQLite. #1238 will retire the SQLite branch.
pub async fn list_bindings(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        return list_bindings_pg(pool).await;
    }
    let Some(legacy_db) = state.legacy_db().or_else(|| state.engine.legacy_db()) else {
        return (StatusCode::OK, Json(json!({"bindings": []})));
    };
    let conn = match legacy_db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let mut stmt = match conn.prepare(
        "SELECT id, provider, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
         FROM agents
         WHERE discord_channel_id IS NOT NULL
            OR discord_channel_alt IS NOT NULL
            OR discord_channel_cc IS NOT NULL
            OR discord_channel_cdx IS NOT NULL
         ORDER BY id",
    ) {
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
            let bindings = AgentChannelBindings {
                provider: row.get(1)?,
                discord_channel_id: row.get(2)?,
                discord_channel_alt: row.get(3)?,
                discord_channel_cc: row.get(4)?,
                discord_channel_cdx: row.get(5)?,
            };
            Ok(json!({
                "agentId": row.get::<_, String>(0)?,
                "channelId": bindings.primary_channel(),
                "counterModelChannelId": bindings.counter_model_channel(),
                "provider": bindings.provider,
                "discord_channel_id": bindings.discord_channel_id,
                "discord_channel_alt": bindings.discord_channel_alt,
                "discord_channel_cc": bindings.discord_channel_cc,
                "discord_channel_cdx": bindings.discord_channel_cdx,
                "source": "config",
            }))
        })
        .ok();

    let bindings: Vec<serde_json::Value> = match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect(),
        None => Vec::new(),
    };

    (StatusCode::OK, Json(json!({"bindings": bindings})))
}

async fn list_bindings_pg(pool: &sqlx::PgPool) -> (StatusCode, Json<serde_json::Value>) {
    let map = match load_all_agent_channel_bindings_pg(pool).await {
        Ok(m) => m,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("pg query failed: {e}")})),
            );
        }
    };

    let bindings: Vec<serde_json::Value> = map
        .into_iter()
        .filter(|(_, b)| {
            b.discord_channel_id.is_some()
                || b.discord_channel_alt.is_some()
                || b.discord_channel_cc.is_some()
                || b.discord_channel_cdx.is_some()
        })
        .map(|(agent_id, b)| {
            json!({
                "agentId": agent_id,
                "channelId": b.primary_channel(),
                "counterModelChannelId": b.counter_model_channel(),
                "provider": b.provider,
                "discord_channel_id": b.discord_channel_id,
                "discord_channel_alt": b.discord_channel_alt,
                "discord_channel_cc": b.discord_channel_cc,
                "discord_channel_cdx": b.discord_channel_cdx,
                "source": "config",
            })
        })
        .collect();

    (StatusCode::OK, Json(json!({"bindings": bindings})))
}

// ── Discord proxy APIs ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct MessagesQuery {
    pub limit: Option<u32>,
    pub before: Option<String>,
    pub after: Option<String>,
}

/// GET /api/discord/channels/:id/messages
///
/// Proxy to Discord REST API — read recent messages from a channel or thread.
pub async fn channel_messages(
    Path(channel_id): Path<String>,
    Query(params): Query<MessagesQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let token = match crate::credential::read_bot_token("announce") {
        Some(t) => t,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "announce bot token not found"})),
            );
        }
    };

    let limit = params.limit.unwrap_or(10).min(100);
    let mut url =
        format!("https://discord.com/api/v10/channels/{channel_id}/messages?limit={limit}");
    if let Some(ref before) = params.before {
        url.push_str(&format!("&before={before}"));
    }
    if let Some(ref after) = params.after {
        url.push_str(&format!("&after={after}"));
    }

    match reqwest::Client::new()
        .get(&url)
        .header("Authorization", format!("Bot {token}"))
        .send()
        .await
    {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(data) => (StatusCode::OK, Json(json!({"messages": data}))),
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("parse error: {e}")})),
            ),
        },
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("discord request failed: {e}")})),
        ),
    }
}

/// GET /api/discord/channels/:id
///
/// Proxy to Discord REST API — get channel/thread info.
pub async fn channel_info(Path(channel_id): Path<String>) -> (StatusCode, Json<serde_json::Value>) {
    let token = match crate::credential::read_bot_token("announce") {
        Some(t) => t,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "announce bot token not found"})),
            );
        }
    };

    let url = format!("https://discord.com/api/v10/channels/{channel_id}");
    match reqwest::Client::new()
        .get(&url)
        .header("Authorization", format!("Bot {token}"))
        .send()
        .await
    {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(data) => (StatusCode::OK, Json(data)),
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("parse error: {e}")})),
            ),
        },
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("discord request failed: {e}")})),
        ),
    }
}
