use crate::voice::{VoiceConfig, barge_in::BargeInSensitivity};
use anyhow::{Context, Result, bail};
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, ProviderConfig>,
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
    /// When true (default), the server watches the on-disk config file and
    /// hot-reloads the hot-swappable settings (routine tunables, thresholds)
    /// without a restart, mirroring the policies watcher: the candidate file is
    /// pre-validated (parsed + runtime defaults applied) and only then atomically
    /// swapped in; a parse/validation failure keeps the running config. Infra
    /// fields (`server` bind/port/auth, `database`, `data.dir`, `cluster`,
    /// Discord/provider/agent launch and voice runtimes, MCP child processes and
    /// credential watcher, `memory`, GitHub sync cadence, prompt retention, and
    /// the config watcher flag itself) are NOT hot-swapped — a change to those is
    /// applied to the shared snapshot but logged as restart-required, since live
    /// subsystems bound them at boot.
    #[serde(default = "default_true")]
    pub config_hot_reload: bool,
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

#[derive(Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_host")]
    pub host: String,
    // Issue #2047 Finding 6 — never echo the bearer token via Serialize or Debug.
    // The struct is reachable through many tracing call sites and JSON dumps, so
    // the secret is stripped on the wire and replaced with `<redacted>` in Debug.
    #[serde(default, skip_serializing)]
    pub auth_token: Option<String>,
    /// Issue #3870 — opt-in escape hatch for binding the unauthenticated
    /// control-plane to a non-loopback interface. When `host` is non-loopback
    /// (e.g. `0.0.0.0`) and `auth_token` is unset, startup force-binds to
    /// loopback unless this is `true`. Operators who genuinely want LAN
    /// exposure without a token must set this explicitly (and accept the risk);
    /// the recommended path is to set `auth_token` instead.
    #[serde(default)]
    pub allow_insecure_nonloopback_bind: bool,
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig")
            .field("port", &self.port)
            .field("host", &self.host)
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "allow_insecure_nonloopback_bind",
                &self.allow_insecure_nonloopback_bind,
            )
            .finish()
    }
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct ProviderConfig {
    #[serde(default, alias = "tuiHosting", skip_serializing_if = "Option::is_none")]
    pub tui_hosting: Option<bool>,
    /// Phase 0 of the claude-e rollout. Accepted values: `pipe`, `tui`,
    /// `claude-e`. When both `runtime` and the legacy `tui_hosting` boolean
    /// are present, `runtime` wins. Unknown strings are ignored and the
    /// legacy `tui_hosting` derivation runs as before. See
    /// `docs/claude-e-rollout/decision-log.md` for rationale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    /// Issue #2193 — Codex remote SSH runtime gate.
    ///
    /// Defaults to `false`. When `true`, the operator asserts that every
    /// prerequisite in `docs/codex-remote-ssh-policy.md` is in place.
    /// At time of writing, the ADR's follow-ups are NOT in place
    /// (`services::remote_stub` still returns errors, the allow-list
    /// schema is not wired, the integration test does not exist).
    /// Bootstrap therefore **hard-fails** when this flag is `true` and
    /// `crate::services::codex_remote_policy::PREREQUISITES_SATISFIED`
    /// is `false`, so a warn-only gate cannot become a persisted
    /// "enabled" signal that a partial future implementation silently
    /// honors.
    #[serde(
        default,
        alias = "remoteSshEnabled",
        skip_serializing_if = "Option::is_none"
    )]
    pub remote_ssh_enabled: Option<bool>,
}

pub fn default_provider_tui_hosting(provider: &str) -> bool {
    matches!(provider.trim().to_ascii_lowercase().as_str(), "claude")
}

impl Config {
    pub fn provider_tui_hosting_enabled(&self, provider: &str) -> bool {
        let key = provider.trim().to_ascii_lowercase();
        self.providers
            .get(&key)
            .and_then(|config| config.tui_hosting)
            .unwrap_or_else(|| default_provider_tui_hosting(&key))
    }

    /// Issue #2193 — Codex remote SSH gate accessor.
    ///
    /// Returns `true` only when the operator has explicitly set
    /// `providers.codex.remote_ssh_enabled: true` in `agentdesk.yaml`.
    /// Defaults to `false` per `docs/codex-remote-ssh-policy.md`.
    pub fn codex_remote_ssh_enabled(&self) -> bool {
        self.providers
            .get("codex")
            .and_then(|cfg| cfg.remote_ssh_enabled)
            .unwrap_or(false)
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct BotConfig {
    // Issue #2047 Finding 6 — bot token must never leave the process via
    // Serialize/Debug. Settings.runtime-config and any future debug log
    // (`tracing::debug!("{:?}", state.config)`) would otherwise echo the raw
    // Discord secret. `skip_serializing` drops the field from JSON output and
    // the manual `Debug` impl below masks the value as `<redacted>`.
    #[serde(default, skip_serializing)]
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

impl std::fmt::Debug for BotConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BotConfig")
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("description", &self.description)
            .field("provider", &self.provider)
            .field("agent", &self.agent)
            .field("auth", &self.auth)
            .finish()
    }
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
    #[serde(default, skip_serializing_if = "AgentVoiceConfig::is_default")]
    pub voice: AgentVoiceConfig,
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
    /// Cluster intake node-affinity labels (#3667). When set and non-empty,
    /// intake for this agent's channels is routed to an online worker node whose
    /// labels satisfy this list; `Some([])` means no preference (intake stays on
    /// the leader). `None` (key absent from yaml) leaves any existing DB value
    /// untouched on sync, so an out-of-band label is never wiped. Maps to the
    /// `agents.preferred_intake_node_labels` JSONB column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_intake_node_labels: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct AgentVoiceConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "AgentVoiceForegroundConfig::is_default"
    )]
    pub foreground: AgentVoiceForegroundConfig,
}

