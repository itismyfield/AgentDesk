use crate::voice::{VoiceConfig, barge_in::BargeInSensitivity};
use anyhow::{Context, Result};
use serde::de::Deserializer;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub server: ServerConfig,
    #[serde(default)]
    pub discord: DiscordConfig,
    #[serde(default, skip_serializing_if = "VoiceConfig::is_default")]
    pub voice: VoiceConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub review_mcp_allowlist: Vec<String>,
    #[serde(default)]
    pub agents: Vec<AgentDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meeting: Option<MeetingSettings>,
    #[serde(default)]
    pub github: GitHubConfig,
    #[serde(default)]
    pub policies: PoliciesConfig,
    #[serde(default)]
    pub data: DataConfig,
    #[serde(default)]
    pub database: DatabaseConfig,
    #[serde(default, skip_serializing_if = "ClusterConfig::is_default")]
    pub cluster: ClusterConfig,
    #[serde(default, skip_serializing_if = "KanbanConfig::is_empty")]
    pub kanban: KanbanConfig,
    #[serde(default, skip_serializing_if = "ReviewConfig::is_empty")]
    pub review: ReviewConfig,
    #[serde(default, skip_serializing_if = "PlaceholderConfig::is_default")]
    pub placeholder: PlaceholderConfig,
    #[serde(default, skip_serializing_if = "RuntimeSettingsConfig::is_empty")]
    pub runtime: RuntimeSettingsConfig,
    #[serde(default, skip_serializing_if = "AutomationConfig::is_empty")]
    pub automation: AutomationConfig,
    #[serde(default, skip_serializing_if = "RoutinesConfig::is_default")]
    pub routines: RoutinesConfig,
    #[serde(default, skip_serializing_if = "EscalationConfig::is_empty")]
    pub escalation: EscalationConfig,
    #[serde(default, skip_serializing_if = "OnboardingConfig::is_empty")]
    pub onboarding: OnboardingConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemoryConfig>,
    #[serde(default, skip_serializing_if = "McpConfig::is_default")]
    pub mcp: McpConfig,
    #[serde(
        default,
        skip_serializing_if = "PromptManifestRetentionConfig::is_default"
    )]
    pub prompt_manifest_retention: PromptManifestRetentionConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct McpConfig {
    /// When true, AgentDesk watches the Claude MCP credential / config files
    /// (`~/.claude.json`, `~/.claude/.mcp.json`, `~/.claude/.credentials.json`,
    /// honoring `$CLAUDE_CONFIG_DIR` if set) and posts a notification to all
    /// active Claude sessions when they change so the operator can run
    /// `/restart` to pick up newly-authenticated MCP servers.
    #[serde(default = "default_true")]
    pub watch_credentials: bool,
    /// Per-channel cooldown between credential-change notifications, in seconds.
    /// Defaults to 300s (5 minutes).
    #[serde(default = "default_credential_notify_dedupe_secs")]
    pub credential_notify_dedupe_secs: u64,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            watch_credentials: true,
            credential_notify_dedupe_secs: default_credential_notify_dedupe_secs(),
        }
    }
}

impl McpConfig {
    pub fn is_default(&self) -> bool {
        *self == McpConfig::default()
    }
}

fn default_credential_notify_dedupe_secs() -> u64 {
    300
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default)]
    pub auth_token: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct DiscordConfig {
    #[serde(default)]
    pub bots: std::collections::HashMap<String, BotConfig>,
    #[serde(default)]
    pub guild_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dm_default_agent: Option<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_u64",
        skip_serializing_if = "Option::is_none"
    )]
    pub owner_id: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct BotConfig {
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "DiscordBotAuthConfig::is_empty")]
    pub auth: DiscordBotAuthConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct DiscordBotAuthConfig {
    #[serde(
        default,
        deserialize_with = "deserialize_optional_u64_vec",
        skip_serializing_if = "Option::is_none"
    )]
    pub allowed_channel_ids: Option<Vec<u64>>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_u64_vec",
        skip_serializing_if = "Option::is_none"
    )]
    pub require_mention_channel_ids: Option<Vec<u64>>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_u64_vec",
        skip_serializing_if = "Option::is_none"
    )]
    pub allowed_user_ids: Option<Vec<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_all_users: Option<bool>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_u64_vec",
        skip_serializing_if = "Option::is_none"
    )]
    pub allowed_bot_ids: Option<Vec<u64>>,
}

impl DiscordBotAuthConfig {
    pub fn is_empty(&self) -> bool {
        self.allowed_channel_ids.is_none()
            && self.require_mention_channel_ids.is_none()
            && self.allowed_user_ids.is_none()
            && self.allowed_tools.is_none()
            && self.allow_all_users.is_none()
            && self.allowed_bot_ids.is_none()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum U64Like {
    Int(u64),
    String(String),
}

impl U64Like {
    fn into_u64(self) -> Option<u64> {
        match self {
            Self::Int(value) => Some(value),
            Self::String(raw) => raw.trim().parse::<u64>().ok(),
        }
    }
}

fn deserialize_optional_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<U64Like>::deserialize(deserializer)?.and_then(U64Like::into_u64))
}

fn deserialize_optional_u64_vec<'de, D>(deserializer: D) -> Result<Option<Vec<u64>>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(
        Option::<Vec<U64Like>>::deserialize(deserializer)?.map(|values| {
            values
                .into_iter()
                .filter_map(U64Like::into_u64)
                .collect::<Vec<_>>()
        }),
    )
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentDef {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub name_ko: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_word: Option<String>,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub voice_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensitivity_mode: Option<BargeInSensitivity>,
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default, skip_serializing_if = "AgentChannels::is_empty")]
    pub channels: AgentChannels,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub department: Option<String>,
    #[serde(default)]
    pub avatar_emoji: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct McpServerConfig {
    #[serde(default)]
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<McpServerAuthConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct McpServerAuthConfig {
    #[serde(rename = "type")]
    pub auth_type: McpServerAuthType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env_var: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum McpServerAuthType {
    Bearer,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct AgentChannels {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<AgentChannel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<AgentChannel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gemini: Option<AgentChannel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opencode: Option<AgentChannel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qwen: Option<AgentChannel>,
}

impl AgentChannels {
    pub fn is_empty(&self) -> bool {
        self.claude
            .as_ref()
            .and_then(AgentChannel::target)
            .is_none()
            && self.codex.as_ref().and_then(AgentChannel::target).is_none()
            && self
                .gemini
                .as_ref()
                .and_then(AgentChannel::target)
                .is_none()
            && self
                .opencode
                .as_ref()
                .and_then(AgentChannel::target)
                .is_none()
            && self.qwen.as_ref().and_then(AgentChannel::target).is_none()
    }

    pub fn iter(&self) -> [(&'static str, Option<&AgentChannel>); 5] {
        [
            ("claude", self.claude.as_ref()),
            ("codex", self.codex.as_ref()),
            ("gemini", self.gemini.as_ref()),
            ("opencode", self.opencode.as_ref()),
            ("qwen", self.qwen.as_ref()),
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum AgentChannel {
    Legacy(String),
    Detailed(AgentChannelConfig),
}

impl From<String> for AgentChannel {
    fn from(value: String) -> Self {
        Self::Legacy(value)
    }
}

impl From<&str> for AgentChannel {
    fn from(value: &str) -> Self {
        Self::Legacy(value.to_string())
    }
}

impl AgentChannel {
    pub fn target(&self) -> Option<String> {
        match self {
            Self::Legacy(raw) => normalized_channel_value(Some(raw.clone())),
            Self::Detailed(config) => config.target(),
        }
    }

    pub fn channel_id(&self) -> Option<String> {
        match self {
            Self::Legacy(raw) => normalized_channel_value(Some(raw.clone()))
                .filter(|value| value.parse::<u64>().is_ok()),
            Self::Detailed(config) => config.channel_id(),
        }
    }

    pub fn channel_name(&self) -> Option<String> {
        match self {
            Self::Legacy(raw) => {
                let value = normalized_channel_value(Some(raw.clone()))?;
                (value.parse::<u64>().is_err()).then_some(value)
            }
            Self::Detailed(config) => config.channel_name(),
        }
    }

    pub fn aliases(&self) -> Vec<String> {
        match self {
            Self::Legacy(raw) => normalized_channel_value(Some(raw.clone()))
                .into_iter()
                .filter(|value| value.parse::<u64>().is_err())
                .collect(),
            Self::Detailed(config) => config.all_names(),
        }
    }

    pub fn prompt_file(&self) -> Option<String> {
        match self {
            Self::Legacy(_) => None,
            Self::Detailed(config) => normalized_channel_value(config.prompt_file.clone()),
        }
    }

    pub fn workspace(&self) -> Option<String> {
        match self {
            Self::Legacy(_) => None,
            Self::Detailed(config) => normalized_channel_value(config.workspace.clone()),
        }
    }

    pub fn provider(&self) -> Option<String> {
        match self {
            Self::Legacy(_) => None,
            Self::Detailed(config) => normalized_channel_value(config.provider.clone()),
        }
    }

    pub fn model(&self) -> Option<String> {
        match self {
            Self::Legacy(_) => None,
            Self::Detailed(config) => normalized_channel_value(config.model.clone()),
        }
    }

    pub fn reasoning_effort(&self) -> Option<String> {
        match self {
            Self::Legacy(_) => None,
            Self::Detailed(config) => normalized_channel_value(config.reasoning_effort.clone()),
        }
    }

    pub fn peer_agents(&self) -> Option<bool> {
        match self {
            Self::Legacy(_) => None,
            Self::Detailed(config) => config.peer_agents,
        }
    }

    pub fn quality_feedback_injection(&self) -> Option<bool> {
        match self {
            Self::Legacy(_) => None,
            Self::Detailed(config) => config.quality_feedback_injection,
        }
    }

    pub fn dispatch_profile(&self) -> Option<String> {
        match self {
            Self::Legacy(_) => None,
            Self::Detailed(config) => normalize_dispatch_profile(config.dispatch_profile.clone()),
        }
    }

    /// Returns the configured prompt-cache TTL in minutes, but only if it is a
    /// supported bucket (5 or 60). Anything else maps to `None` so the default
    /// 5-minute TTL is used.
    pub fn cache_ttl_minutes(&self) -> Option<u32> {
        match self {
            Self::Legacy(_) => None,
            Self::Detailed(config) => normalize_cache_ttl_minutes(config.cache_ttl_minutes),
        }
    }
}

/// Validate the cache TTL minutes config value (#1088).
/// Only `Some(5)` and `Some(60)` are accepted; other values return `None`.
pub fn normalize_cache_ttl_minutes(value: Option<u32>) -> Option<u32> {
    match value {
        Some(5) | Some(60) => value,
        _ => None,
    }
}

pub fn normalize_dispatch_profile(value: Option<String>) -> Option<String> {
    match value?.trim().to_ascii_lowercase().as_str() {
        "lite" => Some("lite".to_string()),
        "full" | "off" | "default" => Some("full".to_string()),
        _ => None,
    }
}

pub fn is_valid_dispatch_profile(value: &str) -> bool {
    normalize_dispatch_profile(Some(value.to_string())).is_some()
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct AgentChannelConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(
        default,
        alias = "channel_name",
        skip_serializing_if = "Option::is_none"
    )]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_agents: Option<bool>,
    #[serde(
        default,
        alias = "qualityFeedbackInjection",
        skip_serializing_if = "Option::is_none"
    )]
    pub quality_feedback_injection: Option<bool>,
    #[serde(
        default,
        alias = "dispatchProfile",
        skip_serializing_if = "Option::is_none"
    )]
    pub dispatch_profile: Option<String>,
    /// Anthropic prompt-cache TTL bucket (#1088). Only `5` (default) or `60`
    /// minutes are valid. Any other value is treated as `None` (default 5m).
    /// When set to `60`, the Claude CLI is invoked with the extended 1h
    /// cache TTL via the `CLAUDE_CODE_EXTENDED_CACHE_TTL` env var so the
    /// underlying API call uses `cache_control.ttl = "1h"`.
    #[serde(
        default,
        alias = "cacheTtlMinutes",
        skip_serializing_if = "Option::is_none"
    )]
    pub cache_ttl_minutes: Option<u32>,
}

