use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{Config, McpServerAuthType, McpServerConfig};
use crate::services::discord::runtime_store::atomic_write;
use crate::services::provider::ProviderKind;

const CODEX_SYNC_STATE_FILE: &str = "codex-mcp-sync-state.json";
const OPENCODE_SYNC_STATE_FILE: &str = "opencode-mcp-sync-state.json";
const QWEN_SYNC_STATE_FILE: &str = "qwen-mcp-sync-state.json";
const GEMINI_SYNC_STATE_FILE: &str = "gemini-mcp-sync-state.json";
const MEMENTO_SERVER_NAME: &str = "memento";
const REVIEW_MCP_ALLOWLIST_ENV: &str = "AGENTDESK_REVIEW_MCP_ALLOWLIST";
const DEFAULT_REVIEW_MCP_ALLOWLIST: &[&str] = &[
    "memento",
    "github",
    "github-mcp",
    "github_mcp",
    "gh",
    "git",
    "filesystem",
    "file",
    "files",
    "fs",
    "grep",
    "ripgrep",
    "rg",
    "editor",
    "edit",
    "apply_patch",
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedMcpServer {
    name: String,
    url: String,
    bearer_token_env_var: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CodexMcpSyncState {
    #[serde(default)]
    managed_servers: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct OpenCodeMcpSyncState {
    #[serde(default)]
    managed_servers: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct QwenMcpSyncState {
    #[serde(default)]
    managed_servers: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct GeminiMcpSyncState {
    #[serde(default)]
    managed_servers: Vec<String>,
}

pub(crate) fn provider_has_memento_mcp(provider: &ProviderKind) -> bool {
    provider_has_mcp_server(provider, MEMENTO_SERVER_NAME)
}

pub(crate) fn provider_has_mcp_server(provider: &ProviderKind, server_name: &str) -> bool {
    let normalized = server_name.trim();
    if normalized.is_empty() {
        return false;
    }

    match provider {
        ProviderKind::Claude => {
            runtime_config_contains_server(normalized)
                || claude_global_mcp_config_contains_server(normalized)
        }
        ProviderKind::Codex => {
            runtime_config_contains_server(normalized) || codex_config_contains_server(normalized)
        }
        ProviderKind::OpenCode => {
            runtime_config_contains_server(normalized)
                || opencode_config_contains_server(normalized)
        }
        ProviderKind::Qwen => {
            runtime_config_contains_server(normalized) || qwen_config_contains_server(normalized)
        }
        ProviderKind::Gemini => {
            runtime_config_contains_server(normalized) || gemini_config_contains_server(normalized)
        }
        _ => false,
    }
}

pub(crate) fn claude_mcp_config_arg(dispatch_type: Option<&str>) -> Option<String> {
    let servers = load_runtime_mcp_servers(dispatch_type);
    claude_mcp_config_arg_from_servers(&servers)
}

#[cfg(test)]
pub(crate) fn claude_mcp_config_arg_from_config(
    config: &Config,
    dispatch_type: Option<&str>,
) -> Option<String> {
    let servers = resolved_mcp_servers(config, dispatch_type);
    claude_mcp_config_arg_from_servers(&servers)
}

pub(crate) fn sync_codex_mcp_servers(config: &Config) -> Result<(), String> {
    let runtime_root = crate::config::runtime_root()
        .ok_or_else(|| "AGENTDESK_ROOT_DIR is unavailable".to_string())?;
    let sync_state_path =
        crate::runtime_layout::config_dir(&runtime_root).join(CODEX_SYNC_STATE_FILE);
    let desired = resolved_mcp_servers(config, None);
    let previous = load_codex_sync_state_from_path(&sync_state_path);
    let desired_names = desired.keys().cloned().collect::<BTreeSet<_>>();
    let previous_names = previous
        .managed_servers
        .into_iter()
        .collect::<BTreeSet<_>>();

    let resolution = crate::services::platform::resolve_provider_binary("codex");
    let codex_bin = match resolution.resolved_path {
        Some(path) => path,
        None => {
            if desired_names.is_empty() && previous_names.is_empty() {
                return Ok(());
            }
            return Err("Codex CLI not found".to_string());
        }
    };

    for removed in previous_names.difference(&desired_names) {
        run_codex_command(&codex_bin, ["mcp", "remove", removed.as_str()])?;
    }

    for server in desired.values() {
        let mut args = vec![
            "mcp".to_string(),
            "add".to_string(),
            server.name.clone(),
            "--url".to_string(),
            server.url.clone(),
        ];
        if let Some(token_env_var) = server
            .bearer_token_env_var
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            args.push("--bearer-token-env-var".to_string());
            args.push(token_env_var.to_string());
        }
        run_codex_command_vec(&codex_bin, &args)?;
    }

    let next_state = CodexMcpSyncState {
        managed_servers: desired_names.into_iter().collect(),
    };
    let serialized = serde_json::to_string_pretty(&next_state)
        .map_err(|error| format!("Failed to serialize Codex MCP sync state: {error}"))?;
    atomic_write(&sync_state_path, &serialized)?;
    Ok(())
}

pub(crate) fn sync_opencode_mcp_servers(config: &Config) -> Result<(), String> {
    let runtime_root = crate::config::runtime_root()
        .ok_or_else(|| "AGENTDESK_ROOT_DIR is unavailable".to_string())?;
    let sync_state_path =
        crate::runtime_layout::config_dir(&runtime_root).join(OPENCODE_SYNC_STATE_FILE);
    let desired = resolved_mcp_servers(config, None);
    let previous = load_opencode_sync_state_from_path(&sync_state_path);
    let desired_names = desired.keys().cloned().collect::<BTreeSet<_>>();
    let previous_names = previous
        .managed_servers
        .into_iter()
        .collect::<BTreeSet<_>>();

    if desired_names.is_empty() && previous_names.is_empty() {
        return Ok(());
    }

    let Some(config_path) = opencode_config_path() else {
        return Err("OpenCode config path unavailable".to_string());
    };
    let mut root = load_opencode_config_value(&config_path)?;
    let root_object = root.as_object_mut().ok_or_else(|| {
        format!(
            "OpenCode config must be a JSON object: {}",
            config_path.display()
        )
    })?;

    let mcp_value = root_object
        .entry("mcp".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let mcp_servers = mcp_value
        .as_object_mut()
        .ok_or_else(|| "OpenCode config field `mcp` must be a JSON object".to_string())?;

    for removed in previous_names.difference(&desired_names) {
        mcp_servers.remove(removed);
    }

    for server in desired.values() {
        mcp_servers.insert(server.name.clone(), opencode_mcp_entry(server));
    }

    let serialized = serde_json::to_string_pretty(&root)
        .map_err(|error| format!("Failed to serialize OpenCode config: {error}"))?;
    atomic_write(&config_path, &serialized)?;

    let next_state = OpenCodeMcpSyncState {
        managed_servers: desired_names.into_iter().collect(),
    };
    let serialized = serde_json::to_string_pretty(&next_state)
        .map_err(|error| format!("Failed to serialize OpenCode MCP sync state: {error}"))?;
    atomic_write(&sync_state_path, &serialized)?;
    Ok(())
}

pub(crate) fn sync_qwen_mcp_servers(config: &Config) -> Result<(), String> {
    let runtime_root = crate::config::runtime_root()
        .ok_or_else(|| "AGENTDESK_ROOT_DIR is unavailable".to_string())?;
    let sync_state_path =
        crate::runtime_layout::config_dir(&runtime_root).join(QWEN_SYNC_STATE_FILE);
    let desired = resolved_mcp_servers(config, None);
    let previous = load_qwen_sync_state_from_path(&sync_state_path);
    let desired_names = desired.keys().cloned().collect::<BTreeSet<_>>();
    let previous_names = previous
        .managed_servers
        .into_iter()
        .collect::<BTreeSet<_>>();

    if desired_names.is_empty() && previous_names.is_empty() {
        return Ok(());
    }

    let Some(config_path) = qwen_config_path() else {
        return Err("Qwen config path unavailable".to_string());
    };
    sync_json_mcp_servers(
        &config_path,
        &desired,
        &desired_names,
        &previous_names,
        qwen_mcp_entry,
        "Qwen",
    )?;

    let next_state = QwenMcpSyncState {
        managed_servers: desired_names.into_iter().collect(),
    };
    let serialized = serde_json::to_string_pretty(&next_state)
        .map_err(|error| format!("Failed to serialize Qwen MCP sync state: {error}"))?;
    atomic_write(&sync_state_path, &serialized)?;
    Ok(())
}

pub(crate) fn sync_gemini_mcp_servers(config: &Config) -> Result<(), String> {
    let runtime_root = crate::config::runtime_root()
        .ok_or_else(|| "AGENTDESK_ROOT_DIR is unavailable".to_string())?;
    let sync_state_path =
        crate::runtime_layout::config_dir(&runtime_root).join(GEMINI_SYNC_STATE_FILE);
    let desired = resolved_mcp_servers(config, None);
    let previous = load_gemini_sync_state_from_path(&sync_state_path);
    let desired_names = desired.keys().cloned().collect::<BTreeSet<_>>();
    let previous_names = previous
        .managed_servers
        .into_iter()
        .collect::<BTreeSet<_>>();

    if desired_names.is_empty() && previous_names.is_empty() {
        return Ok(());
    }

    let Some(config_path) = gemini_config_path() else {
        return Err("Gemini config path unavailable".to_string());
    };
    sync_json_mcp_servers(
        &config_path,
        &desired,
        &desired_names,
        &previous_names,
        gemini_mcp_entry,
        "Gemini",
    )?;

    let next_state = GeminiMcpSyncState {
        managed_servers: desired_names.into_iter().collect(),
    };
    let serialized = serde_json::to_string_pretty(&next_state)
        .map_err(|error| format!("Failed to serialize Gemini MCP sync state: {error}"))?;
    atomic_write(&sync_state_path, &serialized)?;
    Ok(())
}

fn claude_mcp_config_arg_from_servers(
    servers: &BTreeMap<String, ResolvedMcpServer>,
) -> Option<String> {
    if servers.is_empty() {
        return None;
    }

    let mut mcp_servers = Map::new();
    for server in servers.values() {
        let mut entry = Map::new();
        entry.insert("type".to_string(), Value::String("http".to_string()));
        entry.insert("url".to_string(), Value::String(server.url.clone()));
        if let Some(token_env_var) = server
            .bearer_token_env_var
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            // Claude Code SDK does not expand ${VAR} in --mcp-config HTTP
            // headers, so resolve the env var here and emit the literal token.
            // If the env var is unset, omit the header so the SDK can still
            // attempt connection (useful for OAuth-protected servers).
            if let Some(token_value) = std::env::var(token_env_var)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
            {
                let mut headers = Map::new();
                headers.insert(
                    "Authorization".to_string(),
                    Value::String(format!("Bearer {token_value}")),
                );
                entry.insert("headers".to_string(), Value::Object(headers));
            }
        }
        mcp_servers.insert(server.name.clone(), Value::Object(entry));
    }

    serde_json::to_string(&Value::Object(Map::from_iter([(
        "mcpServers".to_string(),
        Value::Object(mcp_servers),
    )])))
    .ok()
}

fn resolved_mcp_servers(
    config: &Config,
    dispatch_type: Option<&str>,
) -> BTreeMap<String, ResolvedMcpServer> {
    let servers = config
        .mcp_servers
        .iter()
        .filter_map(|(name, server)| resolve_mcp_server(name, server))
        .collect::<Vec<_>>();

    if !review_mcp_slim_mode_enabled(dispatch_type) {
        return servers
            .into_iter()
            .map(|server| (server.name.clone(), server))
            .collect();
    }

    let allowlist = review_mcp_allowlist(config);
    let (allowed, filtered): (Vec<_>, Vec<_>) = servers
        .into_iter()
        .partition(|server| allowlist.contains(&normalize_mcp_server_name(&server.name)));
    if !filtered.is_empty() {
        let filtered_names = filtered
            .iter()
            .map(|server| server.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        tracing::info!(
            "[mcp] review slim mode filtered {} MCP server(s): {}",
            filtered.len(),
            filtered_names
        );
    }
    if allowed.is_empty() && !config.mcp_servers.is_empty() {
        tracing::warn!(
            "[mcp] review slim mode produced an empty MCP catalog; set {} or review_mcp_allowlist in agentdesk.yaml if the reviewer needs MCP tools",
            REVIEW_MCP_ALLOWLIST_ENV
        );
    }

    allowed
        .into_iter()
        .map(|server| (server.name.clone(), server))
        .collect()
}

fn resolve_mcp_server(name: &str, server: &McpServerConfig) -> Option<ResolvedMcpServer> {
    let normalized_name = name.trim();
    let normalized_url = server.url.trim();
    if normalized_name.is_empty() || normalized_url.is_empty() {
        return None;
    }

    let bearer_token_env_var = server.auth.as_ref().and_then(|auth| match auth.auth_type {
        McpServerAuthType::Bearer => auth
            .token_env_var
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string),
    });

    Some(ResolvedMcpServer {
        name: normalized_name.to_string(),
        url: normalized_url.to_string(),
        bearer_token_env_var,
    })
}

fn load_runtime_mcp_servers(dispatch_type: Option<&str>) -> BTreeMap<String, ResolvedMcpServer> {
    load_runtime_config()
        .map(|config| resolved_mcp_servers(&config, dispatch_type))
        .unwrap_or_default()
}

fn runtime_config_contains_server(server_name: &str) -> bool {
    load_runtime_mcp_servers(None).contains_key(server_name)
}

fn review_mcp_slim_mode_enabled(dispatch_type: Option<&str>) -> bool {
    matches!(dispatch_type, Some("review") | Some("review-decision"))
}

fn review_mcp_allowlist(config: &Config) -> BTreeSet<String> {
    let mut allowlist = DEFAULT_REVIEW_MCP_ALLOWLIST
        .iter()
        .map(|name| normalize_mcp_server_name(name))
        .collect::<BTreeSet<_>>();

    allowlist.extend(
        config
            .review_mcp_allowlist
            .iter()
            .map(|name| normalize_mcp_server_name(name))
            .filter(|name| !name.is_empty()),
    );

    if let Some(raw) = std::env::var_os(REVIEW_MCP_ALLOWLIST_ENV) {
        allowlist.extend(parse_review_mcp_allowlist(raw.to_string_lossy().as_ref()));
    }

    allowlist
}

fn parse_review_mcp_allowlist(raw: &str) -> impl Iterator<Item = String> + '_ {
    raw.split(|ch: char| ch == ',' || ch == ';' || ch.is_whitespace())
        .map(normalize_mcp_server_name)
        .filter(|name| !name.is_empty())
}

fn normalize_mcp_server_name(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod review_slim_tests {
    use super::*;
    use serde_json::Value;

    static REVIEW_ALLOWLIST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_clean_review_allowlist_env<F>(f: F)
    where
        F: FnOnce(),
    {
        let _guard = REVIEW_ALLOWLIST_ENV_LOCK.lock().unwrap();
        let previous_review_mcp_allowlist = std::env::var_os(REVIEW_MCP_ALLOWLIST_ENV);
        unsafe { std::env::remove_var(REVIEW_MCP_ALLOWLIST_ENV) };
        f();
        match previous_review_mcp_allowlist {
            Some(value) => unsafe { std::env::set_var(REVIEW_MCP_ALLOWLIST_ENV, value) },
            None => unsafe { std::env::remove_var(REVIEW_MCP_ALLOWLIST_ENV) },
        }
    }

    fn config_with_servers(names: &[&str]) -> Config {
        let mut config = Config::default();
        for name in names {
            config.mcp_servers.insert(
                (*name).to_string(),
                McpServerConfig {
                    url: format!("http://localhost/{name}"),
                    auth: None,
                },
            );
        }
        config
    }

    fn rendered_mcp_servers(config: &Config, dispatch_type: Option<&str>) -> Value {
        let rendered = claude_mcp_config_arg_from_config(config, dispatch_type).expect("config");
        serde_json::from_str(&rendered).expect("mcp config json")
    }

    #[test]
    fn claude_mcp_config_arg_keeps_full_catalog_for_non_review_dispatches() {
        with_clean_review_allowlist_env(|| {
            let config = config_with_servers(&["memento", "github", "family-morning-briefing"]);

            let value = rendered_mcp_servers(&config, Some("implementation"));

            assert!(value["mcpServers"]["memento"].is_object());
            assert!(value["mcpServers"]["github"].is_object());
            assert!(value["mcpServers"]["family-morning-briefing"].is_object());
        });
    }

    #[test]
    fn claude_mcp_config_arg_filters_review_catalog_to_allowlist() {
        with_clean_review_allowlist_env(|| {
            let config = config_with_servers(&[
                "memento",
                "github",
                "family-morning-briefing",
                "architecture",
            ]);

            let value = rendered_mcp_servers(&config, Some("review"));

            assert!(value["mcpServers"]["memento"].is_object());
            assert!(value["mcpServers"]["github"].is_object());
            assert!(value["mcpServers"]["family-morning-briefing"].is_null());
            assert!(value["mcpServers"]["architecture"].is_null());
        });
    }

    #[test]
    fn claude_mcp_config_arg_extends_review_allowlist_from_config_and_env() {
        with_clean_review_allowlist_env(|| {
            unsafe {
                std::env::set_var(
                    REVIEW_MCP_ALLOWLIST_ENV,
                    "family-morning-briefing, screenshot",
                )
            };
            let mut config = config_with_servers(&[
                "memento",
                "family-morning-briefing",
                "architecture",
                "screenshot",
                "speech",
            ]);
            config.review_mcp_allowlist.push("architecture".to_string());

            let value = rendered_mcp_servers(&config, Some("review-decision"));

            assert!(value["mcpServers"]["memento"].is_object());
            assert!(value["mcpServers"]["family-morning-briefing"].is_object());
            assert!(value["mcpServers"]["architecture"].is_object());
            assert!(value["mcpServers"]["screenshot"].is_object());
            assert!(value["mcpServers"]["speech"].is_null());
        });
    }
}

fn current_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .or_else(dirs::home_dir)
}

fn load_runtime_config() -> Option<Config> {
    let explicit = std::env::var_os("AGENTDESK_CONFIG")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);

    let mut candidates = Vec::new();
    if let Some(path) = explicit {
        candidates.push(path);
    }
    if let Some(root) = crate::config::runtime_root() {
        candidates.push(crate::runtime_layout::config_file_path(&root));
        candidates.push(crate::runtime_layout::legacy_config_file_path(&root));
    }

    candidates
        .into_iter()
        .find(|path| path.is_file())
        .and_then(|path| crate::config::load_from_path(&path).ok())
}

fn load_codex_sync_state_from_path(path: &Path) -> CodexMcpSyncState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<CodexMcpSyncState>(&raw).ok())
        .unwrap_or_default()
}

