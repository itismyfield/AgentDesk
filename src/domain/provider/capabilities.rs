#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderCapabilities {
    pub binary_name: &'static str,
    pub supports_structured_output: bool,
    pub supports_resume: bool,
    pub supports_tool_stream: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderDefaultBehavior {
    pub resume_without_reset: bool,
    pub runtime_model: Option<&'static str>,
    pub source_label: &'static str,
}

/// Last-resort Codex context window when the adapter cannot use its cache.
pub(crate) const CODEX_FALLBACK_CONTEXT_WINDOW: u64 = 272_000;
