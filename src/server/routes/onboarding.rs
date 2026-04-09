use std::path::Path;

use axum::{Json, extract::State, http::StatusCode};
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::services::provider::ProviderKind;
use crate::services::provider_exec;

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";

/// GET /api/onboarding/status
/// Returns whether onboarding is complete + existing config values.
pub async fn status(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    // Check whether onboarding created any agents yet.
    let has_bots: bool = conn
        .query_row("SELECT COUNT(*) > 0 FROM agents", [], |row| row.get(0))
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

    // Load all bot tokens for pre-fill
    let announce_token: Option<String> = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = 'onboarding_announce_token'",
            [],
            |row| row.get(0),
        )
        .ok();
    let notify_token: Option<String> = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = 'onboarding_notify_token'",
            [],
            |row| row.get(0),
        )
        .ok();
    let command_token_2: Option<String> = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = 'onboarding_command_token_2'",
            [],
            |row| row.get(0),
        )
        .ok();
    let primary_provider: Option<String> = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = 'onboarding_provider'",
            [],
            |row| row.get(0),
        )
        .ok();
    let command_provider_2: Option<String> = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = 'onboarding_command_provider_2'",
            [],
            |row| row.get(0),
        )
        .ok();

    let completed = has_bots && agent_count > 0;

    // Mask tokens after onboarding is complete to prevent unauthenticated leakage.
    // Only show full tokens during initial setup (before completion).
    let mask = |t: Option<String>| -> Option<String> {
        if !completed {
            return t;
        }
        t.map(|s| {
            if s.len() > 8 {
                format!("{}…{}", &s[..4], &s[s.len() - 4..])
            } else {
                "***".to_string()
            }
        })
    };

    (
        StatusCode::OK,
        Json(json!({
            "completed": completed,
            "agent_count": agent_count,
            "bot_tokens": {
                "command": mask(bot_token),
                "announce": mask(announce_token),
                "notify": mask(notify_token),
                "command2": mask(command_token_2),
            },
            "bot_providers": {
                "command": primary_provider,
                "command2": command_provider_2,
            },
            "guild_id": guild_id,
            "owner_id": owner_id,
            "agents": agents,
        })),
    )
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
    State(state): State<AppState>,
    token: Option<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Use provided token or saved token
    let token = token.or_else(|| {
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
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "No token provided"})),
        );
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
            return (
                StatusCode::OK,
                Json(json!({"guilds": [], "error": "Failed to fetch guilds"})),
            );
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

    (StatusCode::OK, Json(json!({"guilds": result_guilds})))
}

/// GET /api/onboarding/channels
/// Fetches Discord guilds + text channels for the given bot token.
pub async fn channels(
    state: State<AppState>,
    axum::extract::Query(query): axum::extract::Query<ChannelsQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    load_channels(state, query.token).await
}

/// POST /api/onboarding/channels
/// Fetches Discord guilds + text channels for the given bot token from request body.
pub async fn channels_post(
    state: State<AppState>,
    Json(body): Json<ChannelsBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    load_channels(state, body.token).await
}

