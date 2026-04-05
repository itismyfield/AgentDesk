use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;

use super::AppState;

// ── Handlers ───────────────────────────────────────────────────

/// GET /api/discord-bindings
pub async fn list_bindings(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let mut stmt = match conn.prepare(
        "SELECT id, discord_channel_id, discord_channel_alt
         FROM agents
         WHERE discord_channel_id IS NOT NULL
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
            Ok(json!({
                "agentId": row.get::<_, String>(0)?,
                "channelId": row.get::<_, String>(1)?,
                "channelName": row.get::<_, Option<String>>(2)?,
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
