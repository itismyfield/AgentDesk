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

    let summary = values
        .iter()
        .map(|(provider, enabled)| format!("{provider}={enabled}"))
        .collect::<Vec<_>>()
        .join(",");
    *PROVIDER_TUI_HOSTING
        .write()
        .unwrap_or_else(|error| error.into_inner()) = values;
    tracing::info!(summary, "provider tui_hosting config installed");
}

pub fn resolve_provider_session_selection(provider: &ProviderKind) -> ProviderSessionSelection {
    resolve_provider_session_selection_with_capability(provider, true)
}

pub fn resolve_provider_session_selection_with_capability(
    provider: &ProviderKind,
    entrypoint_supports_tui_hosting: bool,
) -> ProviderSessionSelection {
    let provider_id = provider.as_str().to_ascii_lowercase();
    let requested_tui_hosting = PROVIDER_TUI_HOSTING
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .get(&provider_id)
        .copied()
        .unwrap_or_else(|| default_provider_tui_hosting(&provider_id));

    if !requested_tui_hosting {
        return ProviderSessionSelection {
            provider_id,
            requested_tui_hosting,
            driver: ProviderSessionDriver::LegacyPrompt,
            fallback_reason: None,
        };
    }

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

pub fn any_requested_tui_hosting_driver_available(config: &Config) -> bool {
    crate::services::provider::supported_provider_ids()
        .iter()
        .filter(|provider_id| config.provider_tui_hosting_enabled(provider_id))
        .filter_map(|provider_id| ProviderKind::from_str(provider_id))
        .any(|provider| provider_tui_hosting_driver_available(&provider))
}

pub fn provider_tui_hosting_driver_available(provider: &ProviderKind) -> bool {
    matches!(provider, ProviderKind::Claude | ProviderKind::Codex)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ProviderConfig};
    use std::sync::{LazyLock, Mutex};

    static TEST_CONFIG_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    #[test]
    fn defaults_request_tui_for_claude_and_codex_only() {
        let config = Config::default();

        assert!(config.provider_tui_hosting_enabled("claude"));
        assert!(config.provider_tui_hosting_enabled("codex"));
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
            },
        );
        config.providers.insert(
            "qwen".to_string(),
            ProviderConfig {
                tui_hosting: Some(true),
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
        install_provider_hosting_config(&Config::default());

        let selection = resolve_provider_session_selection(&ProviderKind::Codex);

        assert!(selection.requested_tui_hosting);
        assert_eq!(selection.driver, ProviderSessionDriver::TuiHosting);
        assert_eq!(selection.fallback_reason, None);
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
            },
        );
        config.providers.insert(
            "qwen".to_string(),
            ProviderConfig {
                tui_hosting: Some(true),
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
}
