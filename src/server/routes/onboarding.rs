use std::path::Path;

use axum::{Json, extract::State, http::StatusCode};
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::services::provider::ProviderKind;
use crate::services::provider_exec;

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

    // Check if bot_settings exists (indicates onboarding was done)
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
}

#[derive(Debug, Deserialize)]
pub struct ChannelMapping {
    pub channel_id: String,
    pub channel_name: String,
    pub role_id: String,
    pub description: Option<String>,
    pub system_prompt: Option<String>,
}

fn upsert_bot_settings_entry(
    object: &mut serde_json::Map<String, serde_json::Value>,
    token: &str,
    provider: &str,
    owner_id: Option<&str>,
) {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return;
    }

    let key = crate::services::discord::settings::discord_token_hash(trimmed);
    let mut entry = json!({
        "token": trimmed,
        "provider": provider,
    });
    if let Some(owner_id) = owner_id.filter(|value| !value.trim().is_empty()) {
        entry["owner_user_id"] = json!(owner_id.trim());
    }
    object.insert(key, entry);
}

fn write_bot_settings(
    runtime_root: &Path,
    primary_token: &str,
    primary_provider: &str,
    secondary_token: Option<&str>,
    secondary_provider: Option<&str>,
    owner_id: Option<&str>,
) -> Result<(), String> {
    let config_dir = runtime_root.join("config");
    std::fs::create_dir_all(&config_dir).map_err(|e| e.to_string())?;
    let path = config_dir.join("bot_settings.json");

    let mut root: serde_json::Value = if path.exists() {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| json!({}))
    } else {
        json!({})
    };

    let obj = root
        .as_object_mut()
        .ok_or_else(|| "bot_settings.json root must be a JSON object".to_string())?;

    upsert_bot_settings_entry(obj, primary_token, primary_provider, owner_id);

    if let Some(token) = secondary_token
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let secondary_provider = secondary_provider
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(match primary_provider {
                "codex" => "claude",
                "gemini" => "codex",
                _ => "codex",
            });
        upsert_bot_settings_entry(obj, token, secondary_provider, owner_id);
    }

    let content = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    std::fs::write(&path, content).map_err(|e| e.to_string())
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

fn strip_legacy_discord_section(existing: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut in_discord = false;

    for line in existing.lines() {
        let is_top_level = !line.starts_with(' ') && !line.starts_with('\t');
        if !in_discord && is_top_level && line.trim_end() == "discord:" {
            in_discord = true;
            continue;
        }

        if in_discord {
            if !line.trim().is_empty() && is_top_level {
                in_discord = false;
            } else {
                continue;
            }
        }

        lines.push(line.to_string());
    }

    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

fn cleanup_legacy_yaml_discord_section(runtime_root: &Path) -> Result<(), String> {
    let yaml_path = runtime_root.join("agentdesk.yaml");
    if !yaml_path.exists() {
        return Ok(());
    }

    let existing = std::fs::read_to_string(&yaml_path).map_err(|e| e.to_string())?;
    let stripped = strip_legacy_discord_section(&existing);
    if stripped != existing {
        std::fs::write(&yaml_path, stripped).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// POST /api/onboarding/complete
/// Saves onboarding configuration and sets up agents.
pub async fn complete(
    State(state): State<AppState>,
    Json(body): Json<CompleteBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let provider = body.provider.as_deref().unwrap_or("claude");

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
    for mapping in &body.channels {
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
    )
    .ok();
    drop(conn);

    let Some(root) = crate::cli::agentdesk_runtime_root() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "cannot determine runtime root"})),
        );
    };

    if let Err(e) = write_bot_settings(
        &root,
        &body.token,
        provider,
        body.command_token_2.as_deref(),
        body.command_provider_2.as_deref(),
        body.owner_id.as_deref(),
    ) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("failed to write bot_settings.json: {e}")})),
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

    if let Err(e) = cleanup_legacy_yaml_discord_section(&root) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("failed to clean legacy yaml tokens: {e}")})),
        );
    }

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "agents_created": created,
            "provider": provider,
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_legacy_discord_section_removes_top_level_block() {
        let input = "server:\n  port: 8791\ndiscord:\n  bots:\n    claude:\n      token: \"secret\"\ndata:\n  dir: ./data\n";

        let output = strip_legacy_discord_section(input);
        assert_eq!(output, "server:\n  port: 8791\ndata:\n  dir: ./data\n");
    }

    #[test]
    fn write_bot_and_credential_artifacts_use_runtime_dirs() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();

        write_bot_settings(
            root,
            "primary-token",
            "claude",
            Some("secondary-token"),
            Some("codex"),
            Some("42"),
        )
        .unwrap();
        write_credential_token(root, "announce", Some("announce-token")).unwrap();
        write_credential_token(root, "notify", Some("notify-token")).unwrap();

        let bot_settings =
            std::fs::read_to_string(root.join("config").join("bot_settings.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&bot_settings).unwrap();
        let obj = parsed.as_object().unwrap();
        assert_eq!(obj.len(), 2);

        let providers: Vec<String> = obj
            .values()
            .filter_map(|entry| entry.get("provider").and_then(|v| v.as_str()))
            .map(ToString::to_string)
            .collect();
        assert!(providers.contains(&"claude".to_string()));
        assert!(providers.contains(&"codex".to_string()));

        assert_eq!(
            std::fs::read_to_string(root.join("credential").join("announce_bot_token")).unwrap(),
            "announce-token\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("credential").join("notify_bot_token")).unwrap(),
            "notify-token\n"
        );
    }
}

// ── Provider Check ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CheckProviderBody {
    pub provider: String,
}

/// POST /api/onboarding/check-provider
/// Checks if a CLI provider (claude/codex/gemini) is installed and authenticated.
pub async fn check_provider(
    Json(body): Json<CheckProviderBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let cmd = match body.provider.as_str() {
        "claude" => "claude",
        "codex" => "codex",
        "gemini" => "gemini",
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "provider must be 'claude', 'codex', or 'gemini'"})),
            );
        }
    };

    // Resolve binary using the exact same provider-specific resolver as the runtime,
    // including known-path fallbacks (~/bin, /opt/homebrew/bin, etc.).
    // This ensures onboarding and actual launch always agree on availability.
    let resolved_path = {
        let provider = cmd.to_string();
        tokio::task::spawn_blocking(move || match provider.as_str() {
            "claude" => crate::services::claude::resolve_claude_path(),
            "codex" => crate::services::codex::resolve_codex_path(),
            "gemini" => crate::services::gemini::resolve_gemini_path(),
            _ => None,
        })
        .await
        .ok()
        .flatten()
    };

    let Some(bin_path) = resolved_path else {
        return (
            StatusCode::OK,
            Json(json!({
                "installed": false,
                "logged_in": false,
                "version": null,
            })),
        );
    };

    // Get version using the resolved binary path (not bare command name)
    // so it works even when PATH doesn't contain the provider.
    let version_out = tokio::process::Command::new(&bin_path)
        .arg("--version")
        .output()
        .await;
    let version = version_out.ok().and_then(|o| {
        if o.status.success() {
            Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
        } else {
            None
        }
    });

    // Check login (heuristic: config directory exists with content)
    let home = std::env::var("HOME").unwrap_or_default();
    let config_dir = if cmd == "claude" {
        format!("{home}/.claude")
    } else if cmd == "codex" {
        format!("{home}/.codex")
    } else {
        format!("{home}/.gemini")
    };
    let logged_in = std::path::Path::new(&config_dir).is_dir();

    (
        StatusCode::OK,
        Json(json!({
            "installed": true,
            "logged_in": logged_in,
            "version": version,
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
