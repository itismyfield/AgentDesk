use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use serenity::ChannelId;
use sha2::{Digest, Sha256};

use poise::serenity_prelude as serenity;

use crate::services::agent_protocol::DEFAULT_ALLOWED_TOOLS;
use crate::services::provider::ProviderKind;

use super::DiscordBotSettings;
use super::formatting::normalize_allowed_tools;
use super::org_schema;
use super::role_map::{
    is_known_agent as is_known_agent_from_role_map,
    load_peer_agents as load_peer_agents_from_role_map,
    load_shared_prompt_path as load_shared_prompt_path_from_role_map,
    resolve_role_binding as resolve_role_binding_from_role_map,
    resolve_workspace as resolve_workspace_from_role_map,
};
use super::runtime_store::{bot_settings_path, discord_uploads_root};

fn json_u64(value: &serde_json::Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|raw| raw.parse::<u64>().ok()))
}

/// Compute a short hash key from the bot token (first 16 chars of SHA-256 hex)
/// Uses "discord_" prefix to namespace Discord bot entries in settings.
pub(crate) fn discord_token_hash(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let result = hasher.finalize();
    format!("discord_{}", hex::encode(&result[..8]))
}

fn default_allowed_tools_for_provider(provider: &ProviderKind) -> Vec<String> {
    let _ = provider;
    DEFAULT_ALLOWED_TOOLS
        .iter()
        .map(|tool| (*tool).to_string())
        .collect()
}

#[derive(Clone, Debug)]
pub(super) struct RoleBinding {
    pub role_id: String,
    pub prompt_file: String,
    pub provider: Option<ProviderKind>,
    /// Optional model override (e.g. "opus", "sonnet", "haiku", "o3")
    pub model: Option<String>,
    /// Optional reasoning effort for Codex (e.g. "low", "normal", "high", "xhigh")
    pub reasoning_effort: Option<String>,
    /// Whether this role may see peer-agent handoff guidance in the system prompt.
    pub peer_agents_enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PeerAgentInfo {
    pub role_id: String,
    pub display_name: String,
    pub keywords: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscordBotLaunchConfig {
    pub hash_key: String,
    pub token: String,
    pub provider: ProviderKind,
}

pub(super) fn channel_supports_provider(
    provider: &ProviderKind,
    channel_name: Option<&str>,
    is_dm: bool,
    role_binding: Option<&RoleBinding>,
) -> bool {
    if is_dm {
        return provider.is_supported();
    }

    if let Some(bound_provider) = role_binding.and_then(|binding| binding.provider.as_ref()) {
        return bound_provider == provider;
    }

    // Check global suffix_map from bot_settings.json
    if let Some(ch) = channel_name {
        if let Some(mapped) = lookup_suffix_provider(ch) {
            return mapped == *provider;
        }
    }

    // When org.yaml is present, require an explicit channel binding or suffix match.
    // This avoids the legacy "Claude catches all generic channels" behavior leaking
    // into deployments that already opted into explicit org routing.
    if org_schema::org_schema_exists() {
        return false;
    }

    provider.is_channel_supported(channel_name, is_dm)
}

pub(super) fn bot_settings_allow_channel(
    settings: &DiscordBotSettings,
    channel_id: ChannelId,
    is_dm: bool,
) -> bool {
    if is_dm {
        return true;
    }
    settings.allowed_channel_ids.is_empty()
        || settings.allowed_channel_ids.contains(&channel_id.get())
}

pub(super) fn bot_settings_allow_agent(
    settings: &DiscordBotSettings,
    role_binding: Option<&RoleBinding>,
    is_dm: bool,
) -> bool {
    if is_dm {
        return true;
    }

    let Some(expected_agent) = settings
        .agent
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return true;
    };

    role_binding.is_some_and(|binding| binding.role_id.eq_ignore_ascii_case(expected_agent))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BotChannelRoutingGuardFailure {
    ChannelNotAllowed,
    AgentMismatch,
    ProviderMismatch,
}

impl std::fmt::Display for BotChannelRoutingGuardFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ChannelNotAllowed => f.write_str("not allowed for bot settings"),
            Self::AgentMismatch => f.write_str("agent mismatch"),
            Self::ProviderMismatch => f.write_str("provider mismatch"),
        }
    }
}

pub(super) fn validate_bot_channel_routing(
    settings: &DiscordBotSettings,
    provider: &ProviderKind,
    channel_id: ChannelId,
    channel_name: Option<&str>,
    is_dm: bool,
) -> Result<(), BotChannelRoutingGuardFailure> {
    let role_binding = resolve_role_binding(channel_id, channel_name);

    if !bot_settings_allow_channel(settings, channel_id, is_dm) {
        return Err(BotChannelRoutingGuardFailure::ChannelNotAllowed);
    }
    if !bot_settings_allow_agent(settings, role_binding.as_ref(), is_dm) {
        return Err(BotChannelRoutingGuardFailure::AgentMismatch);
    }
    if !channel_supports_provider(provider, channel_name, is_dm, role_binding.as_ref()) {
        return Err(BotChannelRoutingGuardFailure::ProviderMismatch);
    }

    Ok(())
}

