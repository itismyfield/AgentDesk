use std::collections::BTreeMap;
use std::sync::{LazyLock, RwLock};

use crate::config::{Config, default_provider_tui_hosting};
use crate::services::provider::ProviderKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderSessionDriver {
    LegacyPrompt,
    TuiHosting,
}

impl ProviderSessionDriver {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LegacyPrompt => "legacy-prompt",
            Self::TuiHosting => "tui-hosting",
        }
    }
}

/// Parsed configuration value; retired names never select an execution driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeMode {
    Pipe,
    Tui,
    Retired,
}

impl RuntimeMode {
    fn tui_request(self) -> Option<bool> {
        match self {
            Self::Pipe => Some(false),
            Self::Tui => Some(true),
            Self::Retired => None,
        }
    }

    /// Unknown values retain the legacy fallback; removed aliases remain distinguishable.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "pipe" | "legacy" | "legacy_prompt" | "legacy-prompt" => Some(Self::Pipe),
            "tui" | "tui_hosting" | "tui-hosting" => Some(Self::Tui),
            "claude-e" | "claude_e" => Some(Self::Retired),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderSessionSelection {
    pub provider_id: String,
    pub requested_tui_hosting: bool,
    pub driver: ProviderSessionDriver,
    pub fallback_reason: Option<&'static str>,
}

impl ProviderSessionSelection {
    pub fn log_start(&self, entrypoint: &'static str) {
        if let Some(fallback_reason) = self.fallback_reason {
            tracing::info!(
                provider = self.provider_id,
                requested_tui_hosting = self.requested_tui_hosting,
                selected_driver = self.driver.as_str(),
                fallback_reason,
                entrypoint,
                "provider tui_hosting requested but unavailable; using legacy prompt driver"
            );
        } else {
            tracing::info!(
                provider = self.provider_id,
                requested_tui_hosting = self.requested_tui_hosting,
                selected_driver = self.driver.as_str(),
                entrypoint,
                "provider session driver resolved"
            );
        }
    }
}

static PROVIDER_TUI_HOSTING: LazyLock<RwLock<BTreeMap<String, bool>>> =
    LazyLock::new(|| RwLock::new(BTreeMap::new()));
static PROVIDER_TUI_HOSTING_BY_CHANNEL: LazyLock<RwLock<BTreeMap<(String, u64), bool>>> =
    LazyLock::new(|| RwLock::new(BTreeMap::new()));

/// Explicit supported runtime choices, taking precedence over legacy `tui_hosting`.
static PROVIDER_RUNTIME_MODE: LazyLock<RwLock<BTreeMap<String, bool>>> =
    LazyLock::new(|| RwLock::new(BTreeMap::new()));
static PROVIDER_RUNTIME_MODE_BY_CHANNEL: LazyLock<RwLock<BTreeMap<(String, u64), bool>>> =
    LazyLock::new(|| RwLock::new(BTreeMap::new()));

/// Issue #2193 — runtime mirror of `providers.codex.remote_ssh_enabled`.
/// Defaults to `false`; the bootstrap step hard-fails before this can be
/// flipped on without the ADR prerequisites in place.
static CODEX_REMOTE_SSH_ENABLED: LazyLock<RwLock<bool>> = LazyLock::new(|| RwLock::new(false));

/// Serializes tests that install the process-wide hosting config.
#[cfg(test)]
pub(crate) static TEST_CONFIG_LOCK: LazyLock<std::sync::Mutex<()>> =
    LazyLock::new(Default::default);

pub fn install_provider_hosting_config(config: &Config) {
    let mut values = BTreeMap::new();
    for provider_id in crate::services::provider::supported_provider_ids() {
        values.insert(
            provider_id.to_string(),
            config.provider_tui_hosting_enabled(provider_id),
        );
    }
    // Preserve explicit unknown-provider keys for diagnostics and future
    // ProviderKind::Unsupported flows; registered ids are a harmless overwrite.
    for (provider_id, provider_config) in &config.providers {
        if let Some(enabled) = provider_config.tui_hosting {
            values.insert(provider_id.trim().to_ascii_lowercase(), enabled);
        }
    }
    let mut channel_values = BTreeMap::new();
    let mut runtime_mode_values: BTreeMap<String, bool> = BTreeMap::new();
    let mut runtime_mode_channel_values: BTreeMap<(String, u64), bool> = BTreeMap::new();
    // Only supported choices enter the execution mirrors. Strict loading rejects retired names.
    for (provider_id, provider_config) in &config.providers {
        if let Some(raw) = provider_config.runtime.as_deref() {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            match RuntimeMode::parse(trimmed) {
                Some(mode) => {
                    if let Some(requested) = mode.tui_request() {
                        runtime_mode_values
                            .insert(provider_id.trim().to_ascii_lowercase(), requested);
                    }
                }
                None => {
                    tracing::warn!(
                        provider = %provider_id,
                        runtime = trimmed,
                        "providers.<provider>.runtime not recognised; \
                         falling back to tui_hosting derivation"
                    );
                }
            }
        }
    }
    for agent in &config.agents {
        for (channel_kind, channel) in agent.channels.iter() {
            let Some(channel_id) = channel
                .channel_id()
                .and_then(|value| value.parse::<u64>().ok())
            else {
                continue;
            };
            let provider_id = channel
                .provider()
                .unwrap_or_else(|| channel_kind.to_string())
                .trim()
                .to_ascii_lowercase();
            if let Some(enabled) = channel.tui_hosting() {
                channel_values.insert((provider_id.clone(), channel_id), enabled);
            }
            // Channel runtime overrides provider runtime and the legacy boolean.
            if let Some(raw) = channel.runtime_mode_raw() {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match RuntimeMode::parse(trimmed) {
                    Some(mode) => {
                        if let Some(requested) = mode.tui_request() {
                            runtime_mode_channel_values
                                .insert((provider_id, channel_id), requested);
                        }
                    }
                    None => {
                        tracing::warn!(
                            provider = %provider_id,
                            channel_id,
                            runtime = trimmed,
                            "channel runtime override not recognised; \
                             falling back to tui_hosting derivation"
                        );
                    }
                }
            }
        }
    }

    let summary = values
        .iter()
        .map(|(provider, enabled)| format!("{provider}={enabled}"))
        .collect::<Vec<_>>()
        .join(",");
    *PROVIDER_TUI_HOSTING
        .write()
        .unwrap_or_else(|error| error.into_inner()) = values;
    tracing::info!(summary, "provider tui_hosting config installed");
    let channel_summary = channel_values
        .iter()
        .map(|((provider, channel_id), enabled)| format!("{provider}:{channel_id}={enabled}"))
        .collect::<Vec<_>>()
        .join(",");
    *PROVIDER_TUI_HOSTING_BY_CHANNEL
        .write()
        .unwrap_or_else(|error| error.into_inner()) = channel_values;
    tracing::info!(
        channel_summary,
        "provider per-channel tui_hosting config installed"
    );

    let runtime_mode_summary = runtime_mode_values
        .iter()
        .map(|(provider, mode)| format!("{provider}={mode}"))
        .collect::<Vec<_>>()
        .join(",");
    *PROVIDER_RUNTIME_MODE
        .write()
        .unwrap_or_else(|error| error.into_inner()) = runtime_mode_values;
    tracing::info!(
        summary = runtime_mode_summary,
        "provider runtime_mode config installed"
    );
    let runtime_mode_channel_summary = runtime_mode_channel_values
        .iter()
        .map(|((provider, channel_id), mode)| format!("{provider}:{channel_id}={mode}"))
        .collect::<Vec<_>>()
        .join(",");
    *PROVIDER_RUNTIME_MODE_BY_CHANNEL
        .write()
        .unwrap_or_else(|error| error.into_inner()) = runtime_mode_channel_values;
    tracing::info!(
        channel_summary = runtime_mode_channel_summary,
        "provider per-channel runtime_mode config installed"
    );

    // Issue #2193 — mirror the codex remote SSH gate into a runtime cell
    // the dispatch path can read without re-parsing the full Config.
    let remote_ssh = config.codex_remote_ssh_enabled();
    *CODEX_REMOTE_SSH_ENABLED
        .write()
        .unwrap_or_else(|error| error.into_inner()) = remote_ssh;
    tracing::info!(
        codex_remote_ssh_enabled = remote_ssh,
        "codex remote SSH gate runtime mirror installed"
    );
}

/// Issue #2193 — runtime read of the codex remote SSH gate.
///
/// `services::codex::execute_command_streaming` calls this on every
/// dispatch where `remote_profile.is_some()`. Together with
/// `services::codex_remote_policy::PREREQUISITES_SATISFIED`, this is
/// the second line of defense beyond the bootstrap hard-fail.
pub fn codex_remote_ssh_enabled() -> bool {
    *CODEX_REMOTE_SSH_ENABLED
        .read()
        .unwrap_or_else(|error| error.into_inner())
}

pub fn resolve_provider_session_selection(provider: &ProviderKind) -> ProviderSessionSelection {
    resolve_provider_session_selection_with_capability(provider, true)
}

pub fn resolve_provider_session_selection_with_capability(
    provider: &ProviderKind,
    entrypoint_supports_tui_hosting: bool,
) -> ProviderSessionSelection {
    resolve_provider_session_selection_with_channel(provider, entrypoint_supports_tui_hosting, None)
}

pub fn resolve_provider_session_selection_with_channel(
    provider: &ProviderKind,
    entrypoint_supports_tui_hosting: bool,
    channel_id: Option<u64>,
) -> ProviderSessionSelection {
    let provider_id = provider.as_str().to_ascii_lowercase();
    let provider_default = PROVIDER_TUI_HOSTING
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .get(&provider_id)
        .copied()
        .unwrap_or_else(|| default_provider_tui_hosting(&provider_id));
    let tui_hosting_from_legacy = channel_id
        .and_then(|channel_id| {
            PROVIDER_TUI_HOSTING_BY_CHANNEL
                .read()
                .unwrap_or_else(|error| error.into_inner())
                .get(&(provider_id.clone(), channel_id))
                .copied()
        })
        .unwrap_or(provider_default);

    // Explicit channel runtime wins over provider runtime, then legacy hosting applies.
    let explicit_runtime_mode = channel_id
        .and_then(|cid| {
            PROVIDER_RUNTIME_MODE_BY_CHANNEL
                .read()
                .unwrap_or_else(|error| error.into_inner())
                .get(&(provider_id.clone(), cid))
                .copied()
        })
        .or_else(|| {
            PROVIDER_RUNTIME_MODE
                .read()
                .unwrap_or_else(|error| error.into_inner())
                .get(&provider_id)
                .copied()
        });

    let requested_tui_hosting = explicit_runtime_mode.unwrap_or(tui_hosting_from_legacy);
    let (driver, fallback_reason) = if !requested_tui_hosting {
        (ProviderSessionDriver::LegacyPrompt, None)
    } else if !provider_tui_hosting_driver_available(provider) {
        (
            ProviderSessionDriver::LegacyPrompt,
            Some("tui_hosting_driver_unavailable"),
        )
    } else if entrypoint_supports_tui_hosting {
        (ProviderSessionDriver::TuiHosting, None)
    } else {
        (
            ProviderSessionDriver::LegacyPrompt,
            Some("entrypoint_not_tui_hosted"),
        )
    };
    ProviderSessionSelection {
        provider_id,
        requested_tui_hosting,
        driver,
        fallback_reason,
    }
}

/// Explicit supported runtime requests also determine whether boot publishes TUI hooks.
fn provider_runtime_mode_explicit(config: &Config, provider_id: &str) -> Option<bool> {
    provider_setting(config, provider_id, |provider| {
        provider
            .runtime
            .as_deref()
            .and_then(RuntimeMode::parse)
            .and_then(RuntimeMode::tui_request)
    })
}

/// A provider's `tui_hosting`, else the provider default, read as the installed resolver reads it.
fn provider_tui_hosting(config: &Config, provider_id: &str) -> bool {
    provider_setting(config, provider_id, |provider| provider.tui_hosting)
        .unwrap_or_else(|| default_provider_tui_hosting(&provider_id.trim().to_ascii_lowercase()))
}

/// `install_provider_hosting_config`'s lookup: keys match trimmed and case-folded, and the last
/// matching entry in key order that sets the field wins.
fn provider_setting<T>(
    config: &Config,
    provider_id: &str,
    field: impl Fn(&crate::config::ProviderConfig) -> Option<T>,
) -> Option<T> {
    let provider_id = provider_id.trim().to_ascii_lowercase();
    let matching = config.providers.iter();
    let matching = matching.filter(|(key, _)| key.trim().to_ascii_lowercase() == provider_id);
    matching.filter_map(|(_, provider)| field(provider)).last()
}

fn channel_runtime_mode_explicit(channel: &crate::config::AgentChannel) -> Option<bool> {
    channel
        .runtime_mode_raw()
        .as_deref()
        .and_then(|raw| RuntimeMode::parse(raw.trim()))
        .and_then(RuntimeMode::tui_request)
}

/// Match the installed resolver precedence so hook publication agrees with driver selection.
pub(crate) fn channel_effective_tui_request(
    config: &Config,
    channel: &crate::config::AgentChannel,
    provider_id: &str,
) -> bool {
    if let Some(mode) = channel_runtime_mode_explicit(channel) {
        return mode;
    }
    if let Some(mode) = provider_runtime_mode_explicit(config, provider_id) {
        return mode;
    }
    if let Some(legacy) = channel.tui_hosting() {
        return legacy;
    }
    provider_tui_hosting(config, provider_id)
}

fn provider_default_effective_tui_request(config: &Config, provider_id: &str) -> bool {
    if let Some(mode) = provider_runtime_mode_explicit(config, provider_id) {
        return mode;
    }
    provider_tui_hosting(config, provider_id)
}

pub fn any_requested_tui_hosting_driver_available(config: &Config) -> bool {
    let provider_level_request = crate::services::provider::supported_provider_ids()
        .iter()
        .any(|provider_id| {
            provider_default_effective_tui_request(config, provider_id)
                && ProviderKind::from_str(provider_id)
                    .as_ref()
                    .is_some_and(provider_tui_hosting_driver_available)
        });

    if provider_level_request {
        return true;
    }

    config.agents.iter().any(|agent| {
        agent.channels.iter().any(|(channel_kind, channel)| {
            let provider_id = channel
                .provider()
                .unwrap_or_else(|| channel_kind.to_string())
                .trim()
                .to_ascii_lowercase();
            if !channel_effective_tui_request(config, channel, &provider_id) {
                return false;
            }
            ProviderKind::from_str(&provider_id)
                .as_ref()
                .is_some_and(provider_tui_hosting_driver_available)
        })
    })
}

pub fn provider_tui_hosting_driver_available(provider: &ProviderKind) -> bool {
    matches!(provider, ProviderKind::Claude | ProviderKind::Codex)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AgentChannel, AgentChannelConfig, AgentChannels, AgentDef, AgentVoiceConfig, Config,
        ProviderConfig,
    };

