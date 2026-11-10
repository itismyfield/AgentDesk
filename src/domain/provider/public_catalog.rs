use serde::{Deserialize, Serialize};

use super::{ProviderDescriptor, provider_catalog};

/// Public, non-secret projection of the canonical provider registry.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProviderCatalogEntry {
    pub id: String,
    pub display_name: String,
    pub channel_suffix: Option<String>,
    pub binary_name: String,
    pub execution_surface: String,
    pub supports_resume: bool,
    pub supports_structured_output: bool,
    pub supports_tool_stream: bool,
    pub supports_restricted_tool_policy: bool,
    pub supports_tui_hosting: bool,
    pub system_prompt_transport: String,
    pub context_window_tokens: Option<u64>,
}

impl ProviderCatalogEntry {
    fn from_descriptor(entry: &ProviderDescriptor) -> Self {
        Self {
            id: entry.id.to_string(),
            display_name: entry.display_name.to_string(),
            channel_suffix: entry.channel_suffix.map(str::to_string),
            binary_name: entry.capabilities.binary_name.to_string(),
            execution_surface: entry.execution_adapter.execution_surface().to_string(),
            supports_resume: entry.capabilities.supports_resume,
            supports_structured_output: entry.capabilities.supports_structured_output,
            supports_tool_stream: entry.capabilities.supports_tool_stream,
            supports_restricted_tool_policy: entry.supports_restricted_tool_policy,
            supports_tui_hosting: entry.supports_tui_hosting,
            system_prompt_transport: entry.system_prompt_transport.to_string(),
            context_window_tokens: entry
                .context_window_known
                .then_some(entry.default_context_window),
        }
    }
}

pub fn public_provider_catalog() -> Vec<ProviderCatalogEntry> {
    provider_catalog()
        .iter()
        .map(ProviderCatalogEntry::from_descriptor)
        .collect()
}
