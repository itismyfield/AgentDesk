use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use serenity::ChannelId;
use sha2::{Digest, Sha256};

use poise::serenity_prelude as serenity;

use crate::runtime_layout;
use crate::services::agent_protocol::DEFAULT_ALLOWED_TOOLS;
use crate::services::provider::ProviderKind;

mod content;
mod memory;
mod read;
mod validation;
mod write;

use super::DiscordBotSettings;
use super::agentdesk_config;
use super::formatting::normalize_allowed_tools;
use super::org_schema;
use super::role_map::{
    is_known_agent as is_known_agent_from_role_map,
    list_registered_channel_bindings as list_registered_channel_bindings_from_role_map,
    load_peer_agents as load_peer_agents_from_role_map,
    load_shared_prompt_path as load_shared_prompt_path_from_role_map,
    resolve_role_binding as resolve_role_binding_from_role_map,
    resolve_workspace as resolve_workspace_from_role_map,
};
use super::runtime_store;
use super::runtime_store::{bot_settings_path, discord_uploads_root};

pub(crate) use content::load_longterm_memory_catalog;
pub(super) use content::{
    channel_upload_dir, cleanup_channel_uploads, cleanup_old_uploads, is_known_agent,
    load_review_tuning_guidance, load_role_prompt, load_shared_prompt_for_profile,
    render_peer_agent_guidance,
};
pub(crate) use memory::{memory_settings_for_binding, resolve_memory_settings};
pub(super) use read::{load_bot_settings, load_last_session_path, save_last_session_runtime};
pub use read::{
    load_discord_bot_launch_configs, resolve_discord_bot_provider, resolve_discord_token_by_hash,
};
pub(crate) use validation::list_registered_channel_bindings;
// #2047 Finding 5 — `resolve_role_binding` is exposed crate-wide so the HTTP
// routes layer (`src/server/routes/discord.rs`) can deny `/api/discord/channels/*`
// proxy lookups for channels that are not registered with this AgentDesk
// instance. All other validation helpers remain super-scoped.
pub(crate) use validation::resolve_role_binding;
pub(crate) use validation::resolve_workspace;
pub(super) use validation::{
    BotChannelRoutingGuardFailure, bot_settings_allow_channel, channel_supports_provider,
    has_configured_channel_binding, resolve_cache_ttl_minutes, resolve_dispatch_profile,
    validate_bot_channel_routing, validate_bot_channel_routing_with_provider_channel,
};
pub(super) use write::save_bot_settings;

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
    // Qwen validates `allowed_tools` against its own QWEN_SUPPORTED_ALLOWED_TOOLS list at
    // session start and errors on unknown entries. The shared DEFAULT_ALLOWED_TOOLS now
    // includes Claude-only tools (Monitor, BashOutput, KillBash, SlashCommand) that Qwen
    // does not recognize, so a Qwen bot with an omitted `allowed_tools` field would fail
    // to launch. Hand Qwen its own supported list instead.
    let source: &[&str] = match provider {
        ProviderKind::Qwen => crate::services::qwen::QWEN_SUPPORTED_ALLOWED_TOOLS,
        _ => DEFAULT_ALLOWED_TOOLS,
    };
    source.iter().map(|tool| (*tool).to_string()).collect()
}

fn find_bot_settings_entry<'a>(
    obj: &'a serde_json::Map<String, serde_json::Value>,
    token: &str,
) -> Option<(&'a String, &'a serde_json::Value)> {
    let canonical_key = discord_token_hash(token);
    if let Some((key, entry)) = obj.get_key_value(&canonical_key) {
        return Some((key, entry));
    }

    obj.iter().find(|(_, entry)| {
        entry
            .get("token")
            .and_then(|value| value.as_str())
            .map(|value| value == token)
            .unwrap_or(false)
    })
}

fn last_session_path_key(token_hash: &str, channel_id: u64) -> String {
    format!("discord:last_session:{token_hash}:{channel_id}")
}

fn last_remote_profile_key(token_hash: &str, channel_id: u64) -> String {
    format!("discord:last_remote:{token_hash}:{channel_id}")
}