    #[test]
    fn defaults_request_tui_for_claude_only() {
        let config = Config::default();

        assert!(config.provider_tui_hosting_enabled("claude"));
        assert!(!config.provider_tui_hosting_enabled("codex"));
        assert!(!config.provider_tui_hosting_enabled("qwen"));
        assert!(!config.provider_tui_hosting_enabled("gemini"));
        assert!(!config.provider_tui_hosting_enabled("opencode"));
    }

    #[test]
    fn explicit_provider_config_overrides_default() {
        let mut config = Config::default();
        config.providers.insert(
            "claude".to_string(),
            ProviderConfig {
                tui_hosting: Some(false),
                ..ProviderConfig::default()
            },
        );
        config.providers.insert(
            "qwen".to_string(),
            ProviderConfig {
                tui_hosting: Some(true),
                ..ProviderConfig::default()
            },
        );

        assert!(!config.provider_tui_hosting_enabled("claude"));
        assert!(config.provider_tui_hosting_enabled("qwen"));
    }

    #[test]
    fn requested_tui_selects_claude_driver_when_available() {
        let _guard = TEST_CONFIG_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        install_provider_hosting_config(&Config::default());

        let selection = resolve_provider_session_selection(&ProviderKind::Claude);

        assert!(selection.requested_tui_hosting);
        assert_eq!(selection.driver, ProviderSessionDriver::TuiHosting);
        assert_eq!(selection.fallback_reason, None);
    }