impl AgentChannelConfig {
    pub fn target(&self) -> Option<String> {
        self.channel_id()
            .or_else(|| self.channel_name())
            .or_else(|| {
                self.aliases
                    .iter()
                    .find_map(|alias| normalized_channel_value(Some(alias.clone())))
            })
    }

    pub fn channel_id(&self) -> Option<String> {
        normalized_channel_value(self.id.clone()).filter(|value| value.parse::<u64>().is_ok())
    }

    pub fn channel_name(&self) -> Option<String> {
        normalized_channel_value(self.name.clone())
    }

    pub fn all_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        if let Some(name) = self.channel_name() {
            names.push(name);
        }
        for alias in &self.aliases {
            if let Some(alias) = normalized_channel_value(Some(alias.clone())) {
                if !names.contains(&alias) {
                    names.push(alias);
                }
            }
        }
        names
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MeetingSettings {
    pub channel_name: String,
    #[serde(default)]
    pub max_rounds: Option<u32>,
    #[serde(
        default,
        alias = "maxParticipants",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_participants: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_agent: Option<MeetingSummaryAgentDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_agents: Vec<MeetingAgentEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum MeetingSummaryAgentDef {
    Static(String),
    Dynamic {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        rules: Vec<MeetingSummaryRuleDef>,
        default: String,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MeetingSummaryRuleDef {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    pub agent: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum MeetingAgentEntry {
    RoleId(String),
    Detailed(MeetingAgentDef),
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct MeetingAgentDef {
    pub role_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_file: Option<String>,
    #[serde(
        default,
        alias = "domainSummary",
        skip_serializing_if = "Option::is_none"
    )]
    pub domain_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub strengths: Vec<String>,
    #[serde(default, alias = "taskTypes", skip_serializing_if = "Vec::is_empty")]
    pub task_types: Vec<String>,
    #[serde(default, alias = "antiSignals", skip_serializing_if = "Vec::is_empty")]
    pub anti_signals: Vec<String>,
    #[serde(
        default,
        alias = "providerHint",
        skip_serializing_if = "Option::is_none"
    )]
    pub provider_hint: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GitHubConfig {
    #[serde(default)]
    pub repos: Vec<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub repo_dirs: std::collections::BTreeMap<String, String>,
    #[serde(default = "default_sync_interval")]
    pub sync_interval_minutes: u64,
}

impl Default for GitHubConfig {
    fn default() -> Self {
        Self {
            repos: Vec::new(),
            repo_dirs: std::collections::BTreeMap::new(),
            sync_interval_minutes: default_sync_interval(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PoliciesConfig {
    #[serde(default = "default_policies_dir")]
    pub dir: PathBuf,
    #[serde(default = "default_true")]
    pub hot_reload: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DataConfig {
    #[serde(default = "default_data_dir")]
    pub dir: PathBuf,
    #[serde(default = "default_db_name")]
    pub db_name: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct DatabaseConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_database_host")]
    pub host: String,
    #[serde(default = "default_database_port")]
    pub port: u16,
    #[serde(default = "default_database_name")]
    pub dbname: String,
    #[serde(default = "default_database_user")]
    pub user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default = "default_database_pool_max")]
    pub pool_max: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct ClusterConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    #[serde(default = "default_cluster_role")]
    pub role: String,
    #[serde(default = "default_cluster_heartbeat_interval_secs")]
    pub heartbeat_interval_secs: u64,
    #[serde(default = "default_cluster_lease_ttl_secs")]
    pub lease_ttl_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub capabilities: serde_json::Map<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub nodes: BTreeMap<String, ClusterNodeConfig>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub blackout_windows: BTreeMap<String, Vec<ClusterBlackoutWindowConfig>>,
    #[serde(
        default,
        skip_serializing_if = "ClusterDispatchRoutingConfig::is_default"
    )]
    pub dispatch_routing: ClusterDispatchRoutingConfig,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub semaphores: BTreeMap<String, ClusterSemaphoreConfig>,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            instance_id: None,
            role: default_cluster_role(),
            heartbeat_interval_secs: default_cluster_heartbeat_interval_secs(),
            lease_ttl_secs: default_cluster_lease_ttl_secs(),
            api_base_url: None,
            labels: Vec::new(),
            capabilities: serde_json::Map::new(),
            nodes: BTreeMap::new(),
            blackout_windows: BTreeMap::new(),
            dispatch_routing: ClusterDispatchRoutingConfig::default(),
            semaphores: BTreeMap::new(),
        }
    }
}

impl ClusterConfig {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct ClusterNodeConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_dispatches: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct ClusterBlackoutWindowConfig {
    pub start: String,
    pub end: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct ClusterDispatchRoutingConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_preferred_labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub opt_out_dispatch_types: Vec<String>,
    #[serde(
        default = "default_dispatch_routing_constraints",
        skip_serializing_if = "is_default_dispatch_routing_constraints"
    )]
    pub constraints: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_timeout_secs: Option<u64>,
    #[serde(
        default = "default_dispatch_routing_wake_interval_secs",
        skip_serializing_if = "is_default_dispatch_routing_wake_interval_secs"
    )]
    pub wake_interval_secs: u64,
}

impl Default for ClusterDispatchRoutingConfig {
    fn default() -> Self {
        Self {
            default_preferred_labels: Vec::new(),
            opt_out_dispatch_types: Vec::new(),
            constraints: default_dispatch_routing_constraints(),
            wait_timeout_secs: None,
            wake_interval_secs: default_dispatch_routing_wake_interval_secs(),
        }
    }
}

impl ClusterDispatchRoutingConfig {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub fn is_opted_out(&self, dispatch_type: &str) -> bool {
        self.opt_out_dispatch_types
            .iter()
            .any(|value| value == dispatch_type)
    }
}

fn default_dispatch_routing_constraints() -> Vec<String> {
    vec![crate::services::dispatches::routing_constraint::NOOP_CONSTRAINT_NAME.to_string()]
}

fn is_default_dispatch_routing_constraints(values: &[String]) -> bool {
    values == default_dispatch_routing_constraints().as_slice()
}

fn default_dispatch_routing_wake_interval_secs() -> u64 {
    30
}

fn is_default_dispatch_routing_wake_interval_secs(value: &u64) -> bool {
    *value == default_dispatch_routing_wake_interval_secs()
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct ClusterSemaphoreConfig {
    #[serde(default = "default_cluster_semaphore_capacity")]
    pub capacity: u32,
    #[serde(default)]
    pub scope: ClusterSemaphoreScope,
}

impl Default for ClusterSemaphoreConfig {
    fn default() -> Self {
        Self {
            capacity: default_cluster_semaphore_capacity(),
            scope: ClusterSemaphoreScope::default(),
        }
    }
}

impl ClusterSemaphoreConfig {
    pub fn effective_capacity(&self) -> i32 {
        self.capacity.clamp(1, 1024) as i32
    }
}

fn default_cluster_semaphore_capacity() -> u32 {
    1
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ClusterSemaphoreScope {
    #[serde(alias = "per_node")]
    PerNode,
    #[serde(alias = "per_cluster")]
    PerCluster,
}

impl Default for ClusterSemaphoreScope {
    fn default() -> Self {
        Self::PerNode
    }
}

impl ClusterSemaphoreScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PerNode => "per-node",
            Self::PerCluster => "per-cluster",
        }
    }

    pub fn scope_key(self, instance_id: &str) -> String {
        match self {
            Self::PerNode => instance_id.to_string(),
            Self::PerCluster => "cluster".to_string(),
        }
    }
}

#[cfg(test)]
mod cluster_semaphore_config_tests {
    use super::{ClusterConfig, ClusterSemaphoreScope};

    #[test]
    fn cluster_semaphores_parse_kebab_scope_and_default_capacity() {
        let config: ClusterConfig = serde_yaml::from_str(
            r#"
enabled: true
semaphores:
  ue_editor:
    capacity: 1
    scope: per-node
  gpu:
    scope: per-cluster
"#,
        )
        .expect("cluster semaphore config parses");

        assert_eq!(
            config.semaphores["ue_editor"].scope,
            ClusterSemaphoreScope::PerNode
        );
        assert_eq!(config.semaphores["ue_editor"].effective_capacity(), 1);
        assert_eq!(
            config.semaphores["gpu"].scope,
            ClusterSemaphoreScope::PerCluster
        );
        assert_eq!(config.semaphores["gpu"].effective_capacity(), 1);
    }

