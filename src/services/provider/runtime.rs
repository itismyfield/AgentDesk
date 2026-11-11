//! Live configuration, authentication, registry and cache access for provider values.

use super::codex_model_context_window;
use super::{ProviderCapabilities, ProviderKind, ProviderRegistryEntry, provider_registry};
use crate::services::platform::BinaryResolution;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderRuntimeProbe {
    pub provider: ProviderKind,
    pub capabilities: ProviderCapabilities,
    pub resolution: BinaryResolution,
    pub version: Option<String>,
    pub probe_failure_kind: Option<String>,
    pub skipped_candidate_failures: Vec<String>,
    pub credential_present: bool,
    pub credential_source: Option<String>,
}

/// Runtime access is supplied by the provider adapter, independently of value ownership.
pub trait ProviderRuntimeExt: Sized {
    fn registry_entry(&self) -> Option<&'static ProviderRegistryEntry>;
    fn default_channel_provider() -> Option<Self>;
    fn from_channel_suffix(channel_name: &str) -> Option<Self>;
    fn resolve_runtime_path(&self) -> Option<String>;
    fn probe_runtime(&self) -> Option<ProviderRuntimeProbe>;
    fn resolve_channel_provider(
        channel_name: Option<&str>,
        explicit_provider: Option<&ProviderKind>,
    ) -> Option<Self>;
    fn is_channel_supported(
        &self,
        channel_name: Option<&str>,
        is_dm: bool,
        explicit_provider: Option<&ProviderKind>,
    ) -> bool;
    fn resolve_context_window(&self, model: Option<&str>) -> u64;
}

impl ProviderRuntimeExt for ProviderKind {
    fn registry_entry(&self) -> Option<&'static ProviderRegistryEntry> {
        provider_registry()
            .iter()
            .find(|entry| entry.id == self.as_str() && !matches!(self, Self::Unsupported(_)))
    }

    fn default_channel_provider() -> Option<Self> {
        crate::config_live_reload::current()
            .map(|config| config.onboarding.effective_default_provider())
            .unwrap_or_else(|| {
                crate::config::OnboardingConfig::default().effective_default_provider()
            })
    }

    fn from_channel_suffix(channel_name: &str) -> Option<Self> {
        crate::config_live_reload::current()
            .map(|config| config.onboarding.provider_from_channel_suffix(channel_name))
            .unwrap_or_else(|| {
                crate::config::OnboardingConfig::default()
                    .provider_from_channel_suffix(channel_name)
            })
            .as_deref()
            .and_then(Self::from_str)
    }

    fn resolve_runtime_path(&self) -> Option<String> {
        super::registry::resolve_kind_runtime_path(self)
    }

    fn probe_runtime(&self) -> Option<ProviderRuntimeProbe> {
        let entry = self.registry_entry()?;
        let capabilities = entry.capabilities;
        let binary_probe =
            crate::services::platform::probe_provider_binary_version(capabilities.binary_name);
        let credentials =
            crate::services::provider_auth::detect_provider_credentials(entry.id, &entry.auth);
        Some(ProviderRuntimeProbe {
            provider: self.clone(),
            capabilities,
            resolution: binary_probe.resolution,
            version: binary_probe.version_output,
            probe_failure_kind: binary_probe.probe_failure_kind,
            skipped_candidate_failures: binary_probe.skipped_candidate_failures,
            credential_present: credentials.credential_present,
            credential_source: credentials.source,
        })
    }

    fn resolve_channel_provider(
        channel_name: Option<&str>,
        explicit_provider: Option<&ProviderKind>,
    ) -> Option<Self> {
        explicit_provider
            .cloned()
            .filter(ProviderKind::is_supported)
            .or_else(|| channel_name.and_then(Self::from_channel_suffix))
            .or_else(Self::default_channel_provider)
    }

    fn is_channel_supported(
        &self,
        channel_name: Option<&str>,
        is_dm: bool,
        explicit_provider: Option<&ProviderKind>,
    ) -> bool {
        if is_dm {
            return self.is_supported();
        }
        Self::resolve_channel_provider(channel_name, explicit_provider)
            .is_some_and(|provider| provider == *self)
    }

    /// Resolve the context window for a specific model.
    ///
    /// #3263: Codex is cache-first REGARDLESS of whether `model` is `Some`/`None`
    /// — `codex_model_context_window` reads `~/.codex/models_cache.json` (exact
    /// slug when supplied, else max-of-cache). A provider-default Codex turn
    /// (`None`) therefore still prefers the cache's max-of-cache window over the
    /// stale constant. Only when that cache is absent/empty/unparseable do we
    /// fall back to the provider default (`CODEX_FALLBACK_CONTEXT_WINDOW`). Other
    /// providers have no local source and always use their registry default.
    fn resolve_context_window(&self, model: Option<&str>) -> u64 {
        // `None` (provider-default turn) maps to an empty slug, which never
        // matches an entry, so `codex_context_window_from_cache` skips the
        // exact-match branch and yields the cache's max-of-cache window.
        let cached = if let Self::Codex = self {
            codex_model_context_window(model.unwrap_or(""))
        } else {
            None
        };
        resolve_context_window_with(self, cached)
    }
}

pub(super) fn resolve_context_window_with(provider: &ProviderKind, cached: Option<u64>) -> u64 {
    cached.unwrap_or_else(|| provider.default_context_window())
}