    #[test]
    fn requested_tui_selects_codex_driver_when_available() {
        let _guard = TEST_CONFIG_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut config = Config::default();
        config.providers.insert(
            "codex".to_string(),
            ProviderConfig {
                tui_hosting: Some(true),
                ..ProviderConfig::default()
            },
        );
        install_provider_hosting_config(&config);

        let selection = resolve_provider_session_selection(&ProviderKind::Codex);

        assert!(selection.requested_tui_hosting);
        assert_eq!(selection.driver, ProviderSessionDriver::TuiHosting);
        assert_eq!(selection.fallback_reason, None);
    }

    #[test]
    fn codex_defaults_to_legacy_prompt_driver() {
        let _guard = TEST_CONFIG_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        install_provider_hosting_config(&Config::default());

        let selection = resolve_provider_session_selection(&ProviderKind::Codex);

        assert!(!selection.requested_tui_hosting);
        assert_eq!(selection.driver, ProviderSessionDriver::LegacyPrompt);
        assert_eq!(selection.fallback_reason, None);
    }

    #[test]
    fn channel_override_can_enable_tui_when_provider_default_is_disabled() {
        let _guard = TEST_CONFIG_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut config = Config::default();
        config.providers.insert(
            "claude".to_string(),
            ProviderConfig {
                tui_hosting: Some(false),
                ..ProviderConfig::default()
            },
        );
        config.agents.push(test_agent_with_claude_channel(
            "1506295332949196840",
            Some(true),
        ));
        install_provider_hosting_config(&config);

        let selected_channel = resolve_provider_session_selection_with_channel(
            &ProviderKind::Claude,
            true,
            Some(1506295332949196840),
        );
        assert!(selected_channel.requested_tui_hosting);
        assert_eq!(selected_channel.driver, ProviderSessionDriver::TuiHosting);

        let other_channel =
            resolve_provider_session_selection_with_channel(&ProviderKind::Claude, true, Some(42));
        assert!(!other_channel.requested_tui_hosting);
        assert_eq!(other_channel.driver, ProviderSessionDriver::LegacyPrompt);

        let no_channel = resolve_provider_session_selection(&ProviderKind::Claude);
        assert!(!no_channel.requested_tui_hosting);
        assert_eq!(no_channel.driver, ProviderSessionDriver::LegacyPrompt);
        assert!(any_requested_tui_hosting_driver_available(&config));
    }