#[derive(Debug, Deserialize)]
pub struct CompleteBody {
    pub token: String,
    pub announce_token: Option<String>,
    pub notify_token: Option<String>,
    pub command_token_2: Option<String>,
    pub command_provider_2: Option<String>,
    pub guild_id: String,
    pub owner_id: Option<String>,
    pub provider: Option<String>,
    pub channels: Vec<ChannelMapping>,
    pub template: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ChannelMapping {
    pub channel_id: String,
    pub channel_name: String,
    pub role_id: String,
    pub description: Option<String>,
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedChannelMapping {
    channel_id: String,
    channel_name: String,
    role_id: String,
    description: Option<String>,
    system_prompt: Option<String>,
    created: bool,
}

fn is_discord_channel_id(value: &str) -> bool {
    let trimmed = value.trim();
    !trimmed.is_empty() && trimmed.bytes().all(|byte| byte.is_ascii_digit())
}

fn normalized_channel_name(value: &str) -> Option<String> {
    let trimmed = value.trim().trim_start_matches('#').trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn desired_channel_name(mapping: &ChannelMapping) -> Result<String, String> {
    normalized_channel_name(&mapping.channel_name)
        .or_else(|| normalized_channel_name(&mapping.channel_id))
        .ok_or_else(|| format!("agent '{}' is missing a channel name", mapping.role_id))
}

async fn discord_list_guild_channels(
    client: &reqwest::Client,
    token: &str,
    api_base: &str,
    guild_id: &str,
) -> Result<Vec<serde_json::Value>, String> {
    let url = format!(
        "{}/guilds/{}/channels",
        api_base.trim_end_matches('/'),
        guild_id
    );
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bot {}", token))
        .send()
        .await
        .map_err(|e| format!("failed to fetch guild channels: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "Discord API {status} while listing channels: {body}"
        ));
    }

    resp.json::<Vec<serde_json::Value>>()
        .await
        .map_err(|e| format!("failed to parse guild channels: {e}"))
}

async fn discord_create_text_channel(
    client: &reqwest::Client,
    token: &str,
    api_base: &str,
    guild_id: &str,
    channel_name: &str,
    topic: Option<&str>,
) -> Result<serde_json::Value, String> {
    let url = format!(
        "{}/guilds/{}/channels",
        api_base.trim_end_matches('/'),
        guild_id
    );

    let mut payload = json!({
        "name": channel_name,
        "type": 0,
    });

    if let Some(topic) = topic.map(str::trim).filter(|value| !value.is_empty()) {
        let truncated: String = topic.chars().take(1024).collect();
        payload["topic"] = json!(truncated);
    }

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bot {}", token))
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("failed to create channel '{channel_name}': {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "Discord API {status} while creating channel '{channel_name}': {body}"
        ));
    }

    resp.json::<serde_json::Value>()
        .await
        .map_err(|e| format!("failed to parse created channel '{channel_name}': {e}"))
}

async fn resolve_channel_mapping(
    client: &reqwest::Client,
    token: &str,
    api_base: &str,
    guild_id: &str,
    mapping: &ChannelMapping,
) -> Result<ResolvedChannelMapping, String> {
    let requested_name = desired_channel_name(mapping)?;

    if is_discord_channel_id(&mapping.channel_id) {
        return Ok(ResolvedChannelMapping {
            channel_id: mapping.channel_id.trim().to_string(),
            channel_name: requested_name,
            role_id: mapping.role_id.clone(),
            description: mapping.description.clone(),
            system_prompt: mapping.system_prompt.clone(),
            created: false,
        });
    }

    let guild_id = guild_id.trim();
    if guild_id.is_empty() {
        return Err(format!(
            "cannot create channel '{}' without selecting a Discord server",
            requested_name
        ));
    }

    let existing = discord_list_guild_channels(client, token, api_base, guild_id)
        .await?
        .into_iter()
        .find(|channel| {
            channel.get("type").and_then(|value| value.as_i64()) == Some(0)
                && channel
                    .get("name")
                    .and_then(|value| value.as_str())
                    .map(|name| name == requested_name)
                    .unwrap_or(false)
        });

    if let Some(channel) = existing {
        let channel_id = channel
            .get("id")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| format!("existing channel '{}' is missing an id", requested_name))?;
        let channel_name = channel
            .get("name")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(&requested_name)
            .to_string();

        return Ok(ResolvedChannelMapping {
            channel_id: channel_id.to_string(),
            channel_name,
            role_id: mapping.role_id.clone(),
            description: mapping.description.clone(),
            system_prompt: mapping.system_prompt.clone(),
            created: false,
        });
    }

    let created = discord_create_text_channel(
        client,
        token,
        api_base,
        guild_id,
        &requested_name,
        mapping.description.as_deref(),
    )
    .await?;

    let channel_id = created
        .get("id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("created channel '{}' is missing an id", requested_name))?;
    let channel_name = created
        .get("name")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(&requested_name)
        .to_string();

    Ok(ResolvedChannelMapping {
        channel_id: channel_id.to_string(),
        channel_name,
        role_id: mapping.role_id.clone(),
        description: mapping.description.clone(),
        system_prompt: mapping.system_prompt.clone(),
        created: true,
    })
}

fn write_credential_token(
    runtime_root: &Path,
    bot_name: &str,
    token: Option<&str>,
) -> Result<(), String> {
    let credential_dir = runtime_root.join("credential");
    std::fs::create_dir_all(&credential_dir).map_err(|e| e.to_string())?;
    let path = credential_dir.join(format!("{bot_name}_bot_token"));

    match token.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => std::fs::write(path, format!("{value}\n")).map_err(|e| e.to_string()),
        None => {
            if path.exists() {
                std::fs::remove_file(path).map_err(|e| e.to_string())?;
            }
            Ok(())
        }
    }
}

