//! Discord token / guild / channel discovery handlers for the onboarding flow.
//!
//! Extracted from the historical monolithic `onboarding.rs`. These handlers
//! depend on two helpers from the parent module (`pg_kv_value` and
//! `load_onboarding_config`) which are exposed as `pub(super)` so this
//! submodule can use them without changing their external visibility.

use axum::{Json, http::StatusCode};
use serde::Deserialize;
use serde_json::json;

use crate::app_state::AppState;
use crate::error::{AppError, AppResult};

use super::{load_onboarding_config, pg_kv_value};

#[derive(Debug, Deserialize)]
pub struct ValidateTokenBody {
    pub token: String,
}

/// POST /api/onboarding/validate-token
/// Validates a Discord bot token and returns bot info.
pub async fn validate_token(body: ValidateTokenBody) -> (StatusCode, Json<serde_json::Value>) {
    let client = reqwest::Client::new();
    let resp = client
        .get("https://discord.com/api/v10/users/@me")
        .header("Authorization", format!("Bot {}", body.token))
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let user: serde_json::Value = r.json().await.unwrap_or(json!({}));
            (
                StatusCode::OK,
                Json(json!({
                    "valid": true,
                    "bot_id": user.get("id").and_then(|v| v.as_str()),
                    "bot_name": user.get("username").and_then(|v| v.as_str()),
                    "avatar": user.get("avatar").and_then(|v| v.as_str()),
                })),
            )
        }
        Ok(r) => {
            let status = r.status();
            (
                StatusCode::OK,
                Json(json!({
                    "valid": false,
                    "error": format!("Discord API error: {status}"),
                })),
            )
        }
        Err(e) => (
            StatusCode::OK,
            Json(json!({
                "valid": false,
                "error": format!("Request failed: {e}"),
            })),
        ),
    }
}

#[derive(Debug, Deserialize)]
pub struct ChannelsQuery {
    pub token: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ChannelsBody {
    pub token: Option<String>,
}

async fn load_channels(
    state: &AppState,
    token: Option<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    // Use provided token or saved token
    let token = match token {
        Some(token) => Some(token),
        None if state.pg_pool_ref().is_some() => {
            match pg_kv_value(
                state.pg_pool_ref().expect("checked pg_pool_ref"),
                "onboarding_bot_token",
            )
            .await
            {
                Ok(token) => token,
                Err(error) => return Err(AppError::internal(error)),
            }
        }
        None => saved_onboarding_bot_token_without_pg(state),
    };

    let Some(token) = token else {
        return Err(AppError::bad_request("No token provided"));
    };

    let client = reqwest::Client::new();

    // Fetch guilds
    let guilds: Vec<serde_json::Value> = match client
        .get("https://discord.com/api/v10/users/@me/guilds")
        .header("Authorization", format!("Bot {}", token))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
        _ => {
            return Ok((
                StatusCode::OK,
                Json(json!({"guilds": [], "error": "Failed to fetch guilds"})),
            ));
        }
    };

    let mut result_guilds = Vec::new();
    for guild in &guilds {
        let guild_id = guild.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let guild_name = guild.get("name").and_then(|v| v.as_str()).unwrap_or("");

        // Fetch channels for this guild
        let channels: Vec<serde_json::Value> = match client
            .get(format!(
                "https://discord.com/api/v10/guilds/{guild_id}/channels"
            ))
            .header("Authorization", format!("Bot {}", token))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
            _ => Vec::new(),
        };

        // Filter text channels (type 0)
        let text_channels: Vec<serde_json::Value> = channels
            .into_iter()
            .filter(|c| c.get("type").and_then(|v| v.as_i64()) == Some(0))
            .map(|c| {
                let parent = c
                    .get("parent_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                json!({
                    "id": c.get("id").and_then(|v| v.as_str()),
                    "name": c.get("name").and_then(|v| v.as_str()),
                    "category_id": parent,
                })
            })
            .collect();

        result_guilds.push(json!({
            "id": guild_id,
            "name": guild_name,
            "channels": text_channels,
        }));
    }

    Ok((StatusCode::OK, Json(json!({"guilds": result_guilds}))))
}

fn saved_onboarding_bot_token_without_pg(_state: &AppState) -> Option<String> {
    crate::cli::agentdesk_runtime_root()
        .as_ref()
        .and_then(|root| load_onboarding_config(root).ok())
        .and_then(|config| {
            config
                .discord
                .bots
                .get("command")
                .and_then(|bot| bot.token.clone())
        })
}

/// GET /api/onboarding/channels
/// Fetches Discord guilds + text channels for the given bot token.
pub async fn channels(
    state: &AppState,
    query: ChannelsQuery,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    load_channels(state, query.token).await
}

/// POST /api/onboarding/channels
/// Fetches Discord guilds + text channels for the given bot token from request body.
pub async fn channels_post(
    state: &AppState,
    body: ChannelsBody,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    load_channels(state, body.token).await
}
