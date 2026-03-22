use axum::{Json, extract::State, http::StatusCode};
use serde::Deserialize;
use serde_json::json;

use super::AppState;

/// GET /api/onboarding/status
/// Returns whether onboarding is complete + existing config values.
pub async fn status(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": format!("{e}")}))),
    };

    // Check if bot_settings exists (indicates onboarding was done)
    let has_bots: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM agents",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);

    // Get existing config
    let bot_token: Option<String> = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = 'onboarding_bot_token'",
            [],
            |row| row.get(0),
        )
        .ok();

    let guild_id: Option<String> = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = 'onboarding_guild_id'",
            [],
            |row| row.get(0),
        )
        .ok();

    let owner_id: Option<String> = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = 'onboarding_owner_id'",
            [],
            |row| row.get(0),
        )
        .ok();

    let agent_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM agents", [], |row| row.get(0))
        .unwrap_or(0);

    // Get channel mappings from agents table
    let mut stmt = conn
        .prepare("SELECT id, name, discord_channel_id FROM agents ORDER BY id")
        .unwrap();
    let agents: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            Ok(json!({
                "agent_id": row.get::<_, String>(0)?,
                "name": row.get::<_, Option<String>>(1)?,
                "channel_id": row.get::<_, Option<String>>(2)?,
            }))
        })
        .ok()
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();

    (StatusCode::OK, Json(json!({
        "completed": has_bots && agent_count > 0,
        "agent_count": agent_count,
        "bot_token_set": bot_token.is_some(),
        "guild_id": guild_id,
        "owner_id": owner_id,
        "agents": agents,
    })))
}

#[derive(Debug, Deserialize)]
pub struct ValidateTokenBody {
    pub token: String,
}

/// POST /api/onboarding/validate-token
/// Validates a Discord bot token and returns bot info.
pub async fn validate_token(
    Json(body): Json<ValidateTokenBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let client = reqwest::Client::new();
    let resp = client
        .get("https://discord.com/api/v10/users/@me")
        .header("Authorization", format!("Bot {}", body.token))
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let user: serde_json::Value = r.json().await.unwrap_or(json!({}));
            (StatusCode::OK, Json(json!({
                "valid": true,
                "bot_id": user.get("id").and_then(|v| v.as_str()),
                "bot_name": user.get("username").and_then(|v| v.as_str()),
                "avatar": user.get("avatar").and_then(|v| v.as_str()),
            })))
        }
        Ok(r) => {
            let status = r.status();
            (StatusCode::OK, Json(json!({
                "valid": false,
                "error": format!("Discord API error: {status}"),
            })))
        }
        Err(e) => {
            (StatusCode::OK, Json(json!({
                "valid": false,
                "error": format!("Request failed: {e}"),
            })))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ChannelsQuery {
    pub token: Option<String>,
}

/// GET /api/onboarding/channels
/// Fetches Discord guilds + text channels for the given bot token.
pub async fn channels(
    State(state): State<AppState>,
    axum::extract::Query(query): axum::extract::Query<ChannelsQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Use provided token or saved token
    let token = query.token.or_else(|| {
        state.db.lock().ok().and_then(|conn| {
            conn.query_row(
                "SELECT value FROM kv_meta WHERE key = 'onboarding_bot_token'",
                [],
                |row| row.get(0),
            )
            .ok()
        })
    });

    let Some(token) = token else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "No token provided"})));
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
        _ => return (StatusCode::OK, Json(json!({"guilds": [], "error": "Failed to fetch guilds"}))),
    };

    let mut result_guilds = Vec::new();
    for guild in &guilds {
        let guild_id = guild.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let guild_name = guild.get("name").and_then(|v| v.as_str()).unwrap_or("");

        // Fetch channels for this guild
        let channels: Vec<serde_json::Value> = match client
            .get(format!("https://discord.com/api/v10/guilds/{guild_id}/channels"))
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
                let parent = c.get("parent_id").and_then(|v| v.as_str()).map(|s| s.to_string());
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

    (StatusCode::OK, Json(json!({"guilds": result_guilds})))
}