    #[test]
    fn cluster_nodes_parse_max_concurrent_dispatches() {
        let config: ClusterConfig = serde_yaml::from_str(
            r#"
enabled: true
nodes:
  mac-mini-release:
    max_concurrent_dispatches: 4
blackout_windows:
  mac-mini-release:
    - start: "23:00"
      end: "23:30"
      reason: maintenance
dispatch_routing:
  wait_timeout_secs: 600
"#,
        )
        .expect("cluster node config parses");

        assert_eq!(
            config.nodes["mac-mini-release"].max_concurrent_dispatches,
            Some(4)
        );
        assert_eq!(
            config.blackout_windows["mac-mini-release"][0]
                .reason
                .as_deref(),
            Some("maintenance")
        );
        assert_eq!(config.dispatch_routing.wait_timeout_secs, Some(600));
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct KanbanConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manager_channel_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadlock_manager_channel_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_alert_channel_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pm_decision_gate_enabled: Option<bool>,
}

impl KanbanConfig {
    pub fn is_empty(&self) -> bool {
        self.manager_channel_id.is_none()
            && self.deadlock_manager_channel_id.is_none()
            && self.human_alert_channel_id.is_none()
            && self.pm_decision_gate_enabled.is_none()
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct ReviewConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<u32>,
}

impl ReviewConfig {
    pub fn is_empty(&self) -> bool {
        self.enabled.is_none() && self.max_rounds.is_none()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct PlaceholderConfig {
    #[serde(default = "default_true")]
    pub live_events_enabled: bool,
    #[serde(default = "default_true")]
    pub status_panel_v2_enabled: bool,
}

impl Default for PlaceholderConfig {
    fn default() -> Self {
        Self {
            live_events_enabled: true,
            status_panel_v2_enabled: true,
        }
    }
}

impl PlaceholderConfig {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Retention + per-layer size cap for `prompt_manifest_layers` (#1699).
///
/// Storage growth bound: rows older than `full_content_days` get their
/// `full_content` trimmed to NULL by the periodic sweeper, while metadata and
/// `content_sha256` are preserved. Layers whose original content exceeds the
/// per-visibility byte cap are stored truncated at write-time with `is_truncated`
/// set; the hash always reflects the *original* content.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct PromptManifestRetentionConfig {
    /// When false, neither write-time truncation nor the periodic sweeper apply.
    /// Defaults to true so deployments are bounded out of the box.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Keep `full_content` for rows newer than this many days. Older rows have
    /// `full_content` trimmed (set to NULL) by `prompt_manifest_retention` job.
    #[serde(default = "default_prompt_full_content_days")]
    pub full_content_days: u32,
    /// Per-layer maximum stored bytes for `adk_provided` content. Layers larger
    /// than this are stored truncated + flagged; the hash covers the original.
    #[serde(default = "default_prompt_max_bytes_adk_provided")]
    pub per_layer_max_bytes_adk_provided: u64,
    /// Per-layer maximum stored bytes for `user_derived` redacted previews.
    /// User-derived layers already store only a redacted preview, so this is the
    /// upper bound on that preview to prevent pathological growth.
    #[serde(default = "default_prompt_max_bytes_user_derived")]
    pub per_layer_max_bytes_user_derived: u64,
}

impl Default for PromptManifestRetentionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            full_content_days: default_prompt_full_content_days(),
            per_layer_max_bytes_adk_provided: default_prompt_max_bytes_adk_provided(),
            per_layer_max_bytes_user_derived: default_prompt_max_bytes_user_derived(),
        }
    }
}

impl PromptManifestRetentionConfig {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Returns `Some(cap)` when the per-layer max is finite (> 0). A zero or
    /// missing cap means "no truncation for this visibility".
    pub fn cap_for(&self, visibility: PromptManifestVisibilityKind) -> Option<usize> {
        let raw = match visibility {
            PromptManifestVisibilityKind::AdkProvided => self.per_layer_max_bytes_adk_provided,
            PromptManifestVisibilityKind::UserDerived => self.per_layer_max_bytes_user_derived,
        };
        if raw == 0 {
            return None;
        }
        usize::try_from(raw).ok()
    }
}

/// Lightweight enum mirror of `db::prompt_manifests::PromptContentVisibility`
/// — kept here so `config` does not depend on `db`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptManifestVisibilityKind {
    AdkProvided,
    UserDerived,
}

fn default_prompt_full_content_days() -> u32 {
    30
}

fn default_prompt_max_bytes_adk_provided() -> u64 {
    64 * 1024
}

fn default_prompt_max_bytes_user_derived() -> u64 {
    16 * 1024
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct RuntimeSettingsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_timeout_min: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_progress_stale_min: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_turn_alert_interval_min: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_compact_percent: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_compact_percent_codex: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_compact_percent_claude: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_poll_sec: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_sync_sec: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_issue_sync_sec: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_rate_limit_poll_sec: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_rate_limit_poll_sec: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_triage_poll_sec: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ceo_warn_depth: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_entry_retries: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_dispatched_grace_min: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_dispatched_terminal_statuses: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_dispatched_recover_null_dispatch: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_dispatched_recover_missing_dispatch: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_reminder_min: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_warning_pct: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_danger_pct: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_repo_cache_sec: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_stale_sec: Option<u64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub reset_overrides_on_restart: bool,
}

impl RuntimeSettingsConfig {
    pub fn is_empty(&self) -> bool {
        self.requested_timeout_min.is_none()
            && self.in_progress_stale_min.is_none()
            && self.long_turn_alert_interval_min.is_none()
            && self.context_compact_percent.is_none()
            && self.context_compact_percent_codex.is_none()
            && self.context_compact_percent_claude.is_none()
            && self.dispatch_poll_sec.is_none()
            && self.agent_sync_sec.is_none()
            && self.github_issue_sync_sec.is_none()
            && self.claude_rate_limit_poll_sec.is_none()
            && self.codex_rate_limit_poll_sec.is_none()
            && self.issue_triage_poll_sec.is_none()
            && self.ceo_warn_depth.is_none()
            && self.max_retries.is_none()
            && self.max_entry_retries.is_none()
            && self.stale_dispatched_grace_min.is_none()
            && self.stale_dispatched_terminal_statuses.is_none()
            && self.stale_dispatched_recover_null_dispatch.is_none()
            && self.stale_dispatched_recover_missing_dispatch.is_none()
            && self.review_reminder_min.is_none()
            && self.rate_limit_warning_pct.is_none()
            && self.rate_limit_danger_pct.is_none()
            && self.github_repo_cache_sec.is_none()
            && self.rate_limit_stale_sec.is_none()
            && !self.reset_overrides_on_restart
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct AutomationConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_authors: Option<String>,
}

impl AutomationConfig {
    pub fn is_empty(&self) -> bool {
        self.enabled.is_none()
            && self.strategy.is_none()
            && self.strategy_mode.is_none()
            && self.allowed_authors.is_none()
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EscalationMode {
    Pm,
    User,
    Scheduled,
}

impl Default for EscalationMode {
    fn default() -> Self {
        Self::Pm
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct EscalationScheduleConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pm_hours: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

impl EscalationScheduleConfig {
    pub fn is_empty(&self) -> bool {
        self.pm_hours.is_none() && self.timezone.is_none()
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct EscalationConfig {
    #[serde(default)]
    pub mode: EscalationMode,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_u64",
        skip_serializing_if = "Option::is_none"
    )]
    pub owner_user_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pm_channel_id: Option<String>,
    #[serde(default, skip_serializing_if = "EscalationScheduleConfig::is_empty")]
    pub schedule: EscalationScheduleConfig,
}

impl EscalationConfig {
    pub fn is_empty(&self) -> bool {
        self.mode == EscalationMode::Pm
            && self.owner_user_id.is_none()
            && self.pm_channel_id.is_none()
            && self.schedule.is_empty()
    }
}

/// Onboarding wizard / agent-factory rules. Externalizes the values that used
/// to be hardcoded inside the `project-agentfactory` Discord prompt: the
/// active Discord guild ID, the named category IDs new channels can be
/// dropped into, and the channel-name suffix → CLI provider map used by
/// dashboard / Discord wizard auto-detection. (#1110, Epic #912 Phase P6)
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct OnboardingConfig {
    /// Override for `discord.guild_id` when onboarding tooling needs to
    /// target a different guild than the runtime bots. Most setups leave
    /// this empty and reuse `discord.guild_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guild_id: Option<String>,
    /// Named Discord category IDs that the onboarding wizard / factory
    /// agent picks from when creating new channels. Keys are operator-
    /// defined labels (e.g. `dev`, `cookingheart`, `notify`). The value
    /// is the raw Discord category (channel) ID.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub default_categories: std::collections::BTreeMap<String, String>,
    /// Channel-name suffix → CLI provider id. Suffixes match the trailing
    /// portion of a channel name (case-insensitive, leading `-` optional).
    /// When empty, the built-in fallback derived from the provider
    /// registry (`provider_suffix_default_map`) is used.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub provider_suffix_map: std::collections::BTreeMap<String, String>,
    /// Optional list of providers offered to the wizard's provider picker.
    /// When empty, all providers from the built-in registry are offered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_options: Vec<String>,
    /// Default provider id when nothing else (channel suffix, agent_id)
    /// gives a hint. When unset, falls back to the registry's
    /// `default_channel_provider` (currently `claude`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
}

impl OnboardingConfig {
    pub fn is_empty(&self) -> bool {
        self.guild_id.is_none()
            && self.default_categories.is_empty()
            && self.provider_suffix_map.is_empty()
            && self.provider_options.is_empty()
            && self.default_provider.is_none()
    }

    /// Built-in suffix→provider table mirrored from the dashboard
    /// `setupWizardHelpers.ts::PROVIDER_SUFFIX_MAP`. Used as a fallback
    /// when `provider_suffix_map` is unset, so backend and dashboard stay
    /// in lockstep without a config file present.
    pub fn provider_suffix_default_map() -> &'static [(&'static str, &'static str)] {
        &[
            ("-cc", "claude"),
            ("-cdx", "codex"),
            ("-gem", "gemini"),
            ("-gm", "gemini"),
            ("-qw", "qwen"),
            ("-oc", "opencode"),
            ("-cop", "copilot"),
            ("-ag", "antigravity"),
            ("-api", "api"),
        ]
    }

    /// Resolves a provider id from a channel name (or any string with the
    /// suffix-bearing trailing token). Reads `provider_suffix_map` first;
    /// falls back to the built-in default map. Case-insensitive.
    pub fn provider_from_channel_suffix(&self, channel_name: &str) -> Option<String> {
        let lowered = channel_name.trim().to_ascii_lowercase();
        if lowered.is_empty() {
            return None;
        }

        let mut entries: Vec<(String, String)> = self
            .provider_suffix_map
            .iter()
            .map(|(k, v)| (normalize_suffix_key(k), v.trim().to_string()))
            .filter(|(suffix, provider)| !suffix.is_empty() && !provider.is_empty())
            .collect();
        if entries.is_empty() {
            entries = Self::provider_suffix_default_map()
                .iter()
                .map(|(suffix, provider)| ((*suffix).to_string(), (*provider).to_string()))
                .collect();
        }
        // Match longest suffix first so `-cdx` wins over `-x`.
        entries.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
        for (suffix, provider) in entries {
            if lowered.ends_with(&suffix) {
                return Some(provider);
            }
        }
        None
    }

    /// Resolve a category by either a label key (e.g. `dev`) or a raw
    /// numeric Discord category ID. Returns the resolved Discord ID.
    pub fn resolve_category(&self, label_or_id: &str) -> Option<String> {
        let trimmed = label_or_id.trim();
        if trimmed.is_empty() {
            return None;
        }
        if let Some(id) = self.default_categories.get(trimmed) {
            return Some(id.trim().to_string());
        }
        // Treat numeric-looking strings as raw Discord IDs.
        if trimmed.chars().all(|c| c.is_ascii_digit()) {
            return Some(trimmed.to_string());
        }
        None
    }

    /// Effective guild ID: prefer the onboarding override, fall back to
    /// `DiscordConfig::guild_id`. Returns `None` if neither is set.
    pub fn effective_guild_id<'a>(&'a self, discord: &'a DiscordConfig) -> Option<&'a str> {
        self.guild_id
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| discord.guild_id.as_deref().filter(|s| !s.trim().is_empty()))
    }
}

/// Normalise a user-provided suffix key so both `cc` and `-cc` resolve to
/// `-cc`. Empty strings stay empty so the caller can drop them.
fn normalize_suffix_key(raw: &str) -> String {
    let trimmed = raw.trim().trim_start_matches('-').to_ascii_lowercase();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("-{trimmed}")
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct MemoryConfig {
    #[serde(default = "default_memory_backend")]
    pub backend: String,
    #[serde(default)]
    pub file: FileMemoryConfig,
    #[serde(default)]
    pub mcp: McpMemoryConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct FileMemoryConfig {
    #[serde(default = "default_sak_path")]
    pub sak_path: String,
    #[serde(default = "default_sam_path")]
    pub sam_path: String,
    #[serde(default = "default_ltm_root")]
    pub ltm_root: String,
    #[serde(default = "default_auto_memory_root")]
    pub auto_memory_root: String,
}

impl Default for FileMemoryConfig {
    fn default() -> Self {
        Self {
            sak_path: default_sak_path(),
            sam_path: default_sam_path(),
            ltm_root: default_ltm_root(),
            auto_memory_root: default_auto_memory_root(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct McpMemoryConfig {
    pub endpoint: String,
    pub access_key_env: String,
}

/// Compile-time defaults loaded from the project-root `defaults.json`.
/// This is the single source of truth for port/host values shared across
/// Rust, Vite, and shell scripts.
mod compiled_defaults {
    use serde::Deserialize;

    #[derive(Deserialize)]
    pub struct Defaults {
        pub port: u16,
        pub host: String,
        pub loopback: String,
    }

    static JSON: &str = include_str!("../defaults.json");

    pub fn load() -> Defaults {
        serde_json::from_str(JSON).expect("defaults.json must be valid")
    }
}

fn default_port() -> u16 {
    compiled_defaults::load().port
}
fn default_host() -> String {
    compiled_defaults::load().host
}
fn default_provider() -> String {
    "claude".into()
}
fn default_true() -> bool {
    true
}
fn is_true(value: &bool) -> bool {
    *value
}

fn normalized_channel_value(value: Option<String>) -> Option<String> {
    value
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty())
}

fn default_sync_interval() -> u64 {
    5
}
fn default_policies_dir() -> PathBuf {
    PathBuf::from("./policies")
}
fn is_false(value: &bool) -> bool {
    !*value
}
fn default_data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("agentdesk")
}
fn default_db_name() -> String {
    // Post-Postgres-cutover the canonical database name is used only by the
    // doctor's legacy on-disk artifact checks (see
    // `src/cli/doctor/orchestrator.rs::stale_zero_byte_db_candidates`). The
    // `.db` extension matches the candidates the doctor already scans for
    // and avoids legacy SQLite filename literals in production sources.
    "agentdesk.db".into()
}
fn default_database_host() -> String {
    "127.0.0.1".into()
}
fn default_database_port() -> u16 {
    5432
}
fn default_database_name() -> String {
    "agentdesk".into()
}
fn default_database_user() -> String {
    "agentdesk".into()
}
fn default_database_pool_max() -> u32 {
    12
}
fn default_cluster_role() -> String {
    "auto".into()
}
fn default_cluster_heartbeat_interval_secs() -> u64 {
    10
}
fn default_cluster_lease_ttl_secs() -> u64 {
    30
}
fn default_memory_backend() -> String {
    "auto".into()
}
fn default_sak_path() -> String {
    "memories/shared-agent-knowledge/shared_knowledge.md".into()
}
fn default_sam_path() -> String {
    "memories/shared-agent-memory".into()
}
fn default_ltm_root() -> String {
    "memories/long-term".into()
}
fn default_auto_memory_root() -> String {
    "~/.claude/projects/*{workspace}*/memory/".into()
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: default_port(),
            host: default_host(),
            auth_token: None,
        }
    }
}

impl ServerConfig {
    /// Loopback address from `defaults.json` (e.g. "127.0.0.1").
    /// Used for self-referencing HTTP requests.
    pub fn loopback() -> String {
        compiled_defaults::load().loopback
    }

    /// Build a base URL for self-referencing API calls: `http://{loopback}:{port}`.
    pub fn local_base_url(&self) -> String {
        format!("http://{}:{}", Self::loopback(), self.port)
    }
}

/// Build a localhost API URL: `http://{loopback}:{port}{path}`.
/// Use this for all self-referencing HTTP calls instead of hardcoding 127.0.0.1.
pub fn local_api_url(port: u16, path: &str) -> String {
    format!("http://{}:{}{}", ServerConfig::loopback(), port, path)
}

/// Returns the loopback address from defaults (e.g. "127.0.0.1").
pub fn loopback() -> String {
    ServerConfig::loopback()
}

/// Canonical runtime root: $AGENTDESK_ROOT_DIR → ~/.adk/release
/// All code that needs the AgentDesk root directory MUST call this function
/// instead of reimplementing the resolution logic.
pub fn runtime_root() -> Option<std::path::PathBuf> {
    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    if let Some(override_root) = test_runtime_root_override() {
        return Some(override_root);
    }

    if let Ok(override_root) = std::env::var("AGENTDESK_ROOT_DIR") {
        let trimmed = override_root.trim();
        if !trimmed.is_empty() {
            return Some(std::path::PathBuf::from(trimmed));
        }
    }
    dirs::home_dir().map(|h| h.join(".adk").join("release"))
}

impl Default for PoliciesConfig {
    fn default() -> Self {
        Self {
            dir: default_policies_dir(),
            hot_reload: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RoutinesConfig {
    /// Master on/off switch. Defaults to false; requires PostgreSQL.
    /// SQLite-only deployments are unaffected when this is false.
    #[serde(default)]
    pub enabled: bool,
    /// Release-managed directory containing bundled *.js routine scripts.
    /// Defaults to `./routines`.
    #[serde(default = "default_routines_dir")]
    pub dir: PathBuf,
    /// Additional operator-managed routine script directories. These are loaded
    /// after `dir`, so a script with the same relative path overrides the
    /// bundled script without being copied into the release directory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_dirs: Vec<PathBuf>,
    /// How often the due-scan tick runs, in seconds. Defaults to 30.
    #[serde(default = "default_routines_tick_interval_secs")]
    pub tick_interval_secs: u64,
    /// Maximum routines to claim per tick. Defaults to 10.
    #[serde(default = "default_routines_max_due_per_tick")]
    pub max_due_per_tick: u32,
    /// Maximum running agent-backed routine runs to poll per tick. Defaults to 10.
    #[serde(default = "default_routines_max_agent_polls_per_tick")]
    pub max_agent_polls_per_tick: u32,
    /// IANA timezone name used when no per-routine timezone is set.
    #[serde(default = "default_routines_timezone")]
    pub default_timezone: String,
    /// Default maximum wait for agent-backed routine completion, in seconds.
    #[serde(default = "default_routines_agent_timeout_secs")]
    pub agent_timeout_secs: u64,
    /// Watch `dir` for script changes and reload without restart.
    #[serde(default = "default_true")]
    pub hot_reload: bool,
}

impl Default for RoutinesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: default_routines_dir(),
            additional_dirs: Vec::new(),
            tick_interval_secs: default_routines_tick_interval_secs(),
            max_due_per_tick: default_routines_max_due_per_tick(),
            max_agent_polls_per_tick: default_routines_max_agent_polls_per_tick(),
            default_timezone: default_routines_timezone(),
            agent_timeout_secs: default_routines_agent_timeout_secs(),
            hot_reload: true,
        }
    }
}

impl RoutinesConfig {
    pub fn is_default(&self) -> bool {
        *self == RoutinesConfig::default()
    }

    pub fn script_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = Vec::with_capacity(1 + self.additional_dirs.len());
        for dir in std::iter::once(&self.dir).chain(self.additional_dirs.iter()) {
            if !dirs.contains(dir) {
                dirs.push(dir.clone());
            }
        }
        dirs
    }
}

fn default_routines_dir() -> PathBuf {
    PathBuf::from("./routines")
}

fn default_routines_tick_interval_secs() -> u64 {
    30
}

fn default_routines_max_due_per_tick() -> u32 {
    10
}

fn default_routines_max_agent_polls_per_tick() -> u32 {
    10
}

fn default_routines_timezone() -> String {
    "Asia/Seoul".to_string()
}

fn default_routines_agent_timeout_secs() -> u64 {
    30 * 60
}

#[cfg(test)]
mod routine_config_unit_tests {
    use super::*;

    #[test]
    fn routine_script_dirs_preserve_default_then_operator_order() {
        let config = RoutinesConfig {
            additional_dirs: vec![
                PathBuf::from("/Users/kunkun/routines"),
                PathBuf::from("./routines"),
                PathBuf::from("/Volumes/ops/agentdesk-routines"),
            ],
            ..RoutinesConfig::default()
        };

        assert_eq!(
            config.script_dirs(),
            vec![
                PathBuf::from("./routines"),
                PathBuf::from("/Users/kunkun/routines"),
                PathBuf::from("/Volumes/ops/agentdesk-routines")
            ]
        );
    }
}

impl Default for DataConfig {
    fn default() -> Self {
        Self {
            dir: default_data_dir(),
            db_name: default_db_name(),
        }
    }
}

const DEFAULT_MEMENTO_MCP_SERVER_NAME: &str = "memento";
const DEFAULT_MEMENTO_MCP_URL: &str = "http://127.0.0.1:57332/mcp";
const DEFAULT_MEMENTO_MCP_TOKEN_ENV_VAR: &str = "MEMENTO_ACCESS_KEY";

fn env_var_is_present(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn default_memento_mcp_server() -> Option<(String, McpServerConfig)> {
    env_var_is_present(DEFAULT_MEMENTO_MCP_TOKEN_ENV_VAR).then(|| {
        (
            DEFAULT_MEMENTO_MCP_SERVER_NAME.to_string(),
            McpServerConfig {
                url: DEFAULT_MEMENTO_MCP_URL.to_string(),
                auth: Some(McpServerAuthConfig {
                    auth_type: McpServerAuthType::Bearer,
                    token_env_var: Some(DEFAULT_MEMENTO_MCP_TOKEN_ENV_VAR.to_string()),
                }),
            },
        )
    })
}

impl Config {
    fn apply_runtime_defaults(mut self) -> Self {
        if let Some((name, server)) = default_memento_mcp_server() {
            self.mcp_servers.entry(name).or_insert(server);
        }
        self
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            discord: DiscordConfig::default(),
            voice: VoiceConfig::default(),
            shared_prompt: None,
            mcp_servers: std::collections::BTreeMap::new(),
            review_mcp_allowlist: Vec::new(),
            agents: Vec::new(),
            meeting: None,
            github: GitHubConfig::default(),
            policies: PoliciesConfig::default(),
            data: DataConfig::default(),
            database: DatabaseConfig::default(),
            cluster: ClusterConfig::default(),
            kanban: KanbanConfig::default(),
            review: ReviewConfig::default(),
            placeholder: PlaceholderConfig::default(),
            runtime: RuntimeSettingsConfig::default(),
            automation: AutomationConfig::default(),
            routines: RoutinesConfig::default(),
            escalation: EscalationConfig::default(),
            onboarding: OnboardingConfig::default(),
            memory: None,
            mcp: McpConfig::default(),
            prompt_manifest_retention: PromptManifestRetentionConfig::default(),
        }
        .apply_runtime_defaults()
    }
}

pub fn load() -> Result<Config> {
    let path = resolve_graceful_config_path(
        std::env::var("AGENTDESK_CONFIG")
            .ok()
            .map(std::path::PathBuf::from),
        runtime_root(),
        std::env::current_dir().ok(),
        dirs::home_dir(),
    );
    let path_display = path.display().to_string();

    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read config: {path_display}"))?;

    let config: Config = serde_yaml::from_str(&contents)
        .with_context(|| format!("Failed to parse config: {path_display}"))?;
    let config = config.apply_runtime_defaults();

    // Ensure data dir exists
    std::fs::create_dir_all(&config.data.dir)?;

    Ok(config)
}

pub fn load_from_path(path: &Path) -> Result<Config> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config {}", path.display()))?;
    let config = serde_yaml::from_str::<Config>(&contents)
        .with_context(|| format!("Failed to parse config {}", path.display()))?;
    Ok(config.apply_runtime_defaults())
}

pub fn save_to_path(path: &Path, config: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let rendered = serde_yaml::to_string(config)
        .with_context(|| format!("Failed to serialize config for {}", path.display()))?;
    std::fs::write(path, rendered)
        .with_context(|| format!("Failed to write config {}", path.display()))?;
    Ok(())
}

fn resolve_graceful_config_path(
    explicit: Option<std::path::PathBuf>,
    runtime_root: Option<std::path::PathBuf>,
    cwd: Option<std::path::PathBuf>,
    home_dir: Option<std::path::PathBuf>,
) -> std::path::PathBuf {
    if let Some(path) = explicit {
        if path.exists() {
            return path;
        }

        let mut candidates = Vec::new();
        if let Some(root) = runtime_root.as_ref() {
            let canonical = crate::runtime_layout::config_file_path(root);
            let legacy = crate::runtime_layout::legacy_config_file_path(root);
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
            crate::runtime_layout::config_file_path(root),
            crate::runtime_layout::legacy_config_file_path(root),
        ] {
            if path.exists() {
                return path;
            }
        }
    }
    if let Some(dir) = cwd {
        for path in [
            dir.join("config").join("agentdesk.yaml"),
            dir.join("agentdesk.yaml"),
        ] {
            if path.exists() {
                return path;
            }
        }
    }
    if let Some(home) = home_dir {
        let release_root = home.join(".adk").join("release");
        for path in [
            crate::runtime_layout::config_file_path(&release_root),
            crate::runtime_layout::legacy_config_file_path(&release_root),
        ] {
            if path.exists() {
                return path;
            }
        }
    }
    runtime_root
        .map(|root| crate::runtime_layout::config_file_path(&root))
        .unwrap_or_else(|| std::path::PathBuf::from("config").join("agentdesk.yaml"))
}

/// Load config gracefully — returns Config::default() if the file doesn't exist
/// or fails to parse, instead of panicking.
/// Searches:
/// $AGENTDESK_CONFIG →
/// $AGENTDESK_ROOT_DIR/config/agentdesk.yaml →
/// $AGENTDESK_ROOT_DIR/agentdesk.yaml →
/// CWD/config/agentdesk.yaml →
/// CWD/agentdesk.yaml →
/// ~/.adk/release/config/agentdesk.yaml →
/// ~/.adk/release/agentdesk.yaml
pub fn load_graceful() -> Config {
    let path = resolve_graceful_config_path(
        std::env::var("AGENTDESK_CONFIG")
            .ok()
            .map(std::path::PathBuf::from),
        std::env::var("AGENTDESK_ROOT_DIR")
            .ok()
            .map(|root| std::path::PathBuf::from(root.trim())),
        std::env::current_dir().ok(),
        dirs::home_dir(),
    );
    let path_display = path.display().to_string();

    let config = match std::fs::read_to_string(&path) {
        Ok(contents) => match serde_yaml::from_str::<Config>(&contents) {
            Ok(cfg) => cfg.apply_runtime_defaults(),
            Err(e) => {
                tracing::warn!("  ⚠ Failed to parse {path_display}: {e} — using defaults");
                Config::default()
            }
        },
        Err(_) => {
            tracing::warn!("  ⚠ {path_display} not found — using defaults");
            Config::default()
        }
    };

    // Ensure data dir exists (best effort)
    let _ = std::fs::create_dir_all(&config.data.dir);

    config
}

#[cfg(test)]
pub(crate) fn shared_test_env_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
thread_local! {
    static TEST_RUNTIME_ROOT_OVERRIDE: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn test_runtime_root_override() -> Option<std::path::PathBuf> {
    TEST_RUNTIME_ROOT_OVERRIDE.with(|slot| slot.borrow().clone())
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) fn current_test_runtime_root_override() -> Option<std::path::PathBuf> {
    test_runtime_root_override()
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) fn set_test_runtime_root_override(path: Option<std::path::PathBuf>) {
    TEST_RUNTIME_ROOT_OVERRIDE.with(|slot| {
        *slot.borrow_mut() = path;
    });
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::{
        AgentChannel, AgentChannelConfig, AgentChannels, AgentDef, AutomationConfig, BotConfig,
        Config, DEFAULT_MEMENTO_MCP_SERVER_NAME, DEFAULT_MEMENTO_MCP_TOKEN_ENV_VAR,
        DEFAULT_MEMENTO_MCP_URL, DiscordBotAuthConfig, DiscordConfig, EscalationConfig,
        EscalationMode, EscalationScheduleConfig, FileMemoryConfig, KanbanConfig, McpMemoryConfig,
        McpServerAuthConfig, McpServerAuthType, McpServerConfig, MemoryConfig, OnboardingConfig,
        PlaceholderConfig, ReviewConfig, RoutinesConfig, RuntimeSettingsConfig,
        is_valid_dispatch_profile, load_from_path, normalize_cache_ttl_minutes,
        normalize_dispatch_profile, resolve_graceful_config_path, runtime_root, save_to_path,
    };
    use std::path::PathBuf;
    use std::sync::MutexGuard;

    fn env_lock() -> MutexGuard<'static, ()> {
        crate::services::discord::runtime_store::lock_test_env()
    }

    #[test]
    fn runtime_root_returns_valid_path() {
        let _lock = env_lock();
        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };

        // runtime_root() should always return Some on systems with a home directory
        let root = runtime_root();

        match previous {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }

        assert!(root.is_some(), "runtime_root() returned None");
        let path = root.unwrap();
        assert!(
            path.ends_with(".adk/release"),
            "expected path ending with .adk/release, got {:?}",
            path
        );
    }

    #[test]
    fn runtime_root_respects_env_override() {
        let _lock = env_lock();
        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        let override_path = std::env::temp_dir().join("adk-test-root");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", &override_path) };
        let root = runtime_root();

        match previous {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }

        assert_eq!(root, Some(override_path));
    }

    fn make_temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agentdesk-config-test-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn resolve_graceful_config_path_prefers_runtime_root_before_cwd() {
        let root = make_temp_dir("root-first");
        let cwd = make_temp_dir("cwd-second");
        let home = make_temp_dir("home-third");
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::write(
            root.join("config").join("agentdesk.yaml"),
            "server:\n  port: 9001\n",
        )
        .unwrap();
        std::fs::create_dir_all(cwd.join("config")).unwrap();
        std::fs::write(
            cwd.join("config").join("agentdesk.yaml"),
            "server:\n  port: 9002\n",
        )
        .unwrap();
        std::fs::create_dir_all(home.join(".adk").join("release")).unwrap();
        std::fs::create_dir_all(home.join(".adk").join("release").join("config")).unwrap();
        std::fs::write(
            home.join(".adk")
                .join("release")
                .join("config")
                .join("agentdesk.yaml"),
            "server:\n  port: 9003\n",
        )
        .unwrap();

        let resolved = resolve_graceful_config_path(
            None,
            Some(root.clone()),
            Some(cwd.clone()),
            Some(home.clone()),
        );
        assert_eq!(resolved, root.join("config").join("agentdesk.yaml"));

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(cwd);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn resolve_graceful_config_path_prefers_cwd_before_release_home() {
        let cwd = make_temp_dir("cwd-before-release");
        let home = make_temp_dir("release-fallback");
        std::fs::create_dir_all(cwd.join("config")).unwrap();
        std::fs::write(
            cwd.join("config").join("agentdesk.yaml"),
            "server:\n  port: 9101\n",
        )
        .unwrap();
        std::fs::create_dir_all(home.join(".adk").join("release")).unwrap();
        std::fs::create_dir_all(home.join(".adk").join("release").join("config")).unwrap();
        std::fs::write(
            home.join(".adk")
                .join("release")
                .join("config")
                .join("agentdesk.yaml"),
            "server:\n  port: 9102\n",
        )
        .unwrap();

        let resolved =
            resolve_graceful_config_path(None, None, Some(cwd.clone()), Some(home.clone()));
        assert_eq!(resolved, cwd.join("config").join("agentdesk.yaml"));

        let _ = std::fs::remove_dir_all(cwd);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn resolve_graceful_config_path_falls_back_to_legacy_runtime_path() {
        let root = make_temp_dir("legacy-runtime");
        std::fs::write(root.join("agentdesk.yaml"), "server:\n  port: 9201\n").unwrap();

        let resolved = resolve_graceful_config_path(None, Some(root.clone()), None, None);
        assert_eq!(resolved, root.join("agentdesk.yaml"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn routines_config_defaults_are_disabled_safe() {
        let config = Config::default();

        assert_eq!(config.routines, RoutinesConfig::default());
        assert!(!config.routines.enabled);
        assert_eq!(config.routines.dir, PathBuf::from("./routines"));
        assert!(config.routines.additional_dirs.is_empty());
        assert_eq!(
            config.routines.script_dirs(),
            vec![PathBuf::from("./routines")]
        );
        assert_eq!(config.routines.tick_interval_secs, 30);
        assert_eq!(config.routines.max_due_per_tick, 10);
        assert_eq!(config.routines.max_agent_polls_per_tick, 10);
        assert_eq!(config.routines.default_timezone, "Asia/Seoul");
        assert_eq!(config.routines.agent_timeout_secs, 1800);
        assert!(config.routines.hot_reload);
        assert!(config.routines.is_default());

        let yaml = serde_yaml::to_string(&config).unwrap();
        assert!(
            !yaml.contains("routines:"),
            "default routines config must be omitted, got: {yaml}"
        );
    }

    #[test]
    fn placeholder_config_defaults_enabled_and_omitted_from_yaml() {
        let config = Config::default();

        assert_eq!(
            config.placeholder,
            PlaceholderConfig {
                live_events_enabled: true,
                status_panel_v2_enabled: true,
            }
        );
        assert!(config.placeholder.is_default());

        let yaml = serde_yaml::to_string(&config).unwrap();
        assert!(
            !yaml.contains("placeholder:"),
            "default placeholder config must be omitted, got: {yaml}"
        );

        let parsed: Config = serde_yaml::from_str("server: {}\n").unwrap();
        assert!(parsed.placeholder.live_events_enabled);
        assert!(parsed.placeholder.status_panel_v2_enabled);
    }

    #[test]
    fn config_deserializes_voice_section_with_defaults() {
        let config: Config = serde_yaml::from_str(
            r#"
server:
  port: 9010
voice:
  enabled: true
  thresholds:
    speech_start_db: -40.0
  wake_words:
    - desk
"#,
        )
        .unwrap();

        assert!(config.voice.enabled);
        assert_eq!(config.voice.thresholds.speech_start_db, -40.0);
        assert_eq!(config.voice.thresholds.speech_end_db, -55.0);
        assert_eq!(config.voice.wake_words, vec!["desk"]);
        assert_eq!(
            config.voice.audio.recordings_dir,
            PathBuf::from("~/.adk/voice/recordings")
        );
    }

    #[test]
    fn placeholder_config_preserves_explicit_disabled_yaml() {
        let config = Config {
            placeholder: PlaceholderConfig {
                live_events_enabled: false,
                status_panel_v2_enabled: false,
            },
            ..Config::default()
        };

        assert!(!config.placeholder.is_default());
        let yaml = serde_yaml::to_string(&config).unwrap();
        assert!(yaml.contains("placeholder:"));
        assert!(yaml.contains("live_events_enabled: false"));
        assert!(yaml.contains("status_panel_v2_enabled: false"));
    }

    #[test]
    fn routines_config_partial_yaml_uses_defaults() {
        let config: Config = serde_yaml::from_str(
            r#"
server:
  port: 9010
routines:
  enabled: true
  dir: ./ops-routines
  additional_dirs:
    - /Users/kunkun/routines
    - /Users/kunkun/private-routines
"#,
        )
        .unwrap();

        assert!(config.routines.enabled);
        assert_eq!(config.routines.dir, PathBuf::from("./ops-routines"));
        assert_eq!(
            config.routines.additional_dirs,
            vec![
                PathBuf::from("/Users/kunkun/routines"),
                PathBuf::from("/Users/kunkun/private-routines")
            ]
        );
        assert_eq!(
            config.routines.script_dirs(),
            vec![
                PathBuf::from("./ops-routines"),
                PathBuf::from("/Users/kunkun/routines"),
                PathBuf::from("/Users/kunkun/private-routines")
            ]
        );
        assert_eq!(config.routines.tick_interval_secs, 30);
        assert_eq!(config.routines.max_due_per_tick, 10);
        assert_eq!(config.routines.max_agent_polls_per_tick, 10);
        assert_eq!(config.routines.default_timezone, "Asia/Seoul");
        assert_eq!(config.routines.agent_timeout_secs, 1800);
        assert!(config.routines.hot_reload);
        assert!(!config.routines.is_default());
    }

    #[test]
    fn routines_config_due_claim_limit_does_not_disable_agent_polling() {
        let config: Config = serde_yaml::from_str(
            r#"
server:
  port: 9010
routines:
  enabled: true
  max_due_per_tick: 0
"#,
        )
        .unwrap();

        assert_eq!(config.routines.max_due_per_tick, 0);
        assert_eq!(config.routines.max_agent_polls_per_tick, 10);
    }

    #[test]
    fn routines_config_round_trips_through_yaml() {
        let mut config = Config::default();
        config.routines = RoutinesConfig {
            enabled: true,
            dir: PathBuf::from("./routines-prod"),
            additional_dirs: vec![
                PathBuf::from("/Users/kunkun/routines"),
                PathBuf::from("/Volumes/ops/agentdesk-routines"),
            ],
            tick_interval_secs: 15,
            max_due_per_tick: 25,
            max_agent_polls_per_tick: 50,
            default_timezone: "Asia/Seoul".to_string(),
            agent_timeout_secs: 900,
            hot_reload: false,
        };

        let yaml = serde_yaml::to_string(&config).unwrap();
        assert!(
            yaml.contains("routines:"),
            "non-default routines config must be serialized, got: {yaml}"
        );
        assert!(
            yaml.contains("enabled: true"),
            "enabled flag must be serialized, got: {yaml}"
        );

        let reloaded: Config = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(reloaded.routines, config.routines);
    }

    #[test]
    fn resolve_graceful_config_path_follows_migrated_runtime_config_when_explicit_legacy_is_missing()
     {
        let root = make_temp_dir("explicit-legacy-migrated");
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::write(
            root.join("config").join("agentdesk.yaml"),
            "server:\n  port: 9301\n",
        )
        .unwrap();

        let resolved = resolve_graceful_config_path(
            Some(root.join("agentdesk.yaml")),
            Some(root.clone()),
            None,
            None,
        );
        assert_eq!(resolved, root.join("config").join("agentdesk.yaml"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn save_and_load_round_trip_preserves_config_fields() {
        let dir = make_temp_dir("roundtrip");
        let path = dir.join("nested").join("agentdesk.yaml");

        let mut config = Config::default();
        config.server.port = 4317;
        config.server.host = "127.0.0.42".to_string();
        config.server.auth_token = Some("secret-token".to_string());
        config.discord.guild_id = Some("guild-123".to_string());
        config.discord.owner_id = Some(343742347365974026);
        config.discord.bots.insert(
            "announce".to_string(),
            BotConfig {
                token: Some("bot-token".to_string()),
                description: Some("announce bot".to_string()),
                provider: Some("codex".to_string()),
                agent: Some("agent-1".to_string()),
                auth: DiscordBotAuthConfig {
                    allowed_channel_ids: Some(vec![123456789012345678]),
                    require_mention_channel_ids: Some(vec![223456789012345678]),
                    allowed_user_ids: Some(vec![343742347365974026]),
                    allowed_tools: Some(vec!["Bash".to_string(), "WebFetch".to_string()]),
                    allow_all_users: Some(false),
                    allowed_bot_ids: Some(vec![1479017284805722200]),
                },
            },
        );
        config.agents.push(AgentDef {
            id: "agent-1".to_string(),
            name: "Agent One".to_string(),
            name_ko: Some("에이전트 원".to_string()),
            aliases: Vec::new(),
            wake_word: None,
            voice_enabled: true,
            sensitivity_mode: None,
            provider: "codex".to_string(),
            channels: AgentChannels {
                claude: Some("123456789012345678".into()),
                codex: None,
                gemini: None,
                opencode: None,
                qwen: None,
            },
            keywords: Vec::new(),
            department: Some("platform".to_string()),
            avatar_emoji: Some(":robot:".to_string()),
        });
        config.kanban = KanbanConfig {
            manager_channel_id: Some("123456789012345678".to_string()),
            deadlock_manager_channel_id: Some("223456789012345678".to_string()),
            human_alert_channel_id: Some("323456789012345678".to_string()),
            pm_decision_gate_enabled: Some(true),
        };
        config.review = ReviewConfig {
            enabled: Some(true),
            max_rounds: Some(4),
        };
        config.runtime = RuntimeSettingsConfig {
            requested_timeout_min: Some(55),
            in_progress_stale_min: Some(180),
            long_turn_alert_interval_min: Some(30),
            context_compact_percent: Some(70),
            context_compact_percent_codex: Some(82),
            context_compact_percent_claude: Some(74),
            dispatch_poll_sec: Some(45),
            agent_sync_sec: Some(420),
            github_issue_sync_sec: Some(1200),
            claude_rate_limit_poll_sec: Some(90),
            codex_rate_limit_poll_sec: Some(105),
            issue_triage_poll_sec: Some(360),
            ceo_warn_depth: Some(4),
            max_retries: Some(5),
            max_entry_retries: Some(6),
            stale_dispatched_grace_min: Some(4),
            stale_dispatched_terminal_statuses: Some("cancelled,failed".to_string()),
            stale_dispatched_recover_null_dispatch: Some(false),
            stale_dispatched_recover_missing_dispatch: Some(true),
            review_reminder_min: Some(25),
            rate_limit_warning_pct: Some(78),
            rate_limit_danger_pct: Some(93),
            github_repo_cache_sec: Some(480),
            rate_limit_stale_sec: Some(750),
            reset_overrides_on_restart: true,
        };
        config.automation = AutomationConfig {
            enabled: Some(true),
            strategy: Some("rebase".to_string()),
            strategy_mode: Some("pr-always".to_string()),
            allowed_authors: Some("itismyfield,octocat".to_string()),
        };
        config.mcp_servers.insert(
            "memento".to_string(),
            McpServerConfig {
                url: "http://127.0.0.1:57332/mcp".to_string(),
                auth: Some(McpServerAuthConfig {
                    auth_type: McpServerAuthType::Bearer,
                    token_env_var: Some("MEMENTO_ACCESS_KEY".to_string()),
                }),
            },
        );
        config.escalation = EscalationConfig {
            mode: EscalationMode::Scheduled,
            owner_user_id: Some(343742347365974026),
            pm_channel_id: Some("323456789012345678".to_string()),
            schedule: EscalationScheduleConfig {
                pm_hours: Some("00:00-08:00".to_string()),
                timezone: Some("Asia/Seoul".to_string()),
            },
        };
        let mut onboarding_categories = std::collections::BTreeMap::new();
        onboarding_categories.insert("dev".to_string(), "1474956560391340242".to_string());
        onboarding_categories.insert(
            "cookingheart".to_string(),
            "1474045427740311582".to_string(),
        );
        let mut onboarding_suffixes = std::collections::BTreeMap::new();
        onboarding_suffixes.insert("-cc".to_string(), "claude".to_string());
        onboarding_suffixes.insert("-cdx".to_string(), "codex".to_string());
        config.onboarding = OnboardingConfig {
            guild_id: Some("1469870512812462284".to_string()),
            default_categories: onboarding_categories,
            provider_suffix_map: onboarding_suffixes,
            provider_options: vec!["claude".to_string(), "codex".to_string()],
            default_provider: Some("claude".to_string()),
        };
        config.memory = Some(MemoryConfig {
            backend: "memento".to_string(),
            file: FileMemoryConfig {
                sak_path: "/tmp/shared.md".to_string(),
                sam_path: "/tmp/sam".to_string(),
                ltm_root: "/tmp/ltm".to_string(),
                auto_memory_root: "/tmp/auto/{workspace}".to_string(),
            },
            mcp: McpMemoryConfig {
                endpoint: "http://127.0.0.1:8765".to_string(),
                access_key_env: "MEMENTO_API_KEY".to_string(),
            },
        });

        save_to_path(&path, &config).unwrap();
        assert!(path.exists());
        let loaded = load_from_path(&path).unwrap();

        assert_eq!(loaded.server.port, 4317);
        assert_eq!(loaded.server.host, "127.0.0.42");
        assert_eq!(loaded.server.auth_token.as_deref(), Some("secret-token"));
        assert_eq!(loaded.discord.guild_id.as_deref(), Some("guild-123"));
        assert_eq!(loaded.discord.owner_id, Some(343742347365974026));
        assert_eq!(loaded.discord.bots.len(), 1);
        assert_eq!(
            loaded.discord.bots["announce"].description.as_deref(),
            Some("announce bot")
        );
        assert_eq!(
            loaded.discord.bots["announce"].provider.as_deref(),
            Some("codex")
        );
        assert_eq!(
            loaded.discord.bots["announce"].agent.as_deref(),
            Some("agent-1")
        );
        assert_eq!(
            loaded.discord.bots["announce"]
                .auth
                .allowed_channel_ids
                .as_deref(),
            Some(&[123456789012345678][..])
        );
        assert_eq!(
            loaded.discord.bots["announce"]
                .auth
                .require_mention_channel_ids
                .as_deref(),
            Some(&[223456789012345678][..])
        );
        assert_eq!(
            loaded.discord.bots["announce"]
                .auth
                .allowed_user_ids
                .as_deref(),
            Some(&[343742347365974026][..])
        );
        assert_eq!(
            loaded.discord.bots["announce"]
                .auth
                .allowed_tools
                .as_deref(),
            Some(&["Bash".to_string(), "WebFetch".to_string()][..])
        );
        assert_eq!(
            loaded.discord.bots["announce"].auth.allow_all_users,
            Some(false)
        );
        assert_eq!(
            loaded.mcp_servers.get("memento"),
            Some(&McpServerConfig {
                url: "http://127.0.0.1:57332/mcp".to_string(),
                auth: Some(McpServerAuthConfig {
                    auth_type: McpServerAuthType::Bearer,
                    token_env_var: Some("MEMENTO_ACCESS_KEY".to_string()),
                }),
            })
        );
        assert_eq!(
            loaded.discord.bots["announce"]
                .auth
                .allowed_bot_ids
                .as_deref(),
            Some(&[1479017284805722200][..])
        );
        assert_eq!(loaded.agents.len(), 1);
        assert_eq!(loaded.agents[0].id, "agent-1");
        assert_eq!(loaded.agents[0].name, "Agent One");
        assert_eq!(loaded.agents[0].name_ko.as_deref(), Some("에이전트 원"));
        assert_eq!(loaded.agents[0].provider, "codex");
        assert_eq!(loaded.agents[0].department.as_deref(), Some("platform"));
        assert_eq!(loaded.agents[0].avatar_emoji.as_deref(), Some(":robot:"));
        assert_eq!(
            loaded.kanban.manager_channel_id.as_deref(),
            Some("123456789012345678")
        );
        assert_eq!(
            loaded.kanban.deadlock_manager_channel_id.as_deref(),
            Some("223456789012345678")
        );
        assert_eq!(
            loaded.kanban.human_alert_channel_id.as_deref(),
            Some("323456789012345678")
        );
        assert_eq!(loaded.kanban.pm_decision_gate_enabled, Some(true));
        assert_eq!(loaded.review.enabled, Some(true));
        assert_eq!(loaded.review.max_rounds, Some(4));
        assert_eq!(loaded.runtime.requested_timeout_min, Some(55));
        assert_eq!(loaded.runtime.in_progress_stale_min, Some(180));
        assert_eq!(loaded.runtime.long_turn_alert_interval_min, Some(30));
        assert_eq!(loaded.runtime.context_compact_percent, Some(70));
        assert_eq!(loaded.runtime.context_compact_percent_codex, Some(82));
        assert_eq!(loaded.runtime.context_compact_percent_claude, Some(74));
        assert_eq!(loaded.runtime.dispatch_poll_sec, Some(45));
        assert_eq!(loaded.runtime.agent_sync_sec, Some(420));
        assert_eq!(loaded.runtime.github_issue_sync_sec, Some(1200));
        assert_eq!(loaded.runtime.claude_rate_limit_poll_sec, Some(90));
        assert_eq!(loaded.runtime.codex_rate_limit_poll_sec, Some(105));
        assert_eq!(loaded.runtime.issue_triage_poll_sec, Some(360));
        assert_eq!(loaded.runtime.ceo_warn_depth, Some(4));
        assert_eq!(loaded.runtime.max_retries, Some(5));
        assert_eq!(loaded.runtime.max_entry_retries, Some(6));
        assert_eq!(loaded.runtime.stale_dispatched_grace_min, Some(4));
        assert_eq!(
            loaded.runtime.stale_dispatched_terminal_statuses.as_deref(),
            Some("cancelled,failed")
        );
        assert_eq!(
            loaded.runtime.stale_dispatched_recover_null_dispatch,
            Some(false)
        );
        assert_eq!(
            loaded.runtime.stale_dispatched_recover_missing_dispatch,
            Some(true)
        );
        assert_eq!(loaded.runtime.review_reminder_min, Some(25));
        assert_eq!(loaded.runtime.rate_limit_warning_pct, Some(78));
        assert_eq!(loaded.runtime.rate_limit_danger_pct, Some(93));
        assert_eq!(loaded.runtime.github_repo_cache_sec, Some(480));
        assert_eq!(loaded.runtime.rate_limit_stale_sec, Some(750));
        assert!(loaded.runtime.reset_overrides_on_restart);
        assert_eq!(loaded.automation.enabled, Some(true));
        assert_eq!(loaded.automation.strategy.as_deref(), Some("rebase"));
        assert_eq!(
            loaded.automation.strategy_mode.as_deref(),
            Some("pr-always")
        );
        assert_eq!(
            loaded.automation.allowed_authors.as_deref(),
            Some("itismyfield,octocat")
        );
        assert_eq!(loaded.escalation.mode, EscalationMode::Scheduled);
        assert_eq!(loaded.escalation.owner_user_id, Some(343742347365974026));
        assert_eq!(
            loaded.escalation.pm_channel_id.as_deref(),
            Some("323456789012345678")
        );
        assert_eq!(
            loaded.escalation.schedule.pm_hours.as_deref(),
            Some("00:00-08:00")
        );
        assert_eq!(
            loaded.escalation.schedule.timezone.as_deref(),
            Some("Asia/Seoul")
        );
        assert_eq!(
            loaded.agents[0]
                .channels
                .claude
                .as_ref()
                .and_then(AgentChannel::target)
                .as_deref(),
            Some("123456789012345678")
        );
        assert_eq!(
            loaded.memory.as_ref().map(|memory| memory.backend.as_str()),
            Some("memento")
        );
        assert_eq!(
            loaded
                .memory
                .as_ref()
                .map(|memory| memory.file.auto_memory_root.as_str()),
            Some("/tmp/auto/{workspace}")
        );
        assert_eq!(
            loaded
                .memory
                .as_ref()
                .map(|memory| memory.mcp.access_key_env.as_str()),
            Some("MEMENTO_API_KEY")
        );
        assert_eq!(
            loaded.onboarding.guild_id.as_deref(),
            Some("1469870512812462284")
        );
        assert_eq!(
            loaded
                .onboarding
                .default_categories
                .get("dev")
                .map(String::as_str),
            Some("1474956560391340242")
        );
        assert_eq!(
            loaded
                .onboarding
                .default_categories
                .get("cookingheart")
                .map(String::as_str),
            Some("1474045427740311582")
        );
        assert_eq!(
            loaded
                .onboarding
                .provider_suffix_map
                .get("-cdx")
                .map(String::as_str),
            Some("codex")
        );
        assert_eq!(
            loaded.onboarding.default_provider.as_deref(),
            Some("claude")
        );
        assert_eq!(
            loaded.onboarding.provider_options,
            vec!["claude".to_string(), "codex".to_string()]
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn onboarding_provider_suffix_uses_config_when_set() {
        let mut config = OnboardingConfig::default();
        config
            .provider_suffix_map
            .insert("-cc".to_string(), "claude".to_string());
        config
            .provider_suffix_map
            .insert("-cdx".to_string(), "codex".to_string());

        assert_eq!(
            config.provider_from_channel_suffix("td-cc").as_deref(),
            Some("claude")
        );
        assert_eq!(
            config.provider_from_channel_suffix("agent-CDX").as_deref(),
            Some("codex")
        );
        assert_eq!(config.provider_from_channel_suffix("plain"), None);
    }

    #[test]
    fn onboarding_provider_suffix_falls_back_to_default_map() {
        let config = OnboardingConfig::default();
        // Built-in fallback mirrors the dashboard PROVIDER_SUFFIX_MAP.
        assert_eq!(
            config
                .provider_from_channel_suffix("research-cc")
                .as_deref(),
            Some("claude")
        );
        assert_eq!(
            config.provider_from_channel_suffix("agent-cdx").as_deref(),
            Some("codex")
        );
        assert_eq!(
            config.provider_from_channel_suffix("any-gem").as_deref(),
            Some("gemini")
        );
        assert_eq!(
            config.provider_from_channel_suffix("any-qw").as_deref(),
            Some("qwen")
        );
        assert_eq!(
            config.provider_from_channel_suffix("any-oc").as_deref(),
            Some("opencode")
        );
        assert_eq!(config.provider_from_channel_suffix(""), None);
    }

    #[test]
    fn onboarding_provider_suffix_normalizes_keys_without_leading_dash() {
        let mut config = OnboardingConfig::default();
        config
            .provider_suffix_map
            .insert("CC".to_string(), "claude".to_string());
        assert_eq!(
            config.provider_from_channel_suffix("td-cc").as_deref(),
            Some("claude")
        );
    }

    #[test]
    fn onboarding_resolve_category_supports_label_and_raw_id() {
        let mut config = OnboardingConfig::default();
        config
            .default_categories
            .insert("dev".to_string(), "1474956560391340242".to_string());
        assert_eq!(
            config.resolve_category("dev").as_deref(),
            Some("1474956560391340242")
        );
        assert_eq!(
            config.resolve_category("1474956560391340242").as_deref(),
            Some("1474956560391340242")
        );
        assert_eq!(config.resolve_category("missing"), None);
        assert_eq!(config.resolve_category(""), None);
    }

    #[test]
    fn onboarding_effective_guild_id_prefers_override() {
        let mut discord = DiscordConfig::default();
        discord.guild_id = Some("guild-discord".to_string());
        let mut onboarding = OnboardingConfig::default();
        assert_eq!(
            onboarding.effective_guild_id(&discord),
            Some("guild-discord")
        );
        onboarding.guild_id = Some("guild-onboarding".to_string());
        assert_eq!(
            onboarding.effective_guild_id(&discord),
            Some("guild-onboarding")
        );
    }

    #[test]
    fn load_from_path_injects_default_memento_mcp_when_access_key_is_present() {
        let _lock = env_lock();
        let dir = make_temp_dir("memento-default");
        let path = dir.join("agentdesk.yaml");
        std::fs::write(
            &path,
            "server:\n  port: 9201\n  host: 127.0.0.1\ndiscord:\n  bots: {}\n",
        )
        .unwrap();

        let previous = std::env::var_os(DEFAULT_MEMENTO_MCP_TOKEN_ENV_VAR);
        unsafe { std::env::set_var(DEFAULT_MEMENTO_MCP_TOKEN_ENV_VAR, "test-memento-access-key") };

        let loaded = load_from_path(&path).unwrap();
        assert_eq!(
            loaded.mcp_servers.get(DEFAULT_MEMENTO_MCP_SERVER_NAME),
            Some(&McpServerConfig {
                url: DEFAULT_MEMENTO_MCP_URL.to_string(),
                auth: Some(McpServerAuthConfig {
                    auth_type: McpServerAuthType::Bearer,
                    token_env_var: Some(DEFAULT_MEMENTO_MCP_TOKEN_ENV_VAR.to_string()),
                }),
            })
        );

        match previous {
            Some(value) => unsafe { std::env::set_var(DEFAULT_MEMENTO_MCP_TOKEN_ENV_VAR, value) },
            None => unsafe { std::env::remove_var(DEFAULT_MEMENTO_MCP_TOKEN_ENV_VAR) },
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    // -------- #1088 prompt-cache TTL config field --------

    #[test]
    fn cache_ttl_minutes_normalize_accepts_only_5_and_60() {
        assert_eq!(normalize_cache_ttl_minutes(None), None);
        assert_eq!(normalize_cache_ttl_minutes(Some(5)), Some(5));
        assert_eq!(normalize_cache_ttl_minutes(Some(60)), Some(60));
        // Reject everything else.
        for invalid in [0u32, 1, 4, 6, 30, 59, 61, 120, 600] {
            assert_eq!(
                normalize_cache_ttl_minutes(Some(invalid)),
                None,
                "minutes={invalid} must normalize to None"
            );
        }
    }

    #[test]
    fn dispatch_profile_normalizes_supported_channel_values() {
        assert_eq!(normalize_dispatch_profile(None), None);
        assert_eq!(
            normalize_dispatch_profile(Some(" lite ".to_string())).as_deref(),
            Some("lite")
        );
        assert_eq!(
            normalize_dispatch_profile(Some("FULL".to_string())).as_deref(),
            Some("full")
        );
        assert_eq!(
            normalize_dispatch_profile(Some("off".to_string())).as_deref(),
            Some("full")
        );
        assert_eq!(normalize_dispatch_profile(Some("review".to_string())), None);
        assert!(is_valid_dispatch_profile("lite"));
        assert!(is_valid_dispatch_profile(" default "));
        assert!(!is_valid_dispatch_profile("lte"));
    }

    #[test]
    fn dispatch_profile_round_trips_through_yaml() {
        let config = AgentChannelConfig {
            id: Some("123".to_string()),
            dispatch_profile: Some("lite".to_string()),
            ..AgentChannelConfig::default()
        };
        let yaml = serde_yaml::to_string(&config).unwrap();
        assert!(
            yaml.contains("dispatch_profile: lite"),
            "expected dispatch_profile: lite in: {yaml}"
        );
        let reloaded: AgentChannelConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(reloaded.dispatch_profile.as_deref(), Some("lite"));
        assert_eq!(
            AgentChannel::Detailed(reloaded)
                .dispatch_profile()
                .as_deref(),
            Some("lite")
        );
    }

    #[test]
    fn dispatch_profile_camel_case_alias_is_accepted() {
        let yaml = "id: '123'\ndispatchProfile: lite\n";
        let config: AgentChannelConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.dispatch_profile.as_deref(), Some("lite"));
        assert_eq!(
            AgentChannel::Detailed(config).dispatch_profile().as_deref(),
            Some("lite")
        );
    }

    #[test]
    fn cache_ttl_minutes_round_trips_through_yaml() {
        // None → omitted entirely (skip_serializing_if).
        let config_none = AgentChannelConfig {
            id: Some("123".to_string()),
            ..AgentChannelConfig::default()
        };
        let yaml = serde_yaml::to_string(&config_none).unwrap();
        assert!(
            !yaml.contains("cache_ttl_minutes"),
            "default None must be omitted, got: {yaml}"
        );

        // 60 round-trips and survives reload.
        let config_60 = AgentChannelConfig {
            id: Some("123".to_string()),
            cache_ttl_minutes: Some(60),
            ..AgentChannelConfig::default()
        };
        let yaml = serde_yaml::to_string(&config_60).unwrap();
        assert!(
            yaml.contains("cache_ttl_minutes: 60"),
            "expected cache_ttl_minutes: 60 in: {yaml}"
        );
        let reloaded: AgentChannelConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(reloaded.cache_ttl_minutes, Some(60));
        assert_eq!(
            AgentChannel::Detailed(reloaded).cache_ttl_minutes(),
            Some(60)
        );

        // 5 round-trips identically.
        let config_5 = AgentChannelConfig {
            id: Some("123".to_string()),
            cache_ttl_minutes: Some(5),
            ..AgentChannelConfig::default()
        };
        let reloaded: AgentChannelConfig =
            serde_yaml::from_str(&serde_yaml::to_string(&config_5).unwrap()).unwrap();
        assert_eq!(reloaded.cache_ttl_minutes, Some(5));
    }

    #[test]
    fn cache_ttl_minutes_camel_case_alias_is_accepted() {
        let yaml = "id: '123'\ncacheTtlMinutes: 60\n";
        let config: AgentChannelConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.cache_ttl_minutes, Some(60));
    }

    #[test]
    fn agent_channel_cache_ttl_minutes_filters_invalid_values() {
        // Detailed with invalid raw value must return None at the accessor level.
        let invalid = AgentChannel::Detailed(AgentChannelConfig {
            cache_ttl_minutes: Some(30),
            ..AgentChannelConfig::default()
        });
        assert_eq!(invalid.cache_ttl_minutes(), None);

        // Legacy entries never expose a TTL.
        let legacy = AgentChannel::Legacy("alpha".to_string());
        assert_eq!(legacy.cache_ttl_minutes(), None);
    }
}

/// Compatibility shim: RCC's `config::Settings` is referenced by discord code
/// for remote_profiles. AgentDesk doesn't have TUI settings, so this returns
/// an empty struct.
pub struct Settings {
    pub remote_profiles: Vec<crate::services::remote::RemoteProfile>,
}

impl Settings {
    pub fn load() -> Self {
        Self {
            remote_profiles: Vec::new(),
        }
    }

    #[allow(dead_code)]
    pub fn config_dir() -> Option<std::path::PathBuf> {
        runtime_root().map(|root| crate::runtime_layout::config_dir(&root))
    }
}