    #[test]
    fn channel_override_can_disable_tui_when_provider_default_is_enabled() {
        let _guard = TEST_CONFIG_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut config = Config::default();
        config.agents.push(test_agent_with_claude_channel(
            "1506295332949196840",
            Some(false),
        ));
        install_provider_hosting_config(&config);

        let selected_channel = resolve_provider_session_selection_with_channel(
            &ProviderKind::Claude,
            true,
            Some(1506295332949196840),
        );
        assert!(!selected_channel.requested_tui_hosting);
        assert_eq!(selected_channel.driver, ProviderSessionDriver::LegacyPrompt);

        let other_channel =
            resolve_provider_session_selection_with_channel(&ProviderKind::Claude, true, Some(42));
        assert!(other_channel.requested_tui_hosting);
        assert_eq!(other_channel.driver, ProviderSessionDriver::TuiHosting);
    }

    #[test]
    fn unsupported_entrypoint_falls_back_even_when_claude_driver_exists() {
        let _guard = TEST_CONFIG_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        install_provider_hosting_config(&Config::default());

        let selection =
            resolve_provider_session_selection_with_capability(&ProviderKind::Claude, false);

        assert!(selection.requested_tui_hosting);
        assert_eq!(selection.driver, ProviderSessionDriver::LegacyPrompt);
        assert_eq!(selection.fallback_reason, Some("entrypoint_not_tui_hosted"));
    }

