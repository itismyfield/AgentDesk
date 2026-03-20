use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    #[serde(default)]
    pub discord: DiscordConfig,
    #[serde(default)]
    pub agents: Vec<AgentDef>,
    #[serde(default)]
    pub github: GitHubConfig,
    #[serde(default)]
    pub policies: PoliciesConfig,
    #[serde(default)]
    pub data: DataConfig,
    #[serde(default)]
    pub kanban: KanbanConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default)]
    pub auth_token: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DiscordConfig {
    #[serde(default)]
    pub bots: std::collections::HashMap<String, BotConfig>,
    #[serde(default)]
    pub guild_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BotConfig {
    pub token: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentDef {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub name_ko: Option<String>,
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default)]
    pub channels: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub department: Option<String>,
    #[serde(default)]
    pub avatar_emoji: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GitHubConfig {
    #[serde(default)]
    pub repos: Vec<String>,
    #[serde(default = "default_sync_interval")]
    pub sync_interval_minutes: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PoliciesConfig {
    #[serde(default = "default_policies_dir")]
    pub dir: PathBuf,
    #[serde(default = "default_true")]
    pub hot_reload: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DataConfig {
    #[serde(default = "default_data_dir")]
    pub dir: PathBuf,
    #[serde(default = "default_db_name")]
    pub db_name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct KanbanConfig {
    #[serde(default = "default_45")]
    pub timeout_requested_minutes: u64,
    #[serde(default = "default_100")]
    pub timeout_in_progress_minutes: u64,
    #[serde(default = "default_3")]
    pub max_review_rounds: u32,
    #[serde(default = "default_5")]
    pub max_chain_depth: u32,
}

fn default_port() -> u16 { 8791 }
fn default_host() -> String { "0.0.0.0".into() }
fn default_provider() -> String { "claude".into() }
fn default_sync_interval() -> u64 { 10 }
fn default_policies_dir() -> PathBuf { PathBuf::from("./policies") }
fn default_true() -> bool { true }
fn default_data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("agentdesk")
}
fn default_db_name() -> String { "agentdesk.sqlite".into() }
fn default_45() -> u64 { 45 }
fn default_100() -> u64 { 100 }
fn default_3() -> u32 { 3 }
fn default_5() -> u32 { 5 }

impl Default for ServerConfig {
    fn default() -> Self {
        Self { port: default_port(), host: default_host(), auth_token: None }
    }
}

impl Default for PoliciesConfig {
    fn default() -> Self {
        Self { dir: default_policies_dir(), hot_reload: true }
    }
}

impl Default for DataConfig {
    fn default() -> Self {
        Self { dir: default_data_dir(), db_name: default_db_name() }
    }
}

impl Default for KanbanConfig {
    fn default() -> Self {
        Self {
            timeout_requested_minutes: 45,
            timeout_in_progress_minutes: 100,
            max_review_rounds: 3,
            max_chain_depth: 5,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            discord: DiscordConfig::default(),
            agents: Vec::new(),
            github: GitHubConfig::default(),
            policies: PoliciesConfig::default(),
            data: DataConfig::default(),
            kanban: KanbanConfig::default(),
        }
    }
}

pub fn load() -> Result<Config> {
    let path = std::env::var("AGENTDESK_CONFIG")
        .unwrap_or_else(|_| "agentdesk.yaml".into());

    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read config: {path}"))?;

    let config: Config = serde_yaml::from_str(&contents)
        .with_context(|| format!("Failed to parse config: {path}"))?;

    // Ensure data dir exists
    std::fs::create_dir_all(&config.data.dir)?;

    Ok(config)
}

/// Load config gracefully — returns Config::default() if the file doesn't exist
/// or fails to parse, instead of panicking.
pub fn load_graceful() -> Config {
    let path = std::env::var("AGENTDESK_CONFIG")
        .unwrap_or_else(|_| "agentdesk.yaml".into());

    let config = match std::fs::read_to_string(&path) {
        Ok(contents) => match serde_yaml::from_str::<Config>(&contents) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("  ⚠ Failed to parse {path}: {e} — using defaults");
                Config::default()
            }
        },
        Err(_) => {
            eprintln!("  ⚠ {path} not found — using defaults");
            Config::default()
        }
    };

    // Ensure data dir exists (best effort)
    let _ = std::fs::create_dir_all(&config.data.dir);

    config
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

    pub fn config_dir() -> Option<std::path::PathBuf> {
        if let Ok(root) = std::env::var("REMOTECC_ROOT_DIR") {
            let trimmed = root.trim();
            if !trimmed.is_empty() {
                return Some(std::path::PathBuf::from(trimmed));
            }
        }
        dirs::home_dir().map(|h| h.join(".remotecc"))
    }
}