#[derive(Debug, Deserialize)]
pub struct CompleteBody {
    pub token: String,
    pub guild_id: String,
    pub owner_id: Option<String>,
    pub provider: Option<String>,
    pub channels: Vec<ChannelMapping>,
}

#[derive(Debug, Deserialize)]
pub struct ChannelMapping {
    pub channel_id: String,
    pub channel_name: String,
    pub role_id: String,
}

/// POST /api/onboarding/complete
/// Saves onboarding configuration and sets up agents.
pub async fn complete(
    State(state): State<AppState>,
    Json(body): Json<CompleteBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": format!("{e}")}))),
    };

    let provider = body.provider.as_deref().unwrap_or("claude");

    // Save onboarding metadata
    conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('onboarding_bot_token', ?1)",
        [&body.token],
    ).ok();
    conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('onboarding_guild_id', ?1)",
        [&body.guild_id],
    ).ok();
    if let Some(ref owner) = body.owner_id {
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('onboarding_owner_id', ?1)",
            [owner],
        ).ok();
    }

    // Create/update agents for each channel mapping
    let mut created = 0;
    for mapping in &body.channels {
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, status, xp) \
             VALUES (?1, ?2, ?3, 'active', 0) \
             ON CONFLICT(id) DO UPDATE SET \
               name = COALESCE(excluded.name, agents.name), \
               discord_channel_id = excluded.discord_channel_id",
            rusqlite::params![mapping.role_id, mapping.role_id, mapping.channel_id],
        ).ok();
        created += 1;
    }

    // Generate role_map.json
    let root = crate::cli::agentdesk_runtime_root();
    if let Some(root) = root {
        let config_dir = root.join("config");
        std::fs::create_dir_all(&config_dir).ok();

        let mut by_channel_id = serde_json::Map::new();
        let mut by_channel_name = serde_json::Map::new();

        for mapping in &body.channels {
            by_channel_id.insert(
                mapping.channel_id.clone(),
                json!({
                    "roleId": mapping.role_id,
                    "provider": provider,
                }),
            );
            by_channel_name.insert(
                mapping.channel_name.clone(),
                json!({
                    "roleId": mapping.role_id,
                    "channelId": mapping.channel_id,
                }),
            );
        }

        let role_map = json!({
            "version": 1,
            "byChannelId": by_channel_id,
            "byChannelName": by_channel_name,
        });

        let role_map_path = config_dir.join("role_map.json");
        if let Ok(json_str) = serde_json::to_string_pretty(&role_map) {
            std::fs::write(&role_map_path, json_str).ok();
        }
    }

    // Mark onboarding complete
    conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('onboarding_complete', 'true')",
        [],
    ).ok();
    drop(conn);

    // Write bot token to agentdesk.yaml so dcserver can use it
    if let Some(root) = crate::cli::agentdesk_runtime_root() {
        let yaml_path = root.join("agentdesk.yaml");
        let mut yaml_content = std::fs::read_to_string(&yaml_path).unwrap_or_default();

        // Update or add bot token in discord.bots section
        if yaml_content.contains("command:") {
            // Replace existing command bot token
            if let Some(start) = yaml_content.find("command:") {
                if let Some(token_line) = yaml_content[start..].find("token:") {
                    let abs_pos = start + token_line;
                    if let Some(end) = yaml_content[abs_pos..].find('\n') {
                        let old_line = &yaml_content[abs_pos..abs_pos + end];
                        let new_line = format!("token: \"{}\"", body.token);
                        yaml_content = yaml_content.replace(old_line, &new_line);
                    }
                }
            }
        } else {
            // Append minimal discord config
            yaml_content.push_str(&format!(
                "\ndiscord:\n  bots:\n    command:\n      bot_id: \"{}\"\n      token: \"{}\"\n",
                "", body.token
            ));
        }

        std::fs::write(&yaml_path, &yaml_content).ok();
    }

    (StatusCode::OK, Json(json!({
        "ok": true,
        "agents_created": created,
        "provider": provider,
    })))
}