    #[test]
    fn requested_provider_has_available_claude_driver() {
        let config = Config::default();

        assert!(any_requested_tui_hosting_driver_available(&config));
    }

    #[test]
    fn requested_non_claude_tui_still_falls_back_until_driver_exists() {
        let _guard = TEST_CONFIG_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut config = Config::default();
        config.providers.insert(
            "claude".to_string(),
            ProviderConfig {
                tui_hosting: Some(false),
                ..ProviderConfig::default()
            },
        );
        config.providers.insert(
            "qwen".to_string(),
            ProviderConfig {
                tui_hosting: Some(true),
                ..ProviderConfig::default()
            },
        );
        install_provider_hosting_config(&config);

        let selection = resolve_provider_session_selection(&ProviderKind::Qwen);

        assert!(selection.requested_tui_hosting);
        assert_eq!(selection.driver, ProviderSessionDriver::LegacyPrompt);
        assert_eq!(
            selection.fallback_reason,
            Some("tui_hosting_driver_unavailable")
        );
    }

    // Issue #2193 — gate mirror defaults to false and only flips when
    // the operator explicitly sets the flag in `providers.codex`.
    #[test]
    fn codex_remote_ssh_gate_defaults_off() {
        let _guard = TEST_CONFIG_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        install_provider_hosting_config(&Config::default());

        assert!(!codex_remote_ssh_enabled());
    }