fn default_secondary_command_provider(primary_provider: &str) -> &'static str {
    match primary_provider {
        "codex" => "claude",
        "gemini" => "codex",
        _ => "codex",
    }
}

fn parse_owner_id(owner_id: Option<&str>) -> Option<u64> {
    owner_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<u64>().ok())
}

fn upsert_command_bot(
    config: &mut crate::config::Config,
    bot_name: &str,
    token: &str,
    provider: &str,
) {
    let mut bot = config
        .discord
        .bots
        .get(bot_name)
        .cloned()
        .unwrap_or_default();
    bot.token = Some(token.trim().to_string());
    bot.provider = Some(provider.trim().to_string());
    config.discord.bots.insert(bot_name.to_string(), bot);
}

fn write_agentdesk_discord_config(
    runtime_root: &Path,
    guild_id: &str,
    primary_token: &str,
    primary_provider: &str,
    secondary_token: Option<&str>,
    secondary_provider: Option<&str>,
    owner_id: Option<&str>,
) -> Result<(), String> {
    let canonical = crate::runtime_layout::config_file_path(runtime_root);
    let legacy = crate::runtime_layout::legacy_config_file_path(runtime_root);
    let config_path = if canonical.is_file() || !legacy.is_file() {
        canonical
    } else {
        legacy
    };
    let mut config = if config_path.is_file() {
        crate::config::load_from_path(&config_path)
            .map_err(|e| format!("Failed to load config {}: {e}", config_path.display()))?
    } else {
        crate::config::Config::default()
    };

    config.discord.guild_id = Some(guild_id.trim().to_string());
    config.discord.owner_id = parse_owner_id(owner_id);

    upsert_command_bot(&mut config, "command", primary_token, primary_provider);

    match secondary_token
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(token) => {
            let provider = secondary_provider
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(default_secondary_command_provider(primary_provider));
            upsert_command_bot(&mut config, "command_2", token, provider);
        }
        None => {
            config.discord.bots.remove("command_2");
        }
    }

    crate::config::save_to_path(&config_path, &config)
        .map_err(|e| format!("Failed to write config {}: {e}", config_path.display()))
}

fn tilde_display_path(path: &Path) -> String {
    dirs::home_dir()
        .and_then(|home| {
            path.strip_prefix(&home)
                .ok()
                .map(|relative| format!("~/{}", relative.display()))
        })
        .unwrap_or_else(|| path.display().to_string())
}

fn agent_channel_slot_mut<'a>(
    channels: &'a mut crate::config::AgentChannels,
    provider: &str,
) -> Option<&'a mut Option<crate::config::AgentChannel>> {
    match provider {
        "claude" => Some(&mut channels.claude),
        "codex" => Some(&mut channels.codex),
        "gemini" => Some(&mut channels.gemini),
        "qwen" => Some(&mut channels.qwen),
        _ => None,
    }
}

fn channel_config_from_existing(
    current: Option<crate::config::AgentChannel>,
) -> crate::config::AgentChannelConfig {
    match current {
        Some(crate::config::AgentChannel::Detailed(config)) => config,
        Some(crate::config::AgentChannel::Legacy(raw)) => {
            let mut config = crate::config::AgentChannelConfig::default();
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                if trimmed.parse::<u64>().is_ok() {
                    config.id = Some(trimmed.to_string());
                } else {
                    config.name = Some(trimmed.to_string());
                }
            }
            config
        }
        None => crate::config::AgentChannelConfig::default(),
    }
}

fn push_channel_alias(config: &mut crate::config::AgentChannelConfig, alias: String) {
    let trimmed = alias.trim();
    if trimmed.is_empty() || config.name.as_deref() == Some(trimmed) {
        return;
    }
    if !config.aliases.iter().any(|existing| existing == trimmed) {
        config.aliases.push(trimmed.to_string());
        config.aliases.sort();
        config.aliases.dedup();
    }
}

