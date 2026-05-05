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
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use content::load_peer_agents;
pub(super) use content::{
    channel_upload_dir, cleanup_channel_uploads, cleanup_old_uploads, is_known_agent,
    load_review_tuning_guidance, load_role_prompt, render_peer_agent_guidance,
};
pub(crate) use memory::{memory_settings_for_binding, resolve_memory_settings};
pub(super) use read::{
    load_bot_settings, load_last_remote_profile, load_last_session_path, save_last_session_runtime,
};
pub use read::{
    load_discord_bot_launch_configs, resolve_discord_bot_provider, resolve_discord_token_by_hash,
};
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use validation::bot_settings_allow_agent;
pub(crate) use validation::list_registered_channel_bindings;
pub(super) use validation::{
    BotChannelRoutingGuardFailure, bot_settings_allow_channel, channel_supports_provider,
    has_configured_channel_binding, resolve_cache_ttl_minutes, resolve_dispatch_profile,
    resolve_role_binding, resolve_workspace, validate_bot_channel_routing,
    validate_bot_channel_routing_with_provider_channel,
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
    pub recall_timeout_ms: u64,
    pub capture_timeout_ms: u64,
}

impl Default for ResolvedMemorySettings {
    fn default() -> Self {
        Self {
            backend: MemoryBackendKind::File,
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use poise::serenity_prelude::ChannelId;
    use tempfile::TempDir;

    use crate::services::provider::ProviderKind;

    use super::{
        BotChannelRoutingGuardFailure, bot_settings_allow_agent, bot_settings_allow_channel,
        channel_supports_provider, discord_token_hash, list_registered_channel_bindings,
        load_bot_settings, load_discord_bot_launch_configs, load_peer_agents,
        render_peer_agent_guidance, resolve_memory_settings, resolve_role_binding,
        save_bot_settings, validate_bot_channel_routing,
        validate_bot_channel_routing_with_provider_channel,
    };

    fn with_temp_home<F>(f: F)
    where
        F: FnOnce(&TempDir),
    {
        let _guard = super::super::runtime_store::lock_test_env();
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

    fn write_memory_backend_config(temp_home: &TempDir, value: serde_json::Value) {
        let path = temp_home
            .path()
            .join(".adk")
            .join("config")
            .join("memory-backend.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
    }

    fn write_agentdesk_yaml(temp_home: &TempDir, contents: &str) {
        let path = temp_home
            .path()
            .join(".adk")
            .join("config")
            .join("agentdesk.yaml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn with_env_vars<F>(values: &[(&str, Option<&str>)], f: F)
    where
        F: FnOnce(),
    {
        let mut previous = BTreeMap::new();
        for (name, value) in values {
            previous.insert((*name).to_string(), std::env::var_os(name));
            match value {
                Some(value) => unsafe { std::env::set_var(name, value) },
                None => unsafe { std::env::remove_var(name) },
            }
        }

        f();

        for (name, previous) in previous {
            match previous {
                Some(value) => unsafe { std::env::set_var(&name, value) },
                None => unsafe { std::env::remove_var(&name) },
            }
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
    fn test_resolve_memory_settings_defaults_to_file_and_code_defaults() {
        crate::services::memory::reset_backend_health_for_tests();
        with_temp_home(|temp_home: &TempDir| {
            write_agentdesk_yaml(
                temp_home,
                "server:\n  port: 8791\nmemory:\n  backend: auto\n",
            );

            with_env_vars(&[("MEMENTO_TEST_KEY", None)], || {
                let resolved = resolve_memory_settings(None, None);
                assert_eq!(resolved.backend, super::MemoryBackendKind::File);
                assert_eq!(resolved.recall_timeout_ms, 500);
                assert_eq!(resolved.capture_timeout_ms, 5_000);
            });
        });
    }

    #[test]
    fn test_resolve_memory_settings_applies_override_and_clamps_values() {
        crate::services::memory::reset_backend_health_for_tests();
        with_temp_home(|temp_home: &TempDir| {
            write_memory_backend_config(
                temp_home,
                serde_json::json!({
                    "version": 2,
                    "backend": "memento",
                    "mcp": {
                        "endpoint": "http://127.0.0.1:8765",
                        "access_key_env": "MEMENTO_TEST_KEY"
                    }
                }),
            );

            let agent = super::MemoryConfigOverride {
                backend: Some("memento".to_string()),
                recall_timeout_ms: Some(50),
                capture_timeout_ms: Some(60_000),
                ..Default::default()
            };
            let channel = super::MemoryConfigOverride {
                backend: Some("memento".to_string()),
                recall_timeout_ms: Some(5_000),
                capture_timeout_ms: Some(100),
                ..Default::default()
            };

            with_env_vars(&[("MEMENTO_TEST_KEY", Some("memento-key"))], || {
                let resolved = resolve_memory_settings(Some(&agent), Some(&channel));
                assert_eq!(resolved.backend, super::MemoryBackendKind::Memento);
                assert_eq!(resolved.recall_timeout_ms, 2_000);
                assert_eq!(resolved.capture_timeout_ms, 500);
            });
        });
    }

    #[test]
    fn test_resolve_memory_settings_auto_detects_memento_then_file() {
        crate::services::memory::reset_backend_health_for_tests();
        with_temp_home(|temp_home: &TempDir| {
            write_agentdesk_yaml(
                temp_home,
                r#"server:
  port: 8791
memory:
  backend: auto
  mcp:
    endpoint: http://127.0.0.1:8765
    access_key_env: MEMENTO_TEST_KEY
"#,
            );

            with_env_vars(&[("MEMENTO_TEST_KEY", Some("memento-key"))], || {
                let resolved = resolve_memory_settings(None, None);
                assert_eq!(resolved.backend, super::MemoryBackendKind::Memento);
            });

            with_env_vars(&[("MEMENTO_TEST_KEY", None)], || {
                let resolved = resolve_memory_settings(None, None);
                assert_eq!(resolved.backend, super::MemoryBackendKind::File);
            });
        });
    }

    #[test]
    fn test_resolve_memory_settings_explicit_backend_skips_auto_detection() {
        crate::services::memory::reset_backend_health_for_tests();
        with_temp_home(|temp_home: &TempDir| {
            write_agentdesk_yaml(
                temp_home,
                r#"server:
  port: 8791
memory:
  backend: auto
  mcp:
    endpoint: http://127.0.0.1:8765
    access_key_env: MEMENTO_TEST_KEY
"#,
            );

            with_env_vars(&[("MEMENTO_TEST_KEY", Some("memento-key"))], || {
                let file = resolve_memory_settings(
                    Some(&super::MemoryConfigOverride {
                        backend: Some("file".to_string()),
                        ..Default::default()
                    }),
                    None,
                );
                assert_eq!(file.backend, super::MemoryBackendKind::File);

                let memento = resolve_memory_settings(
                    Some(&super::MemoryConfigOverride {
                        backend: Some("memento".to_string()),
                        ..Default::default()
                    }),
                    None,
                );
                assert_eq!(memento.backend, super::MemoryBackendKind::Memento);
            });
        });
    }

    #[test]
    fn test_resolve_memory_settings_accepts_local_alias_and_ignores_available_mcps() {
        crate::services::memory::reset_backend_health_for_tests();
        with_temp_home(|temp_home: &TempDir| {
            write_agentdesk_yaml(
                temp_home,
                r#"server:
  port: 8791
memory:
  backend: auto
  mcp:
    endpoint: http://127.0.0.1:8765
    access_key_env: MEMENTO_TEST_KEY
"#,
            );

            with_env_vars(&[("MEMENTO_TEST_KEY", Some("memento-key"))], || {
                let resolved = resolve_memory_settings(
                    Some(&super::MemoryConfigOverride {
                        backend: Some("local".to_string()),
                        ..Default::default()
                    }),
                    None,
                );
                assert_eq!(resolved.backend, super::MemoryBackendKind::File);
            });
        });
    }

    #[test]
    fn test_resolve_memory_settings_treats_legacy_mem0_alias_as_file() {
        crate::services::memory::reset_backend_health_for_tests();
        with_temp_home(|temp_home: &TempDir| {
            write_agentdesk_yaml(
                temp_home,
                r#"server:
  port: 8791
memory:
  backend: auto
  mcp:
    endpoint: http://127.0.0.1:8765
    access_key_env: MEMENTO_TEST_KEY
"#,
            );

            with_env_vars(&[("MEMENTO_TEST_KEY", Some("memento-key"))], || {
                let resolved = resolve_memory_settings(
                    Some(&super::MemoryConfigOverride {
                        backend: Some("mem0".to_string()),
                        ..Default::default()
                    }),
                    None,
                );
                assert_eq!(resolved.backend, super::MemoryBackendKind::File);
            });
        });
    }

    #[test]
    fn test_resolve_memory_settings_explicit_backend_falls_back_to_file_when_unavailable() {
        crate::services::memory::reset_backend_health_for_tests();
        with_temp_home(|temp_home: &TempDir| {
            write_agentdesk_yaml(
                temp_home,
                r#"server:
  port: 8791
memory:
  backend: auto
  mcp:
    endpoint: http://127.0.0.1:8765
    access_key_env: MEMENTO_TEST_KEY
"#,
            );

            with_env_vars(&[("MEMENTO_TEST_KEY", None)], || {
                let memento = resolve_memory_settings(
                    Some(&super::MemoryConfigOverride {
                        backend: Some("memento".to_string()),
                        ..Default::default()
                    }),
                    None,
                );
                assert_eq!(memento.backend, super::MemoryBackendKind::File);
            });
        });
    }

    #[test]
    fn test_unavailable_explicit_backend_uses_local_prompt_guidance() {
        crate::services::memory::reset_backend_health_for_tests();
        with_temp_home(|_temp_home: &TempDir| {
            with_env_vars(
                &[("MEMENTO_TEST_KEY", None), ("MEMENTO_WORKSPACE", None)],
                || {
                    let resolved = resolve_memory_settings(
                        Some(&super::MemoryConfigOverride {
                            backend: Some("memento".to_string()),
                            ..Default::default()
                        }),
                        None,
                    );
                    assert_eq!(resolved.backend, super::MemoryBackendKind::File);

                    let prompt = super::super::prompt_builder::build_system_prompt(
                        "ctx",
                        &[],
                        "/tmp",
                        ChannelId::new(1),
                        "tok",
                        None,
                        false,
                        super::super::prompt_builder::DispatchProfile::Full,
                        None,
                        None,
                        None,
                        None,
                        Some(&resolved),
                        true,
                    );

                    assert!(prompt.contains("[Proactive Memory Guidance]"));
                    assert!(prompt.contains("`memory-read` skill"));
                    assert!(prompt.contains("`memory-write` skill"));
                    assert!(!prompt.contains("`recall` MCP tool"));
                    assert!(!prompt.contains("`remember` MCP tool"));
                },
            );
        });
    }

    #[test]
    fn test_resolve_memory_settings_uses_legacy_json_fallback_when_yaml_memory_is_absent() {
        crate::services::memory::reset_backend_health_for_tests();
        with_temp_home(|temp_home: &TempDir| {
            write_memory_backend_config(
                temp_home,
                serde_json::json!({
                    "version": 2,
                    "backend": "memento",
                    "mcp": {
                        "endpoint": "http://127.0.0.1:8765",
                        "access_key_env": "MEMENTO_TEST_KEY"
                    }
                }),
            );

            with_env_vars(&[("MEMENTO_TEST_KEY", Some("memento-key"))], || {
                let resolved = resolve_memory_settings(None, None);
                assert_eq!(resolved.backend, super::MemoryBackendKind::Memento);
            });
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
    fn test_load_bot_settings_uses_qwen_supported_tools_for_qwen_when_omitted() {
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
                crate::services::qwen::QWEN_SUPPORTED_ALLOWED_TOOLS
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
    fn test_load_bot_launch_configs_excludes_unmapped_utility_bots_from_yaml() {
        with_temp_home(|temp_home: &TempDir| {
            write_agentdesk_yaml(
                temp_home,
                r#"
server:
  port: 8791
discord:
  owner_id: "343742347365974026"
  bots:
    claude:
      token: "claude-token"
      provider: "claude"
    codex:
      token: "codex-token"
      provider: "codex"
    announce:
      token: "announce-token"
      provider: "claude"
    notify:
      token: "notify-token"
      provider: "claude"
agents:
  - id: project-agentdesk
    name: "AgentDesk"
    provider: claude
    channels:
      claude:
        id: "1479671298497183835"
        name: "adk-cc"
      codex:
        id: "1479671301387059200"
        name: "adk-cdx"
"#,
            );

            let configs = load_discord_bot_launch_configs();
            assert_eq!(configs.len(), 2);
            assert_eq!(
                configs
                    .iter()
                    .map(|cfg| cfg.token.as_str())
                    .collect::<Vec<_>>(),
                vec!["codex-token", "claude-token"]
            );
            assert_eq!(
                configs
                    .iter()
                    .map(|cfg| cfg.provider.clone())
                    .collect::<Vec<ProviderKind>>(),
                vec![ProviderKind::Codex, ProviderKind::Claude]
            );
        });
    }

    #[test]
    fn test_load_bot_launch_configs_includes_command_alias_bots_from_yaml() {
        with_temp_home(|temp_home: &TempDir| {
            write_agentdesk_yaml(
                temp_home,
                r#"
server:
  port: 8791
discord:
  owner_id: "343742347365974026"
  bots:
    command:
      token: "claude-token"
      provider: "claude"
    command_2:
      token: "codex-token"
      provider: "codex"
    notify:
      token: "notify-token"
      provider: "claude"
agents:
  - id: project-agentdesk
    name: "AgentDesk"
    provider: claude
    channels:
      claude:
        id: "1479671298497183835"
        name: "adk-cc"
      codex:
        id: "1479671301387059200"
        name: "adk-cdx"
"#,
            );

            let mut tokens = load_discord_bot_launch_configs()
                .into_iter()
                .map(|cfg| cfg.token)
                .collect::<Vec<_>>();
            tokens.sort();
            assert_eq!(tokens, vec!["claude-token", "codex-token"]);
        });
    }

    #[test]
    fn test_load_bot_launch_configs_includes_allowlisted_alias_bot_from_yaml() {
        with_temp_home(|temp_home: &TempDir| {
            write_agentdesk_yaml(
                temp_home,
                r#"
server:
  port: 8791
discord:
  owner_id: "343742347365974026"
  bots:
    workspace-bot:
      token: "workspace-token"
      provider: "claude"
      auth:
        allowed_channel_ids:
          - "1479671298497183835"
    notify:
      token: "notify-token"
      provider: "claude"
agents:
  - id: project-agentdesk
    name: "AgentDesk"
    provider: claude
    channels:
      claude:
        id: "1479671298497183835"
        name: "adk-cc"
"#,
            );

            let configs = load_discord_bot_launch_configs();
            assert_eq!(configs.len(), 1);
            assert_eq!(configs[0].token, "workspace-token");
            assert_eq!(configs[0].provider, ProviderKind::Claude);
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
    fn test_load_bot_settings_reads_channel_fast_modes() {
        with_temp_home(|temp_home: &TempDir| {
            let settings_dir = temp_home.path().join(".adk").join("config");
            fs::create_dir_all(&settings_dir).unwrap();
            let token = "test-token";
            let key = discord_token_hash(token);
            let json = serde_json::json!({
                key: {
                    "token": token,
                    "channel_fast_modes": {
                        "123": true,
                        "456": false
                    }
                }
            });
            fs::write(
                settings_dir.join("bot_settings.json"),
                serde_json::to_string_pretty(&json).unwrap(),
            )
            .unwrap();

            let settings = load_bot_settings(token);
            assert_eq!(settings.channel_fast_modes.get("123"), Some(&true));
            assert_eq!(settings.channel_fast_modes.get("456"), Some(&false));
        });
    }

    #[test]
    fn test_load_bot_settings_reads_channel_fast_mode_reset_pending() {
        with_temp_home(|temp_home: &TempDir| {
            let settings_dir = temp_home.path().join(".adk").join("config");
            fs::create_dir_all(&settings_dir).unwrap();
            let token = "test-token";
            let key = discord_token_hash(token);
            let json = serde_json::json!({
                key: {
                    "token": token,
                    "channel_fast_mode_reset_pending": ["123", "456"]
                }
            });
            fs::write(
                settings_dir.join("bot_settings.json"),
                serde_json::to_string_pretty(&json).unwrap(),
            )
            .unwrap();

            let settings = load_bot_settings(token);
            assert!(settings.channel_fast_mode_reset_pending.contains("123"));
            assert!(settings.channel_fast_mode_reset_pending.contains("456"));
        });
    }

    #[test]
    fn test_load_bot_settings_reads_channel_codex_goals() {
        with_temp_home(|temp_home: &TempDir| {
            let settings_dir = temp_home.path().join(".adk").join("config");
            fs::create_dir_all(&settings_dir).unwrap();
            let token = "test-token";
            let key = discord_token_hash(token);
            let json = serde_json::json!({
                key: {
                    "token": token,
                    "channel_codex_goals": {
                        "123": true,
                        "456": false
                    },
                    "channel_codex_goals_reset_pending": ["123"]
                }
            });
            fs::write(
                settings_dir.join("bot_settings.json"),
                serde_json::to_string_pretty(&json).unwrap(),
            )
            .unwrap();

            let settings = load_bot_settings(token);
            assert_eq!(settings.channel_codex_goals.get("123"), Some(&true));
            assert_eq!(settings.channel_codex_goals.get("456"), Some(&false));
            assert!(settings.channel_codex_goals_reset_pending.contains("123"));
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
    fn test_load_bot_settings_reads_require_mention_channel_ids() {
        with_temp_home(|temp_home: &TempDir| {
            let settings_dir = temp_home.path().join(".adk").join("config");
            fs::create_dir_all(&settings_dir).unwrap();
            let token = "test-token";
            let key = discord_token_hash(token);
            let json = serde_json::json!({
                key: {
                    "token": token,
                    "require_mention_channel_ids": ["123", 456]
                }
            });
            fs::write(
                settings_dir.join("bot_settings.json"),
                serde_json::to_string_pretty(&json).unwrap(),
            )
            .unwrap();

            let settings = load_bot_settings(token);
            assert_eq!(settings.require_mention_channel_ids, vec![123, 456]);
        });
    }

    struct TestLogWriter {
        buffer: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    }

    impl std::io::Write for TestLogWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.buffer.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for TestLogWriter {
        type Writer = TestLogWriter;
        fn make_writer(&'a self) -> Self::Writer {
            TestLogWriter {
                buffer: self.buffer.clone(),
            }
        }
    }

    fn capture_logs<F>(f: F) -> String
    where
        F: FnOnce(),
    {
        use tracing_subscriber::fmt;
        use tracing_subscriber::prelude::*;

        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = TestLogWriter {
            buffer: buffer.clone(),
        };
        let layer = fmt::layer()
            .with_writer(writer)
            .with_ansi(false)
            .with_target(false)
            .with_level(true);
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, f);
        String::from_utf8(buffer.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn test_load_bot_settings_falls_back_to_legacy_same_token_entry() {
        with_temp_home(|temp_home: &TempDir| {
            let settings_dir = temp_home.path().join(".adk").join("config");
            fs::create_dir_all(&settings_dir).unwrap();
            let token = "test-token";
            let json = serde_json::json!({
                "claude": {
                    "token": token,
                    "owner_user_id": 42,
                    "allowed_user_ids": [7],
                    "allow_all_users": true
                }
            });
            fs::write(
                settings_dir.join("bot_settings.json"),
                serde_json::to_string_pretty(&json).unwrap(),
            )
            .unwrap();

            let log_output = capture_logs(|| {
                let settings = load_bot_settings(token);
                assert_eq!(settings.owner_user_id, Some(42));
                assert_eq!(settings.allowed_user_ids, vec![7]);
                assert!(settings.allow_all_users);
            });
            assert!(
                log_output.contains("falling back to legacy bot_settings.json"),
                "expected legacy fallback warning in log output, got: {log_output}"
            );
        });
    }

    #[test]
    fn test_load_bot_settings_does_not_warn_when_yaml_has_fields() {
        with_temp_home(|temp_home: &TempDir| {
            write_agentdesk_yaml(
                temp_home,
                r#"
server:
  port: 8791
discord:
  owner_id: "99"
  bots:
    command:
      token: "yaml-token"
      provider: "claude"
      agent: "AgentDesk"
      auth:
        allowed_user_ids:
          - "7"
        allow_all_users: false
        allowed_bot_ids:
          - "9"
        allowed_channel_ids:
          - "123"
        require_mention_channel_ids:
          - "456"
agents:
  - id: project-agentdesk
    name: "AgentDesk"
    provider: claude
    channels:
      claude:
        id: "123"
        name: "adk-cc"
"#,
            );

            let log_output = capture_logs(|| {
                let settings = load_bot_settings("yaml-token");
                assert_eq!(settings.provider, ProviderKind::Claude);
                assert_eq!(settings.owner_user_id, Some(99));
                assert_eq!(settings.allowed_user_ids, vec![7]);
                assert!(!settings.allow_all_users);
                assert_eq!(settings.allowed_bot_ids, vec![9]);
                assert_eq!(settings.allowed_channel_ids, vec![123]);
                assert_eq!(settings.require_mention_channel_ids, vec![456]);
            });
            assert!(
                !log_output.contains("falling back to legacy bot_settings.json"),
                "should not warn when all fields come from YAML, got: {log_output}"
            );
        });
    }

    #[test]
    fn test_load_bot_launch_configs_dedupes_same_token_preferring_hash_key() {
        with_temp_home(|temp_home: &TempDir| {
            let settings_dir = temp_home.path().join(".adk").join("config");
            fs::create_dir_all(&settings_dir).unwrap();
            let token = "shared-token";
            let canonical_key = discord_token_hash(token);
            let other_token = "other-token";
            let other_key = discord_token_hash(other_token);
            fs::write(
                settings_dir.join("bot_settings.json"),
                serde_json::to_string_pretty(&serde_json::json!({
                    "claude": { "token": token, "provider": "claude" },
                    canonical_key.clone(): { "token": token, "provider": "codex" },
                    other_key: { "token": other_token, "provider": "claude" }
                }))
                .unwrap(),
            )
            .unwrap();

            let configs = load_discord_bot_launch_configs();
            assert_eq!(configs.len(), 2);

            let shared = configs.iter().find(|config| config.token == token).unwrap();
            assert_eq!(shared.hash_key, canonical_key);
            assert_eq!(shared.provider, ProviderKind::Codex);
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
    fn test_save_bot_settings_persists_channel_fast_modes() {
        with_temp_home(|_temp_home: &TempDir| {
            let token = "test-token";
            let mut settings = super::super::DiscordBotSettings::default();
            settings.channel_fast_modes.insert("123".to_string(), true);

            save_bot_settings(token, &settings);

            let loaded = load_bot_settings(token);
            assert_eq!(loaded.channel_fast_modes.get("123"), Some(&true));
        });
    }

    #[test]
    fn test_save_bot_settings_persists_channel_fast_mode_reset_pending() {
        with_temp_home(|_temp_home: &TempDir| {
            let token = "test-token";
            let mut settings = super::super::DiscordBotSettings::default();
            settings
                .channel_fast_mode_reset_pending
                .insert("123".to_string());

            save_bot_settings(token, &settings);

            let loaded = load_bot_settings(token);
            assert!(loaded.channel_fast_mode_reset_pending.contains("123"));
        });
    }

    #[test]
    fn test_save_bot_settings_persists_channel_codex_goals() {
        with_temp_home(|_temp_home: &TempDir| {
            let token = "test-token";
            let mut settings = super::super::DiscordBotSettings::default();
            settings.channel_codex_goals.insert("123".to_string(), true);
            settings
                .channel_codex_goals_reset_pending
                .insert("123".to_string());

            save_bot_settings(token, &settings);

            let loaded = load_bot_settings(token);
            assert_eq!(loaded.channel_codex_goals.get("123"), Some(&true));
            assert!(loaded.channel_codex_goals_reset_pending.contains("123"));
        });
    }

    #[test]
    fn test_save_bot_settings_persists_allowed_channel_ids() {
        with_temp_home(|temp_home: &TempDir| {
            let token = "test-token";
            write_agentdesk_yaml(
                temp_home,
                &format!(
                    "server:\n  port: 8791\ndiscord:\n  bots:\n    command:\n      token: \"{token}\"\n"
                ),
            );
            let mut settings = super::super::DiscordBotSettings::default();
            settings.allowed_channel_ids = vec![123, 456];

            save_bot_settings(token, &settings);

            let loaded = load_bot_settings(token);
            assert_eq!(loaded.allowed_channel_ids, vec![123, 456]);
        });
    }

    #[test]
    fn test_save_bot_settings_persists_require_mention_channel_ids() {
        with_temp_home(|temp_home: &TempDir| {
            let token = "test-token";
            write_agentdesk_yaml(
                temp_home,
                &format!(
                    "server:\n  port: 8791\ndiscord:\n  bots:\n    command:\n      token: \"{token}\"\n"
                ),
            );
            let mut settings = super::super::DiscordBotSettings::default();
            settings.require_mention_channel_ids = vec![123, 456];

            save_bot_settings(token, &settings);

            let loaded = load_bot_settings(token);
            assert_eq!(loaded.require_mention_channel_ids, vec![123, 456]);
        });
    }

    #[test]
    fn test_save_bot_settings_does_not_overwrite_yaml_owner_for_unconfigured_bot() {
        with_temp_home(|temp_home: &TempDir| {
            let configured_token = "configured-token";
            let unconfigured_token = "unconfigured-token";
            write_agentdesk_yaml(
                temp_home,
                &format!(
                    "server:\n  port: 8791\ndiscord:\n  owner_id: 7\n  bots:\n    command:\n      token: \"{configured_token}\"\n"
                ),
            );

            let mut settings = super::super::DiscordBotSettings::default();
            settings.owner_user_id = Some(42);
            save_bot_settings(unconfigured_token, &settings);

            let yaml_after = fs::read_to_string(
                temp_home
                    .path()
                    .join(".adk")
                    .join("config")
                    .join("agentdesk.yaml"),
            )
            .unwrap();
            assert!(yaml_after.contains("owner_id: 7"));
            assert!(!yaml_after.contains("owner_id: 42"));
        });
    }

    #[test]
    fn test_save_bot_settings_rolls_back_yaml_and_json_when_runtime_write_fails() {
        with_temp_home(|temp_home: &TempDir| {
            struct ResetRuntimeWriteFailureFlag;

            impl Drop for ResetRuntimeWriteFailureFlag {
                fn drop(&mut self) {
                    super::write::set_force_runtime_settings_write_failure_for_tests(false);
                }
            }

            let settings_dir = temp_home.path().join(".adk").join("config");
            fs::create_dir_all(&settings_dir).unwrap();
            let token = "test-token";
            write_agentdesk_yaml(
                temp_home,
                &format!(
                    "server:\n  port: 8791\ndiscord:\n  owner_id: 7\n  bots:\n    command:\n      token: \"{token}\"\n"
                ),
            );

            let json_path = settings_dir.join("bot_settings.json");
            let json_before = serde_json::to_string_pretty(&serde_json::json!({
                "legacy_alias": {
                    "token": token,
                    "owner_user_id": 99,
                    "allowed_channel_ids": [123]
                }
            }))
            .unwrap();
            fs::write(&json_path, &json_before).unwrap();

            let mut settings = super::super::DiscordBotSettings::default();
            settings.owner_user_id = Some(42);
            settings.allowed_channel_ids = vec![555];

            let _reset_flag = ResetRuntimeWriteFailureFlag;
            super::write::set_force_runtime_settings_write_failure_for_tests(true);
            save_bot_settings(token, &settings);

            let yaml_after = fs::read_to_string(
                temp_home
                    .path()
                    .join(".adk")
                    .join("config")
                    .join("agentdesk.yaml"),
            )
            .unwrap();
            assert!(yaml_after.contains("owner_id: 7"));
            assert!(!yaml_after.contains("owner_id: 42"));

            let json_after = fs::read_to_string(&json_path).unwrap();
            assert_eq!(json_after, json_before);
        });
    }

    #[test]
    fn test_save_bot_settings_removes_same_token_legacy_entries() {
        with_temp_home(|temp_home: &TempDir| {
            let settings_dir = temp_home.path().join(".adk").join("config");
            fs::create_dir_all(&settings_dir).unwrap();
            let token = "test-token";
            let canonical_key = discord_token_hash(token);
            let other_token = "other-token";
            let other_key = discord_token_hash(other_token);
            let path = settings_dir.join("bot_settings.json");
            let json = serde_json::json!({
                "claude": { "token": token, "owner_user_id": 1 },
                other_key.clone(): { "token": other_token, "owner_user_id": 2 }
            });
            fs::write(&path, serde_json::to_string_pretty(&json).unwrap()).unwrap();
            write_agentdesk_yaml(
                temp_home,
                &format!(
                    "server:\n  port: 8791\ndiscord:\n  bots:\n    command:\n      token: \"{token}\"\n"
                ),
            );

            let mut settings = super::super::DiscordBotSettings::default();
            settings.owner_user_id = Some(42);
            save_bot_settings(token, &settings);

            let raw = fs::read_to_string(&path).unwrap();
            let saved: serde_json::Value = serde_json::from_str(&raw).unwrap();
            let obj = saved.as_object().unwrap();
            assert!(obj.get("claude").is_none());
            assert!(obj.get(&canonical_key).is_none());
            assert!(obj.get(&other_key).is_some());
            assert_eq!(obj.len(), 1);

            let loaded = load_bot_settings(token);
            assert_eq!(loaded.owner_user_id, Some(42));
        });
    }

    #[test]
    fn test_save_bot_settings_preserves_existing_yaml_owner_id() {
        with_temp_home(|temp_home: &TempDir| {
            let settings_dir = temp_home.path().join(".adk").join("config");
            fs::create_dir_all(&settings_dir).unwrap();
            let token = "test-token";
            write_agentdesk_yaml(
                temp_home,
                &format!(
                    "server:\n  port: 8791\ndiscord:\n  owner_id: 1469509284508340276\n  bots:\n    command:\n      token: \"{token}\"\n"
                ),
            );

            let path = settings_dir.join("bot_settings.json");
            let json = serde_json::json!({
                "legacy_alias": {
                    "token": token,
                    "owner_user_id": 7
                }
            });
            fs::write(&path, serde_json::to_string_pretty(&json).unwrap()).unwrap();

            let mut settings = load_bot_settings(token);
            settings.owner_user_id = Some(7);
            settings.allowed_channel_ids = vec![555];
            save_bot_settings(token, &settings);

            let yaml_after = fs::read_to_string(
                temp_home
                    .path()
                    .join(".adk")
                    .join("config")
                    .join("agentdesk.yaml"),
            )
            .unwrap();
            assert!(yaml_after.contains("owner_id: 1469509284508340276"));
            assert!(!yaml_after.contains("owner_id: 7"));

            let loaded = load_bot_settings(token);
            assert_eq!(loaded.owner_user_id, Some(1469509284508340276));
            assert_eq!(loaded.allowed_channel_ids, vec![555]);
        });
    }

    #[test]
    fn test_save_bot_settings_preserves_legacy_launch_metadata_without_yaml_match() {
        with_temp_home(|temp_home: &TempDir| {
            let settings_dir = temp_home.path().join(".adk").join("config");
            fs::create_dir_all(&settings_dir).unwrap();
            let token = "legacy-token";
            let key = discord_token_hash(token);
            let path = settings_dir.join("bot_settings.json");
            let json = serde_json::json!({
                "legacy_alias": {
                    "token": token,
                    "provider": "codex",
                    "agent": "codex",
                    "allowed_channel_ids": [123],
                    "owner_user_id": 7
                }
            });
            fs::write(&path, serde_json::to_string_pretty(&json).unwrap()).unwrap();

            let mut settings = load_bot_settings(token);
            settings
                .channel_model_overrides
                .insert("123".to_string(), "gpt-5.4".to_string());
            save_bot_settings(token, &settings);

            let raw = fs::read_to_string(&path).unwrap();
            let saved: serde_json::Value = serde_json::from_str(&raw).unwrap();
            let entry = saved.get(&key).unwrap();
            assert_eq!(
                entry.get("token").and_then(|value| value.as_str()),
                Some(token)
            );
            assert_eq!(
                entry.get("provider").and_then(|value| value.as_str()),
                Some("codex")
            );
            assert_eq!(
                entry.get("agent").and_then(|value| value.as_str()),
                Some("codex")
            );
            assert_eq!(
                entry
                    .get("allowed_channel_ids")
                    .and_then(|value| value.as_array())
                    .map(|ids| ids.len()),
                Some(1)
            );
            assert_eq!(
                entry.get("owner_user_id").and_then(|value| value.as_u64()),
                Some(7)
            );
            assert_eq!(
                entry
                    .get("channel_model_overrides")
                    .and_then(|value| value.get("123"))
                    .and_then(|value| value.as_str()),
                Some("gpt-5.4")
            );

            let configs = load_discord_bot_launch_configs();
            assert_eq!(configs.len(), 1);
            assert_eq!(configs[0].token, token);
            assert_eq!(configs[0].provider, ProviderKind::Codex);
        });
    }

    #[test]
    fn test_load_bot_settings_reads_allow_all_users() {
        with_temp_home(|temp_home: &TempDir| {
            let settings_dir = temp_home.path().join(".adk").join("config");
            fs::create_dir_all(&settings_dir).unwrap();
            let token = "test-token";
            let key = discord_token_hash(token);
            let json = serde_json::json!({
                key: {
                    "token": token,
                    "allow_all_users": true
                }
            });
            fs::write(
                settings_dir.join("bot_settings.json"),
                serde_json::to_string_pretty(&json).unwrap(),
            )
            .unwrap();

            let settings = load_bot_settings(token);
            assert!(settings.allow_all_users);
        });
    }

    #[test]
    fn test_save_bot_settings_persists_allow_all_users() {
        with_temp_home(|temp_home: &TempDir| {
            let token = "test-token";
            write_agentdesk_yaml(
                temp_home,
                &format!(
                    "server:\n  port: 8791\ndiscord:\n  bots:\n    command:\n      token: \"{token}\"\n"
                ),
            );
            let mut settings = super::super::DiscordBotSettings::default();
            settings.allow_all_users = true;

            save_bot_settings(token, &settings);

            let loaded = load_bot_settings(token);
            assert!(loaded.allow_all_users);
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
        with_temp_home(|temp_home: &TempDir| {
            let token = "test-token";
            write_agentdesk_yaml(
                temp_home,
                &format!(
                    "server:\n  port: 8791\ndiscord:\n  bots:\n    command:\n      token: \"{token}\"\n"
                ),
            );
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
    fn test_list_registered_channel_bindings_falls_back_to_role_map_when_org_has_no_by_id() {
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
            fs::write(
                settings_dir.join("org.yaml"),
                r#"
version: 1
name: AgentDesk
agents:
  codex:
    display_name: Codex
    provider: codex
channels:
  by_name:
    enabled: true
    mappings:
      test-channel:
        agent: codex
"#,
            )
            .unwrap();

            let bindings = list_registered_channel_bindings();
            assert_eq!(bindings.len(), 1);
            assert_eq!(bindings[0].channel_id, 123);
            assert_eq!(bindings[0].owner_provider, ProviderKind::Codex);
        });
    }

    #[test]
    fn test_list_registered_channel_bindings_merges_org_and_role_map_with_org_precedence() {
        with_temp_home(|temp_home: &TempDir| {
            let settings_dir = temp_home.path().join(".adk").join("config");
            fs::create_dir_all(&settings_dir).unwrap();
            fs::write(
                settings_dir.join("role_map.json"),
                serde_json::to_string_pretty(&serde_json::json!({
                    "version": 1,
                    "byChannelId": {
                        "123": {
                            "roleId": "legacy-codex",
                            "promptFile": "/tmp/legacy-codex.prompt.md",
                            "provider": "codex"
                        },
                        "456": {
                            "roleId": "legacy-claude",
                            "promptFile": "/tmp/legacy-claude.prompt.md",
                            "provider": "claude"
                        }
                    }
                }))
                .unwrap(),
            )
            .unwrap();
            fs::write(
                settings_dir.join("org.yaml"),
                r#"
version: 1
name: AgentDesk
agents:
  org-gemini:
    display_name: Org Gemini
    provider: gemini
  org-codex:
    display_name: Org Codex
    provider: codex
channels:
  by_id:
    "123":
      agent: org-gemini
    "789":
      agent: org-codex
"#,
            )
            .unwrap();

            let bindings = list_registered_channel_bindings();
            assert_eq!(bindings.len(), 3);
            assert_eq!(
                bindings
                    .iter()
                    .map(|binding| (binding.channel_id, binding.owner_provider.clone()))
                    .collect::<Vec<_>>(),
                vec![
                    (123, ProviderKind::Gemini),
                    (456, ProviderKind::Claude),
                    (789, ProviderKind::Codex),
                ]
            );
        });
    }

    #[test]
    fn test_list_registered_channel_bindings_includes_agentdesk_with_highest_precedence() {
        with_temp_home(|temp_home: &TempDir| {
            let settings_dir = temp_home.path().join(".adk").join("config");
            fs::create_dir_all(&settings_dir).unwrap();
            fs::write(
                settings_dir.join("role_map.json"),
                serde_json::to_string_pretty(&serde_json::json!({
                    "version": 1,
                    "byChannelId": {
                        "123": {
                            "roleId": "legacy-codex",
                            "promptFile": "/tmp/legacy-codex.prompt.md",
                            "provider": "codex"
                        },
                        "456": {
                            "roleId": "legacy-claude",
                            "promptFile": "/tmp/legacy-claude.prompt.md",
                            "provider": "claude"
                        }
                    }
                }))
                .unwrap(),
            )
            .unwrap();
            fs::write(
                settings_dir.join("org.yaml"),
                r#"
version: 1
name: AgentDesk
agents:
  org-gemini:
    display_name: Org Gemini
    provider: gemini
channels:
  by_id:
    "123":
      agent: org-gemini
    "789":
      agent: org-gemini
"#,
            )
            .unwrap();
            write_agentdesk_yaml(
                temp_home,
                r#"
server:
  port: 8791
discord:
  bots: {}
agents:
  - id: project-agentdesk
    name: "AgentDesk"
    provider: codex
    channels:
      codex:
        id: "123"
        name: "adk-cdx"
  - id: project-claude
    name: "Claude Agent"
    provider: claude
    channels:
      claude:
        id: "999"
        name: "adk-cc"
"#,
            );

            let bindings = list_registered_channel_bindings();
            assert_eq!(
                bindings
                    .iter()
                    .map(|binding| (binding.channel_id, binding.owner_provider.clone()))
                    .collect::<Vec<_>>(),
                vec![
                    (123, ProviderKind::Codex),
                    (456, ProviderKind::Claude),
                    (789, ProviderKind::Gemini),
                    (999, ProviderKind::Claude),
                ]
            );
            assert_eq!(
                bindings
                    .iter()
                    .find(|binding| binding.channel_id == 123)
                    .and_then(|binding| binding.fallback_name.as_deref()),
                Some("adk-cdx")
            );
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
            assert!(rendered.contains("Name 1-2 peer agents"));
            assert!(rendered.contains("agentdesk send-to-agent"));
            let removed_skill = ["send", "agent", "message"].join("-");
            assert!(!rendered.contains(&removed_skill));
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
            quality_feedback_injection_enabled: true,
            memory: Default::default(),
        };
        let spark_binding = super::RoleBinding {
            role_id: "spark".to_string(),
            prompt_file: "/tmp/spark.md".to_string(),
            provider: Some(ProviderKind::Codex),
            model: None,
            reasoning_effort: None,
            peer_agents_enabled: false,
            quality_feedback_injection_enabled: true,
            memory: Default::default(),
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
    fn test_has_configured_channel_binding_requires_matching_explicit_channel_id() {
        with_temp_home(|temp_home: &TempDir| {
            write_agentdesk_yaml(
                temp_home,
                r#"
server:
  port: 8791
agents:
  - id: project-agentdesk
    name: "AgentDesk"
    provider: claude
    channels:
      claude:
        id: "1484070499783803081"
        name: "adk-cc"
        aliases: ["agentdesk-cc"]
"#,
            );

            assert!(super::has_configured_channel_binding(
                ChannelId::new(1484070499783803081),
                Some("adk-cc"),
            ));
            assert!(!super::has_configured_channel_binding(
                ChannelId::new(1479671298497183835),
                Some("adk-cc"),
            ));
        });
    }

    #[test]
    fn test_has_configured_channel_binding_ignores_org_by_name_fallback_for_ownership() {
        with_temp_home(|temp_home: &TempDir| {
            let settings_dir = temp_home.path().join(".adk").join("config");
            fs::create_dir_all(&settings_dir).unwrap();
            fs::write(
                settings_dir.join("org.yaml"),
                r#"
version: 1
name: AgentDesk
agents:
  codex:
    display_name: Codex
    provider: codex
channels:
  by_name:
    enabled: true
    mappings:
      agentdesk-codex:
        agent: codex
"#,
            )
            .unwrap();

            assert!(!super::has_configured_channel_binding(
                ChannelId::new(1486017489027469493),
                Some("agentdesk-codex"),
            ));
        });
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
    fn test_validate_bot_channel_routing_with_provider_channel_keeps_thread_binding() {
        with_temp_home(|temp_home: &TempDir| {
            let settings_dir = temp_home.path().join(".adk").join("config");
            fs::create_dir_all(&settings_dir).unwrap();
            fs::write(
                settings_dir.join("org.yaml"),
                r#"
version: 1
name: "Test Org"
agents:
  openclaw-maker:
    display_name: "Maker"
    provider: codex
channels:
  by_id:
    '1470034105176424533':
      agent: openclaw-maker
      provider: codex
"#,
            )
            .unwrap();

            let mut settings = super::super::DiscordBotSettings::default();
            settings.provider = ProviderKind::Codex;
            settings.agent = Some("openclaw-maker".to_string());
            settings.allowed_channel_ids = vec![1470034105176424533];

            let result = validate_bot_channel_routing_with_provider_channel(
                &settings,
                &ProviderKind::Codex,
                ChannelId::new(1470034105176424533),
                Some("openclaw-maker-thread"),
                Some("agent-sandbox-lab"),
                false,
            );

            assert_eq!(result, Ok(()));

            let parent_result = validate_bot_channel_routing(
                &settings,
                &ProviderKind::Codex,
                ChannelId::new(1470643507201839189),
                Some("agent-sandbox-lab"),
                false,
            );

            assert_eq!(
                parent_result,
                Err(BotChannelRoutingGuardFailure::ChannelNotAllowed)
            );
        });
    }

    #[test]
    fn test_validate_bot_channel_routing_with_provider_channel_ignores_thread_name_binding() {
        with_temp_home(|temp_home: &TempDir| {
            let settings_dir = temp_home.path().join(".adk").join("config");
            fs::create_dir_all(&settings_dir).unwrap();
            fs::write(
                settings_dir.join("org.yaml"),
                r#"
version: 1
name: "Test Org"
agents:
  privileged-agent:
    display_name: "Privileged"
    provider: codex
channels:
  by_name:
    enabled: true
    mappings:
      privileged-thread:
        agent: privileged-agent
"#,
            )
            .unwrap();

            let mut settings = super::super::DiscordBotSettings::default();
            settings.provider = ProviderKind::Codex;
            settings.agent = Some("privileged-agent".to_string());
            settings.allowed_channel_ids = vec![1470034105176424533];

            let result = validate_bot_channel_routing_with_provider_channel(
                &settings,
                &ProviderKind::Codex,
                ChannelId::new(1470034105176424533),
                Some("privileged-thread"),
                Some("team-general"),
                false,
            );

            assert_eq!(result, Err(BotChannelRoutingGuardFailure::AgentMismatch));
        });
    }

    #[test]
    fn test_cross_bot_skip_classification_only_hides_expected_misses() {
        assert!(BotChannelRoutingGuardFailure::ChannelNotAllowed.is_expected_cross_bot_skip());
        assert!(BotChannelRoutingGuardFailure::AgentMismatch.is_expected_cross_bot_skip());
        assert!(!BotChannelRoutingGuardFailure::ProviderMismatch.is_expected_cross_bot_skip());
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
            quality_feedback_injection_enabled: true,
            memory: Default::default(),
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
