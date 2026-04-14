use anyhow::{Context, Result};
use serde::de::Deserializer;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub server: ServerConfig,
    #[serde(default)]
    pub discord: DiscordConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_prompt: Option<String>,
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
    #[serde(default, skip_serializing_if = "KanbanConfig::is_empty")]
    pub kanban: KanbanConfig,
    #[serde(default, skip_serializing_if = "ReviewConfig::is_empty")]
    pub review: ReviewConfig,
    #[serde(default, skip_serializing_if = "RuntimeSettingsConfig::is_empty")]
    pub runtime: RuntimeSettingsConfig,
    #[serde(default, skip_serializing_if = "AutomationConfig::is_empty")]
    pub automation: AutomationConfig,
    #[serde(default, skip_serializing_if = "EscalationConfig::is_empty")]
    pub escalation: EscalationConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemoryConfig>,
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct AgentChannels {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<AgentChannel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<AgentChannel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gemini: Option<AgentChannel>,
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
            && self.qwen.as_ref().and_then(AgentChannel::target).is_none()
    }

    pub fn iter(&self) -> [(&'static str, Option<&AgentChannel>); 4] {
        [
            ("claude", self.claude.as_ref()),
            ("codex", self.codex.as_ref()),
            ("gemini", self.gemini.as_ref()),
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

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GitHubConfig {
    #[serde(default)]
    pub repos: Vec<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub repo_dirs: std::collections::BTreeMap<String, String>,
    #[serde(default = "default_sync_interval")]
    pub sync_interval_minutes: u64,
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
pub struct KanbanConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manager_channel_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadlock_manager_channel_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pm_decision_gate_enabled: Option<bool>,
}

impl KanbanConfig {
    pub fn is_empty(&self) -> bool {
        self.manager_channel_id.is_none()
            && self.deadlock_manager_channel_id.is_none()
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

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct RuntimeSettingsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_timeout_min: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_progress_stale_min: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_compact_percent: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_compact_percent_codex: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_compact_percent_claude: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub narrate_progress: Option<bool>,
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
            && self.context_compact_percent.is_none()
            && self.context_compact_percent_codex.is_none()
            && self.context_compact_percent_claude.is_none()
            && self.narrate_progress.is_none()
            && self.dispatch_poll_sec.is_none()
            && self.agent_sync_sec.is_none()
            && self.github_issue_sync_sec.is_none()
            && self.claude_rate_limit_poll_sec.is_none()
            && self.codex_rate_limit_poll_sec.is_none()
            && self.issue_triage_poll_sec.is_none()
            && self.ceo_warn_depth.is_none()
            && self.max_retries.is_none()
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

fn normalized_channel_value(value: Option<String>) -> Option<String> {
    value
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty())
}

fn default_sync_interval() -> u64 {
    10
}
fn default_policies_dir() -> PathBuf {
    PathBuf::from("./policies")
}
fn default_true() -> bool {
    true
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
    "agentdesk.sqlite".into()
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

impl Default for DataConfig {
    fn default() -> Self {
        Self {
            dir: default_data_dir(),
            db_name: default_db_name(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            discord: DiscordConfig::default(),
            shared_prompt: None,
            agents: Vec::new(),
            meeting: None,
            github: GitHubConfig::default(),
            policies: PoliciesConfig::default(),
            data: DataConfig::default(),
            kanban: KanbanConfig::default(),
            review: ReviewConfig::default(),
            runtime: RuntimeSettingsConfig::default(),
            automation: AutomationConfig::default(),
            escalation: EscalationConfig::default(),
            memory: None,
        }
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

    // Ensure data dir exists
    std::fs::create_dir_all(&config.data.dir)?;

    Ok(config)
}

pub fn load_from_path(path: &Path) -> Result<Config> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config {}", path.display()))?;
    let config = serde_yaml::from_str::<Config>(&contents)
        .with_context(|| format!("Failed to parse config {}", path.display()))?;
    Ok(config)
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
            Ok(cfg) => cfg,
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
mod tests {
    use super::{
        AgentChannel, AgentChannels, AgentDef, AutomationConfig, BotConfig, Config,
        DiscordBotAuthConfig, EscalationConfig, EscalationMode, EscalationScheduleConfig,
        FileMemoryConfig, KanbanConfig, McpMemoryConfig, MemoryConfig, ReviewConfig,
        RuntimeSettingsConfig, load_from_path, resolve_graceful_config_path, runtime_root,
        save_to_path,
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
            provider: "codex".to_string(),
            channels: AgentChannels {
                claude: Some("123456789012345678".into()),
                codex: None,
                gemini: None,
                qwen: None,
            },
            keywords: Vec::new(),
            department: Some("platform".to_string()),
            avatar_emoji: Some(":robot:".to_string()),
        });
        config.kanban = KanbanConfig {
            manager_channel_id: Some("123456789012345678".to_string()),
            deadlock_manager_channel_id: Some("223456789012345678".to_string()),
            pm_decision_gate_enabled: Some(true),
        };
        config.review = ReviewConfig {
            enabled: Some(true),
            max_rounds: Some(4),
        };
        config.runtime = RuntimeSettingsConfig {
            requested_timeout_min: Some(55),
            in_progress_stale_min: Some(180),
            context_compact_percent: Some(70),
            context_compact_percent_codex: Some(82),
            context_compact_percent_claude: Some(74),
            narrate_progress: Some(false),
            dispatch_poll_sec: Some(45),
            agent_sync_sec: Some(420),
            github_issue_sync_sec: Some(1200),
            claude_rate_limit_poll_sec: Some(90),
            codex_rate_limit_poll_sec: Some(105),
            issue_triage_poll_sec: Some(360),
            ceo_warn_depth: Some(4),
            max_retries: Some(5),
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
        config.escalation = EscalationConfig {
            mode: EscalationMode::Scheduled,
            owner_user_id: Some(343742347365974026),
            pm_channel_id: Some("323456789012345678".to_string()),
            schedule: EscalationScheduleConfig {
                pm_hours: Some("00:00-08:00".to_string()),
                timezone: Some("Asia/Seoul".to_string()),
            },
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
        assert_eq!(loaded.kanban.pm_decision_gate_enabled, Some(true));
        assert_eq!(loaded.review.enabled, Some(true));
        assert_eq!(loaded.review.max_rounds, Some(4));
        assert_eq!(loaded.runtime.requested_timeout_min, Some(55));
        assert_eq!(loaded.runtime.in_progress_stale_min, Some(180));
        assert_eq!(loaded.runtime.context_compact_percent, Some(70));
        assert_eq!(loaded.runtime.context_compact_percent_codex, Some(82));
        assert_eq!(loaded.runtime.context_compact_percent_claude, Some(74));
        assert_eq!(loaded.runtime.narrate_progress, Some(false));
        assert_eq!(loaded.runtime.dispatch_poll_sec, Some(45));
        assert_eq!(loaded.runtime.agent_sync_sec, Some(420));
        assert_eq!(loaded.runtime.github_issue_sync_sec, Some(1200));
        assert_eq!(loaded.runtime.claude_rate_limit_poll_sec, Some(90));
        assert_eq!(loaded.runtime.codex_rate_limit_poll_sec, Some(105));
        assert_eq!(loaded.runtime.issue_triage_poll_sec, Some(360));
        assert_eq!(loaded.runtime.ceo_warn_depth, Some(4));
        assert_eq!(loaded.runtime.max_retries, Some(5));
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

        let _ = std::fs::remove_dir_all(dir);
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