fn write_agentdesk_channel_bindings(
    runtime_root: &Path,
    provider: &str,
    resolved_channels: &[ResolvedChannelMapping],
) -> Result<(), String> {
    let config_path = crate::runtime_layout::config_file_path(runtime_root);
    let mut config = if config_path.is_file() {
        crate::config::load_from_path(&config_path)
            .map_err(|e| format!("Failed to load config {}: {e}", config_path.display()))?
    } else {
        crate::config::Config::default()
    };

    for mapping in resolved_channels {
        let workspace = tilde_display_path(&runtime_root.join("workspaces").join(&mapping.role_id));
        let agent_index = if let Some(index) = config
            .agents
            .iter()
            .position(|agent| agent.id == mapping.role_id)
        {
            index
        } else {
            config.agents.push(crate::config::AgentDef {
                id: mapping.role_id.clone(),
                name: mapping.role_id.clone(),
                name_ko: None,
                provider: provider.to_string(),
                channels: crate::config::AgentChannels::default(),
                keywords: Vec::new(),
                department: None,
                avatar_emoji: None,
            });
            config.agents.len() - 1
        };

        let agent = &mut config.agents[agent_index];
        agent.provider = provider.to_string();

        let Some(slot) = agent_channel_slot_mut(&mut agent.channels, provider) else {
            return Err(format!(
                "unsupported provider for onboarding yaml sync: {provider}"
            ));
        };

        let mut channel = channel_config_from_existing(slot.clone());
        if let Some(existing_name) = channel
            .name
            .clone()
            .filter(|existing| existing != &mapping.channel_name)
        {
            push_channel_alias(&mut channel, existing_name);
        }
        channel.id = Some(mapping.channel_id.clone());
        channel.name = Some(mapping.channel_name.clone());
        channel.workspace = Some(workspace);
        channel.provider = Some(provider.to_string());
        *slot = Some(crate::config::AgentChannel::Detailed(channel));
    }

    crate::config::save_to_path(&config_path, &config)
        .map_err(|e| format!("Failed to write config {}: {e}", config_path.display()))
}