fn load_opencode_sync_state_from_path(path: &Path) -> OpenCodeMcpSyncState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<OpenCodeMcpSyncState>(&raw).ok())
        .unwrap_or_default()
}

fn load_qwen_sync_state_from_path(path: &Path) -> QwenMcpSyncState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<QwenMcpSyncState>(&raw).ok())
        .unwrap_or_default()
}

fn load_gemini_sync_state_from_path(path: &Path) -> GeminiMcpSyncState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<GeminiMcpSyncState>(&raw).ok())
        .unwrap_or_default()
}

fn claude_global_mcp_config_contains_server(server_name: &str) -> bool {
    let Some(path) = current_home_dir().map(|home| home.join(".claude").join(".mcp.json")) else {
        return false;
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    value
        .get("mcpServers")
        .and_then(Value::as_object)
        .is_some_and(|servers| servers.contains_key(server_name))
}

fn codex_config_contains_server(server_name: &str) -> bool {
    let Some(path) = current_home_dir().map(|home| home.join(".codex").join("config.toml")) else {
        return false;
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return false;
    };
    raw.lines()
        .filter_map(parse_codex_mcp_section_name)
        .any(|candidate| candidate == server_name)
}

fn opencode_config_path() -> Option<PathBuf> {
    current_home_dir().map(|home| home.join(".config").join("opencode").join("opencode.json"))
}

fn qwen_config_path() -> Option<PathBuf> {
    std::env::var_os("QWEN_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(current_home_dir)
        .map(|home| home.join(".qwen").join("settings.json"))
}

fn gemini_config_path() -> Option<PathBuf> {
    std::env::var_os("GEMINI_CLI_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(current_home_dir)
        .map(|home| home.join(".gemini").join("settings.json"))
}

fn load_opencode_config_value(path: &Path) -> Result<Value, String> {
    load_json_object_config_value(path, "OpenCode")
}

fn load_json_object_config_value(path: &Path, label: &str) -> Result<Value, String> {
    match std::fs::read_to_string(path) {
        Ok(raw) if raw.trim().is_empty() => Ok(Value::Object(Map::new())),
        Ok(raw) => serde_json::from_str::<Value>(&raw)
            .map_err(|error| format!("Failed to parse {label} config {}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Value::Object(Map::new())),
        Err(error) => Err(format!(
            "Failed to read {label} config {}: {error}",
            path.display()
        )),
    }
}

fn sync_json_mcp_servers(
    config_path: &Path,
    desired: &BTreeMap<String, ResolvedMcpServer>,
    desired_names: &BTreeSet<String>,
    previous_names: &BTreeSet<String>,
    entry_fn: fn(&ResolvedMcpServer) -> Value,
    label: &str,
) -> Result<(), String> {
    let mut root = load_json_object_config_value(config_path, label)?;
    let root_object = root.as_object_mut().ok_or_else(|| {
        format!(
            "{label} config must be a JSON object: {}",
            config_path.display()
        )
    })?;

    let mcp_value = root_object
        .entry("mcpServers".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let mcp_servers = mcp_value
        .as_object_mut()
        .ok_or_else(|| format!("{label} config field `mcpServers` must be a JSON object"))?;

    for removed in previous_names.difference(desired_names) {
        mcp_servers.remove(removed);
    }

    for server in desired.values() {
        mcp_servers.insert(server.name.clone(), entry_fn(server));
    }

    let serialized = serde_json::to_string_pretty(&root)
        .map_err(|error| format!("Failed to serialize {label} config: {error}"))?;
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            format!(
                "Failed to create {label} config directory {}: {error}",
                parent.display()
            )
        })?;
    }
    atomic_write(config_path, &serialized)?;
    Ok(())
}

fn opencode_mcp_entry(server: &ResolvedMcpServer) -> Value {
    let mut entry = Map::new();
    entry.insert("type".to_string(), Value::String("remote".to_string()));
    entry.insert("url".to_string(), Value::String(server.url.clone()));
    entry.insert("enabled".to_string(), Value::Bool(true));
    if let Some(token_env_var) = server
        .bearer_token_env_var
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let mut headers = Map::new();
        headers.insert(
            "Authorization".to_string(),
            Value::String(format!("Bearer {{env:{token_env_var}}}")),
        );
        entry.insert("headers".to_string(), Value::Object(headers));
    }
    Value::Object(entry)
}

fn qwen_mcp_entry(server: &ResolvedMcpServer) -> Value {
    let mut entry = Map::new();
    entry.insert("httpUrl".to_string(), Value::String(server.url.clone()));
    entry.insert("trust".to_string(), Value::Bool(true));
    if let Some(headers) = bearer_headers(server, "${", "}") {
        entry.insert("headers".to_string(), Value::Object(headers));
    }
    Value::Object(entry)
}

fn gemini_mcp_entry(server: &ResolvedMcpServer) -> Value {
    let mut entry = Map::new();
    entry.insert("httpUrl".to_string(), Value::String(server.url.clone()));
    entry.insert("trust".to_string(), Value::Bool(true));
    if let Some(headers) = bearer_headers(server, "${", "}") {
        entry.insert("headers".to_string(), Value::Object(headers));
    }
    Value::Object(entry)
}

fn bearer_headers(
    server: &ResolvedMcpServer,
    env_var_prefix: &str,
    env_var_suffix: &str,
) -> Option<Map<String, Value>> {
    let token_env_var = server
        .bearer_token_env_var
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let mut headers = Map::new();
    headers.insert(
        "Authorization".to_string(),
        Value::String(format!(
            "Bearer {env_var_prefix}{token_env_var}{env_var_suffix}"
        )),
    );
    Some(headers)
}

fn opencode_config_contains_server(server_name: &str) -> bool {
    let Some(path) = opencode_config_path() else {
        return false;
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    ["mcp", "mcpServers"].iter().any(|field| {
        value
            .get(field)
            .and_then(Value::as_object)
            .and_then(|servers| servers.get(server_name))
            .is_some_and(opencode_mcp_entry_enabled)
    })
}

fn qwen_config_contains_server(server_name: &str) -> bool {
    let Some(path) = qwen_config_path() else {
        return false;
    };
    json_mcp_config_contains_server(&path, server_name)
}

fn gemini_config_contains_server(server_name: &str) -> bool {
    let Some(path) = gemini_config_path() else {
        return false;
    };
    json_mcp_config_contains_server(&path, server_name)
}

fn json_mcp_config_contains_server(path: &Path, server_name: &str) -> bool {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    value
        .get("mcpServers")
        .and_then(Value::as_object)
        .is_some_and(|servers| servers.contains_key(server_name))
}

fn opencode_mcp_entry_enabled(value: &Value) -> bool {
    value
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

fn parse_codex_mcp_section_name(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let prefix = "[mcp_servers.";
    if !trimmed.starts_with(prefix) || !trimmed.ends_with(']') {
        return None;
    }

    let mut section = &trimmed[prefix.len()..trimmed.len() - 1];
    if section.starts_with('"') && section.ends_with('"') && section.len() >= 2 {
        section = &section[1..section.len() - 1];
    }

    Some(section.to_string())
}

fn run_codex_command(codex_bin: &str, args: [&str; 3]) -> Result<(), String> {
    run_codex_command_vec(
        codex_bin,
        &args
            .iter()
            .map(|value| (*value).to_string())
            .collect::<Vec<_>>(),
    )
}

fn run_codex_command_vec(codex_bin: &str, args: &[String]) -> Result<(), String> {
    let output = Command::new(codex_bin)
        .args(args)
        .output()
        .map_err(|error| format!("Failed to run `{codex_bin} {}`: {error}", args.join(" ")))?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Err(if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        format!(
            "`{codex_bin} {}` exited with code {:?}",
            args.join(" "),
            output.status.code()
        )
    })
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::path::Path;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn with_test_env<F>(f: F)
    where
        F: FnOnce(&Path),
    {
        let _guard = crate::services::discord::runtime_store::lock_test_env();
        let temp = tempfile::tempdir().unwrap();
        let runtime_root = temp.path().join(".adk").join("release");
        fs::create_dir_all(crate::runtime_layout::config_dir(&runtime_root)).unwrap();
        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
        let previous_home = std::env::var_os("HOME");
        let previous_userprofile = std::env::var_os("USERPROFILE");
        let previous_path = std::env::var_os("PATH");
        let previous_review_mcp_allowlist = std::env::var_os(REVIEW_MCP_ALLOWLIST_ENV);
        let previous_qwen_home = std::env::var_os("QWEN_HOME");
        let previous_gemini_cli_home = std::env::var_os("GEMINI_CLI_HOME");
        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", &runtime_root);
            std::env::set_var("HOME", temp.path());
            std::env::set_var("USERPROFILE", temp.path());
            std::env::remove_var(REVIEW_MCP_ALLOWLIST_ENV);
            std::env::set_var("QWEN_HOME", temp.path());
            std::env::set_var("GEMINI_CLI_HOME", temp.path());
        }
        f(temp.path());
        match previous_root {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
        match previous_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match previous_userprofile {
            Some(value) => unsafe { std::env::set_var("USERPROFILE", value) },
            None => unsafe { std::env::remove_var("USERPROFILE") },
        }
        match previous_path {
            Some(value) => unsafe { std::env::set_var("PATH", value) },
            None => unsafe { std::env::remove_var("PATH") },
        }
        match previous_review_mcp_allowlist {
            Some(value) => unsafe { std::env::set_var(REVIEW_MCP_ALLOWLIST_ENV, value) },
            None => unsafe { std::env::remove_var(REVIEW_MCP_ALLOWLIST_ENV) },
        }
        match previous_qwen_home {
            Some(value) => unsafe { std::env::set_var("QWEN_HOME", value) },
            None => unsafe { std::env::remove_var("QWEN_HOME") },
        }
        match previous_gemini_cli_home {
            Some(value) => unsafe { std::env::set_var("GEMINI_CLI_HOME", value) },
            None => unsafe { std::env::remove_var("GEMINI_CLI_HOME") },
        }
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, contents: &str) {
        fs::write(path, contents).unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    #[test]
    fn claude_mcp_config_arg_from_config_renders_http_servers() {
        let env_var = "MEMENTO_ACCESS_KEY_CLAUDE_MCP_TEST";
        let previous = std::env::var_os(env_var);
        unsafe { std::env::set_var(env_var, "literal-token-value") };

        let mut config = Config::default();
        config.mcp_servers.insert(
            "memento".to_string(),
            McpServerConfig {
                url: "http://localhost:57332/mcp".to_string(),
                auth: Some(crate::config::McpServerAuthConfig {
                    auth_type: McpServerAuthType::Bearer,
                    token_env_var: Some(env_var.to_string()),
                }),
            },
        );

        let rendered = claude_mcp_config_arg_from_config(&config, None).expect("config");
        let value: Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(
            value["mcpServers"]["memento"]["type"],
            Value::String("http".to_string())
        );
        assert_eq!(
            value["mcpServers"]["memento"]["url"],
            Value::String("http://localhost:57332/mcp".to_string())
        );
        assert_eq!(
            value["mcpServers"]["memento"]["headers"]["Authorization"],
            Value::String("Bearer literal-token-value".to_string())
        );

        match previous {
            Some(value) => unsafe { std::env::set_var(env_var, value) },
            None => unsafe { std::env::remove_var(env_var) },
        }
    }

    #[test]
    fn claude_mcp_config_arg_omits_auth_header_when_env_var_unset() {
        let env_var = "MEMENTO_ACCESS_KEY_CLAUDE_MCP_TEST_UNSET";
        let previous = std::env::var_os(env_var);
        unsafe { std::env::remove_var(env_var) };

        let mut config = Config::default();
        config.mcp_servers.insert(
            "memento".to_string(),
            McpServerConfig {
                url: "http://localhost:57332/mcp".to_string(),
                auth: Some(crate::config::McpServerAuthConfig {
                    auth_type: McpServerAuthType::Bearer,
                    token_env_var: Some(env_var.to_string()),
                }),
            },
        );

        let rendered = claude_mcp_config_arg_from_config(&config, None).expect("config");
        let value: Value = serde_json::from_str(&rendered).unwrap();

        assert!(value["mcpServers"]["memento"].get("headers").is_none());

        if let Some(value) = previous {
            unsafe { std::env::set_var(env_var, value) };
        }
    }

    #[test]
    fn provider_has_memento_mcp_reflects_runtime_config_for_mcp_aware_providers() {
        with_test_env(|temp_root| {
            let runtime_root = temp_root.join(".adk").join("release");
            let config_path = crate::runtime_layout::config_file_path(&runtime_root);
            let mut config = Config::default();
            config.mcp_servers.insert(
                "memento".to_string(),
                McpServerConfig {
                    url: "http://localhost:57332/mcp".to_string(),
                    auth: None,
                },
            );
            crate::config::save_to_path(&config_path, &config).unwrap();

            assert!(provider_has_memento_mcp(&ProviderKind::Claude));
            assert!(provider_has_memento_mcp(&ProviderKind::Codex));
            assert!(provider_has_memento_mcp(&ProviderKind::OpenCode));
            assert!(provider_has_memento_mcp(&ProviderKind::Qwen));
            assert!(provider_has_memento_mcp(&ProviderKind::Gemini));
        });
    }

    #[test]
    fn provider_has_memento_mcp_detects_manual_claude_config() {
        with_test_env(|temp_root| {
            let claude_dir = temp_root.join(".claude");
            fs::create_dir_all(&claude_dir).unwrap();
            fs::write(
                claude_dir.join(".mcp.json"),
                r#"{"mcpServers":{"memento":{"type":"http","url":"http://localhost:57332/mcp"}}}"#,
            )
            .unwrap();

            assert!(provider_has_memento_mcp(&ProviderKind::Claude));
        });
    }

    #[test]
    fn provider_has_mcp_server_falls_back_to_runtime_config_for_codex() {
        with_test_env(|temp_root| {
            let runtime_root = temp_root.join(".adk").join("release");
            let config_path = crate::runtime_layout::config_file_path(&runtime_root);
            let mut config = Config::default();
            config.mcp_servers.insert(
                "manual".to_string(),
                McpServerConfig {
                    url: "http://manual.local/mcp".to_string(),
                    auth: None,
                },
            );
            crate::config::save_to_path(&config_path, &config).unwrap();

            assert!(provider_has_mcp_server(&ProviderKind::Codex, "manual"));
        });
    }

    #[test]
    fn provider_has_mcp_server_detects_manual_opencode_config() {
        with_test_env(|temp_root| {
            let opencode_dir = temp_root.join(".config").join("opencode");
            fs::create_dir_all(&opencode_dir).unwrap();
            fs::write(
                opencode_dir.join("opencode.json"),
                r#"{"mcp":{"memento":{"type":"remote","url":"http://localhost:57332/mcp","enabled":true},"disabled":{"enabled":false}}}"#,
            )
            .unwrap();

            assert!(provider_has_memento_mcp(&ProviderKind::OpenCode));
            assert!(!provider_has_mcp_server(
                &ProviderKind::OpenCode,
                "disabled"
            ));
        });
    }

    #[test]
    fn provider_has_mcp_server_detects_manual_qwen_and_gemini_config() {
        with_test_env(|temp_root| {
            let qwen_dir = temp_root.join(".qwen");
            let gemini_dir = temp_root.join(".gemini");
            fs::create_dir_all(&qwen_dir).unwrap();
            fs::create_dir_all(&gemini_dir).unwrap();
            fs::write(
                qwen_dir.join("settings.json"),
                r#"{"mcpServers":{"manual":{"httpUrl":"http://manual.local/mcp"}}}"#,
            )
            .unwrap();
            fs::write(
                gemini_dir.join("settings.json"),
                r#"{"mcpServers":{"manual":{"httpUrl":"http://manual.local/mcp"}}}"#,
            )
            .unwrap();

            assert!(provider_has_mcp_server(&ProviderKind::Qwen, "manual"));
            assert!(provider_has_mcp_server(&ProviderKind::Gemini, "manual"));
        });
    }

    #[test]
    fn provider_has_memento_mcp_stays_false_for_codex_without_matching_config() {
        with_test_env(|temp_root| {
            let previous_memento_access_key = std::env::var_os("MEMENTO_ACCESS_KEY");
            unsafe { std::env::remove_var("MEMENTO_ACCESS_KEY") };
            let runtime_root = temp_root.join(".adk").join("release");
            let config_path = crate::runtime_layout::config_file_path(&runtime_root);
            let mut config = Config::default();
            config.mcp_servers.insert(
                "manual".to_string(),
                McpServerConfig {
                    url: "http://manual.local/mcp".to_string(),
                    auth: None,
                },
            );
            crate::config::save_to_path(&config_path, &config).unwrap();

            let codex_dir = temp_root.join(".codex");
            fs::create_dir_all(&codex_dir).unwrap();
            fs::write(
                codex_dir.join("config.toml"),
                "[mcp_servers.other]\nurl = \"http://other.local/mcp\"\n",
            )
            .unwrap();

            assert!(!provider_has_memento_mcp(&ProviderKind::Codex));

            match previous_memento_access_key {
                Some(value) => unsafe { std::env::set_var("MEMENTO_ACCESS_KEY", value) },
                None => unsafe { std::env::remove_var("MEMENTO_ACCESS_KEY") },
            }
        });
    }

    #[cfg(unix)]
    #[test]
    fn sync_codex_mcp_servers_updates_managed_servers_without_touching_others() {
        with_test_env(|temp_root| {
            let runtime_root = temp_root.join(".adk").join("release");
            let bin_dir = temp_root.join("bin");
            fs::create_dir_all(&bin_dir).unwrap();
            let codex_path = bin_dir.join("codex");
            let script = r#"#!/bin/sh
set -eu
CONFIG_DIR="${HOME}/.codex"
CONFIG_FILE="${CONFIG_DIR}/config.toml"
mkdir -p "${CONFIG_DIR}"
touch "${CONFIG_FILE}"
if [ "$1" = "mcp" ] && [ "$2" = "add" ]; then
  name="$3"
  shift 3
  url=""
  token=""
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --url)
        url="$2"
        shift 2
        ;;
      --bearer-token-env-var)
        token="$2"
        shift 2
        ;;
      *)
        shift
        ;;
    esac
  done
  python3 - "$CONFIG_FILE" "$name" "$url" "$token" <<'PY'
from pathlib import Path
import sys
path = Path(sys.argv[1])
name, url, token = sys.argv[2], sys.argv[3], sys.argv[4]
lines = []
if path.exists():
    inside = False
    for raw in path.read_text().splitlines():
        if raw.strip() == f"[mcp_servers.{name}]":
            inside = True
            continue
        if inside and raw.startswith("[") and raw.endswith("]"):
            inside = False
        if not inside:
            lines.append(raw)
lines.append(f"[mcp_servers.{name}]")
lines.append(f'url = "{url}"')
if token:
    lines.append(f'bearer_token_env_var = "{token}"')
path.write_text("\n".join(line for line in lines if line.strip()) + "\n")
PY
  exit 0
fi
if [ "$1" = "mcp" ] && [ "$2" = "remove" ]; then
  name="$3"
  python3 - "$CONFIG_FILE" "$name" <<'PY'
from pathlib import Path
import sys
path = Path(sys.argv[1])
name = sys.argv[2]
if not path.exists():
    sys.exit(0)
lines = []
inside = False
for raw in path.read_text().splitlines():
    if raw.strip() == f"[mcp_servers.{name}]":
        inside = True
        continue
    if inside and raw.startswith("[") and raw.endswith("]"):
        inside = False
    if not inside:
        lines.append(raw)
path.write_text("\n".join(line for line in lines if line.strip()) + ("\n" if lines else ""))
PY
  exit 0
fi
echo "unexpected args: $*" >&2
exit 1
"#;
            write_executable(&codex_path, script);

            let previous_path = std::env::var_os("PATH").unwrap_or_default();
            let combined_path =
                format!("{}:{}", bin_dir.display(), previous_path.to_string_lossy());
            unsafe { std::env::set_var("PATH", combined_path) };

            let codex_config_dir = temp_root.join(".codex");
            fs::create_dir_all(&codex_config_dir).unwrap();
            fs::write(
                codex_config_dir.join("config.toml"),
                "[mcp_servers.manual]\nurl = \"http://manual.local/mcp\"\n[mcp_servers.old]\nurl = \"http://old.local/mcp\"\n",
            )
            .unwrap();
            let sync_state_path =
                crate::runtime_layout::config_dir(&runtime_root).join(CODEX_SYNC_STATE_FILE);
            fs::write(
                &sync_state_path,
                serde_json::to_string(&CodexMcpSyncState {
                    managed_servers: vec!["old".to_string()],
                })
                .unwrap(),
            )
            .unwrap();

            let mut config = Config::default();
            config.mcp_servers.insert(
                "memento".to_string(),
                McpServerConfig {
                    url: "http://localhost:57332/mcp".to_string(),
                    auth: Some(crate::config::McpServerAuthConfig {
                        auth_type: McpServerAuthType::Bearer,
                        token_env_var: Some("MEMENTO_ACCESS_KEY".to_string()),
                    }),
                },
            );

            sync_codex_mcp_servers(&config).unwrap();

            let rendered = fs::read_to_string(codex_config_dir.join("config.toml")).unwrap();
            assert!(rendered.contains("[mcp_servers.manual]"));
            assert!(rendered.contains("[mcp_servers.memento]"));
            assert!(rendered.contains("bearer_token_env_var = \"MEMENTO_ACCESS_KEY\""));
            assert!(!rendered.contains("[mcp_servers.old]"));

            let state = load_codex_sync_state_from_path(&sync_state_path);
            assert_eq!(state.managed_servers, vec!["memento".to_string()]);
            assert!(provider_has_memento_mcp(&ProviderKind::Codex));
        });
    }

    #[test]
    fn codex_config_contains_server_detects_manual_sections() {
        with_test_env(|temp_root| {
            let codex_dir = temp_root.join(".codex");
            fs::create_dir_all(&codex_dir).unwrap();
            fs::write(
                codex_dir.join("config.toml"),
                "[mcp_servers.manual]\nurl = \"http://manual.local/mcp\"\n[mcp_servers.\"memento\"]\nurl = \"http://localhost:57332/mcp\"\n",
            )
            .unwrap();

            assert!(provider_has_memento_mcp(&ProviderKind::Codex));
            assert!(codex_config_contains_server("manual"));
        });
    }

    #[test]
    fn sync_opencode_mcp_servers_updates_managed_servers_without_touching_others() {
        with_test_env(|temp_root| {
            let runtime_root = temp_root.join(".adk").join("release");
            let opencode_dir = temp_root.join(".config").join("opencode");
            fs::create_dir_all(&opencode_dir).unwrap();
            fs::write(
                opencode_dir.join("opencode.json"),
                r#"{"$schema":"https://opencode.ai/config.json","mcp":{"manual":{"type":"local","command":["manual"]},"old":{"type":"remote","url":"http://old.local/mcp"}},"provider":{"custom":{}}}"#,
            )
            .unwrap();
            let sync_state_path =
                crate::runtime_layout::config_dir(&runtime_root).join(OPENCODE_SYNC_STATE_FILE);
            fs::write(
                &sync_state_path,
                serde_json::to_string(&OpenCodeMcpSyncState {
                    managed_servers: vec!["old".to_string()],
                })
                .unwrap(),
            )
            .unwrap();

            let mut config = Config::default();
            config.mcp_servers.insert(
                "memento".to_string(),
                McpServerConfig {
                    url: "http://localhost:57332/mcp".to_string(),
                    auth: Some(crate::config::McpServerAuthConfig {
                        auth_type: McpServerAuthType::Bearer,
                        token_env_var: Some("MEMENTO_ACCESS_KEY".to_string()),
                    }),
                },
            );

            sync_opencode_mcp_servers(&config).unwrap();

            let rendered = fs::read_to_string(opencode_dir.join("opencode.json")).unwrap();
            let value: Value = serde_json::from_str(&rendered).unwrap();
            assert!(value["mcp"]["manual"].is_object());
            assert_eq!(value["mcp"]["memento"]["type"], json!("remote"));
            assert_eq!(
                value["mcp"]["memento"]["headers"]["Authorization"],
                json!("Bearer {env:MEMENTO_ACCESS_KEY}")
            );
            assert!(value["provider"]["custom"].is_object());
            assert!(value["mcp"]["old"].is_null());

            let state = load_opencode_sync_state_from_path(&sync_state_path);
            assert_eq!(state.managed_servers, vec!["memento".to_string()]);
            assert!(provider_has_memento_mcp(&ProviderKind::OpenCode));
        });
    }

    #[test]
    fn sync_opencode_mcp_servers_rejects_malformed_config_without_overwrite() {
        with_test_env(|temp_root| {
            let opencode_dir = temp_root.join(".config").join("opencode");
            fs::create_dir_all(&opencode_dir).unwrap();
            let config_path = opencode_dir.join("opencode.json");
            fs::write(&config_path, "{not-json").unwrap();

            let mut config = Config::default();
            config.mcp_servers.insert(
                "memento".to_string(),
                McpServerConfig {
                    url: "http://localhost:57332/mcp".to_string(),
                    auth: None,
                },
            );

            let err = sync_opencode_mcp_servers(&config).unwrap_err();
            assert!(err.contains("Failed to parse OpenCode config"));
            assert_eq!(fs::read_to_string(config_path).unwrap(), "{not-json");
        });
    }

    #[test]
    fn sync_qwen_mcp_servers_updates_managed_servers_without_touching_others() {
        with_test_env(|temp_root| {
            let runtime_root = temp_root.join(".adk").join("release");
            let qwen_dir = temp_root.join(".qwen");
            fs::create_dir_all(&qwen_dir).unwrap();
            fs::write(
                qwen_dir.join("settings.json"),
                r#"{"mcpServers":{"serena":{"command":"serena","args":["start-mcp-server"]},"old":{"httpUrl":"http://old.local/mcp"}},"tools":{"approvalMode":"yolo"}}"#,
            )
            .unwrap();
            let sync_state_path =
                crate::runtime_layout::config_dir(&runtime_root).join(QWEN_SYNC_STATE_FILE);
            fs::write(
                &sync_state_path,
                serde_json::to_string(&QwenMcpSyncState {
                    managed_servers: vec!["old".to_string()],
                })
                .unwrap(),
            )
            .unwrap();

            let mut config = Config::default();
            config.mcp_servers.insert(
                "memento".to_string(),
                McpServerConfig {
                    url: "http://localhost:57332/mcp".to_string(),
                    auth: Some(crate::config::McpServerAuthConfig {
                        auth_type: McpServerAuthType::Bearer,
                        token_env_var: Some("MEMENTO_ACCESS_KEY".to_string()),
                    }),
                },
            );
            config.mcp_servers.insert(
                "context7".to_string(),
                McpServerConfig {
                    url: "https://mcp.context7.com/mcp".to_string(),
                    auth: Some(crate::config::McpServerAuthConfig {
                        auth_type: McpServerAuthType::Bearer,
                        token_env_var: Some("CONTEXT7_API_KEY".to_string()),
                    }),
                },
            );

            sync_qwen_mcp_servers(&config).unwrap();

            let rendered = fs::read_to_string(qwen_dir.join("settings.json")).unwrap();
            let value: Value = serde_json::from_str(&rendered).unwrap();
            assert!(value["mcpServers"]["serena"].is_object());
            assert!(value["mcpServers"]["old"].is_null());
            assert_eq!(
                value["mcpServers"]["memento"]["httpUrl"],
                json!("http://localhost:57332/mcp")
            );
            assert_eq!(
                value["mcpServers"]["context7"]["headers"]["Authorization"],
                json!("Bearer ${CONTEXT7_API_KEY}")
            );
            assert_eq!(value["tools"]["approvalMode"], json!("yolo"));

            let state = load_qwen_sync_state_from_path(&sync_state_path);
            assert_eq!(
                state.managed_servers,
                vec!["context7".to_string(), "memento".to_string()]
            );
            assert!(provider_has_mcp_server(&ProviderKind::Qwen, "context7"));
        });
    }

    #[test]
    fn sync_gemini_mcp_servers_updates_managed_servers_without_touching_others() {
        with_test_env(|temp_root| {
            let runtime_root = temp_root.join(".adk").join("release");
            let gemini_dir = temp_root.join(".gemini");
            fs::create_dir_all(&gemini_dir).unwrap();
            fs::write(
                gemini_dir.join("settings.json"),
                r#"{"mcpServers":{"serena":{"command":"serena","args":["start-mcp-server"]},"old":{"httpUrl":"http://old.local/mcp"}},"approvalMode":"yolo"}"#,
            )
            .unwrap();
            let sync_state_path =
                crate::runtime_layout::config_dir(&runtime_root).join(GEMINI_SYNC_STATE_FILE);
            fs::write(
                &sync_state_path,
                serde_json::to_string(&GeminiMcpSyncState {
                    managed_servers: vec!["old".to_string()],
                })
                .unwrap(),
            )
            .unwrap();

            let mut config = Config::default();
            config.mcp_servers.insert(
                "memento".to_string(),
                McpServerConfig {
                    url: "http://localhost:57332/mcp".to_string(),
                    auth: Some(crate::config::McpServerAuthConfig {
                        auth_type: McpServerAuthType::Bearer,
                        token_env_var: Some("MEMENTO_ACCESS_KEY".to_string()),
                    }),
                },
            );
            config.mcp_servers.insert(
                "context7".to_string(),
                McpServerConfig {
                    url: "https://mcp.context7.com/mcp".to_string(),
                    auth: Some(crate::config::McpServerAuthConfig {
                        auth_type: McpServerAuthType::Bearer,
                        token_env_var: Some("CONTEXT7_API_KEY".to_string()),
                    }),
                },
            );

            sync_gemini_mcp_servers(&config).unwrap();

            let rendered = fs::read_to_string(gemini_dir.join("settings.json")).unwrap();
            let value: Value = serde_json::from_str(&rendered).unwrap();
            assert!(value["mcpServers"]["serena"].is_object());
            assert!(value["mcpServers"]["old"].is_null());
            assert_eq!(
                value["mcpServers"]["memento"]["httpUrl"],
                json!("http://localhost:57332/mcp")
            );
            assert_eq!(
                value["mcpServers"]["context7"]["headers"]["Authorization"],
                json!("Bearer ${CONTEXT7_API_KEY}")
            );
            assert_eq!(value["approvalMode"], json!("yolo"));

            let state = load_gemini_sync_state_from_path(&sync_state_path);
            assert_eq!(
                state.managed_servers,
                vec!["context7".to_string(), "memento".to_string()]
            );
            assert!(provider_has_mcp_server(&ProviderKind::Gemini, "context7"));
        });
    }
}
