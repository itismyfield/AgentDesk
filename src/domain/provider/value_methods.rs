use super::{
    LegacyDispatchKind, ProviderCapabilities, ProviderCompactionAdapter, ProviderDefaultBehavior,
    ProviderDescriptor, ProviderExecutionAdapter, ProviderKind, ProviderReadinessAdapter,
    StreamJsonDialectId, derived_counterpart_ids, provider_catalog,
};

impl ProviderKind {
    fn catalog_entry(&self) -> Option<&'static ProviderDescriptor> {
        provider_catalog()
            .iter()
            .find(|entry| entry.id == self.as_str() && !matches!(self, Self::Unsupported(_)))
    }

    pub fn display_name(&self) -> &str {
        self.catalog_entry()
            .map(|entry| entry.display_name)
            .unwrap_or_else(|| match self {
                Self::Unsupported(s) => s.as_str(),
                _ => self.as_str(),
            })
    }

    pub fn preferred_counterparts(&self) -> Vec<Self> {
        if self.catalog_entry().is_none() {
            return Vec::new();
        }
        derived_counterpart_ids(self.as_str())
            .into_iter()
            .filter_map(Self::from_str)
            .collect()
    }

    pub fn counterpart(&self) -> Self {
        self.preferred_counterparts()
            .into_iter()
            .next()
            .unwrap_or_else(|| self.clone())
    }

    pub fn capabilities(&self) -> Option<ProviderCapabilities> {
        self.catalog_entry().map(|entry| entry.capabilities)
    }

    pub fn execution_adapter(&self) -> Option<ProviderExecutionAdapter> {
        self.catalog_entry().map(|entry| entry.execution_adapter)
    }

    #[cfg(test)]
    pub fn stream_json_dialect(&self) -> Option<StreamJsonDialectId> {
        match self.execution_adapter()? {
            ProviderExecutionAdapter::StreamJsonCli(dialect) => Some(dialect),
            _ => None,
        }
    }

    pub fn compaction_adapter(&self) -> Option<ProviderCompactionAdapter> {
        self.catalog_entry().map(|entry| entry.compaction_adapter)
    }

    pub fn readiness_adapter(&self) -> Option<ProviderReadinessAdapter> {
        self.catalog_entry().map(|entry| entry.readiness_adapter)
    }

    /// Provider-specific behavior when AgentDesk clears its explicit model
    /// override and falls through to the provider-managed default path.
    pub fn default_model_behavior(&self) -> ProviderDefaultBehavior {
        self.catalog_entry()
            .map(|entry| entry.default_behavior)
            .unwrap_or(ProviderDefaultBehavior {
                resume_without_reset: true,
                runtime_model: None,
                source_label: "provider default",
            })
    }

    /// Parse a known provider string. Returns None for unknown providers.
    pub fn from_str(raw: &str) -> Option<Self> {
        let normalized = raw.trim().to_ascii_lowercase();
        provider_catalog()
            .iter()
            .find(|entry| entry.matches_id_or_alias(&normalized))
            .and_then(ProviderDescriptor::kind)
    }

    pub fn cli_init_labels() -> Vec<&'static str> {
        provider_catalog()
            .iter()
            .map(|entry| entry.cli_init_label)
            .collect()
    }

    #[cfg(test)]
    pub fn cli_init_label(&self) -> &'static str {
        self.catalog_entry()
            .map(|entry| entry.cli_init_label)
            .unwrap_or("claude")
    }

    pub(crate) fn legacy_streaming_dispatch_kind(&self) -> LegacyDispatchKind {
        match self {
            Self::Claude => LegacyDispatchKind::Claude,
            Self::Codex => LegacyDispatchKind::Codex,
            Self::Gemini => LegacyDispatchKind::Gemini,
            Self::OpenCode => LegacyDispatchKind::OpenCode,
            Self::Grok => LegacyDispatchKind::StreamJsonCli(StreamJsonDialectId::Grok),
            Self::Qwen => LegacyDispatchKind::Qwen,
            Self::Antigravity => LegacyDispatchKind::StreamJsonCli(StreamJsonDialectId::Agy),
            Self::Unsupported(name) => LegacyDispatchKind::Unsupported(name.clone()),
        }
    }

    pub fn provider_for_cli_init_index(index: usize) -> Option<Self> {
        provider_catalog()
            .get(index)
            .and_then(|entry| Self::from_str(entry.id))
    }

    /// Parse a provider string, returning Unsupported for unknown providers.
    pub fn from_str_or_unsupported(raw: &str) -> Self {
        Self::from_str(raw).unwrap_or_else(|| Self::Unsupported(raw.trim().to_string()))
    }

    /// Returns generic provider environment variables for auto-compact.
    ///
    /// Claude intentionally has no generic percent environment variable: its
    /// launch path resolves a model-aware absolute
    /// `CLAUDE_CODE_AUTO_COMPACT_WINDOW` from launch provenance instead.
    #[cfg(test)]
    pub fn compact_env_vars(&self, percent: u64) -> Vec<(String, String)> {
        let Some(adapter) = self.compaction_adapter() else {
            return Vec::new();
        };
        match adapter {
            ProviderCompactionAdapter::ClaudeEnvironment => {
                let _ = percent;
                Vec::new()
            }
            ProviderCompactionAdapter::CodexCli
            | ProviderCompactionAdapter::GeminiDisabled
            | ProviderCompactionAdapter::OpenCodeDisabled
            | ProviderCompactionAdapter::QwenDisabled
            | ProviderCompactionAdapter::StreamJsonDisabled(_) => Vec::new(),
        }
    }

    /// Default context window size in tokens for this provider.
    pub fn default_context_window(&self) -> u64 {
        self.catalog_entry()
            .map(|entry| entry.default_context_window)
            .unwrap_or(200_000)
    }

    /// Returns Codex-specific CLI config overrides for auto-compact.
    /// Codex uses model_auto_compact_token_limit (absolute token count).
    pub fn compact_cli_config(&self, percent: u64, context_window: u64) -> Vec<(String, String)> {
        let Some(adapter) = self.compaction_adapter() else {
            return Vec::new();
        };
        match adapter {
            ProviderCompactionAdapter::CodexCli => {
                let token_limit = context_window * percent / 100;
                vec![(
                    "model_auto_compact_token_limit".to_string(),
                    token_limit.to_string(),
                )]
            }
            ProviderCompactionAdapter::ClaudeEnvironment
            | ProviderCompactionAdapter::GeminiDisabled
            | ProviderCompactionAdapter::OpenCodeDisabled
            | ProviderCompactionAdapter::QwenDisabled
            | ProviderCompactionAdapter::StreamJsonDisabled(_) => Vec::new(),
        }
    }

    /// Returns true when this provider can own a reusable local tmux/process
    /// session that AgentDesk may need to clear or pre-seed in inflight state.
    pub fn uses_managed_tmux_backend(&self) -> bool {
        self.catalog_entry()
            .map(|entry| entry.managed_tmux_backend)
            .unwrap_or(false)
    }

    #[cfg(test)]
    pub fn managed_tmux_wrapper_subcommand(&self) -> Option<&'static str> {
        self.catalog_entry()
            .and_then(|entry| entry.managed_tmux_wrapper_subcommand)
    }
}