impl AgentVoiceConfig {
    pub fn is_default(&self) -> bool {
        self.channel_id.as_deref().unwrap_or("").trim().is_empty() && self.foreground.is_default()
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct AgentVoiceForegroundConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_chars: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl AgentVoiceForegroundConfig {
    pub fn is_default(&self) -> bool {
        self.provider.as_deref().unwrap_or("").trim().is_empty()
            && self.model.as_deref().unwrap_or("").trim().is_empty()
            && self.max_chars.is_none()
            && self.timeout_ms.is_none()
    }
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
#[allow(clippy::large_enum_variant)]
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

    pub fn tui_hosting(&self) -> Option<bool> {
        match self {
            Self::Legacy(_) => None,
            Self::Detailed(config) => config.tui_hosting,
        }
    }

    /// Phase 0 of the claude-e rollout. Returns the per-channel `runtime`
    /// override string (`pipe` / `tui` / `claude-e`) if explicitly set,
    /// without parsing or back-compat derivation. Callers should use
    /// `provider_hosting::resolve_runtime_mode` for the resolved value.
    pub fn runtime_mode_raw(&self) -> Option<String> {
        match self {
            Self::Legacy(_) => None,
            Self::Detailed(config) => normalized_channel_value(config.runtime.clone()),
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

    pub fn isolate_override(&self) -> Option<bool> {
        match self {
            Self::Legacy(_) => None,
            Self::Detailed(config) => config.isolate_override,
        }
    }

    /// Returns the configured prompt-cache TTL selector in minutes, but only
    /// for the retained 5/60 buckets. Subscription Claude automatically
    /// requests the 1h TTL without an env override, so this value is currently
    /// inert for native sessions; forwarding it to claude-e remains an
    /// unimplemented gap documented in `docs/claude-e-rollout/`.
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

/// Read the retained global prompt-cache TTL selector from the environment
/// (#2661).
///
/// `AGENTDESK_PROMPT_CACHE_DEFAULT_MINUTES` accepts `5` or `60`; anything
/// else (including the variable being unset) returns `None`. The Discord
/// resolver still carries this value through the existing config chain, but
/// native subscription Claude already auto-requests the 1h TTL (no env is
/// needed), and claude-e forwarding is not implemented yet; see
/// `docs/claude-e-rollout/`.
pub fn default_cache_ttl_minutes_from_env() -> Option<u32> {
    let raw = std::env::var("AGENTDESK_PROMPT_CACHE_DEFAULT_MINUTES").ok()?;
    let parsed = raw.trim().parse::<u32>().ok()?;
    normalize_cache_ttl_minutes(Some(parsed))
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
    #[serde(default, alias = "tuiHosting", skip_serializing_if = "Option::is_none")]
    pub tui_hosting: Option<bool>,
    /// Phase 0 of the claude-e rollout. Per-channel override. Same accepted
    /// values as `ProviderConfig::runtime`: `pipe`, `tui`, `claude-e`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
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
    /// Optional per-channel worktree isolation override. When unset, Discord
    /// turns for a channel whose effective provider differs from the agent's
    /// main provider are isolated in a worktree.
    #[serde(
        default,
        alias = "isolateOverride",
        skip_serializing_if = "Option::is_none"
    )]
    pub isolate_override: Option<bool>,
    /// Retained Anthropic prompt-cache TTL selector (#1088). Only `5` or `60`
    /// minutes are valid. Subscription Claude automatically requests the 1h
    /// TTL without an env override, so this field is currently inert for
    /// native sessions. Forwarding it to claude-e is an unimplemented gap;
    /// see `docs/claude-e-rollout/`.
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
    /// QuickJS heap limit for policy runtimes. `0` disables the limit for
    /// local diagnostics; release deployments should keep the default.
    #[serde(default = "default_policy_memory_limit_bytes")]
    pub memory_limit_bytes: usize,
    /// Per-policy hook wall-clock deadline. `0` disables hook deadlines for
    /// emergency diagnostics; hot-reload evals still use their own loader budget.
    #[serde(default = "default_policy_hook_timeout_ms")]
    pub hook_timeout_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DataConfig {
    #[serde(default = "default_data_dir")]
    pub dir: PathBuf,
    #[serde(default = "default_db_name")]
    pub db_name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
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
    /// #3651: best-effort advisory headroom (in connections) for foreground
    /// turn ingestion. Selected high-frequency background chore loops (cluster
    /// stale-GC, outbox claiming, observability retention/snapshot,
    /// session-discovery) voluntarily yield their tick whenever doing so would
    /// push the live in-flight connection count into this band, making
    /// foreground acquire() much less likely to wait under background bursts.
    /// This is an advisory momentary snapshot, NOT a semaphore-backed exclusive
    /// reservation: under churn a few background loops may transiently dip into
    /// the band (self-healing next tick), and not every DB consumer participates
    /// — so it reduces, but does not guarantee elimination of, foreground
    /// starvation. Clamped at startup to keep `pool_max - reserve >= 1`. `0`
    /// disables the backpressure entirely (behavior identical to pre-#3651).
    /// Default `6` leaves `pool_max - 6 = 12` connections for background work,
    /// matching the historical saturation threshold.
    #[serde(default = "default_database_foreground_reserve")]
    pub foreground_reserve: u32,
}

impl Default for DatabaseConfig {
    /// Manual impl so that `DatabaseConfig::default()` (the `Config::default()`
    /// path used when no config file is present) agrees with the per-field
    /// `#[serde(default = ...)]` values used when deserializing a config file.
    /// In particular `foreground_reserve` must be `6` in both paths — a derived
    /// `Default` would give `0`, silently disabling #3651 backpressure in the
    /// no-config-file path.
    fn default() -> Self {
        Self {
            enabled: false,
            host: default_database_host(),
            port: default_database_port(),
            dbname: default_database_name(),
            user: default_database_user(),
            password: None,
            pool_max: default_database_pool_max(),
            foreground_reserve: default_database_foreground_reserve(),
        }
    }
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
    /// #4351: instance that should own the Discord gateway singleton lease — in
    /// practice, the node every conversational tmux session runs on. `None` keeps
    /// the pre-#4351 first-come behavior. Yield protocol and failover semantics:
    /// `discord::runtime_bootstrap::gateway_lease`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_preferred_instance_id: Option<String>,
    /// #4351: how long a non-preferred node stands by for the preferred node
    /// before taking the lease itself. Only consulted while the preferred node is
    /// online and advertising gateway intent.
    #[serde(default = "default_gateway_yield_grace_secs")]
    pub gateway_yield_grace_secs: u64,
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
    #[serde(
        default,
        skip_serializing_if = "ClusterIntakeRoutingConfig::is_default"
    )]
    pub intake_routing: ClusterIntakeRoutingConfig,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub semaphores: BTreeMap<String, ClusterSemaphoreConfig>,
    /// Epic #2285 / E3 + E4 + E5 gate. When `true` (default since E5 / #2412),
    /// the session-bound `WatcherSupervisor` + `StreamRelay` infrastructure runs
    /// in production with a Discord `RelaySink`, and the production tmux frame
    /// producer (`services::discord::tmux_watcher`) pushes every chunk it reads
    /// into the supervisor-owned relay via `RelayProducerRegistry`. The
    /// session-bound sink owns Discord terminal delivery for eligible inflight
    /// shapes (rebind-origin/adopted sessions and watcher-owned relays); the
    /// legacy watcher remains a fallback for bridge-owned/no-inflight envelopes
    /// and runtimes that have no Discord health registry. Setting the flag to
    /// `false` skips the supervisor entirely and the producer-side lookups
    /// become silent no-ops (the registry stays empty), restoring pre-E5
    /// behavior.
    #[serde(default = "default_session_bound_relay_enabled")]
    pub session_bound_relay_enabled: bool,
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
            gateway_preferred_instance_id: None,
            gateway_yield_grace_secs: default_gateway_yield_grace_secs(),
            labels: Vec::new(),
            capabilities: serde_json::Map::new(),
            nodes: BTreeMap::new(),
            blackout_windows: BTreeMap::new(),
            dispatch_routing: ClusterDispatchRoutingConfig::default(),
            intake_routing: ClusterIntakeRoutingConfig::default(),
            semaphores: BTreeMap::new(),
            session_bound_relay_enabled: default_session_bound_relay_enabled(),
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
pub struct ClusterIntakeRoutingConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "ClusterIntakeRoutingMode::is_default")]
    pub mode: ClusterIntakeRoutingMode,
    /// Raw top-level Discord channel IDs opted into owner-authority planning.
    /// PR-1 records this scope in telemetry only; it does not change enforcement.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owner_authority_channel_ids: Vec<String>,
    #[serde(default = "default_intake_forward_pre_claim_timeout_secs")]
    pub forward_pre_claim_timeout_secs: u64,
    #[serde(default = "default_intake_stale_claim_recovery_secs")]
    pub stale_claim_recovery_secs: u64,
}

