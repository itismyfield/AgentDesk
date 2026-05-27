use std::collections::BTreeMap;
use std::sync::{LazyLock, RwLock};

use crate::config::{Config, default_provider_tui_hosting};
use crate::services::provider::ProviderKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderSessionDriver {
    LegacyPrompt,
    TuiHosting,
    ClaudeE,
}

impl ProviderSessionDriver {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LegacyPrompt => "legacy-prompt",
            Self::TuiHosting => "tui-hosting",
            Self::ClaudeE => "claude-e",
        }
    }
}

/// Phase 0 of the claude-e rollout. `RuntimeMode` is the operator-facing
/// shape of `providers.{provider}.runtime` / per-channel `runtime`. It maps
/// onto `ProviderSessionDriver` once driver availability and entrypoint
/// support are considered:
///
/// | RuntimeMode | Resolved driver (when available) |
/// |-------------|----------------------------------|
/// | `Pipe`      | `LegacyPrompt`                   |
/// | `Tui`       | `TuiHosting`                     |
/// | `ClaudeE`   | `ClaudeE` (Claude provider only) |
///
/// See `docs/claude-e-rollout/decision-log.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeMode {
    Pipe,
    Tui,
    ClaudeE,
}

impl RuntimeMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pipe => "pipe",
            Self::Tui => "tui",
            Self::ClaudeE => "claude-e",
        }
    }

    /// Parse the YAML `runtime` field. Unknown / empty values return `None`
    /// so the legacy `tui_hosting` derivation can run as before.
    ///
    /// Accepted aliases are minimal on purpose: the canonical spellings
    /// (`pipe` / `tui` / `claude-e`) plus the underscored form for each.
    /// Typos like `claudee` are intentionally rejected so the warn-and-
    /// fallback path runs instead of silently honouring the intent.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "pipe" | "legacy" | "legacy_prompt" | "legacy-prompt" => Some(Self::Pipe),
            "tui" | "tui_hosting" | "tui-hosting" => Some(Self::Tui),
            "claude-e" | "claude_e" => Some(Self::ClaudeE),
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

/// Phase 0 of the claude-e rollout. Mirrors the `runtime` field on
/// `ProviderConfig` and per-channel config. Wins over `tui_hosting` when
/// both are set, per `docs/claude-e-rollout/decision-log.md`.
static PROVIDER_RUNTIME_MODE: LazyLock<RwLock<BTreeMap<String, RuntimeMode>>> =
    LazyLock::new(|| RwLock::new(BTreeMap::new()));
static PROVIDER_RUNTIME_MODE_BY_CHANNEL: LazyLock<RwLock<BTreeMap<(String, u64), RuntimeMode>>> =
    LazyLock::new(|| RwLock::new(BTreeMap::new()));