/// Look up the provider for a channel name using the global suffix_map
/// from org.yaml or bot_settings.json.
fn lookup_suffix_provider(channel_name: &str) -> Option<ProviderKind> {
    // Try org schema first
    if org_schema::org_schema_exists() {
        if let Some(provider) = org_schema::lookup_suffix_provider(channel_name) {
            return Some(provider);
        }
    }
    // Fallback to bot_settings.json
    let path = bot_settings_path()?;
    let content = fs::read_to_string(&path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    let map = json.get("suffix_map")?.as_object()?;
    for (suffix, provider_val) in map {
        if channel_name.ends_with(suffix.as_str()) {
            let provider_str = provider_val.as_str()?;
            return Some(ProviderKind::from_str_or_unsupported(provider_str));
        }
    }
    None
}

pub(super) fn resolve_role_binding(
    channel_id: ChannelId,
    channel_name: Option<&str>,
) -> Option<RoleBinding> {
    if org_schema::org_schema_exists() {
        if let Some(binding) = org_schema::resolve_role_binding(channel_id, channel_name) {
            return Some(binding);
        }
    }
    resolve_role_binding_from_role_map(channel_id, channel_name)
}

/// Resolve workspace path from role_map.json (or org.yaml) for a given channel.
pub(super) fn resolve_workspace(
    channel_id: ChannelId,
    channel_name: Option<&str>,
) -> Option<String> {
    if org_schema::org_schema_exists() {
        if let Some(ws) = org_schema::resolve_workspace(channel_id, channel_name) {
            return Some(ws);
        }
    }
    resolve_workspace_from_role_map(channel_id, channel_name)
}

pub(super) fn load_role_prompt(binding: &RoleBinding) -> Option<String> {
    let raw = fs::read_to_string(Path::new(&binding.prompt_file)).ok()?;
    const MAX_CHARS: usize = 12_000;
    if raw.chars().count() <= MAX_CHARS {
        return Some(raw);
    }
    let truncated: String = raw.chars().take(MAX_CHARS).collect();
    Some(truncated)
}

/// Build a catalog of long-term memory files for a given role.
/// Scans $AGENTDESK_ROOT_DIR/role-context/{role_id}.memory/ for .md files and extracts
/// name + description from YAML frontmatter (or first heading as fallback).
/// Returns None if directory doesn't exist or has no .md files.
pub(super) fn load_longterm_memory_catalog(role_id: &str) -> Option<String> {
    let root = super::runtime_store::agentdesk_root()?;
    let memory_dir = root
        .join("role-context")
        .join(format!("{}.memory", role_id));
    if !memory_dir.is_dir() {
        return None;
    }

    let mut entries: Vec<(String, String)> = Vec::new();
    let Ok(read_dir) = std::fs::read_dir(&memory_dir) else {
        return None;
    };

    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().map_or(true, |ext| ext != "md") {
            continue;
        }
        let filename = path.file_name()?.to_string_lossy().to_string();
        let content = std::fs::read_to_string(&path).unwrap_or_default();

        // Try YAML frontmatter first: ---\n..description: X..\n---
        let description = extract_frontmatter_description(&content)
            .or_else(|| extract_first_heading(&content))
            .unwrap_or_else(|| filename.trim_end_matches(".md").to_string());

        let abs_path = path.display().to_string();
        entries.push((abs_path, description));
    }

    if entries.is_empty() {
        return None;
    }

    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let catalog: Vec<String> = entries
        .iter()
        .map(|(path, desc)| format!("  - {}: {}", path, desc))
        .collect();

    Some(catalog.join("\n"))
}

fn extract_frontmatter_description(content: &str) -> Option<String> {
    if !content.starts_with("---") {
        return None;
    }
    let rest = &content[3..];
    let end = rest.find("\n---")?;
    let frontmatter = &rest[..end];
    for line in frontmatter.lines() {
        let trimmed = line.trim();
        if let Some(desc) = trimmed.strip_prefix("description:") {
            let desc = desc.trim().trim_matches('"').trim_matches('\'');
            if !desc.is_empty() {
                return Some(desc.to_string());
            }
        }
    }
    None
}

fn extract_first_heading(content: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(heading) = trimmed.strip_prefix('#') {
            let heading = heading.trim_start_matches('#').trim();
            if !heading.is_empty() {
                return Some(heading.to_string());
            }
        }
    }
    None
}

