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
    #[serde(default)]
    pub token: Option<String>,
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
    let path = std::env::var("AGENTDESK_CONFIG").unwrap_or_else(|_| "agentdesk.yaml".into());

    let contents =
        std::fs::read_to_string(&path).with_context(|| format!("Failed to read config: {path}"))?;

    let config: Config = serde_yaml::from_str(&contents)
        .with_context(|| format!("Failed to parse config: {path}"))?;

    // Ensure data dir exists
    std::fs::create_dir_all(&config.data.dir)?;

    Ok(config)
}

fn resolve_graceful_config_path(
    explicit: Option<std::path::PathBuf>,
    runtime_root: Option<std::path::PathBuf>,
    cwd: Option<std::path::PathBuf>,
    home_dir: Option<std::path::PathBuf>,
) -> std::path::PathBuf {
    if let Some(path) = explicit {
        return path;
    }
    if let Some(root) = runtime_root {
        let path = root.join("agentdesk.yaml");
        if path.exists() {
            return path;
        }
    }
    if let Some(dir) = cwd {
        let path = dir.join("agentdesk.yaml");
        if path.exists() {
            return path;
        }
    }
    if let Some(home) = home_dir {
        let path = home.join(".adk").join("release").join("agentdesk.yaml");
        if path.exists() {
            return path;
        }
    }
    std::path::PathBuf::from("agentdesk.yaml")
}

/// Load config gracefully — returns Config::default() if the file doesn't exist
/// or fails to parse, instead of panicking.
/// Searches: $AGENTDESK_CONFIG → $AGENTDESK_ROOT_DIR/agentdesk.yaml → CWD/agentdesk.yaml → ~/.adk/release/agentdesk.yaml
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
mod tests {
    use super::{resolve_graceful_config_path, runtime_root};
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
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
        std::fs::write(root.join("agentdesk.yaml"), "server:\n  port: 9001\n").unwrap();
        std::fs::write(cwd.join("agentdesk.yaml"), "server:\n  port: 9002\n").unwrap();
        std::fs::create_dir_all(home.join(".adk").join("release")).unwrap();
        std::fs::write(
            home.join(".adk").join("release").join("agentdesk.yaml"),
            "server:\n  port: 9003\n",
        )
        .unwrap();

        let resolved = resolve_graceful_config_path(
            None,
            Some(root.clone()),
            Some(cwd.clone()),
            Some(home.clone()),
        );
        assert_eq!(resolved, root.join("agentdesk.yaml"));

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(cwd);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn resolve_graceful_config_path_prefers_cwd_before_release_home() {
        let cwd = make_temp_dir("cwd-before-release");
        let home = make_temp_dir("release-fallback");
        std::fs::write(cwd.join("agentdesk.yaml"), "server:\n  port: 9101\n").unwrap();
        std::fs::create_dir_all(home.join(".adk").join("release")).unwrap();
        std::fs::write(
            home.join(".adk").join("release").join("agentdesk.yaml"),
            "server:\n  port: 9102\n",
        )
        .unwrap();

        let resolved =
            resolve_graceful_config_path(None, None, Some(cwd.clone()), Some(home.clone()));
        assert_eq!(resolved, cwd.join("agentdesk.yaml"));

        let _ = std::fs::remove_dir_all(cwd);
        let _ = std::fs::remove_dir_all(home);
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

    pub fn config_dir() -> Option<std::path::PathBuf> {
        runtime_root()
    }
}
