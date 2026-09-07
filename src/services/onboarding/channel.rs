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

use super::{ChannelMapping, CompleteBody, load_onboarding_config, pg_kv_value};

pub(super) fn apply_runtime_config_defaults(state: &AppState, body: &CompleteBody) -> CompleteBody {
    let live = crate::config_live_reload::current();
    apply_config_defaults(live.as_deref().unwrap_or(&state.config), body)
}

pub(super) fn apply_config_defaults(
    config: &crate::config::Config,
    body: &CompleteBody,
) -> CompleteBody {
    let mut body = body.clone();
    if body.guild_id.trim().is_empty() {
        body.guild_id = config
            .onboarding
            .effective_guild_id(&config.discord)
            .unwrap_or_default()
            .trim()
            .to_string();
    }
    if body.provider.is_none() {
        body.provider = config
            .onboarding
            .effective_default_provider()
            .map(|provider| provider.as_str().to_string());
    }
    for mapping in &mut body.channels {
        if let Some(category) = &mapping.category {
            mapping.category = config
                .onboarding
                .resolve_category(category)
                .or_else(|| Some(category.clone()));
        }
    }
    body
}

pub(super) fn create_payload(channel_name: &str, mapping: &ChannelMapping) -> serde_json::Value {
    let mut payload = json!({"name": channel_name, "type": 0});
    if let Some(topic) = mapping
        .description
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        payload["topic"] = json!(topic.chars().take(1024).collect::<String>());
    }
    if let Some(category) = &mapping.category {
        payload["parent_id"] = json!(category);
    }
    payload
}

pub(super) fn validate_category(mapping: &ChannelMapping) -> Result<(), String> {
    if let Some(category) = &mapping.category {
        if !super::is_discord_channel_id(category) {
            return Err(format!(
                "unknown or invalid category {category:?} for agent {:?}; use an onboarding.default_categories label or Discord category ID",
                mapping.role_id
            ));
        }
    }
    Ok(())
}

/// Categories participate in retry identity only when requested, so existing
/// uncategorized completion checkpoints keep their historical fingerprint.
pub(super) fn fingerprint_categories(payload: &mut serde_json::Value, channels: &[ChannelMapping]) {
    let categories: std::collections::BTreeMap<_, _> = channels
        .iter()
        .filter_map(|mapping| {
            mapping
                .category
                .as_ref()
                .map(|category| (&mapping.role_id, category))
        })
        .collect();
    if !categories.is_empty() {
        payload["categories"] = json!(categories);
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> CompleteBody {
        serde_json::from_value(json!({
            "token": "test-token", "channels": [{
                "channel_id": "", "channel_name": "dev-gem", "role_id": "dev",
                "description": "test topic", "category": "dev"
            }]
        }))
        .unwrap()
    }

    fn config() -> crate::config::Config {
        serde_yaml::from_str("server: {}\ndiscord: {guild_id: '111'}\nonboarding:\n  guild_id: '222'\n  default_provider: gemini\n  default_categories: {dev: '333'}\n").unwrap()
    }

    #[test]
    fn onboarding_routing_defaults_and_category_retry_identity() {
        let config = config();
        let body = apply_config_defaults(&config, &request());
        assert_eq!(body.guild_id, "222");
        assert_eq!(body.provider.as_deref(), Some("gemini"));
        assert_eq!(body.channels[0].category.as_deref(), Some("333"));
        let first = super::super::requested_channel_fingerprint(&body, "gemini").unwrap();
        let mut other = body.clone();
        other.channels[0].category = Some("444".into());
        assert_ne!(
            first,
            super::super::requested_channel_fingerprint(&other, "gemini").unwrap()
        );
        other.guild_id = "555".into();
        other.provider = Some("claude".into());
        let explicit = apply_config_defaults(&config, &other);
        assert_eq!(explicit.guild_id, "555");
        assert_eq!(explicit.provider.as_deref(), Some("claude"));
        other.channels[0].category = Some("missing".into());
        assert!(
            super::super::requested_channel_fingerprint(
                &apply_config_defaults(&config, &other),
                "claude"
            )
            .unwrap_err()
            .contains("invalid category")
        );
        let mut fallback = config.clone();
        fallback.onboarding.guild_id = None;
        assert_eq!(apply_config_defaults(&fallback, &request()).guild_id, "111");
        let mut plain = body.channels[0].clone();
        plain.category = None;
        assert_eq!(
            create_payload("dev-gem", &plain),
            json!({"name": "dev-gem", "type": 0, "topic": "test topic"})
        );
        let mut fingerprint = json!({"channels": []});
        fingerprint_categories(&mut fingerprint, &[plain]);
        assert_eq!(fingerprint, json!({"channels": []}));
    }

    #[tokio::test]
    async fn onboarding_routing_channel_creation_sends_configured_guild_and_category() {
        use axum::{Router, routing::get};
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let app = Router::new().route(
            "/guilds/222/channels",
            get(|| async {
                // Same name in another category must not swallow the requested placement.
                Json(json!([{"id": "777", "name": "dev-gem", "type": 0, "parent_id": "999"}]))
            })
            .post(move |Json(payload): Json<serde_json::Value>| async move {
                tx.send(payload).await.unwrap();
                Json(json!({"id": "888", "name": "dev-gem"}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let body = apply_config_defaults(&config(), &request());
        let result = super::super::resolve_channel_mapping(
            &reqwest::Client::new(),
            &body.token,
            &format!("http://{addr}"),
            &body.guild_id,
            &body.channels[0],
            None,
        )
        .await
        .unwrap();
        assert!(result.created);
        assert_eq!(result.channel_id, "888");
        let payload = rx.recv().await.unwrap();
        assert_eq!(
            payload,
            json!({"name": "dev-gem", "type": 0, "topic": "test topic", "parent_id": "333"})
        );
        server.abort();
    }
}