/// Load the shared agent prompt (e.g. AGENTS.md) configured in org.yaml or role_map.json.
/// Returns None if not configured or file not found.
pub(super) fn load_shared_prompt() -> Option<String> {
    let path_str = if org_schema::org_schema_exists() {
        org_schema::load_shared_prompt_path()
    } else {
        None
    }
    .or_else(load_shared_prompt_path_from_role_map)?;

    let raw = fs::read_to_string(Path::new(&path_str)).ok()?;
    const MAX_CHARS: usize = 6_000;
    if raw.chars().count() <= MAX_CHARS {
        return Some(raw);
    }
    let truncated: String = raw.chars().take(MAX_CHARS).collect();
    Some(truncated)
}

/// #119: Load review tuning guidance from the well-known runtime file.
/// Returns None if file doesn't exist or is empty.
pub(super) fn load_review_tuning_guidance() -> Option<String> {
    let root = super::runtime_store::agentdesk_root()?;
    let path = root.join("runtime").join("review-tuning-guidance.txt");
    let content = fs::read_to_string(path).ok()?;
    if content.trim().is_empty() {
        return None;
    }
    // Cap at 2000 chars to avoid bloating the prompt
    const MAX_CHARS: usize = 2_000;
    if content.chars().count() <= MAX_CHARS {
        Some(content)
    } else {
        Some(content.chars().take(MAX_CHARS).collect())
    }
}

/// Check if a role_id is a known agent in org schema or role_map channel bindings.
/// Unlike load_peer_agents() which reads meeting.available_agents in legacy mode,
/// this checks the full agent/channel binding registry.
pub(super) fn is_known_agent(role_id: &str) -> bool {
    if org_schema::org_schema_exists() {
        if let Some(known) = org_schema::is_known_agent(role_id) {
            return known;
        }
    }
    is_known_agent_from_role_map(role_id)
}

pub(super) fn load_peer_agents() -> Vec<PeerAgentInfo> {
    if org_schema::org_schema_exists() {
        let peers = org_schema::load_peer_agents();
        if !peers.is_empty() {
            return peers;
        }
    }
    load_peer_agents_from_role_map()
}

pub(super) fn render_peer_agent_guidance(current_role_id: &str) -> Option<String> {
    let peers: Vec<PeerAgentInfo> = load_peer_agents()
        .into_iter()
        .filter(|agent| agent.role_id != current_role_id)
        .collect();
    if peers.is_empty() {
        return None;
    }

    let mut lines = vec![
        "[Peer Agent Directory]".to_string(),
        "You are one role agent among multiple specialist agents in this workspace.".to_string(),
        "If a request is mostly outside your scope, do not bluff ownership or silently proceed as if it were yours.".to_string(),
        "Instead, name the 1-2 most suitable peer agents below, explain why they fit better, and ask: \"해당 에이전트에게 전달할까요?\"".to_string(),
        "If the user approves, use the `send-agent-message` skill to forward the request context to the recommended agent.".to_string(),
        "If the user explicitly wants your perspective anyway, answer only within your scope and mention the handoff option.".to_string(),
        String::new(),
        "Available peer agents:".to_string(),
    ];

    for peer in peers {
        let keywords = if peer.keywords.is_empty() {
            String::new()
        } else {
            let short = peer.keywords.iter().take(4).cloned().collect::<Vec<_>>();
            format!(" — best for: {}", short.join(", "))
        };
        lines.push(format!(
            "- {} ({}){}",
            peer.role_id, peer.display_name, keywords
        ));
    }

    Some(lines.join("\n"))
}

pub(super) fn channel_upload_dir(channel_id: ChannelId) -> Option<std::path::PathBuf> {
    discord_uploads_root().map(|p| p.join(channel_id.get().to_string()))
}

pub(super) fn cleanup_old_uploads(max_age: Duration) {
    let Some(root) = discord_uploads_root() else {
        return;
    };
    if !root.exists() {
        return;
    }

    let now = SystemTime::now();
    let Ok(channels) = fs::read_dir(&root) else {
        return;
    };

    for ch in channels.filter_map(|e| e.ok()) {
        let ch_path = ch.path();
        if !ch_path.is_dir() {
            continue;
        }

        let Ok(files) = fs::read_dir(&ch_path) else {
            continue;
        };

        for f in files.filter_map(|e| e.ok()) {
            let f_path = f.path();
            if !f_path.is_file() {
                continue;
            }

            let should_delete = fs::metadata(&f_path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|mtime| now.duration_since(mtime).ok())
                .map(|age| age >= max_age)
                .unwrap_or(false);

            if should_delete {
                let _ = fs::remove_file(&f_path);
            }
        }

        // Remove empty channel dir
        if fs::read_dir(&ch_path)
            .ok()
            .map(|mut it| it.next().is_none())
            .unwrap_or(false)
        {
            let _ = fs::remove_dir(&ch_path);
        }
    }
}

pub(super) fn cleanup_channel_uploads(channel_id: ChannelId) {
    if let Some(dir) = channel_upload_dir(channel_id) {
        let _ = fs::remove_dir_all(dir);
    }
}

