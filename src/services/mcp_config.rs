use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{Config, McpServerAuthType, McpServerConfig};
use crate::services::discord::runtime_store::atomic_write;
use crate::services::provider::ProviderKind;

const CODEX_SYNC_STATE_FILE: &str = "codex-mcp-sync-state.json";
const MEMENTO_SERVER_NAME: &str = "memento";

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
        _ => false,
    }
}

pub(crate) fn claude_mcp_config_arg() -> Option<String> {
    let servers = load_runtime_mcp_servers();
    claude_mcp_config_arg_from_servers(&servers)
}

#[cfg(test)]
pub(crate) fn claude_mcp_config_arg_from_config(config: &Config) -> Option<String> {
    let servers = resolved_mcp_servers(config);
    claude_mcp_config_arg_from_servers(&servers)
}

pub(crate) fn sync_codex_mcp_servers(config: &Config) -> Result<(), String> {
    let runtime_root = crate::config::runtime_root()
        .ok_or_else(|| "AGENTDESK_ROOT_DIR is unavailable".to_string())?;
    let sync_state_path =
        crate::runtime_layout::config_dir(&runtime_root).join(CODEX_SYNC_STATE_FILE);
    let desired = resolved_mcp_servers(config);
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
            let mut headers = Map::new();
            headers.insert(
                "Authorization".to_string(),
                Value::String(format!("Bearer ${{{token_env_var}}}")),
            );
            entry.insert("headers".to_string(), Value::Object(headers));
        }
        mcp_servers.insert(server.name.clone(), Value::Object(entry));
    }

    serde_json::to_string(&Value::Object(Map::from_iter([(
        "mcpServers".to_string(),
        Value::Object(mcp_servers),
    )])))
    .ok()
}

fn resolved_mcp_servers(config: &Config) -> BTreeMap<String, ResolvedMcpServer> {
    config
        .mcp_servers
        .iter()
        .filter_map(|(name, server)| resolve_mcp_server(name, server))
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

fn load_runtime_mcp_servers() -> BTreeMap<String, ResolvedMcpServer> {
    load_runtime_config()
        .map(|config| resolved_mcp_servers(&config))
        .unwrap_or_default()
}

fn runtime_config_contains_server(server_name: &str) -> bool {
    load_runtime_mcp_servers().contains_key(server_name)
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

#[cfg(test)]
mod tests {
    use super::*;
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
        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", &runtime_root);
            std::env::set_var("HOME", temp.path());
            std::env::set_var("USERPROFILE", temp.path());
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

        let rendered = claude_mcp_config_arg_from_config(&config).expect("config");
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
            Value::String("Bearer ${MEMENTO_ACCESS_KEY}".to_string())
        );
    }

    #[test]
    fn provider_has_memento_mcp_reflects_runtime_config_for_claude_and_codex() {
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
}
