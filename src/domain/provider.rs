mod capabilities;
mod catalog;
mod execution;
pub mod naming;
mod public_catalog;
mod value_methods;

pub(crate) use capabilities::CODEX_FALLBACK_CONTEXT_WINDOW;
pub use capabilities::{ProviderCapabilities, ProviderDefaultBehavior};
pub use catalog::{ProviderDescriptor, derived_counterpart_ids, provider_catalog};
pub(crate) use execution::LegacyDispatchKind;
pub use execution::{
    ProviderCompactionAdapter, ProviderExecutionAdapter, ProviderReadinessAdapter,
    StreamJsonDialectId,
};
pub use public_catalog::{ProviderCatalogEntry, public_provider_catalog};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ProviderKind {
    Claude,
    Codex,
    Gemini,
    OpenCode,
    Qwen,
    Grok,
    Antigravity,
    Unsupported(String),
}

impl ProviderKind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::OpenCode => "opencode",
            Self::Qwen => "qwen",
            Self::Grok => "grok",
            Self::Antigravity => "antigravity",
            Self::Unsupported(s) => s.as_str(),
        }
    }

    pub fn doctor_check_id(&self) -> &'static str {
        match self {
            Self::Claude => "provider_claude",
            Self::Codex => "provider_codex",
            Self::Gemini => "provider_gemini",
            Self::OpenCode => "provider_opencode",
            Self::Qwen => "provider_qwen",
            Self::Grok => "provider_grok",
            Self::Antigravity => "provider_antigravity",
            Self::Unsupported(_) => "provider_unsupported",
        }
    }

    pub fn is_supported(&self) -> bool {
        !matches!(self, Self::Unsupported(_))
    }
}

#[cfg(test)]
mod tests;