impl Default for ClusterIntakeRoutingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: ClusterIntakeRoutingMode::default(),
            owner_authority_channel_ids: Vec::new(),
            forward_pre_claim_timeout_secs: default_intake_forward_pre_claim_timeout_secs(),
            stale_claim_recovery_secs: default_intake_stale_claim_recovery_secs(),
        }
    }
}

impl ClusterIntakeRoutingConfig {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ClusterIntakeRoutingMode {
    Disabled,
    #[default]
    Observe,
    Enforce,
}

impl ClusterIntakeRoutingMode {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Observe => "observe",
            Self::Enforce => "enforce",
        }
    }
}

fn default_intake_forward_pre_claim_timeout_secs() -> u64 {
    12
}

fn default_intake_stale_claim_recovery_secs() -> u64 {
    60
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
    use super::{ClusterConfig, ClusterIntakeRoutingMode, ClusterSemaphoreScope};

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

    #[test]
    fn cluster_intake_routing_parses_defaults_and_yaml_fields() {
        let default_config: ClusterConfig =
            serde_yaml::from_str("enabled: true\n").expect("cluster config parses");
        assert!(!default_config.intake_routing.enabled);
        assert_eq!(
            default_config.intake_routing.mode,
            ClusterIntakeRoutingMode::Observe
        );
        assert_eq!(
            default_config.intake_routing.forward_pre_claim_timeout_secs,
            12
        );
        assert_eq!(default_config.intake_routing.stale_claim_recovery_secs, 60);
        assert!(
            default_config
                .intake_routing
                .owner_authority_channel_ids
                .is_empty()
        );

        let config: ClusterConfig = serde_yaml::from_str(
            r#"
enabled: true
intake_routing:
  enabled: true
  mode: enforce
  owner_authority_channel_ids:
    - "123456789012345678"
    - "223456789012345678"
  forward_pre_claim_timeout_secs: 13
  stale_claim_recovery_secs: 61
"#,
        )
        .expect("cluster intake routing config parses");

        assert!(config.intake_routing.enabled);
        assert_eq!(
            config.intake_routing.mode,
            ClusterIntakeRoutingMode::Enforce
        );
        assert_eq!(
            config.intake_routing.owner_authority_channel_ids,
            ["123456789012345678", "223456789012345678"]
        );
        assert_eq!(config.intake_routing.forward_pre_claim_timeout_secs, 13);
        assert_eq!(config.intake_routing.stale_claim_recovery_secs, 61);

        let invalid: Result<ClusterConfig, _> = serde_yaml::from_str(
            r#"
intake_routing:
  mode: maybe
"#,
        );
        assert!(invalid.is_err());
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
    /// #3561 — operator override for the relay-signal alert threshold. When
    /// set, every per-hour relay invariant signal (terminal-ack timeout,
    /// uncommitted-inflight-cleared, owner-unknown, offset invariant
    /// violation) fires once its hourly window count reaches this value,
    /// overriding the conservative per-signal code defaults. `None` keeps the
    /// built-in defaults; it does NOT disable alerting (the alert target —
    /// `kanban_human_alert_channel_id` — remaining unset is the real off
    /// switch, so an unconfigured deploy never spams).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_alert_threshold: Option<u32>,
}

impl KanbanConfig {
    pub fn is_empty(&self) -> bool {
        self.manager_channel_id.is_none()
            && self.deadlock_manager_channel_id.is_none()
            && self.human_alert_channel_id.is_none()
            && self.pm_decision_gate_enabled.is_none()
            && self.relay_alert_threshold.is_none()
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
    /// #3805 P2: rollout gate for the two-message panel model (answer chunks
    /// first, the live status panel re-anchored beneath the latest chunk).
    /// Default OFF and restart-required — the whole `placeholder` section is
    /// boot-bound, copied into shared UI state once at startup (see
    /// `config_live_reload::restart_required_changes`). PR-A is pure
    /// scaffolding: no branch reads this flag yet (later PR-B~ gates the
    /// two-message create/re-anchor path behind it).
    #[serde(default)]
    pub two_message_panel_enabled: bool,
}

impl Default for PlaceholderConfig {
    fn default() -> Self {
        Self {
            live_events_enabled: true,
            status_panel_v2_enabled: true,
            two_message_panel_enabled: false,
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
    /// Minimum token occupancy at which context compaction may be requested.
    ///
    /// Unset uses the live consumer default (currently 300_000 tokens for
    /// Claude). This is deliberately provider-neutral because other providers
    /// can share the lower-bound policy without inheriting Claude's transport.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_compact_lower_bound_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub claude_gateway_proxy_enabled: bool,
    #[serde(
        default = "default_claude_gateway_proxy_url",
        skip_serializing_if = "is_default_claude_gateway_proxy_url"
    )]
    pub claude_gateway_proxy_url: String,
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
    /// Number of completed user/assistant pairs from the same Discord channel
    /// added as background context when a fresh provider session starts.
    /// Unset defaults to 3, `0` disables the layer, and values are clamped to 10.
    /// Read live for each turn through `config_live_reload::current()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_context_recent_pairs: Option<u64>,
    /// Optional override (seconds) for the Follow-up TUI prompt-readiness wait.
    /// When unset (or `0`), both the Claude and Codex TUI follow-up waits keep
    /// the compiled-in 45s default (`FOLLOWUP_PROMPT_READY_TIMEOUT`). Read live
    /// via `config_live_reload::current()` so an `agentdesk.yaml` edit applies
    /// on the next readiness wait without a restart.
    ///
    /// Note: the Claude follow-up wait is still bounded by an independent 900s
    /// busy-turn ceiling (`PROMPT_READY_ACTIVE_TURN_WAIT_CEILING`), but the Codex
    /// follow-up wait has no such ceiling, so a very large value lets a long
    /// prior Codex turn block the follow-up wait for the full configured duration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub followup_prompt_ready_timeout_secs: Option<u64>,
    /// Master rollback flag for the read-only DB active-session mismatch audit
    /// surfaced on `/api/health/detail` (`active_session_audit` block). When
    /// unset it defaults to ON; `Some(false)` makes the audit report
    /// `enabled:false` with empty candidates and skips the DB query entirely.
    /// Read live via `config_live_reload::current()` so an `agentdesk.yaml` edit
    /// applies on the next `/api/health/detail` call without a restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_session_audit_enabled: Option<bool>,
    /// Minimum seconds since `last_heartbeat` before a raw-active session can be
    /// flagged by the active-session mismatch audit (post-restart/long-turn
    /// grace). Unset (or `0`) falls back to the compiled-in 120s default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_session_audit_stale_secs: Option<u64>,
    /// Hard cap on audit candidate rows AND the SQL `LIMIT`. Unset falls back to
    /// the compiled-in 50 default; clamped to `1..=500` when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_session_audit_max_candidates: Option<u64>,
    /// TTL (seconds) for the in-memory TUI hook registry buffer. A hook that has
    /// been buffered longer than this is swept and never replayed to a claiming
    /// listener, so a stale Stop from a previous turn cannot wake a fresh turn.
    /// When unset (or `0`) the compiled-in 30s default
    /// (`hook_registry::DEFAULT_HOOK_BUFFER_TTL`) is used.
    ///
    /// NOT hot-reloadable: this value is captured ONCE when the process-global
    /// `hook_registry::GLOBAL` is first accessed (effectively at process start)
    /// and stored on the immutable `HookRegistry.ttl`. Editing it in
    /// `agentdesk.yaml` takes effect only on the next process start (restart
    /// required). Only `tui_hook_registry_enabled` is read live per-hook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tui_hook_buffer_ttl_secs: Option<u64>,
    /// Diagnostic delay (milliseconds) before an unclaimed Stop in the TUI hook
    /// registry is considered "elapsed". Diagnostic-only in P0 — it never
    /// triggers a transcript sync or finalization. When unset (or `0`) the
    /// compiled-in 2000ms default (`hook_registry::DEFAULT_UNCLAIMED_STOP_DELAY`)
    /// is used.
    ///
    /// NOT hot-reloadable: like `tui_hook_buffer_ttl_secs`, this is captured ONCE
    /// when `hook_registry::GLOBAL` is first accessed (process start) and stored
    /// on the immutable `HookRegistry.unclaimed_stop_delay`. Editing it in
    /// `agentdesk.yaml` takes effect only on the next process start (restart
    /// required). Only `tui_hook_registry_enabled` is genuinely hot-reloadable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tui_unclaimed_stop_delay_ms: Option<u64>,
    /// Rollback switch for the TUI hook registry buffering layer. Defaults to ON
    /// (`None` => enabled). Set to `false` in `agentdesk.yaml` to stop feeding
    /// the registry from the hook receiver, leaving the legacy broadcast +
    /// polling path exactly as before. Genuinely hot-reloadable (no restart
    /// required): `registry_enabled()` reads it live per-hook. This is the ONLY
    /// live-reloadable key of the three TUI hook registry settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tui_hook_registry_enabled: Option<bool>,
    /// Enable the in-process Codex rollout discovery index cache used by the
    /// Codex TUI resume / follow-up readiness paths
    /// (`codex_tui::rollout_index`). The cache avoids re-walking
    /// `~/.codex/sessions` and re-reading rollout headers on every lookup.
    ///
    /// Defaults ON when unset (`None`). Set to `false` to force the legacy
    /// per-lookup recursive scan + header read — the built-in rollback for the
    /// `codex-rollout-index-cache` feature. Read live via
    /// `config_live_reload::current()` so an `agentdesk.yaml` edit applies on the
    /// next lookup without a restart; not part of the restart-required set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_rollout_index_cache_enabled: Option<bool>,
    /// Rate-limit-aware dispatch gate toggle (feature:
    /// rate-limit-aware-dispatch-gate). When `Some(true)` or unset (`None` —
    /// the safe default is ON), auto-queue activation defers a pending entry
    /// whose target provider is at/above `rate_limit_danger_pct` utilization in
    /// the live in-memory rate-limit snapshot, instead of creating a doomed
    /// dispatch. The entry stays `pending` (never `skipped`) and resumes
    /// automatically once pressure clears. Read live via
    /// `config_live_reload::current()` so an `agentdesk.yaml` edit applies on
    /// the next activation without a restart. Set to `false` to disable the
    /// gate cleanly (every activation then falls through to normal dispatch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_rate_limit_gate_enabled: Option<bool>,
    /// Gate-specific danger threshold (utilization %) for the rate-limit-aware
    /// dispatch gate (feature: rate-limit-aware-dispatch-gate). This is a
    /// SEPARATE knob from `rate_limit_danger_pct` (which drives the dashboard's
    /// "danger" coloring at 95): the operator wants the dispatch gate to defer
    /// ONLY when a provider is fully rate-limited (utilization at/above 100),
    /// so the gate defaults to 100 here and never touches `rate_limit_danger_pct`
    /// — other consumers of `rate_limit_danger_pct` are unaffected. When unset
    /// (`None`), the gate uses the compiled-in default of 100. Read live (via
    /// the persisted runtime-config / `config_live_reload::current()`) so an
    /// edit applies on the next activation without a restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_rate_limit_gate_danger_pct: Option<u8>,
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
            && self.context_compact_lower_bound_tokens.is_none()
            && !self.claude_gateway_proxy_enabled
            && is_default_claude_gateway_proxy_url(&self.claude_gateway_proxy_url)
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
            && self.session_context_recent_pairs.is_none()
            && self.followup_prompt_ready_timeout_secs.is_none()
            && self.active_session_audit_enabled.is_none()
            && self.active_session_audit_stale_secs.is_none()
            && self.active_session_audit_max_candidates.is_none()
            && self.tui_hook_buffer_ttl_secs.is_none()
            && self.tui_unclaimed_stop_delay_ms.is_none()
            && self.tui_hook_registry_enabled.is_none()
            && self.codex_rollout_index_cache_enabled.is_none()
            && self.dispatch_rate_limit_gate_enabled.is_none()
            && self.dispatch_rate_limit_gate_danger_pct.is_none()
            && !self.reset_overrides_on_restart
    }
}

pub(crate) const DEFAULT_CLAUDE_GATEWAY_PROXY_URL: &str = "http://127.0.0.1:10100";

fn default_claude_gateway_proxy_url() -> String {
    DEFAULT_CLAUDE_GATEWAY_PROXY_URL.to_string()
}

fn is_default_claude_gateway_proxy_url(value: &str) -> bool {
    value.is_empty() || value == DEFAULT_CLAUDE_GATEWAY_PROXY_URL
}

impl RuntimeSettingsConfig {
    pub(crate) fn resolved_claude_gateway_proxy_url(&self) -> &str {
        if self.claude_gateway_proxy_url.is_empty() {
            DEFAULT_CLAUDE_GATEWAY_PROXY_URL
        } else {
            &self.claude_gateway_proxy_url
        }
    }
}

#[cfg(test)]
mod runtime_hook_registry_config_tests {
    use super::*;

    // #tui-hook-ttl-buffer: the new runtime keys participate in `is_empty` so a
    // config that sets only one of them still serializes the `runtime` section
    // (otherwise the `skip_serializing_if = "RuntimeSettingsConfig::is_empty"`
    // on the parent would drop the operator's override on a round-trip).
    #[test]
    fn hook_registry_keys_count_toward_is_empty() {
        assert!(RuntimeSettingsConfig::default().is_empty());

        let ttl_only = RuntimeSettingsConfig {
            tui_hook_buffer_ttl_secs: Some(45),
            ..RuntimeSettingsConfig::default()
        };
        assert!(!ttl_only.is_empty());

        let delay_only = RuntimeSettingsConfig {
            tui_unclaimed_stop_delay_ms: Some(3000),
            ..RuntimeSettingsConfig::default()
        };
        assert!(!delay_only.is_empty());

        // The rollback flag also keeps the section alive when set to either bool.
        let disabled = RuntimeSettingsConfig {
            tui_hook_registry_enabled: Some(false),
            ..RuntimeSettingsConfig::default()
        };
        assert!(!disabled.is_empty());
    }

    #[test]
    fn claude_gateway_proxy_defaults_off_with_loopback_url() {
        let parsed: RuntimeSettingsConfig = serde_yaml::from_str("{}").unwrap();
        assert!(!parsed.claude_gateway_proxy_enabled);
        assert_eq!(
            parsed.claude_gateway_proxy_url,
            DEFAULT_CLAUDE_GATEWAY_PROXY_URL
        );
        assert!(parsed.is_empty());

        let enabled: RuntimeSettingsConfig =
            serde_yaml::from_str("claude_gateway_proxy_enabled: true\n").unwrap();
        assert!(enabled.claude_gateway_proxy_enabled);
        assert_eq!(
            enabled.resolved_claude_gateway_proxy_url(),
            DEFAULT_CLAUDE_GATEWAY_PROXY_URL
        );
        assert!(!enabled.is_empty());
    }

    // The keys survive a YAML round-trip with their types intact, and an absent
    // section deserializes to `None` (defaults applied by the consumer).
    #[test]
    fn hook_registry_keys_round_trip_through_yaml() {
        let yaml = "tui_hook_buffer_ttl_secs: 60\n\
                    tui_unclaimed_stop_delay_ms: 1500\n\
                    tui_hook_registry_enabled: false\n";
        let parsed: RuntimeSettingsConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(parsed.tui_hook_buffer_ttl_secs, Some(60));
        assert_eq!(parsed.tui_unclaimed_stop_delay_ms, Some(1500));
        assert_eq!(parsed.tui_hook_registry_enabled, Some(false));

        let reserialized = serde_yaml::to_string(&parsed).unwrap();
        let reparsed: RuntimeSettingsConfig = serde_yaml::from_str(&reserialized).unwrap();
        assert_eq!(reparsed, parsed);

        // Absent keys default to None so the consumer falls back to the
        // compiled-in defaults (None-safe handling).
        let empty: RuntimeSettingsConfig = serde_yaml::from_str("{}").unwrap();
        assert_eq!(empty.tui_hook_buffer_ttl_secs, None);
        assert_eq!(empty.tui_unclaimed_stop_delay_ms, None);
        assert_eq!(empty.tui_hook_registry_enabled, None);
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

/// Default PM-hours window applied when escalation settings leave it unset.
pub const DEFAULT_ESCALATION_PM_HOURS: &str = "00:00-08:00";
/// Default timezone applied when escalation settings leave it unset.
pub const DEFAULT_ESCALATION_TIMEZONE: &str = "Asia/Seoul";

/// Resolved (non-optional) escalation schedule used by the settings API and
/// Discord text commands. Distinct from [`EscalationScheduleConfig`], which is
/// the optional on-disk config representation; this is the materialized
/// wire/runtime shape with defaults applied.
///
/// Lives in the config (domain) layer so service-layer callers can depend on it
/// without reaching back into the server route module that owns the HTTP
/// handlers (#3037 service→server backflow).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct EscalationScheduleSettings {
    pub pm_hours: String,
    pub timezone: String,
}

impl Default for EscalationScheduleSettings {
    fn default() -> Self {
        Self {
            pm_hours: DEFAULT_ESCALATION_PM_HOURS.to_string(),
            timezone: DEFAULT_ESCALATION_TIMEZONE.to_string(),
        }
    }
}

/// Resolved escalation settings (materialized wire/runtime shape with defaults
/// applied). See [`EscalationScheduleSettings`] for the layering rationale.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct EscalationSettings {
    pub mode: EscalationMode,
    pub owner_user_id: Option<u64>,
    pub pm_channel_id: Option<String>,
    pub schedule: EscalationScheduleSettings,
}

impl Default for EscalationSettings {
    fn default() -> Self {
        Self {
            mode: EscalationMode::Pm,
            owner_user_id: None,
            pm_channel_id: None,
            schedule: EscalationScheduleSettings::default(),
        }
    }
}

/// API response body for the escalation settings endpoint: the current
/// effective settings plus the computed defaults.
#[derive(Debug, Deserialize, Serialize)]
pub struct EscalationSettingsResponse {
    pub current: EscalationSettings,
    pub defaults: EscalationSettings,
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
    // reason: onboarding suffix-resolution config API mirroring the dashboard
    // `setupWizardHelpers.ts`; test-covered but not yet wired into a production
    // caller (the live path uses dispatch::provider_from_channel_suffix).
    #[allow(dead_code)]
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
    // reason: onboarding config-driven suffix resolver; covered by config tests,
    // pending wiring into the setup flow.
    #[allow(dead_code)]
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
    // reason: onboarding category resolver (label or raw Discord ID); test-covered,
    // pending wiring into the setup flow.
    #[allow(dead_code)]
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
// reason: helper for provider_from_channel_suffix (onboarding config API), dead
// until that resolver is wired into the setup flow.
#[allow(dead_code)]
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

fn default_policy_memory_limit_bytes() -> usize {
    128 * 1024 * 1024
}

fn default_policy_hook_timeout_ms() -> u64 {
    5_000
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
    // Sized for the always-on background DB consumers (cluster heartbeat,
    // dispatch/message outbox claiming, observability flush, session-discovery,
    // policy-tick, routine recovery) plus foreground turn ingestion. At 12 the
    // steady-state pool was already near-saturated and any burst (e.g. a
    // post-restart catch-up sweep coinciding with a turn) pushed it into
    // sustained `acquire_timeout` errors, delaying message ingestion.
    //
    // Upper bound is the shared Postgres `max_connections` (100, minus 3
    // superuser-reserved = 97 usable). During boot a node runs BOTH the runtime
    // pool (this value) AND the startup warmup pool (1.5x, see
    // `startup_pool_settings`) concurrently until boot reconcile drops the
    // warmup pool — a per-node peak of `2.5 * pool_max`. With two cluster nodes
    // potentially booting at once (coordinated deploy), the worst case is
    // `2 * 2.5 * pool_max`, which must stay under 97. 18 → 2*(18+27)=90, leaving
    // headroom for psql/admin. Going higher requires raising Postgres
    // `max_connections` or capping the warmup multiplier first.
    18
}
fn default_database_foreground_reserve() -> u32 {
    // #3651: best-effort advisory headroom held back from selected background
    // chore loops so foreground turn ingestion is much less likely to wait for
    // a connection (not a hard guarantee — see DatabaseConfig::foreground_reserve).
    // With pool_max=18 this leaves 12 connections for background work — the same
    // level at which the pool was historically near-saturated, so steady-state
    // background behaviour is unchanged. `0` disables the backpressure
    // (behaviour-preserving).
    6
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
/// #4351. Covers `deploy-release.sh` restarting the local node then SSH-deploying
/// a peer; a dead preferred node only delays the gateway by this much.
fn default_gateway_yield_grace_secs() -> u64 {
    90
}
fn default_session_bound_relay_enabled() -> bool {
    // Epic #2285 / E5 (#2412): flipped to `true` once the production tmux
    // frame producer (`services::discord::tmux_watcher`) pushed frames into
    // the supervisor-owned StreamRelay via `RelayProducerRegistry`. E4
    // (#2346) now wires that relay to a Discord sink for session-bound
    // terminal delivery on eligible inflight shapes, while preserving the
    // legacy watcher as fallback for bridge-owned/no-inflight envelopes.
    true
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
            allow_insecure_nonloopback_bind: false,
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
            memory_limit_bytes: default_policy_memory_limit_bytes(),
            hook_timeout_ms: default_policy_hook_timeout_ms(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RoutinesConfig {
    /// Master on/off switch. Defaults to false; requires PostgreSQL and is
    /// unavailable without the PG control plane.
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
    /// Maximum serialized checkpoint payload accepted from a routine run.
    #[serde(default = "default_routines_max_checkpoint_bytes")]
    pub max_checkpoint_bytes: usize,
    /// Watch `dir` for script changes and reload without restart.
    #[serde(default = "default_true")]
    pub hot_reload: bool,
    /// Alert the operator (Discord) when a routine has been `paused` for at
    /// least this many seconds. A routine that fails/times out can become stuck
    /// in `paused` indefinitely (it is excluded from claims), so this surfaces
    /// the stall instead of letting it silently never run again (#3564).
    /// Defaults to 0, which disables the alert and preserves prior behavior.
    #[serde(default)]
    pub stale_paused_alert_secs: u64,
    /// Dedupe window for the stale-paused alert, in seconds. The tick fires
    /// every `tick_interval_secs`, so without a long dedupe TTL the alert would
    /// re-fire every tick for every stuck routine. Defaults to 86400 (once per
    /// day per routine).
    #[serde(default = "default_routines_stale_paused_alert_ttl_secs")]
    pub stale_paused_alert_ttl_secs: u64,
    /// Opt-in auto-resume: automatically re-enable routines whose
    /// `pause_reason = 'failure'` and that have been paused for at least this
    /// many seconds (backoff window). Routines with `pause_reason = 'manual'`,
    /// `'migration_invalid'`, or `NULL` (pre-existing rows) are NEVER touched.
    /// The `ResumeRequiresNextDueAt` guard also applies: schedule-less routines
    /// with no `next_due_at` are skipped.
    ///
    /// Defaults to 0 (disabled). Set to e.g. 3600 (1 hour) to enable.
    #[serde(default)]
    pub failure_pause_auto_resume_secs: u64,
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
            max_checkpoint_bytes: default_routines_max_checkpoint_bytes(),
            hot_reload: true,
            stale_paused_alert_secs: 0,
            stale_paused_alert_ttl_secs: default_routines_stale_paused_alert_ttl_secs(),
            failure_pause_auto_resume_secs: 0,
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

fn default_routines_max_checkpoint_bytes() -> usize {
    256 * 1024
}

fn default_routines_stale_paused_alert_ttl_secs() -> u64 {
    24 * 60 * 60
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

    #[test]
    fn stale_paused_knobs_default_to_disabled_alert_preserving_prior_behavior() {
        // Omitting the new field must leave the stale-paused alert disabled (0)
        // so the tick loop never enters the block — i.e. existing deployments
        // behave exactly as before (#3564).
        let config: RoutinesConfig = serde_json::from_str("{}").expect("empty routines config");
        assert_eq!(config.stale_paused_alert_secs, 0);
        // The dedupe TTL still defaults to a non-spammy once-per-day window so
        // that, once the alert is enabled, it cannot flood the channel.
        assert_eq!(config.stale_paused_alert_ttl_secs, 24 * 60 * 60);
        assert_eq!(config, RoutinesConfig::default());
    }

    #[test]
    fn stale_paused_knobs_deserialize_when_provided() {
        let config: RoutinesConfig = serde_json::from_str(
            r#"{
                "stale_paused_alert_secs": 86400,
                "stale_paused_alert_ttl_secs": 43200
            }"#,
        )
        .expect("routines config with stale-paused knobs");
        assert_eq!(config.stale_paused_alert_secs, 86_400);
        assert_eq!(config.stale_paused_alert_ttl_secs, 43_200);
    }

    #[test]
    fn database_foreground_reserve_default_is_consistent_across_paths() {
        // #3651: the `Config::default()` path (used when no config file exists)
        // and the serde-deserialization path (used for an on-disk config) must
        // agree on the foreground reserve. A derived `Default` would give 0 here
        // and silently disable the backpressure in the no-config-file path.
        assert_eq!(DatabaseConfig::default().foreground_reserve, 6);
        assert_eq!(default_database_foreground_reserve(), 6);

        let from_empty: DatabaseConfig = serde_json::from_str("{}").expect("empty database config");
        assert_eq!(from_empty.foreground_reserve, 6);
        // The hand-written Default must match a fully-defaulted deserialization
        // for every field, not just the reserve.
        assert_eq!(from_empty, DatabaseConfig::default());
    }

    #[test]
    fn database_foreground_reserve_deserializes_when_provided() {
        // Operators can set 0 to disable the backpressure entirely.
        let disabled: DatabaseConfig =
            serde_json::from_str(r#"{ "foreground_reserve": 0 }"#).expect("reserve 0");
        assert_eq!(disabled.foreground_reserve, 0);

        let custom: DatabaseConfig =
            serde_json::from_str(r#"{ "foreground_reserve": 9 }"#).expect("reserve 9");
        assert_eq!(custom.foreground_reserve, 9);
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

    fn resolve_runtime_relative_paths(mut self, runtime_root: Option<&Path>) -> Self {
        let Some(root) = runtime_root else {
            return self;
        };

        self.policies.dir = resolve_runtime_path(root, &self.policies.dir);
        self.data.dir = resolve_runtime_path(root, &self.data.dir);
        self.routines.dir = resolve_runtime_path(root, &self.routines.dir);
        self.routines.additional_dirs = self
            .routines
            .additional_dirs
            .into_iter()
            .map(|dir| resolve_runtime_path(root, &dir))
            .collect();
        self
    }
}

fn resolve_runtime_path(root: &Path, raw: &Path) -> PathBuf {
    let expanded = crate::runtime_layout::expand_user_path(&raw.to_string_lossy())
        .unwrap_or_else(|| raw.to_path_buf());
    if expanded.is_absolute() {
        expanded
    } else {
        root.join(expanded)
    }
}

fn runtime_root_for_config_path(path: &Path) -> Option<PathBuf> {
    if let Ok(override_root) = std::env::var("AGENTDESK_ROOT_DIR") {
        let trimmed = override_root.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }

    let file_name = path.file_name()?;
    if file_name != OsStr::new("agentdesk.yaml") {
        return None;
    }

    let parent = path.parent()?;
    if parent.file_name() == Some(OsStr::new("config")) {
        return parent.parent().map(Path::to_path_buf);
    }
    Some(parent.to_path_buf())
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            discord: DiscordConfig::default(),
            providers: std::collections::BTreeMap::new(),
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
            config_hot_reload: default_true(),
        }
        .apply_runtime_defaults()
    }
}

pub fn load() -> Result<Config> {
    crate::utils::redact::register_common_env_secrets();
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
    let runtime_root = runtime_root_for_config_path(&path);
    let config = config
        .apply_runtime_defaults()
        .resolve_runtime_relative_paths(runtime_root.as_deref());
    register_config_secrets(&config);
    audit_config_file_permissions_if_secret_bearing(&path, &config);
    validate_config(&config).with_context(|| format!("Invalid config: {path_display}"))?;

    // Ensure data dir exists
    std::fs::create_dir_all(&config.data.dir)?;

    Ok(config)
}

/// The on-disk config path the running server loaded from, resolved with the
/// same precedence as [`load`] (`$AGENTDESK_CONFIG` → runtime root → cwd → home).
/// Used by the config file watcher so it reloads from the exact same file the
/// boot path read. The returned path is the resolved canonical candidate even if
/// it does not currently exist (matching the graceful resolver).
pub fn resolved_config_path() -> PathBuf {
    resolve_graceful_config_path(
        std::env::var("AGENTDESK_CONFIG")
            .ok()
            .map(std::path::PathBuf::from),
        runtime_root(),
        std::env::current_dir().ok(),
        dirs::home_dir(),
    )
}

pub fn load_from_path(path: &Path) -> Result<Config> {
    crate::utils::redact::register_common_env_secrets();
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config {}", path.display()))?;
    let config = serde_yaml::from_str::<Config>(&contents)
        .with_context(|| format!("Failed to parse config {}", path.display()))?;
    let runtime_root = runtime_root_for_config_path(path);
    let config = config
        .apply_runtime_defaults()
        .resolve_runtime_relative_paths(runtime_root.as_deref());
    register_config_secrets(&config);
    audit_config_file_permissions_if_secret_bearing(path, &config);
    validate_config(&config).with_context(|| format!("Invalid config {}", path.display()))?;
    Ok(config)
}

fn validate_config(config: &Config) -> Result<()> {
    validate_escalation_schedule(&config.escalation.schedule)
}

fn validate_escalation_schedule(schedule: &EscalationScheduleConfig) -> Result<()> {
    if let Some(timezone) = schedule.timezone.as_deref()
        && timezone.parse::<chrono_tz::Tz>().is_err()
    {
        bail!("schedule.timezone must be a valid IANA timezone");
    }
    if let Some(pm_hours) = schedule.pm_hours.as_deref()
        && parse_escalation_time_window(pm_hours).is_none()
    {
        bail!("schedule.pm_hours must be HH:MM-HH:MM");
    }
    Ok(())
}

fn parse_escalation_time_window(raw: &str) -> Option<(chrono::NaiveTime, chrono::NaiveTime)> {
    let (start, end) = raw.trim().split_once('-')?;
    let start = chrono::NaiveTime::parse_from_str(start.trim(), "%H:%M").ok()?;
    let end = chrono::NaiveTime::parse_from_str(end.trim(), "%H:%M").ok()?;
    Some((start, end))
}

fn register_config_secrets(config: &Config) {
    if let Some(token) = config.server.auth_token.as_deref() {
        crate::utils::redact::register_known_secret(token);
    }
    if let Some(password) = config.database.password.as_deref() {
        crate::utils::redact::register_known_secret(password);
    }
    for bot in config.discord.bots.values() {
        if let Some(token) = bot.token.as_deref() {
            crate::utils::redact::register_known_secret(token);
        }
    }
    for server in config.mcp_servers.values() {
        if let Some(auth) = server.auth.as_ref()
            && let Some(env_var) = auth.token_env_var.as_deref()
            && let Ok(value) = std::env::var(env_var)
        {
            crate::utils::redact::register_known_secret(&value);
        }
    }
}

fn config_contains_file_secrets(config: &Config) -> bool {
    config.server.auth_token.is_some()
        || config.database.password.is_some()
        || config.discord.bots.values().any(|bot| {
            bot.token
                .as_deref()
                .is_some_and(|token| !token.trim().is_empty())
        })
}

fn audit_config_file_permissions_if_secret_bearing(path: &Path, config: &Config) {
    if config_contains_file_secrets(config) {
        crate::utils::secret_file::audit_or_harden_secret_file(path, "agentdesk-config");
    }
}

pub fn save_to_path(path: &Path, config: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let rendered = serde_yaml::to_string(config)
        .with_context(|| format!("Failed to serialize config for {}", path.display()))?;
    if config_contains_file_secrets(config) {
        crate::utils::secret_file::write_secret_file(path, rendered)
            .with_context(|| format!("Failed to write config {}", path.display()))?;
    } else {
        std::fs::write(path, rendered)
            .with_context(|| format!("Failed to write config {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod secret_bearing_config_file_tests {
    use super::*;

    #[test]
    fn load_from_path_rejects_invalid_escalation_schedule_timezone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentdesk.yaml");
        let mut config = Config::default();
        config.escalation.schedule.timezone = Some("Mars/Olympus".to_string());
        save_to_path(&path, &config).unwrap();

        let error = format!("{:#}", load_from_path(&path).unwrap_err());

        assert!(error.contains("schedule.timezone must be a valid IANA timezone"));
    }

    #[test]
    fn load_from_path_rejects_invalid_escalation_schedule_pm_hours() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentdesk.yaml");
        let mut config = Config::default();
        config.escalation.schedule.pm_hours = Some("soon".to_string());
        save_to_path(&path, &config).unwrap();

        let error = format!("{:#}", load_from_path(&path).unwrap_err());

        assert!(error.contains("schedule.pm_hours must be HH:MM-HH:MM"));
    }

    #[test]
    fn save_and_load_harden_secret_bearing_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentdesk.yaml");
        let mut config = Config::default();
        config.database.password = Some("database-secret".to_string());

        save_to_path(&path, &config).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let saved_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(saved_mode, 0o600);

            let mut loose_permissions = std::fs::metadata(&path).unwrap().permissions();
            loose_permissions.set_mode(0o644);
            std::fs::set_permissions(&path, loose_permissions).unwrap();

            let loaded = load_from_path(&path).unwrap();
            assert_eq!(loaded.database.password.as_deref(), Some("database-secret"));

            let hardened_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(hardened_mode, 0o600);
        }

        #[cfg(not(unix))]
        {
            let loaded = load_from_path(&path).unwrap();
            assert_eq!(loaded.database.password.as_deref(), Some("database-secret"));
        }
    }

    #[test]
    #[cfg(unix)]
    fn load_from_path_hardens_secret_file_before_semantic_validation_error() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentdesk.yaml");
        let mut config = Config::default();
        config.database.password = Some("database-secret".to_string());
        config.escalation.schedule.timezone = Some("Mars/Olympus".to_string());
        save_to_path(&path, &config).unwrap();

        let mut loose_permissions = std::fs::metadata(&path).unwrap().permissions();
        loose_permissions.set_mode(0o644);
        std::fs::set_permissions(&path, loose_permissions).unwrap();

        let error = format!("{:#}", load_from_path(&path).unwrap_err());

        assert!(error.contains("schedule.timezone must be a valid IANA timezone"));
        let hardened_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(hardened_mode, 0o600);
    }
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
            Ok(cfg) => {
                let runtime_root = runtime_root_for_config_path(&path);
                cfg.apply_runtime_defaults()
                    .resolve_runtime_relative_paths(runtime_root.as_deref())
            }
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

#[cfg(test)]
pub(crate) mod test_env_lock {
    //! Canonical acquisition path for the shared test-environment mutex.
    //! New test sites must use `acquire_shared_test_env_lock`; directly locking
    //! `shared_test_env_lock` is forbidden.

    thread_local! {
        static SHARED_TEST_ENV_LOCK_HELD: std::cell::Cell<bool> =
            const { std::cell::Cell::new(false) };
    }

    pub(crate) struct SharedTestEnvLockGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for SharedTestEnvLockGuard {
        fn drop(&mut self) {
            SHARED_TEST_ENV_LOCK_HELD.with(|held| held.set(false));
        }
    }

    pub(crate) fn acquire_shared_test_env_lock() -> SharedTestEnvLockGuard {
        SHARED_TEST_ENV_LOCK_HELD.with(|held| {
            if held.get() {
                panic!("shared_test_env_lock re-entry detected before mutex acquisition");
            }
            held.set(true);
        });
        let lock = super::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        SharedTestEnvLockGuard { _lock: lock }
    }
}

#[cfg(test)]
pub(crate) struct TestEnvVarGuard {
    _lock: Option<test_env_lock::SharedTestEnvLockGuard>,
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

#[cfg(test)]
impl TestEnvVarGuard {
    pub(crate) fn set_path(key: &'static str, value: &std::path::Path) -> Self {
        let lock = test_env_lock::acquire_shared_test_env_lock();
        let previous = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self {
            _lock: Some(lock),
            key,
            previous,
        }
    }

    pub(crate) fn set_path_after_shared_test_env_lock(
        key: &'static str,
        value: &std::path::Path,
    ) -> Self {
        let previous = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self {
            _lock: None,
            key,
            previous,
        }
    }
}

#[cfg(test)]
impl Drop for TestEnvVarGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

#[cfg(test)]
pub(crate) fn set_agentdesk_root_for_test(path: &std::path::Path) -> TestEnvVarGuard {
    TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", path)
}

/// Compatibility shim for legacy provider signatures that still mention
/// `remote_profiles`.
///
/// AgentDesk does not load remote profiles from operator config. `Settings`
/// intentionally returns an empty list so those signatures cannot be mistaken
/// for supported remote SSH behavior. Future remote SSH work must use the
/// #2193 `providers.codex.remote_hosts` contract instead.
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

#[cfg(test)]
mod remote_settings_tests {
    use super::Settings;

    #[test]
    fn settings_load_exposes_no_remote_profiles() {
        let settings = Settings::load();
        assert!(
            settings.remote_profiles.is_empty(),
            "remote profiles are not loaded from AgentDesk config; use the #2193 remote_hosts ADR"
        );
    }
}

#[cfg(test)]
mod shared_test_env_lock_tests {
    #[test]
    fn acquire_shared_test_env_lock_panics_on_same_thread_reentry_before_deadlock() {
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let _lock = super::test_env_lock::acquire_shared_test_env_lock();
            let reentry = std::panic::catch_unwind(|| {
                let _nested = super::test_env_lock::acquire_shared_test_env_lock();
            });
            tx.send(reentry.is_err()).expect("send reentry result");
        });

        let panicked = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("same-thread reentry must panic before waiting on the mutex");
        assert!(
            panicked,
            "same-thread reentry should fail before attempting the mutex lock"
        );
        handle.join().expect("reentry proof thread should finish");
    }
}
