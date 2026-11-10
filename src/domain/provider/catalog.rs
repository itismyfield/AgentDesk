use super::{
    ProviderCapabilities, ProviderCompactionAdapter, ProviderDefaultBehavior,
    ProviderExecutionAdapter, ProviderKind, ProviderReadinessAdapter,
};

mod rows;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderDescriptor {
    pub id: &'static str,
    pub aliases: &'static [&'static str],
    pub display_name: &'static str,
    pub cli_init_label: &'static str,
    pub channel_suffix: Option<&'static str>,
    pub default_channel_provider: bool,
    pub capabilities: ProviderCapabilities,
    pub execution_adapter: ProviderExecutionAdapter,
    pub compaction_adapter: ProviderCompactionAdapter,
    pub readiness_adapter: ProviderReadinessAdapter,
    pub default_behavior: ProviderDefaultBehavior,
    pub default_context_window: u64,
    pub context_window_known: bool,
    pub supports_restricted_tool_policy: bool,
    pub supports_tui_hosting: bool,
    pub system_prompt_transport: &'static str,
    pub managed_tmux_backend: bool,
    pub managed_tmux_wrapper_subcommand: Option<&'static str>,
}

pub const fn provider_catalog() -> &'static [ProviderDescriptor] {
    rows::PROVIDER_CATALOG
}

// Keep the historical first choice, then sort the remaining catalog IDs.
const FROZEN_FIRST_COUNTERPART: &[(&str, &str)] = &[
    ("claude", "codex"),
    ("codex", "claude"),
    ("gemini", "codex"),
    ("opencode", "codex"),
    ("qwen", "codex"),
    ("grok", "codex"),
    ("antigravity", "codex"),
];

pub fn frozen_first_counterpart_id(provider_id: &str) -> Option<&'static str> {
    FROZEN_FIRST_COUNTERPART
        .iter()
        .find(|(id, _)| *id == provider_id)
        .map(|(_, first)| *first)
}

pub fn derived_counterpart_ids(provider_id: &str) -> Vec<&'static str> {
    let first = frozen_first_counterpart_id(provider_id);
    let mut rest: Vec<&'static str> = provider_catalog()
        .iter()
        .map(|entry| entry.id)
        .filter(|id| *id != provider_id && Some(*id) != first)
        .collect();
    rest.sort_unstable();
    match first {
        Some(first) => std::iter::once(first).chain(rest).collect(),
        None => rest,
    }
}

impl ProviderDescriptor {
    pub fn matches_id_or_alias(&self, normalized: &str) -> bool {
        self.id == normalized || self.aliases.iter().any(|alias| *alias == normalized)
    }

    pub fn kind(&self) -> Option<ProviderKind> {
        Some(match self.id {
            "claude" => ProviderKind::Claude,
            "codex" => ProviderKind::Codex,
            "gemini" => ProviderKind::Gemini,
            "opencode" => ProviderKind::OpenCode,
            "qwen" => ProviderKind::Qwen,
            "grok" => ProviderKind::Grok,
            "antigravity" => ProviderKind::Antigravity,
            _ => return None,
        })
    }
}