    #[test]
    fn codex_remote_ssh_gate_mirrors_explicit_true() {
        let _guard = TEST_CONFIG_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut config = Config::default();
        config.providers.insert(
            "codex".to_string(),
            ProviderConfig {
                remote_ssh_enabled: Some(true),
                ..ProviderConfig::default()
            },
        );
        install_provider_hosting_config(&config);

        assert!(codex_remote_ssh_enabled());

        // Reset for other tests.
        install_provider_hosting_config(&Config::default());
        assert!(!codex_remote_ssh_enabled());
    }

    #[test]
    fn runtime_mode_parse_accepts_canonical_and_aliases() {
        assert_eq!(RuntimeMode::parse("pipe"), Some(RuntimeMode::Pipe));
        assert_eq!(RuntimeMode::parse("Pipe"), Some(RuntimeMode::Pipe));
        assert_eq!(RuntimeMode::parse("legacy"), Some(RuntimeMode::Pipe));
        assert_eq!(RuntimeMode::parse("tui"), Some(RuntimeMode::Tui));
        assert_eq!(RuntimeMode::parse("tui_hosting"), Some(RuntimeMode::Tui));
        assert_eq!(RuntimeMode::parse("claude-e"), Some(RuntimeMode::Retired));
        assert_eq!(RuntimeMode::parse("Claude-E"), Some(RuntimeMode::Retired));
        assert_eq!(RuntimeMode::parse("claude_e"), Some(RuntimeMode::Retired));
        assert_eq!(RuntimeMode::parse(""), None);
        assert_eq!(RuntimeMode::parse("bogus"), None);
        // Phase 0 counter-review MINOR 4: typo variants must trigger the
        // warn-and-fallback path, not silent acceptance.
        assert_eq!(RuntimeMode::parse("claudee"), None);
        assert_eq!(RuntimeMode::parse("ClaudeE"), None);
    }

    #[test]
    fn provider_runtime_pipe_overrides_tui_hosting_true() {
        let _guard = TEST_CONFIG_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut config = Config::default();
        config.providers.insert(
            "claude".to_string(),
            ProviderConfig {
                tui_hosting: Some(true),
                runtime: Some("pipe".to_string()),
                ..ProviderConfig::default()
            },
        );
        install_provider_hosting_config(&config);

        let selection = resolve_provider_session_selection(&ProviderKind::Claude);

        assert!(!selection.requested_tui_hosting);
        assert_eq!(selection.driver, ProviderSessionDriver::LegacyPrompt);
        assert_eq!(selection.fallback_reason, None);

        install_provider_hosting_config(&Config::default());
    }

    #[test]
    fn provider_runtime_tui_overrides_tui_hosting_false() {
        let _guard = TEST_CONFIG_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut config = Config::default();
        config.providers.insert(
            "claude".to_string(),
            ProviderConfig {
                tui_hosting: Some(false),
                runtime: Some("tui".to_string()),
                ..ProviderConfig::default()
            },
        );
        install_provider_hosting_config(&config);

        let selection = resolve_provider_session_selection(&ProviderKind::Claude);

        assert!(selection.requested_tui_hosting);
        assert_eq!(selection.driver, ProviderSessionDriver::TuiHosting);
        assert_eq!(selection.fallback_reason, None);

        install_provider_hosting_config(&Config::default());
    }

