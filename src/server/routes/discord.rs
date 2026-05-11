use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use poise::serenity_prelude::ChannelId;
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::db::agents::load_all_agent_channel_bindings_pg;

// ── Handlers ───────────────────────────────────────────────────

/// GET /api/discord/bindings
///
/// Reads agent channel bindings from Postgres.
pub async fn list_bindings(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        return list_bindings_pg(pool).await;
    }

    (StatusCode::OK, Json(json!({"bindings": []})))
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

/// Parse a channel id string into a `ChannelId`. Returns 400 if it isn't a
/// valid u64.
fn parse_channel_id(raw: &str) -> Result<ChannelId, (StatusCode, Json<serde_json::Value>)> {
    raw.parse::<u64>().map(ChannelId::new).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid channel id"})),
        )
    })
}

/// Issue #2047 Finding 5 — confused-deputy fix. The proxy uses the announce
/// bot token which is a member of *many* channels. Without an authorisation
/// check the dashboard would happily read any channel the bot can see. Limit
/// the proxy to channels that are registered in the agentdesk role-map.
///
/// We accept any binding the resolver returns (`agentdesk_config`,
/// `org_schema`, or `role_map.json`) — the goal is "is this channel known to
/// the operator?" not "which agent owns it?". Threads inherit the parent's
/// binding via the resolver's parent walk where applicable.
async fn ensure_channel_is_role_mapped(
    channel_id: ChannelId,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    use crate::services::discord::resolve_channel_role_binding as resolve_role_binding;

    // First pass: try without a channel name (fast path for `byChannelId`
    // entries). `byChannelName` fallback requires the channel name so we
    // fetch it from Discord when the cheap lookup misses — same trade-off as
    // the `/api/discord/send` handler.
    if resolve_role_binding(channel_id, None).is_some() {
        return Ok(());
    }

    let token = match crate::credential::read_bot_token("announce") {
        Some(token) => token,
        None => {
            // Without a bot token we can't fetch the channel name; behave as
            // a hard deny rather than open the proxy by accident.
            return Err((
                StatusCode::FORBIDDEN,
                Json(json!({"error": "channel not in role-map"})),
            ));
        }
    };

    let url = format!(
        "https://discord.com/api/v10/channels/{}",
        channel_id.get()
    );
    let channel_name = match reqwest::Client::new()
        .get(&url)
        .header("Authorization", format!("Bot {token}"))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => resp
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|payload| {
                payload
                    .get("name")
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
            }),
        _ => None,
    };

    if resolve_role_binding(channel_id, channel_name.as_deref()).is_some() {
        return Ok(());
    }

    Err((
        StatusCode::FORBIDDEN,
        Json(json!({"error": "channel not in role-map"})),
    ))
}

/// GET /api/discord/channels/:id/messages
///
/// Proxy to Discord REST API — read recent messages from a channel or thread.
pub async fn channel_messages(
    Path(channel_id_raw): Path<String>,
    Query(params): Query<MessagesQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let channel_id = match parse_channel_id(&channel_id_raw) {
        Ok(id) => id,
        Err(err) => return err,
    };

    if let Err(err) = ensure_channel_is_role_mapped(channel_id).await {
        return err;
    }

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

    // Issue #2047 Finding 12 — build the query with `Client::query` so values
    // are URL-encoded and cannot inject extra parameters.
    let mut query_params: Vec<(&str, String)> = vec![("limit", limit.to_string())];
    if let Some(before) = params.before.as_ref().and_then(snowflake_or_none) {
        query_params.push(("before", before));
    }
    if let Some(after) = params.after.as_ref().and_then(snowflake_or_none) {
        query_params.push(("after", after));
    }

    let url = format!(
        "https://discord.com/api/v10/channels/{}/messages",
        channel_id.get()
    );

    match reqwest::Client::new()
        .get(&url)
        .header("Authorization", format!("Bot {token}"))
        .query(&query_params)
        .send()
        .await
    {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(data) => (StatusCode::OK, Json(json!({"messages": data}))),
            Err(_) => (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": "discord response decode failed"})),
            ),
        },
        Err(_) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": "discord request failed"})),
        ),
    }
}

/// Snowflake validator — Discord IDs are decimal u64. Anything else is
/// dropped so a caller cannot smuggle extra `&key=value` segments through the
/// `before` / `after` parameters.
fn snowflake_or_none(value: &String) -> Option<String> {
    let trimmed = value.trim();
    if !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_digit()) {
        Some(trimmed.to_string())
    } else {
        None
    }
}

/// GET /api/discord/channels/:id
///
/// Proxy to Discord REST API — get channel/thread info.
pub async fn channel_info(
    Path(channel_id_raw): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let channel_id = match parse_channel_id(&channel_id_raw) {
        Ok(id) => id,
        Err(err) => return err,
    };

    if let Err(err) = ensure_channel_is_role_mapped(channel_id).await {
        return err;
    }

    let token = match crate::credential::read_bot_token("announce") {
        Some(t) => t,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "announce bot token not found"})),
            );
        }
    };

    let url = format!(
        "https://discord.com/api/v10/channels/{}",
        channel_id.get()
    );
    match reqwest::Client::new()
        .get(&url)
        .header("Authorization", format!("Bot {token}"))
        .send()
        .await
    {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(data) => (StatusCode::OK, Json(data)),
            Err(_) => (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": "discord response decode failed"})),
            ),
        },
        Err(_) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": "discord request failed"})),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snowflake_or_none_accepts_digits_only() {
        assert_eq!(
            snowflake_or_none(&"1234567890".to_string()),
            Some("1234567890".to_string())
        );
        assert_eq!(
            snowflake_or_none(&"  555  ".to_string()),
            Some("555".to_string())
        );
    }

    #[test]
    fn snowflake_or_none_rejects_injection_attempts() {
        assert_eq!(snowflake_or_none(&"123&malicious=1".to_string()), None);
        assert_eq!(snowflake_or_none(&"12 OR 1=1".to_string()), None);
        assert_eq!(snowflake_or_none(&"123abc".to_string()), None);
        assert_eq!(snowflake_or_none(&"".to_string()), None);
    }

    #[test]
    fn parse_channel_id_validates_u64() {
        // ChannelId::new(0) panics in serenity, so we don't exercise the
        // zero case — Discord never issues 0 snowflakes anyway.
        assert!(parse_channel_id("1234567890").is_ok());
        assert!(parse_channel_id("not-a-number").is_err());
        assert!(parse_channel_id("-1").is_err());
        assert!(parse_channel_id("").is_err());
    }
}