/// POST /api/onboarding/complete
/// Saves onboarding configuration and sets up agents.
pub async fn complete(
    State(state): State<AppState>,
    Json(body): Json<CompleteBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let provider = body.provider.as_deref().unwrap_or("claude");
    let discord_token = body
        .announce_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(body.token.as_str());
    let client = reqwest::Client::new();
    let mut resolved_channels = Vec::with_capacity(body.channels.len());

    for mapping in &body.channels {
        let resolved = match resolve_channel_mapping(
            &client,
            discord_token,
            DISCORD_API_BASE,
            &body.guild_id,
            mapping,
        )
        .await
        {
            Ok(resolved) => resolved,
            Err(error) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": format!(
                            "failed to resolve channel for agent '{}': {}",
                            mapping.role_id, error
                        )
                    })),
                );
            }
        };
        resolved_channels.push(resolved);
    }

    let channels_created = resolved_channels
        .iter()
        .filter(|mapping| mapping.created)
        .count();

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    // Save onboarding metadata
    conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('onboarding_bot_token', ?1)",
        [&body.token],
    )
    .ok();
    conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('onboarding_guild_id', ?1)",
        [&body.guild_id],
    )
    .ok();
    conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('onboarding_provider', ?1)",
        [provider],
    )
    .ok();
    if let Some(ref owner) = body.owner_id {
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('onboarding_owner_id', ?1)",
            [owner],
        )
        .ok();
    }
    if let Some(ref ann) = body.announce_token {
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('onboarding_announce_token', ?1)",
            [ann],
        )
        .ok();
    }
    if let Some(ref ntf) = body.notify_token {
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('onboarding_notify_token', ?1)",
            [ntf],
        )
        .ok();
    }
    if let Some(ref cmd2) = body.command_token_2 {
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('onboarding_command_token_2', ?1)",
            [cmd2],
        )
        .ok();
    }
    if let Some(ref provider2) = body.command_provider_2 {
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('onboarding_command_provider_2', ?1)",
            [provider2],
        )
        .ok();
    }

    // Create/update agents for each channel mapping
    let mut created = 0;
    for mapping in &resolved_channels {
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, description, system_prompt, status, xp) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'active', 0) \
             ON CONFLICT(id) DO UPDATE SET \
               name = COALESCE(excluded.name, agents.name), \
               discord_channel_id = excluded.discord_channel_id, \
               description = COALESCE(excluded.description, agents.description), \
               system_prompt = COALESCE(excluded.system_prompt, agents.system_prompt)",
            rusqlite::params![mapping.role_id, mapping.role_id, mapping.channel_id, mapping.description, mapping.system_prompt],
        )
        .ok();
        created += 1;
    }

    // Create default office + department and assign agents
    if !resolved_channels.is_empty() {
        let (template_name, template_name_ko, template_icon, template_color) =
            match body.template.as_deref() {
                Some("household") => (
                    "Household & Schedule",
                    "가사 및 일정 도우미",
                    "🏠",
                    "#10b981",
                ),
                Some("startup") => ("Small Startup", "소규모 스타트업", "🚀", "#8b5cf6"),
                Some("office") => ("Office Work", "사무업무", "🏢", "#3b82f6"),
                _ => ("General", "일반", "📁", "#6b7280"),
            };

        let office_id = "hq";
        conn.execute(
            "INSERT OR IGNORE INTO offices (id, name, name_ko, icon) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![office_id, "Headquarters", "본사", "🏛️"],
        )
        .ok();

        let dept_id = body.template.as_deref().unwrap_or("general").to_string();
        conn.execute(
            "INSERT OR IGNORE INTO departments (id, name, name_ko, icon, color, office_id, sort_order) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
            rusqlite::params![
                dept_id,
                template_name,
                template_name_ko,
                template_icon,
                template_color,
                office_id,
            ],
        )
        .ok();

        for mapping in &resolved_channels {
            conn.execute(
                "INSERT OR REPLACE INTO office_agents (office_id, agent_id, department_id) \
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![office_id, mapping.role_id, dept_id],
            )
            .ok();
            conn.execute(
                "UPDATE agents SET department = ?1 WHERE id = ?2",
                rusqlite::params![dept_id, mapping.role_id],
            )
            .ok();
        }
    }

    let Some(root) = crate::cli::agentdesk_runtime_root() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "cannot determine runtime root"})),
        );
    };

    if let Err(error) = crate::runtime_layout::ensure_runtime_layout(&root) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("failed to prepare runtime layout: {error}")})),
        );
    }

    let config_dir = crate::runtime_layout::config_dir(&root);
    std::fs::create_dir_all(&config_dir).ok();

    // Create workspace directories for each agent
    let workspaces_dir = root.join("workspaces");
    std::fs::create_dir_all(&workspaces_dir).ok();
    for mapping in &resolved_channels {
        let ws_dir = workspaces_dir.join(&mapping.role_id);
        std::fs::create_dir_all(&ws_dir).ok();
    }

    let mut by_channel_id = serde_json::Map::new();
    let mut by_channel_name = serde_json::Map::new();

    for mapping in &resolved_channels {
        let workspace_tilde = tilde_display_path(&root.join("workspaces").join(&mapping.role_id));
        by_channel_id.insert(
            mapping.channel_id.clone(),
            json!({
                "roleId": mapping.role_id,
                "provider": provider,
                "workspace": workspace_tilde,
            }),
        );
        by_channel_name.insert(
            mapping.channel_name.clone(),
            json!({
                "roleId": mapping.role_id,
                "channelId": mapping.channel_id,
                "workspace": workspace_tilde,
            }),
        );
    }

    let role_map = json!({
        "version": 1,
        "byChannelId": by_channel_id,
        "byChannelName": by_channel_name,
        "fallbackByChannelName": { "enabled": true },
    });

    let role_map_path = crate::runtime_layout::role_map_path(&root);
    if let Ok(json_str) = serde_json::to_string_pretty(&role_map) {
        std::fs::write(&role_map_path, json_str).ok();
    }

    if let Err(error) = write_agentdesk_channel_bindings(&root, provider, &resolved_channels) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("failed to write agentdesk.yaml: {error}")})),
        );
    }

    // Mark onboarding complete
    conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('onboarding_complete', 'true')",
        [],
    )
    .ok();
    drop(conn);

    if let Err(e) = write_agentdesk_discord_config(
        &root,
        &body.guild_id,
        &body.token,
        provider,
        body.command_token_2.as_deref(),
        body.command_provider_2.as_deref(),
        body.owner_id.as_deref(),
    ) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("failed to write agentdesk.yaml discord config: {e}")})),
        );
    }

    if let Err(e) = write_credential_token(&root, "announce", body.announce_token.as_deref()) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("failed to write announce credential: {e}")})),
        );
    }

    if let Err(e) = write_credential_token(&root, "notify", body.notify_token.as_deref()) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("failed to write notify credential: {e}")})),
        );
    }

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "agents_created": created,
            "channels_created": channels_created,
            "provider": provider,
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::{
        Arc, MutexGuard,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{Router, extract::Path as AxumPath, routing::get};

    fn env_guard() -> MutexGuard<'static, ()> {
        crate::services::discord::runtime_store::lock_test_env()
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(path, contents).unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    #[test]
    fn write_agentdesk_discord_config_prefers_config_dir_path() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::write(
            root.join("config").join("agentdesk.yaml"),
            "server:\n  port: 8791\n",
        )
        .unwrap();

        write_agentdesk_discord_config(
            root,
            "guild-123",
            "primary-token",
            "claude",
            None,
            None,
            Some("42"),
        )
        .unwrap();

        assert!(!root.join("agentdesk.yaml").exists());
        let config =
            crate::config::load_from_path(&root.join("config").join("agentdesk.yaml")).unwrap();
        assert_eq!(config.server.port, 8791);
        assert_eq!(config.discord.guild_id.as_deref(), Some("guild-123"));
        assert_eq!(config.discord.owner_id, Some(42));
        assert_eq!(
            config.discord.bots["command"].provider.as_deref(),
            Some("claude")
        );
        assert_eq!(
            config.discord.bots["command"].token.as_deref(),
            Some("primary-token")
        );
    }

    #[test]
    fn write_discord_and_credential_artifacts_use_runtime_dirs() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();

        write_agentdesk_discord_config(
            root,
            "guild-123",
            "primary-token",
            "claude",
            Some("secondary-token"),
            Some("codex"),
            Some("42"),
        )
        .unwrap();
        write_credential_token(root, "announce", Some("announce-token")).unwrap();
        write_credential_token(root, "notify", Some("notify-token")).unwrap();

        let config =
            crate::config::load_from_path(&root.join("config").join("agentdesk.yaml")).unwrap();
        assert_eq!(config.discord.guild_id.as_deref(), Some("guild-123"));
        assert_eq!(config.discord.owner_id, Some(42));
        assert_eq!(config.discord.bots.len(), 2);
        assert_eq!(
            config.discord.bots["command"].provider.as_deref(),
            Some("claude")
        );
        assert_eq!(
            config.discord.bots["command_2"].provider.as_deref(),
            Some("codex")
        );

        assert_eq!(
            std::fs::read_to_string(root.join("credential").join("announce_bot_token")).unwrap(),
            "announce-token\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("credential").join("notify_bot_token")).unwrap(),
            "notify-token\n"
        );
    }

    #[test]
    fn desired_channel_name_strips_leading_hash() {
        let mapping = ChannelMapping {
            channel_id: String::new(),
            channel_name: "#agentdesk-cdx".to_string(),
            role_id: "adk-cdx".to_string(),
            description: None,
            system_prompt: None,
        };

        assert_eq!(desired_channel_name(&mapping).unwrap(), "agentdesk-cdx");
    }

    #[tokio::test]
    async fn resolve_channel_mapping_reuses_existing_channel() {
        let post_count = Arc::new(AtomicUsize::new(0));
        let post_count_for_route = post_count.clone();
        let app = Router::new().route(
            "/guilds/{guild_id}/channels",
            get(|AxumPath(_guild_id): AxumPath<String>| async move {
                Json(json!([
                    {"id": "42", "name": "agentdesk-cdx", "type": 0}
                ]))
            })
            .post(
                move |AxumPath(_guild_id): AxumPath<String>,
                      Json(body): Json<serde_json::Value>| {
                    let post_count = post_count_for_route.clone();
                    async move {
                        post_count.fetch_add(1, Ordering::SeqCst);
                        Json(json!({
                            "id": "77",
                            "name": body.get("name").and_then(|value| value.as_str()).unwrap_or("created"),
                            "type": 0
                        }))
                    }
                },
            ),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let mapping = ChannelMapping {
            channel_id: "agentdesk-cdx".to_string(),
            channel_name: "agentdesk-cdx".to_string(),
            role_id: "adk-cdx".to_string(),
            description: Some("desc".to_string()),
            system_prompt: Some("prompt".to_string()),
        };

        let resolved = resolve_channel_mapping(
            &reqwest::Client::new(),
            "token",
            &format!("http://{}", addr),
            "123",
            &mapping,
        )
        .await
        .unwrap();

        assert_eq!(resolved.channel_id, "42");
        assert_eq!(resolved.channel_name, "agentdesk-cdx");
        assert!(!resolved.created);
        assert_eq!(post_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn resolve_channel_mapping_creates_missing_channel() {
        let post_count = Arc::new(AtomicUsize::new(0));
        let post_count_for_route = post_count.clone();
        let app = Router::new().route(
            "/guilds/{guild_id}/channels",
            get(|AxumPath(_guild_id): AxumPath<String>| async move { Json(json!([])) }).post(
                move |AxumPath(_guild_id): AxumPath<String>,
                      Json(body): Json<serde_json::Value>| {
                    let post_count = post_count_for_route.clone();
                    async move {
                        post_count.fetch_add(1, Ordering::SeqCst);
                        Json(json!({
                            "id": "77",
                            "name": body.get("name").and_then(|value| value.as_str()).unwrap_or("created"),
                            "type": 0
                        }))
                    }
                },
            ),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let mapping = ChannelMapping {
            channel_id: "agentdesk-cdx".to_string(),
            channel_name: "agentdesk-cdx".to_string(),
            role_id: "adk-cdx".to_string(),
            description: Some("desc".to_string()),
            system_prompt: Some("prompt".to_string()),
        };

        let resolved = resolve_channel_mapping(
            &reqwest::Client::new(),
            "token",
            &format!("http://{}", addr),
            "123",
            &mapping,
        )
        .await
        .unwrap();

        assert_eq!(resolved.channel_id, "77");
        assert_eq!(resolved.channel_name, "agentdesk-cdx");
        assert!(resolved.created);
        assert_eq!(post_count.load(Ordering::SeqCst), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn check_provider_uses_resolver_exec_path_under_minimal_path() {
        let _guard = env_guard();
        let temp = tempfile::tempdir().unwrap();
        let helper = temp.path().join("provider-helper");
        let provider = temp.path().join("claude");
        let original_path = std::env::var_os("PATH");
        let original_home = std::env::var_os("HOME");

        write_executable(&helper, "#!/bin/sh\nprintf 'claude-test 9.9.9\\n'\n");
        write_executable(
            &provider,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  provider-helper\nelse\n  exit 64\nfi\n",
        );

        unsafe {
            std::env::set_var("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
            std::env::set_var("HOME", temp.path());
            std::env::set_var("AGENTDESK_CLAUDE_PATH", &provider);
        }

        let (status, Json(body)) = check_provider(Json(CheckProviderBody {
            provider: "claude".to_string(),
        }))
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["installed"], json!(true));
        assert_eq!(body["logged_in"], json!(false));
        assert_eq!(body["version"], json!("claude-test 9.9.9"));
        assert_eq!(body["source"], json!("env_override"));
        assert_eq!(body["path"], json!(provider.to_string_lossy().to_string()));

        unsafe {
            std::env::remove_var("AGENTDESK_CLAUDE_PATH");
            match original_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
            match original_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn check_provider_reports_permission_denied() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = env_guard();
        let temp = tempfile::tempdir().unwrap();
        let provider = temp.path().join("claude");
        let original_path = std::env::var_os("PATH");
        let original_home = std::env::var_os("HOME");

        std::fs::write(&provider, "#!/bin/sh\nprintf 'claude-test 9.9.9\\n'\n").unwrap();
        let mut perms = std::fs::metadata(&provider).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&provider, perms).unwrap();

        unsafe {
            std::env::set_var("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
            std::env::set_var("HOME", temp.path());
            std::env::set_var("AGENTDESK_CLAUDE_PATH", &provider);
        }

        let (status, Json(body)) = check_provider(Json(CheckProviderBody {
            provider: "claude".to_string(),
        }))
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["installed"], json!(false));
        assert_eq!(body["version"], json!(null));
        assert_eq!(body["failure_kind"], json!("permission_denied"));
        assert_eq!(body["path"], json!(null));

        unsafe {
            std::env::remove_var("AGENTDESK_CLAUDE_PATH");
            match original_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
            match original_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}

// ── Provider Check ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CheckProviderBody {
    pub provider: String,
}

/// POST /api/onboarding/check-provider
/// Checks if a CLI provider (claude/codex/gemini/qwen) is installed and authenticated.
pub async fn check_provider(
    Json(body): Json<CheckProviderBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let cmd = match body.provider.as_str() {
        "claude" => "claude",
        "codex" => "codex",
        "gemini" => "gemini",
        "qwen" => "qwen",
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "provider must be 'claude', 'codex', 'gemini', or 'qwen'"})),
            );
        }
    };

    // Resolve binary using the exact same provider-specific resolver as the runtime,
    // including known-path fallbacks (~/bin, /opt/homebrew/bin, etc.).
    // This ensures onboarding and actual launch always agree on availability.
    let resolution = {
        let provider = cmd.to_string();
        tokio::task::spawn_blocking(move || match provider.as_str() {
            "claude" | "codex" | "gemini" | "qwen" => Some(
                crate::services::platform::resolve_provider_binary(&provider),
            ),
            _ => None,
        })
        .await
        .ok()
        .flatten()
    }
    .unwrap_or_else(|| crate::services::platform::resolve_provider_binary(cmd));
    let mut failure_kind = resolution.failure_kind.clone();

    let Some(bin_path) = resolution.resolved_path.clone() else {
        return (
            StatusCode::OK,
            Json(json!({
                "installed": false,
                "logged_in": false,
                "version": null,
                "path": null,
                "canonical_path": null,
                "source": null,
                "failure_kind": resolution.failure_kind,
                "attempts": resolution.attempts,
            })),
        );
    };

    // Get version using the resolved binary path (not bare command name)
    // so it works even when PATH doesn't contain the provider.
    let (version, probe_failure_kind) = {
        let resolution = resolution.clone();
        let bin_path = bin_path.clone();
        tokio::task::spawn_blocking(move || {
            crate::services::platform::probe_resolved_binary_version(&bin_path, &resolution)
        })
        .await
        .ok()
        .unwrap_or((None, Some("version_probe_spawn_failed".to_string())))
    };
    if failure_kind.is_none() {
        failure_kind = probe_failure_kind.clone();
    }

    // Check login (heuristic: config directory exists with content)
    let logged_in = dirs::home_dir()
        .map(|home| {
            let config_dir = if cmd == "claude" {
                home.join(".claude")
            } else if cmd == "codex" {
                home.join(".codex")
            } else if cmd == "qwen" {
                home.join(".qwen")
            } else {
                home.join(".gemini")
            };
            config_dir.is_dir()
        })
        .unwrap_or(false);

    (
        StatusCode::OK,
        Json(json!({
            "installed": true,
            "logged_in": logged_in,
            "version": version,
            "path": resolution.resolved_path,
            "canonical_path": resolution.canonical_path,
            "source": resolution.source,
            "failure_kind": failure_kind,
            "attempts": resolution.attempts,
        })),
    )
}

// ── AI Prompt Generation ────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct GeneratePromptBody {
    pub name: String,
    pub description: String,
    pub provider: Option<String>,
}

