use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize, Serialize)]
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
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BotConfig {
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
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
    #[serde(default)]
    pub department: Option<String>,
    #[serde(default)]
    pub avatar_emoji: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct AgentChannels {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<String>,
}

impl AgentChannels {
    pub fn is_empty(&self) -> bool {
        normalized_channel_value(self.claude.clone()).is_none()
            && normalized_channel_value(self.codex.clone()).is_none()
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GitHubConfig {
    #[serde(default)]
    pub repos: Vec<String>,
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
fn default_data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("agentdesk")
}
fn default_db_name() -> String {
    "agentdesk.sqlite".into()
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
            agents: Vec::new(),
            github: GitHubConfig::default(),
            policies: PoliciesConfig::default(),
            data: DataConfig::default(),
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
                eprintln!("  ⚠ Failed to parse {path_display}: {e} — using defaults");
                Config::default()
            }
        },
        Err(_) => {
            eprintln!("  ⚠ {path_display} not found — using defaults");
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
        AgentChannels, AgentDef, BotConfig, Config, load_from_path, resolve_graceful_config_path,
        runtime_root, save_to_path,
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
        config.discord.bots.insert(
            "announce".to_string(),
            BotConfig {
                token: Some("bot-token".to_string()),
                description: Some("announce bot".to_string()),
            },
        );
        config.agents.push(AgentDef {
            id: "agent-1".to_string(),
            name: "Agent One".to_string(),
            name_ko: Some("에이전트 원".to_string()),
            provider: "codex".to_string(),
            channels: AgentChannels {
                claude: Some("123456789012345678".to_string()),
                codex: None,
            },
            department: Some("platform".to_string()),
            avatar_emoji: Some(":robot:".to_string()),
        });

        save_to_path(&path, &config).unwrap();
        assert!(path.exists());
        let loaded = load_from_path(&path).unwrap();

        assert_eq!(loaded.server.port, 4317);
        assert_eq!(loaded.server.host, "127.0.0.42");
        assert_eq!(loaded.server.auth_token.as_deref(), Some("secret-token"));
        assert_eq!(loaded.discord.guild_id.as_deref(), Some("guild-123"));
        assert_eq!(loaded.discord.bots.len(), 1);
        assert_eq!(
            loaded.discord.bots["announce"].description.as_deref(),
            Some("announce bot")
        );
        assert_eq!(loaded.agents.len(), 1);
        assert_eq!(loaded.agents[0].id, "agent-1");
        assert_eq!(loaded.agents[0].name, "Agent One");
        assert_eq!(loaded.agents[0].name_ko.as_deref(), Some("에이전트 원"));
        assert_eq!(loaded.agents[0].provider, "codex");
        assert_eq!(loaded.agents[0].department.as_deref(), Some("platform"));
        assert_eq!(loaded.agents[0].avatar_emoji.as_deref(), Some(":robot:"));
        assert_eq!(
            loaded.agents[0].channels.claude.as_deref(),
            Some("123456789012345678")
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