/// Load Discord bot settings from bot_settings.json
pub(super) fn load_bot_settings(token: &str) -> DiscordBotSettings {
    let Some(path) = bot_settings_path() else {
        return DiscordBotSettings::default();
    };
    let Ok(content) = fs::read_to_string(&path) else {
        return DiscordBotSettings::default();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
        return DiscordBotSettings::default();
    };
    let key = discord_token_hash(token);
    let Some(entry) = json.get(&key) else {
        return DiscordBotSettings::default();
    };
    let agent = entry
        .get("agent")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);
    let owner_user_id = entry.get("owner_user_id").and_then(json_u64);
    let provider = entry
        .get("provider")
        .and_then(|v| v.as_str())
        .map(ProviderKind::from_str_or_unsupported)
        .unwrap_or(ProviderKind::Claude);
    let last_sessions = entry
        .get("last_sessions")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let last_remotes = entry
        .get("last_remotes")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let allowed_channel_ids = entry
        .get("allowed_channel_ids")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(json_u64).collect())
        .unwrap_or_default();
    let allowed_user_ids = entry
        .get("allowed_user_ids")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(json_u64).collect())
        .unwrap_or_default();
    let allowed_bot_ids = entry
        .get("allowed_bot_ids")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(json_u64).collect())
        .unwrap_or_default();
    let channel_model_overrides = entry
        .get("channel_model_overrides")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(channel_id, model)| {
                    model
                        .as_str()
                        .map(|model| (channel_id.clone(), model.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    let allowed_tools = match entry.get("allowed_tools") {
        None => default_allowed_tools_for_provider(&provider),
        Some(value) => {
            let Some(tools_arr) = value.as_array() else {
                let allowed_tools = default_allowed_tools_for_provider(&provider);
                return DiscordBotSettings {
                    agent,
                    provider,
                    allowed_tools,
                    allowed_channel_ids,
                    owner_user_id,
                    last_sessions,
                    last_remotes,
                    allowed_user_ids,
                    allowed_bot_ids,
                    ..DiscordBotSettings::default()
                };
            };
            normalize_allowed_tools(tools_arr.iter().filter_map(|v| v.as_str()))
        }
    };
    DiscordBotSettings {
        agent,
        provider,
        allowed_tools,
        allowed_channel_ids,
        last_sessions,
        last_remotes,
        channel_model_overrides,
        owner_user_id,
        allowed_user_ids,
        allowed_bot_ids,
    }
}

/// Save Discord bot settings to bot_settings.json
pub(super) fn save_bot_settings(token: &str, settings: &DiscordBotSettings) {
    let Some(path) = bot_settings_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut json: serde_json::Value = if let Ok(content) = fs::read_to_string(&path) {
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    let key = discord_token_hash(token);
    let normalized_tools = normalize_allowed_tools(&settings.allowed_tools);
    let mut entry = serde_json::json!({
        "token": token,
        "agent": settings.agent,
        "provider": settings.provider.as_str(),
        "allowed_tools": normalized_tools,
        "allowed_channel_ids": settings.allowed_channel_ids,
        "last_sessions": settings.last_sessions,
        "last_remotes": settings.last_remotes,
        "channel_model_overrides": settings.channel_model_overrides,
        "allowed_user_ids": settings.allowed_user_ids,
        "allowed_bot_ids": settings.allowed_bot_ids,
    });
    if let Some(owner_id) = settings.owner_user_id {
        entry["owner_user_id"] = serde_json::json!(owner_id);
    }
    json[key] = entry;
    if let Ok(s) = serde_json::to_string_pretty(&json) {
        let _ = fs::write(&path, s);
    }
}

pub fn load_discord_bot_launch_configs() -> Vec<DiscordBotLaunchConfig> {
    let Some(path) = bot_settings_path() else {
        return Vec::new();
    };
    let Ok(content) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
        return Vec::new();
    };
    let Some(obj) = json.as_object() else {
        return Vec::new();
    };

    let mut configs = Vec::new();
    for (hash_key, entry) in obj {
        let Some(token) = entry.get("token").and_then(|v| v.as_str()) else {
            continue;
        };
        let provider = entry
            .get("provider")
            .and_then(|v| v.as_str())
            .map(ProviderKind::from_str_or_unsupported)
            .unwrap_or(ProviderKind::Claude);
        configs.push(DiscordBotLaunchConfig {
            hash_key: hash_key.clone(),
            token: token.to_string(),
            provider,
        });
    }
    configs
}

/// Resolve a Discord bot token from its hash by searching bot_settings.json
pub fn resolve_discord_token_by_hash(hash: &str) -> Option<String> {
    let path = bot_settings_path()?;
    let content = fs::read_to_string(&path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    let obj = json.as_object()?;
    let entry = obj.get(hash)?;
    entry
        .get("token")
        .and_then(|v| v.as_str())
        .map(String::from)
}

pub fn resolve_discord_bot_provider(token: &str) -> ProviderKind {
    load_bot_settings(token).provider
}

#[cfg(test)]
mod tests {
    use std::fs;

    use poise::serenity_prelude::ChannelId;
    use tempfile::TempDir;

    use crate::services::agent_protocol::DEFAULT_ALLOWED_TOOLS;
    use crate::services::provider::ProviderKind;

    use super::{
        BotChannelRoutingGuardFailure, bot_settings_allow_agent, bot_settings_allow_channel,
        channel_supports_provider, discord_token_hash, load_bot_settings,
        load_discord_bot_launch_configs, load_peer_agents, render_peer_agent_guidance,
        resolve_role_binding, save_bot_settings, validate_bot_channel_routing,
    };

    fn with_temp_home<F>(f: F)
    where
        F: FnOnce(&TempDir),
    {
        let _guard = super::super::runtime_store::test_env_lock().lock().unwrap();
        let temp_home = TempDir::new().unwrap();
        let root = temp_home.path().join(".adk");
        fs::create_dir_all(&root).unwrap();
        let prev = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", &root) };
        f(&temp_home);
        match prev {
            Some(v) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", v) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }

    #[test]
    fn test_load_bot_settings_keeps_explicit_empty_allowed_tools() {
        with_temp_home(|temp_home: &TempDir| {
            let settings_dir = temp_home.path().join(".adk").join("config");
            fs::create_dir_all(&settings_dir).unwrap();
            let token = "test-token";
            let key = discord_token_hash(token);
            let json = serde_json::json!({
                key: {
                    "token": token,
                    "allowed_tools": [],
                    "owner_user_id": 42,
                    "allowed_user_ids": [7],
                    "allowed_bot_ids": [9]
                }
            });
            fs::write(
                settings_dir.join("bot_settings.json"),
                serde_json::to_string_pretty(&json).unwrap(),
            )
            .unwrap();

            let settings = load_bot_settings(token);
            assert!(settings.allowed_tools.is_empty());
            assert_eq!(settings.provider, ProviderKind::Claude);
            assert_eq!(settings.owner_user_id, Some(42));
            assert_eq!(settings.allowed_user_ids, vec![7]);
            assert_eq!(settings.allowed_bot_ids, vec![9]);
        });
    }

    #[test]
    fn test_load_bot_settings_normalizes_and_dedupes_tool_names() {
        with_temp_home(|temp_home: &TempDir| {
            let settings_dir = temp_home.path().join(".adk").join("config");
            fs::create_dir_all(&settings_dir).unwrap();
            let token = "test-token";
            let key = discord_token_hash(token);
            let json = serde_json::json!({
                key: {
                    "token": token,
                    "allowed_tools": ["webfetch", "WebFetch", "BASH", "unknown-tool"]
                }
            });
            fs::write(
                settings_dir.join("bot_settings.json"),
                serde_json::to_string_pretty(&json).unwrap(),
            )
            .unwrap();

            let settings = load_bot_settings(token);
            assert_eq!(
                settings.allowed_tools,
                vec!["WebFetch".to_string(), "Bash".to_string()]
            );
        });
    }

    #[test]
    fn test_load_bot_settings_uses_default_allowed_tools_for_qwen_when_omitted() {
        with_temp_home(|temp_home: &TempDir| {
            let settings_dir = temp_home.path().join(".adk").join("config");
            fs::create_dir_all(&settings_dir).unwrap();
            let token = "qwen-token";
            let key = discord_token_hash(token);
            let json = serde_json::json!({
                key: {
                    "token": token,
                    "provider": "qwen"
                }
            });
            fs::write(
                settings_dir.join("bot_settings.json"),
                serde_json::to_string_pretty(&json).unwrap(),
            )
            .unwrap();

            let settings = load_bot_settings(token);
            assert_eq!(settings.provider, ProviderKind::Qwen);
            assert_eq!(
                settings.allowed_tools,
                DEFAULT_ALLOWED_TOOLS
                    .iter()
                    .map(|tool| (*tool).to_string())
                    .collect::<Vec<_>>()
            );
        });
    }

    #[test]
    fn test_load_bot_launch_configs_reads_provider() {
        with_temp_home(|temp_home: &TempDir| {
            let settings_dir = temp_home.path().join(".adk").join("config");
            fs::create_dir_all(&settings_dir).unwrap();
            fs::write(
                settings_dir.join("bot_settings.json"),
                serde_json::to_string_pretty(&serde_json::json!({
                    "discord_a": { "token": "claude-token", "provider": "claude" },
                    "discord_b": { "token": "codex-token", "provider": "codex" }
                }))
                .unwrap(),
            )
            .unwrap();

            let configs = load_discord_bot_launch_configs();
            assert_eq!(configs.len(), 2);
            assert_eq!(configs[0].provider, ProviderKind::Claude);
            assert_eq!(configs[1].provider, ProviderKind::Codex);
        });
    }

    #[test]
    fn test_load_bot_settings_accepts_string_encoded_ids() {
        with_temp_home(|temp_home: &TempDir| {
            let settings_dir = temp_home.path().join(".adk").join("config");
            fs::create_dir_all(&settings_dir).unwrap();
            let token = "test-token";
            let key = discord_token_hash(token);
            let json = serde_json::json!({
                key: {
                    "token": token,
                    "owner_user_id": "343742347365974000",
                    "allowed_user_ids": ["429955158974136300"],
                    "allowed_bot_ids": ["1479017284805722200"]
                }
            });
            fs::write(
                settings_dir.join("bot_settings.json"),
                serde_json::to_string_pretty(&json).unwrap(),
            )
            .unwrap();

            let settings = load_bot_settings(token);
            assert_eq!(settings.owner_user_id, Some(343742347365974000));
            assert_eq!(settings.allowed_user_ids, vec![429955158974136300]);
            assert_eq!(settings.allowed_bot_ids, vec![1479017284805722200]);
        });
    }

    #[test]
    fn test_load_bot_settings_reads_channel_model_overrides() {
        with_temp_home(|temp_home: &TempDir| {
            let settings_dir = temp_home.path().join(".adk").join("config");
            fs::create_dir_all(&settings_dir).unwrap();
            let token = "test-token";
            let key = discord_token_hash(token);
            let json = serde_json::json!({
                key: {
                    "token": token,
                    "channel_model_overrides": {
                        "123": "gpt-5.4",
                        "456": "sonnet"
                    }
                }
            });
            fs::write(
                settings_dir.join("bot_settings.json"),
                serde_json::to_string_pretty(&json).unwrap(),
            )
            .unwrap();

            let settings = load_bot_settings(token);
            assert_eq!(
                settings
                    .channel_model_overrides
                    .get("123")
                    .map(String::as_str),
                Some("gpt-5.4")
            );
            assert_eq!(
                settings
                    .channel_model_overrides
                    .get("456")
                    .map(String::as_str),
                Some("sonnet")
            );
        });
    }

    #[test]
    fn test_load_bot_settings_reads_allowed_channel_ids() {
        with_temp_home(|temp_home: &TempDir| {
            let settings_dir = temp_home.path().join(".adk").join("config");
            fs::create_dir_all(&settings_dir).unwrap();
            let token = "test-token";
            let key = discord_token_hash(token);
            let json = serde_json::json!({
                key: {
                    "token": token,
                    "allowed_channel_ids": ["123", 456]
                }
            });
            fs::write(
                settings_dir.join("bot_settings.json"),
                serde_json::to_string_pretty(&json).unwrap(),
            )
            .unwrap();

            let settings = load_bot_settings(token);
            assert_eq!(settings.allowed_channel_ids, vec![123, 456]);
        });
    }

    #[test]
    fn test_save_bot_settings_persists_channel_model_overrides() {
        with_temp_home(|_temp_home: &TempDir| {
            let token = "test-token";
            let mut settings = super::super::DiscordBotSettings::default();
            settings
                .channel_model_overrides
                .insert("123".to_string(), "gpt-5.4".to_string());
            settings
                .channel_model_overrides
                .insert("456".to_string(), "sonnet".to_string());

            save_bot_settings(token, &settings);

            let loaded = load_bot_settings(token);
            assert_eq!(
                loaded
                    .channel_model_overrides
                    .get("123")
                    .map(String::as_str),
                Some("gpt-5.4")
            );
            assert_eq!(
                loaded
                    .channel_model_overrides
                    .get("456")
                    .map(String::as_str),
                Some("sonnet")
            );
        });
    }

    #[test]
    fn test_save_bot_settings_persists_allowed_channel_ids() {
        with_temp_home(|_temp_home: &TempDir| {
            let token = "test-token";
            let mut settings = super::super::DiscordBotSettings::default();
            settings.allowed_channel_ids = vec![123, 456];

            save_bot_settings(token, &settings);

            let loaded = load_bot_settings(token);
            assert_eq!(loaded.allowed_channel_ids, vec![123, 456]);
        });
    }

    #[test]
    fn test_load_bot_settings_reads_agent_identity() {
        with_temp_home(|temp_home: &TempDir| {
            let settings_dir = temp_home.path().join(".adk").join("config");
            fs::create_dir_all(&settings_dir).unwrap();
            let token = "test-token";
            let key = discord_token_hash(token);
            let json = serde_json::json!({
                key: {
                    "token": token,
                    "agent": "spark"
                }
            });
            fs::write(
                settings_dir.join("bot_settings.json"),
                serde_json::to_string_pretty(&json).unwrap(),
            )
            .unwrap();

            let settings = load_bot_settings(token);
            assert_eq!(settings.agent.as_deref(), Some("spark"));
        });
    }

    #[test]
    fn test_save_bot_settings_persists_agent_identity() {
        with_temp_home(|_temp_home: &TempDir| {
            let token = "test-token";
            let mut settings = super::super::DiscordBotSettings::default();
            settings.agent = Some("codex".to_string());

            save_bot_settings(token, &settings);

            let loaded = load_bot_settings(token);
            assert_eq!(loaded.agent.as_deref(), Some("codex"));
        });
    }

    #[test]
    fn test_resolve_role_binding_reads_optional_provider() {
        with_temp_home(|temp_home: &TempDir| {
            let settings_dir = temp_home.path().join(".adk").join("config");
            fs::create_dir_all(&settings_dir).unwrap();
            fs::write(
                settings_dir.join("role_map.json"),
                serde_json::to_string_pretty(&serde_json::json!({
                    "version": 1,
                    "byChannelId": {
                        "123": {
                            "roleId": "family-routine",
                            "promptFile": "/tmp/family-routine.prompt.md",
                            "provider": "codex"
                        }
                    }
                }))
                .unwrap(),
            )
            .unwrap();

            let binding = resolve_role_binding(ChannelId::new(123), Some("쇼핑도우미")).unwrap();
            assert_eq!(binding.role_id, "family-routine");
            assert_eq!(binding.provider, Some(ProviderKind::Codex));
            assert!(channel_supports_provider(
                &ProviderKind::Codex,
                Some("쇼핑도우미"),
                false,
                Some(&binding)
            ));
            assert!(!channel_supports_provider(
                &ProviderKind::Claude,
                Some("쇼핑도우미"),
                false,
                Some(&binding)
            ));
        });
    }

    #[test]
    fn test_load_peer_agents_reads_meeting_config() {
        with_temp_home(|temp_home: &TempDir| {
            let settings_dir = temp_home.path().join(".adk").join("config");
            fs::create_dir_all(&settings_dir).unwrap();
            let json = serde_json::json!({
                "meeting": {
                    "available_agents": [
                        {
                            "role_id": "ch-td",
                            "display_name": "TD (테크니컬 디렉터)",
                            "keywords": ["아키텍처", "코드", "성능"]
                        },
                        {
                            "role_id": "ch-pd",
                            "display_name": "PD (프로덕트 디렉터)",
                            "keywords": ["제품", "로드맵"]
                        }
                    ]
                }
            });
            fs::write(
                settings_dir.join("role_map.json"),
                serde_json::to_string_pretty(&json).unwrap(),
            )
            .unwrap();

            let agents = load_peer_agents();
            assert_eq!(agents.len(), 2);
            assert_eq!(agents[0].role_id, "ch-td");
            assert_eq!(agents[1].display_name, "PD (프로덕트 디렉터)");
        });
    }

    #[test]
    fn test_render_peer_agent_guidance_excludes_current_role() {
        with_temp_home(|temp_home: &TempDir| {
            let settings_dir = temp_home.path().join(".adk").join("config");
            fs::create_dir_all(&settings_dir).unwrap();
            let json = serde_json::json!({
                "meeting": {
                    "available_agents": [
                        {
                            "role_id": "ch-td",
                            "display_name": "TD (테크니컬 디렉터)",
                            "keywords": ["아키텍처", "코드", "성능"]
                        },
                        {
                            "role_id": "ch-pd",
                            "display_name": "PD (프로덕트 디렉터)",
                            "keywords": ["제품", "로드맵"]
                        }
                    ]
                }
            });
            fs::write(
                settings_dir.join("role_map.json"),
                serde_json::to_string_pretty(&json).unwrap(),
            )
            .unwrap();

            let rendered = render_peer_agent_guidance("ch-pd").unwrap();
            assert!(rendered.contains("ch-td"));
            assert!(!rendered.contains("ch-pd (PD"));
            assert!(rendered.contains("name the 1-2 most suitable peer agents"));
        });
    }

    // ── P0 tests ─────────────────────────────────────────────────────────

    #[test]
    fn test_discord_token_hash_sha256_correct() {
        let hash = discord_token_hash("my-bot-token");
        // Must start with "discord_" prefix
        assert!(hash.starts_with("discord_"));
        // After prefix: 16 hex chars (8 bytes of SHA-256)
        let hex_part = &hash["discord_".len()..];
        assert_eq!(hex_part.len(), 16);
        assert!(hex_part.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_discord_token_hash_reproducible() {
        let hash1 = discord_token_hash("same-token-abc");
        let hash2 = discord_token_hash("same-token-abc");
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_discord_token_hash_different_tokens() {
        let hash1 = discord_token_hash("token-alpha");
        let hash2 = discord_token_hash("token-beta");
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_channel_supports_provider_dm_always_true() {
        // DM → all supported providers should return true
        assert!(channel_supports_provider(
            &ProviderKind::Claude,
            None,
            true,
            None,
        ));
        assert!(channel_supports_provider(
            &ProviderKind::Codex,
            None,
            true,
            None,
        ));
    }

    #[test]
    fn test_bot_settings_allow_channel_honors_allowlist() {
        let mut settings = super::super::DiscordBotSettings::default();
        settings.allowed_channel_ids = vec![1488022491992424448];

        assert!(bot_settings_allow_channel(
            &settings,
            ChannelId::new(1488022491992424448),
            false
        ));
        assert!(!bot_settings_allow_channel(
            &settings,
            ChannelId::new(1486017489027469493),
            false
        ));
        assert!(bot_settings_allow_channel(
            &settings,
            ChannelId::new(999),
            true
        ));
    }

    #[test]
    fn test_bot_settings_allow_agent_requires_matching_role_binding() {
        let mut settings = super::super::DiscordBotSettings::default();
        settings.agent = Some("codex".to_string());

        let codex_binding = super::RoleBinding {
            role_id: "codex".to_string(),
            prompt_file: "/tmp/codex.md".to_string(),
            provider: Some(ProviderKind::Codex),
            model: None,
            reasoning_effort: None,
            peer_agents_enabled: false,
        };
        let spark_binding = super::RoleBinding {
            role_id: "spark".to_string(),
            prompt_file: "/tmp/spark.md".to_string(),
            provider: Some(ProviderKind::Codex),
            model: None,
            reasoning_effort: None,
            peer_agents_enabled: false,
        };

        assert!(bot_settings_allow_agent(
            &settings,
            Some(&codex_binding),
            false
        ));
        assert!(!bot_settings_allow_agent(
            &settings,
            Some(&spark_binding),
            false
        ));
        assert!(!bot_settings_allow_agent(&settings, None, false));
        assert!(bot_settings_allow_agent(&settings, None, true));
    }

    #[test]
    fn test_validate_bot_channel_routing_reports_channel_not_allowed() {
        let mut settings = super::super::DiscordBotSettings::default();
        settings.allowed_channel_ids = vec![1488022491992424448];

        let result = validate_bot_channel_routing(
            &settings,
            &ProviderKind::Codex,
            ChannelId::new(1486017489027469493),
            Some("agentdesk-codex"),
            false,
        );

        assert_eq!(
            result,
            Err(BotChannelRoutingGuardFailure::ChannelNotAllowed)
        );
    }

    #[test]
    fn test_channel_supports_provider_cc_claude_only() {
        use super::RoleBinding;

        let binding = RoleBinding {
            role_id: "test-role".to_string(),
            prompt_file: "/tmp/test.md".to_string(),
            provider: Some(ProviderKind::Claude),
            model: None,
            reasoning_effort: None,
            peer_agents_enabled: true,
        };

        // With a role binding specifying Claude, only Claude should match
        assert!(channel_supports_provider(
            &ProviderKind::Claude,
            Some("test-cc"),
            false,
            Some(&binding),
        ));
        assert!(!channel_supports_provider(
            &ProviderKind::Codex,
            Some("test-cc"),
            false,
            Some(&binding),
        ));
    }

    #[test]
    fn test_channel_supports_provider_org_schema_disables_generic_claude_fallback() {
        with_temp_home(|temp_home: &TempDir| {
            let settings_dir = temp_home.path().join(".adk").join("config");
            fs::create_dir_all(&settings_dir).unwrap();
            fs::write(
                settings_dir.join("org.yaml"),
                r#"
version: 1
name: "Test Org"
agents:
  claude:
    display_name: "claude"
    provider: claude
channels:
  by_name:
    enabled: true
    mappings:
      "agentdesk-claude":
        agent: claude
        provider: claude
"#,
            )
            .unwrap();

            assert!(!channel_supports_provider(
                &ProviderKind::Claude,
                Some("random-general"),
                false,
                None,
            ));
            assert!(!channel_supports_provider(
                &ProviderKind::Codex,
                Some("random-general"),
                false,
                None,
            ));
        });
    }

    #[test]
    fn test_channel_supports_provider_org_schema_suffix_match_still_works() {
        with_temp_home(|temp_home: &TempDir| {
            let settings_dir = temp_home.path().join(".adk").join("config");
            fs::create_dir_all(&settings_dir).unwrap();
            fs::write(
                settings_dir.join("org.yaml"),
                r#"
version: 1
name: "Test Org"
agents:
  codex:
    display_name: "codex"
    provider: codex
suffix_map:
  "-cdx": "codex"
"#,
            )
            .unwrap();

            assert!(channel_supports_provider(
                &ProviderKind::Codex,
                Some("agentdesk-cdx"),
                false,
                None,
            ));
            assert!(!channel_supports_provider(
                &ProviderKind::Claude,
                Some("agentdesk-cdx"),
                false,
                None,
            ));
        });
    }
}