    #[test]
    fn channel_runtime_overrides_provider_runtime() {
        let _guard = TEST_CONFIG_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut config = Config::default();
        config.providers.insert(
            "claude".to_string(),
            ProviderConfig {
                tui_hosting: Some(true),
                runtime: Some("tui".to_string()),
                ..ProviderConfig::default()
            },
        );
        config.agents.push(test_agent_with_claude_channel_runtime(
            "1506295332949196840",
            Some("pipe"),
        ));
        install_provider_hosting_config(&config);

        let selected_channel = resolve_provider_session_selection_with_channel(
            &ProviderKind::Claude,
            true,
            Some(1506295332949196840),
        );
        assert!(!selected_channel.requested_tui_hosting);
        assert_eq!(selected_channel.driver, ProviderSessionDriver::LegacyPrompt);
        assert_eq!(selected_channel.fallback_reason, None);

        // Without channel override, provider-level `runtime: tui` is honoured.
        let other_channel =
            resolve_provider_session_selection_with_channel(&ProviderKind::Claude, true, Some(42));
        assert!(other_channel.requested_tui_hosting);
        assert_eq!(other_channel.driver, ProviderSessionDriver::TuiHosting);

        install_provider_hosting_config(&Config::default());
    }

    #[test]
    fn any_requested_tui_hosting_picks_up_explicit_runtime_tui() {
        // Phase 0 counter-review MAJOR-2: an operator who only sets
        // `runtime: tui` (without the legacy `tui_hosting` boolean) must
        // still cause the hook endpoint to be published at boot.
        let _guard = TEST_CONFIG_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut config = Config::default();
        // Wipe the Claude default so we are not relying on the per-provider
        // default returning `true` for Claude.
        config.providers.insert(
            "claude".to_string(),
            ProviderConfig {
                tui_hosting: Some(false),
                runtime: Some("tui".to_string()),
                ..ProviderConfig::default()
            },
        );

        // `tui_hosting: false` alone would short-circuit to `false`; the new
        // resolver must observe `runtime: tui` and return `true` regardless.
        assert!(any_requested_tui_hosting_driver_available(&config));

        install_provider_hosting_config(&Config::default());
    }

    #[test]
    fn any_requested_tui_hosting_provider_runtime_pipe_overrides_channel_tui_hosting_legacy() {
        // Phase 0 counter-review round 2: a provider with explicit
        // `runtime: pipe` must suppress the hook endpoint even when a
        // channel has the legacy `tui_hosting: true` boolean set, because
        // the resolver would route that channel through LegacyPrompt.
        // Without this fix the hook predicate and the resolver disagree.
        let _guard = TEST_CONFIG_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut config = Config::default();
        config.providers.insert(
            "claude".to_string(),
            ProviderConfig {
                tui_hosting: Some(true),
                runtime: Some("pipe".to_string()),
                ..ProviderConfig::default()
            },
        );
        config.agents.push(test_agent_with_claude_channel(
            "1506295332949196840",
            Some(true),
        ));

        // Provider-level `runtime: pipe` wins for every channel that does
        // not have its own `runtime` set, so no channel will route through
        // TuiHosting and no hook is required.
        assert!(!any_requested_tui_hosting_driver_available(&config));

        install_provider_hosting_config(&Config::default());
    }

    #[test]
    fn any_requested_tui_hosting_channel_runtime_tui_alone_is_enough() {
        // Phase 0 counter-review round 2 (non-blocking suggestion):
        // a single channel with explicit `runtime: tui` must light up the
        // predicate even when the provider has neither `runtime` nor
        // `tui_hosting` requesting TUI.
        let _guard = TEST_CONFIG_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut config = Config::default();
        config.providers.insert(
            "claude".to_string(),
            ProviderConfig {
                tui_hosting: Some(false),
                ..ProviderConfig::default()
            },
        );
        config.agents.push(test_agent_with_claude_channel_runtime(
            "1506295332949196840",
            Some("tui"),
        ));

        assert!(any_requested_tui_hosting_driver_available(&config));

        install_provider_hosting_config(&Config::default());
    }

    #[test]
    fn any_requested_tui_hosting_skips_explicit_runtime_pipe() {
        let _guard = TEST_CONFIG_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());

        let mut pipe_config = Config::default();
        pipe_config.providers.insert(
            "claude".to_string(),
            ProviderConfig {
                tui_hosting: Some(true),
                runtime: Some("pipe".to_string()),
                ..ProviderConfig::default()
            },
        );
        // Other providers default to no TUI, so this should be false overall.
        assert!(!any_requested_tui_hosting_driver_available(&pipe_config));