/// POST /api/onboarding/generate-prompt
/// Generates a system prompt for a custom agent using the local CLI.
pub async fn generate_prompt(
    Json(body): Json<GeneratePromptBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let provider = body
        .provider
        .as_deref()
        .and_then(ProviderKind::from_str)
        .unwrap_or(ProviderKind::Claude);

    let instruction = format!(
        "다음 AI 에이전트의 시스템 프롬프트를 한국어로 작성해줘.\n\
         이름: {}\n설명: {}\n\n\
         에이전트의 역할, 핵심 능력, 소통 스타일을 포함해서 5-10줄로 작성해.\n\
         시스템 프롬프트 텍스트만 출력하고 다른 설명은 붙이지 마.",
        body.name, body.description
    );

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        provider_exec::execute_simple(provider, instruction),
    )
    .await;

    if let Ok(Ok(text)) = result {
        if !text.trim().is_empty() {
            return (
                StatusCode::OK,
                Json(json!({ "prompt": text.trim(), "source": "ai" })),
            );
        }
    }

    // Fallback to template
    let fallback = format!(
        "당신은 '{name}'입니다. {desc}\n\n\
         ## 역할\n\
         - 위 설명에 맞는 업무를 수행합니다\n\
         - 사용자의 요청에 정확하고 친절하게 응답합니다\n\n\
         ## 소통 원칙\n\
         - 한국어로 소통합니다\n\
         - 간결하고 명확하게 답변합니다\n\
         - 필요시 확인 질문을 합니다",
        name = body.name,
        desc = body.description,
    );

    (
        StatusCode::OK,
        Json(json!({ "prompt": fallback, "source": "template" })),
    )
}