#[derive(Clone, Debug)]
pub(crate) struct RoleBinding {
    pub role_id: String,
    pub prompt_file: String,
    pub provider: Option<ProviderKind>,
    /// Optional model override (e.g. "opus", "sonnet", "haiku", "o3")
    pub model: Option<String>,
    /// Optional reasoning effort for Codex (e.g. "low", "normal", "high", "xhigh")
    // #3034: config field carried through the role binding; the consumer that
    // applies per-role reasoning effort is not wired yet (the Codex path reads
    // effort from dispatch options today).
    #[allow(dead_code)]
    pub reasoning_effort: Option<String>,
    /// Whether this role may see peer-agent handoff guidance in the system prompt.
    pub peer_agents_enabled: bool,
    /// Whether hourly agent quality feedback may be injected into the system prompt.
    pub quality_feedback_injection_enabled: bool,
    pub memory: ResolvedMemorySettings,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RegisteredChannelBinding {
    pub channel_id: u64,
    pub owner_provider: ProviderKind,
    pub fallback_name: Option<String>,
}

const DEFAULT_MEMORY_RECALL_TIMEOUT_MS: u64 = 500;
const DEFAULT_MEMORY_CAPTURE_TIMEOUT_MS: u64 = 5_000;
const MIN_MEMORY_RECALL_TIMEOUT_MS: u64 = 100;
const MAX_MEMORY_RECALL_TIMEOUT_MS: u64 = 2_000;
const MIN_MEMORY_CAPTURE_TIMEOUT_MS: u64 = 500;
const MAX_MEMORY_CAPTURE_TIMEOUT_MS: u64 = 30_000;

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct MemoryConfigOverride {
    pub backend: Option<String>,
    pub recall_timeout_ms: Option<u64>,
    pub capture_timeout_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum MemoryBackendKind {
    #[default]
    File,
    Memento,
}

impl MemoryBackendKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Memento => "memento",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ResolvedMemorySettings {
    pub backend: MemoryBackendKind,
    /// True only when the resolved `backend` is `File` because memento was the
    /// requested/configured backend but is degraded (unreachable). Lets the
    /// guidance layer distinguish a deliberate file backend from a transparent
    /// memento fallback, which have different write policies. Always false for a
    /// deliberately configured file backend and for an active memento backend.
    pub memento_fallback: bool,
    pub recall_timeout_ms: u64,
    pub capture_timeout_ms: u64,
}

impl Default for ResolvedMemorySettings {
    fn default() -> Self {
        Self {
            backend: MemoryBackendKind::File,
            memento_fallback: false,
            recall_timeout_ms: DEFAULT_MEMORY_RECALL_TIMEOUT_MS,
            capture_timeout_ms: DEFAULT_MEMORY_CAPTURE_TIMEOUT_MS,
        }
    }
}

fn clamp_timeout(name: &str, value: u64, min: u64, max: u64, default: u64) -> u64 {
    let clamped = value.clamp(min, max);
    if value != clamped {
        eprintln!(
            "  [memory] Warning: {name}={} is out of range; clamping to {clamped}",
            value
        );
    }
    if clamped == 0 { default } else { clamped }
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

fn config_path_for_write() -> Option<PathBuf> {
    let explicit = std::env::var_os("AGENTDESK_CONFIG")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty());
    let runtime_root = crate::config::runtime_root();
    let cwd = std::env::current_dir().ok();
    let home_dir = dirs::home_dir();

    Some(resolve_config_path_for_write(
        explicit,
        runtime_root,
        cwd,
        home_dir,
    ))
}

fn resolve_config_path_for_write(
    explicit: Option<PathBuf>,
    runtime_root: Option<PathBuf>,
    cwd: Option<PathBuf>,
    home_dir: Option<PathBuf>,
) -> PathBuf {
    if let Some(path) = explicit {
        if path.exists() {
            return path;
        }

        let mut candidates = Vec::new();
        if let Some(root) = runtime_root.as_ref() {
            let canonical = runtime_layout::config_file_path(root);
            let legacy = runtime_layout::legacy_config_file_path(root);
            if path == legacy {
                candidates.push(canonical);
            } else if path == canonical {
                candidates.push(legacy);
            }
        }

        if path.file_name() == Some(OsStr::new("agentdesk.yaml")) {
            if let Some(parent) = path.parent() {
                if parent.file_name() == Some(OsStr::new("config")) {
                    if let Some(root) = parent.parent() {
                        let legacy = root.join("agentdesk.yaml");
                        if legacy != path {
                            candidates.push(legacy);
                        }
                    }
                } else {
                    let canonical = parent.join("config").join("agentdesk.yaml");
                    if canonical != path {
                        candidates.push(canonical);
                    }
                }
            }
        }

        if let Some(candidate) = candidates.into_iter().find(|candidate| candidate.exists()) {
            return candidate;
        }
        return path;
    }

    if let Some(root) = runtime_root.as_ref() {
        for path in [
            runtime_layout::config_file_path(root),
            runtime_layout::legacy_config_file_path(root),
        ] {
            if path.exists() {
                return path;
            }
        }
    }

    if let Some(dir) = cwd.as_ref() {
        for path in [
            dir.join("config").join("agentdesk.yaml"),
            dir.join("agentdesk.yaml"),
        ] {
            if path.exists() {
                return path;
            }
        }
    }

    if let Some(home) = home_dir.as_ref() {
        let release_root = home.join(".adk").join("release");
        for path in [
            runtime_layout::config_file_path(&release_root),
            runtime_layout::legacy_config_file_path(&release_root),
        ] {
            if path.exists() {
                return path;
            }
        }
    }

    runtime_root
        .map(|root| runtime_layout::config_file_path(&root))
        .or_else(|| cwd.map(|dir| dir.join("config").join("agentdesk.yaml")))
        .unwrap_or_else(|| PathBuf::from("config").join("agentdesk.yaml"))
}

fn resolved_config_bot_name(config: &crate::config::Config, token: &str) -> Option<String> {
    let mut bot_names = config.discord.bots.keys().cloned().collect::<Vec<_>>();
    bot_names.sort();
    bot_names.into_iter().find(|name| {
        config
            .discord
            .bots
            .get(name)
            .and_then(|bot| resolve_bot_token(name, bot))
            .as_deref()
            == Some(token)
    })
}

fn resolve_bot_token(bot_name: &str, bot: &crate::config::BotConfig) -> Option<String> {
    bot.token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .or_else(|| crate::credential::read_bot_token(bot_name))
}