        install_provider_hosting_config(&Config::default());
    }

    #[test]
    fn unknown_runtime_string_falls_back_to_tui_hosting_derivation() {
        let _guard = TEST_CONFIG_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut config = Config::default();
        config.providers.insert(
            "claude".to_string(),
            ProviderConfig {
                tui_hosting: Some(true),
                runtime: Some("bogus".to_string()),
                ..ProviderConfig::default()
            },
        );
        install_provider_hosting_config(&config);

        let selection = resolve_provider_session_selection(&ProviderKind::Claude);

        // Unknown string is ignored; legacy `tui_hosting: true` is honoured.
        assert!(selection.requested_tui_hosting);
        assert_eq!(selection.driver, ProviderSessionDriver::TuiHosting);

        install_provider_hosting_config(&Config::default());
    }

    #[test]
    fn graceful_retired_runtime_uses_supported_mirrors_and_matching_hook_predicates() {
        let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let _guard = TEST_CONFIG_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for (provider_runtime, channel_runtime, legacy, expected) in [
            (Some("claude-e"), None, true, true),
            (Some(" CLAUDE_E "), None, false, false),
            (Some("pipe"), Some("claude-e"), true, false),
            (Some("claude_e"), Some("tui"), false, true),
            (Some("tui"), Some("claude_e"), false, true),
            (None, Some("claude-e"), false, false),
            (None, Some(" CLAUDE_E "), true, true),
        ] {
            let mut config = Config::default();
            config.providers.insert(
                "claude".to_owned(),
                ProviderConfig {
                    runtime: provider_runtime.map(str::to_owned),
                    tui_hosting: Some(legacy),
                    ..ProviderConfig::default()
                },
            );
            config.agents.push(test_agent_with_claude_channel_runtime(
                "42",
                channel_runtime,
            ));
            let channel = config.agents[0].channels.get("claude").unwrap();
            assert_eq!(
                channel_effective_tui_request(&config, channel, "claude"),
                expected
            );
            install_provider_hosting_config(&config);
            let selection = resolve_provider_session_selection_with_channel(
                &ProviderKind::Claude,
                true,
                Some(42),
            );
            assert_eq!(selection.requested_tui_hosting, expected);
            assert_eq!(
                selection.driver,
                if expected {
                    ProviderSessionDriver::TuiHosting
                } else {
                    ProviderSessionDriver::LegacyPrompt
                }
            );
        }
        install_provider_hosting_config(&Config::default());
    }
    fn test_agent_with_claude_channel_runtime(channel_id: &str, runtime: Option<&str>) -> AgentDef {
        AgentDef {
            id: "adk-dashboard-e2e".to_string(),
            name: "ADK Dashboard E2E".to_string(),
            name_ko: None,
            aliases: Vec::new(),
            wake_word: None,
            voice_enabled: true,
            sensitivity_mode: None,
            voice: AgentVoiceConfig::default(),
            provider: "codex".to_string(),
            channels: AgentChannels::new().with(
                "claude",
                AgentChannel::Detailed(AgentChannelConfig {
                    id: Some(channel_id.to_string()),
                    provider: Some("claude".to_string()),
                    runtime: runtime.map(str::to_string),
                    ..AgentChannelConfig::default()
                }),
            ),
            keywords: Vec::new(),
            department: None,
            avatar_emoji: None,
            preferred_intake_node_labels: None,
            memory_group: None,
        }
    }
    fn test_agent_with_claude_channel(channel_id: &str, tui_hosting: Option<bool>) -> AgentDef {
        AgentDef {
            id: "adk-dashboard-e2e".to_string(),
            name: "ADK Dashboard E2E".to_string(),
            name_ko: None,
            aliases: Vec::new(),
            wake_word: None,
            voice_enabled: true,
            sensitivity_mode: None,
            voice: AgentVoiceConfig::default(),
            provider: "codex".to_string(),
            channels: AgentChannels::new().with(
                "claude",
                AgentChannel::Detailed(AgentChannelConfig {
                    id: Some(channel_id.to_string()),
                    provider: Some("claude".to_string()),
                    tui_hosting,
                    ..AgentChannelConfig::default()
                }),
            ),
            keywords: Vec::new(),
            department: None,
            avatar_emoji: None,
            preferred_intake_node_labels: None,
            memory_group: None,
        }
    }
}