/// Issue #2193 — runtime mirror of `providers.codex.remote_ssh_enabled`.
/// Defaults to `false`; the bootstrap step hard-fails before this can be
/// flipped on without the ADR prerequisites in place.
static CODEX_REMOTE_SSH_ENABLED: LazyLock<RwLock<bool>> = LazyLock::new(|| RwLock::new(false));

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
    let mut runtime_mode_values: BTreeMap<String, RuntimeMode> = BTreeMap::new();
    let mut runtime_mode_channel_values: BTreeMap<(String, u64), RuntimeMode> = BTreeMap::new();
    // Phase 0: read the new `runtime` field for each provider, normalize the
    // string, and surface invalid values as a single tracing warning so the
    // operator can spot typos without breaking startup.
    for (provider_id, provider_config) in &config.providers {
        if let Some(raw) = provider_config.runtime.as_deref() {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            match RuntimeMode::parse(trimmed) {
                Some(mode) => {
                    runtime_mode_values.insert(provider_id.trim().to_ascii_lowercase(), mode);
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
            let Some(channel) = channel else {
                continue;
            };
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
            // Phase 0: per-channel `runtime` override. `runtime` wins over
            // `tui_hosting` per decision log entry 2026-05-27.
            if let Some(raw) = channel.runtime_mode_raw() {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match RuntimeMode::parse(trimmed) {
                    Some(mode) => {
                        runtime_mode_channel_values.insert((provider_id, channel_id), mode);
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

    // Phase 0: install runtime mode mirrors (claude-e rollout).
    let runtime_mode_summary = runtime_mode_values
        .iter()
        .map(|(provider, mode)| format!("{provider}={}", mode.as_str()))
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
        .map(|((provider, channel_id), mode)| format!("{provider}:{channel_id}={}", mode.as_str()))
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

    // Phase 0 of the claude-e rollout: the explicit `runtime` field wins
    // over the legacy `tui_hosting` boolean when both are set. Channel-level
    // override takes precedence over provider-level. See
    // `docs/claude-e-rollout/decision-log.md`.
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

    let (effective_mode, requested_tui_hosting) = match explicit_runtime_mode {
        Some(RuntimeMode::Pipe) => (RuntimeMode::Pipe, false),
        Some(RuntimeMode::Tui) => (RuntimeMode::Tui, true),
        Some(RuntimeMode::ClaudeE) => (RuntimeMode::ClaudeE, false),
        None if tui_hosting_from_legacy => (RuntimeMode::Tui, true),
        None => (RuntimeMode::Pipe, false),
    };

    match effective_mode {
        RuntimeMode::Pipe => ProviderSessionSelection {
            provider_id,
            requested_tui_hosting,
            driver: ProviderSessionDriver::LegacyPrompt,
            fallback_reason: None,
        },
        RuntimeMode::Tui => {
            if !provider_tui_hosting_driver_available(provider) {
                return ProviderSessionSelection {
                    provider_id,
                    requested_tui_hosting,
                    driver: ProviderSessionDriver::LegacyPrompt,
                    fallback_reason: Some("tui_hosting_driver_unavailable"),
                };
            }
            if entrypoint_supports_tui_hosting {
                ProviderSessionSelection {
                    provider_id,
                    requested_tui_hosting,
                    driver: ProviderSessionDriver::TuiHosting,
                    fallback_reason: None,
                }
            } else {
                ProviderSessionSelection {
                    provider_id,
                    requested_tui_hosting,
                    driver: ProviderSessionDriver::LegacyPrompt,
                    fallback_reason: Some("entrypoint_not_tui_hosted"),
                }
            }
        }
        RuntimeMode::ClaudeE => {
            // Phase 0: only Claude can host `claude-e`. Other providers
            // requesting it fall back to the legacy pipe driver with an
            // explicit reason so the operator can correct the config.
            if !matches!(provider, ProviderKind::Claude) {
                return ProviderSessionSelection {
                    provider_id,
                    requested_tui_hosting,
                    driver: ProviderSessionDriver::LegacyPrompt,
                    fallback_reason: Some("claude_e_unsupported_for_provider"),
                };
            }
            // Phase 1 of the claude-e rollout: the adapter module is
            // wired. If the `claude-e` binary is missing on PATH, we
            // fall back to the legacy `-p` driver with an explicit
            // reason so a misconfiguration cannot break dispatch.
            if !crate::services::claude_e::adapter_available() {
                return ProviderSessionSelection {
                    provider_id,
                    requested_tui_hosting,
                    driver: ProviderSessionDriver::LegacyPrompt,
                    fallback_reason: Some("claude_e_binary_missing"),
                };
            }
            ProviderSessionSelection {
                provider_id,
                requested_tui_hosting,
                driver: ProviderSessionDriver::ClaudeE,
                fallback_reason: None,
            }
        }
    }
}

/// Phase 0 of the claude-e rollout. `runtime: tui` on a provider or channel
/// counts as a TUI hosting request even when `tui_hosting` is unset, so the
/// boot path (e.g. `claude_tui::hook_server` publish) does not skip the hook
/// endpoint for operators who only set the new `runtime` field.
fn provider_runtime_mode_explicit(config: &Config, provider_id: &str) -> Option<RuntimeMode> {
    config
        .providers
        .get(&provider_id.trim().to_ascii_lowercase())
        .and_then(|provider_config| provider_config.runtime.as_deref())
        .and_then(|raw| RuntimeMode::parse(raw.trim()))
}

fn channel_runtime_mode_explicit(channel: &crate::config::AgentChannel) -> Option<RuntimeMode> {
    channel
        .runtime_mode_raw()
        .as_deref()
        .and_then(|raw| RuntimeMode::parse(raw.trim()))
}

/// Phase 0 of the claude-e rollout. Match `resolve_provider_session_*`
/// precedence exactly: channel-level `runtime` wins, then provider-level
/// `runtime`, then channel-level `tui_hosting`, then provider-level
/// `tui_hosting`. Keeping this predicate aligned with the resolver
/// prevents "hook published but no channel routes through TUI" drift
/// (Phase 0 counter-review round 2 MAJOR).
fn channel_effective_tui_request(
    config: &Config,
    channel: &crate::config::AgentChannel,
    provider_id: &str,
) -> bool {
    if let Some(mode) = channel_runtime_mode_explicit(channel) {
        return matches!(mode, RuntimeMode::Tui);
    }
    if let Some(mode) = provider_runtime_mode_explicit(config, provider_id) {
        return matches!(mode, RuntimeMode::Tui);
    }
    if let Some(legacy) = channel.tui_hosting() {
        return legacy;
    }
    config.provider_tui_hosting_enabled(provider_id)
}

fn provider_default_effective_tui_request(config: &Config, provider_id: &str) -> bool {
    if let Some(mode) = provider_runtime_mode_explicit(config, provider_id) {
        return matches!(mode, RuntimeMode::Tui);
    }
    config.provider_tui_hosting_enabled(provider_id)
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
        agent
            .channels
            .iter()
            .into_iter()
            .any(|(channel_kind, channel)| {
                let Some(channel) = channel else {
                    return false;
                };
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
    use std::sync::{LazyLock, Mutex};

    static TEST_CONFIG_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

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
        let _guard = TEST_CONFIG_LOCK.lock().unwrap();
        install_provider_hosting_config(&Config::default());

        let selection = resolve_provider_session_selection(&ProviderKind::Claude);

        assert!(selection.requested_tui_hosting);
        assert_eq!(selection.driver, ProviderSessionDriver::TuiHosting);
        assert_eq!(selection.fallback_reason, None);
    }

    #[test]
    fn requested_tui_selects_codex_driver_when_available() {
        let _guard = TEST_CONFIG_LOCK.lock().unwrap();
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
        let _guard = TEST_CONFIG_LOCK.lock().unwrap();
        install_provider_hosting_config(&Config::default());

        let selection = resolve_provider_session_selection(&ProviderKind::Codex);

        assert!(!selection.requested_tui_hosting);
        assert_eq!(selection.driver, ProviderSessionDriver::LegacyPrompt);
        assert_eq!(selection.fallback_reason, None);
    }

    #[test]
    fn channel_override_can_enable_tui_when_provider_default_is_disabled() {
        let _guard = TEST_CONFIG_LOCK.lock().unwrap();
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
        let _guard = TEST_CONFIG_LOCK.lock().unwrap();
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
        let _guard = TEST_CONFIG_LOCK.lock().unwrap();
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
        let _guard = TEST_CONFIG_LOCK.lock().unwrap();
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
        let _guard = TEST_CONFIG_LOCK.lock().unwrap();
        install_provider_hosting_config(&Config::default());

        assert!(!codex_remote_ssh_enabled());
    }

    #[test]
    fn codex_remote_ssh_gate_mirrors_explicit_true() {
        let _guard = TEST_CONFIG_LOCK.lock().unwrap();
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

    // -------------------- Phase 0: claude-e rollout tests --------------------

    #[test]
    fn runtime_mode_parse_accepts_canonical_and_aliases() {
        assert_eq!(RuntimeMode::parse("pipe"), Some(RuntimeMode::Pipe));
        assert_eq!(RuntimeMode::parse("Pipe"), Some(RuntimeMode::Pipe));
        assert_eq!(RuntimeMode::parse("legacy"), Some(RuntimeMode::Pipe));
        assert_eq!(RuntimeMode::parse("tui"), Some(RuntimeMode::Tui));
        assert_eq!(RuntimeMode::parse("tui_hosting"), Some(RuntimeMode::Tui));
        assert_eq!(RuntimeMode::parse("claude-e"), Some(RuntimeMode::ClaudeE));
        assert_eq!(RuntimeMode::parse("Claude-E"), Some(RuntimeMode::ClaudeE));
        assert_eq!(RuntimeMode::parse("claude_e"), Some(RuntimeMode::ClaudeE));
        assert_eq!(RuntimeMode::parse(""), None);
        assert_eq!(RuntimeMode::parse("bogus"), None);
        // Phase 0 counter-review MINOR 4: typo variants must trigger the
        // warn-and-fallback path, not silent acceptance.
        assert_eq!(RuntimeMode::parse("claudee"), None);
        assert_eq!(RuntimeMode::parse("ClaudeE"), None);
    }

    #[test]
    fn provider_runtime_pipe_overrides_tui_hosting_true() {
        let _guard = TEST_CONFIG_LOCK.lock().unwrap();
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
        let _guard = TEST_CONFIG_LOCK.lock().unwrap();
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
    fn provider_runtime_claude_e_on_claude_routes_to_adapter_when_available() {
        // Phase 1 of the claude-e rollout: when the `claude-e` binary
        // is on PATH the resolver returns `ProviderSessionDriver::ClaudeE`;
        // otherwise it falls back to `LegacyPrompt` with
        // `claude_e_binary_missing` so a misconfiguration cannot break
        // dispatch. The assertion adapts to whichever environment the
        // test runs in (developer host vs. clean CI).
        let _guard = TEST_CONFIG_LOCK.lock().unwrap();
        let mut config = Config::default();
        config.providers.insert(
            "claude".to_string(),
            ProviderConfig {
                tui_hosting: Some(true),
                runtime: Some("claude-e".to_string()),
                ..ProviderConfig::default()
            },
        );
        install_provider_hosting_config(&config);

        let selection = resolve_provider_session_selection(&ProviderKind::Claude);
        assert!(!selection.requested_tui_hosting);
        if crate::services::claude_e::adapter_available() {
            assert_eq!(selection.driver, ProviderSessionDriver::ClaudeE);
            assert_eq!(selection.fallback_reason, None);
        } else {
            assert_eq!(selection.driver, ProviderSessionDriver::LegacyPrompt);
            assert_eq!(selection.fallback_reason, Some("claude_e_binary_missing"));
        }

        install_provider_hosting_config(&Config::default());
    }

    #[test]
    fn provider_runtime_claude_e_on_codex_falls_back_with_unsupported_reason() {
        let _guard = TEST_CONFIG_LOCK.lock().unwrap();
        let mut config = Config::default();
        config.providers.insert(
            "codex".to_string(),
            ProviderConfig {
                tui_hosting: Some(true),
                runtime: Some("claude-e".to_string()),
                ..ProviderConfig::default()
            },
        );
        install_provider_hosting_config(&config);

        let selection = resolve_provider_session_selection(&ProviderKind::Codex);

        assert_eq!(selection.driver, ProviderSessionDriver::LegacyPrompt);
        assert_eq!(
            selection.fallback_reason,
            Some("claude_e_unsupported_for_provider")
        );

        install_provider_hosting_config(&Config::default());
    }

    #[test]
    fn channel_runtime_overrides_provider_runtime() {
        let _guard = TEST_CONFIG_LOCK.lock().unwrap();
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
        let _guard = TEST_CONFIG_LOCK.lock().unwrap();
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
        let _guard = TEST_CONFIG_LOCK.lock().unwrap();
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
        let _guard = TEST_CONFIG_LOCK.lock().unwrap();
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
    fn any_requested_tui_hosting_skips_explicit_runtime_pipe_or_claude_e() {
        // Phase 0: `runtime: pipe` and `runtime: claude-e` must NOT light up
        // the hook endpoint even when `tui_hosting: true` is also present
        // (explicit `runtime` wins).
        let _guard = TEST_CONFIG_LOCK.lock().unwrap();

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

        let mut claude_e_config = Config::default();
        claude_e_config.providers.insert(
            "claude".to_string(),
            ProviderConfig {
                tui_hosting: Some(true),
                runtime: Some("claude-e".to_string()),
                ..ProviderConfig::default()
            },
        );
        assert!(!any_requested_tui_hosting_driver_available(
            &claude_e_config
        ));

        install_provider_hosting_config(&Config::default());
    }

    #[test]
    fn unknown_runtime_string_falls_back_to_tui_hosting_derivation() {
        let _guard = TEST_CONFIG_LOCK.lock().unwrap();
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
            channels: AgentChannels {
                claude: Some(AgentChannel::Detailed(AgentChannelConfig {
                    id: Some(channel_id.to_string()),
                    provider: Some("claude".to_string()),
                    runtime: runtime.map(str::to_string),
                    ..AgentChannelConfig::default()
                })),
                ..AgentChannels::default()
            },
            keywords: Vec::new(),
            department: None,
            avatar_emoji: None,
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
            channels: AgentChannels {
                claude: Some(AgentChannel::Detailed(AgentChannelConfig {
                    id: Some(channel_id.to_string()),
                    provider: Some("claude".to_string()),
                    tui_hosting,
                    ..AgentChannelConfig::default()
                })),
                ..AgentChannels::default()
            },
            keywords: Vec::new(),
            department: None,
            avatar_emoji: None,
        }
    }
}
