//! Typed tool-policy provenance for StreamJson CLI providers.

use std::collections::BTreeSet;

/// AgentDesk-normalized tool names (Read/Grep/Glob/…).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct AgentTool(pub String);

impl AgentTool {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn canonical_key(&self) -> String {
        self.0.trim().to_ascii_lowercase()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolPolicy {
    ProviderDefault,
    ReadOnly,
    AllowListed(BTreeSet<AgentTool>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfiguredToolPolicy {
    Explicit(ToolPolicy),
    LegacyAllowedTools(Vec<AgentTool>),
}

impl ConfiguredToolPolicy {
    pub fn from_raw(mode: Option<&str>, allowed_tools: Option<&[String]>) -> Result<Self, String> {
        let tools: Vec<AgentTool> = allowed_tools
            .unwrap_or(&[])
            .iter()
            .map(|tool| AgentTool::new(tool.trim()))
            .filter(|tool| !tool.as_str().is_empty())
            .collect();
        match mode.map(str::trim).filter(|value| !value.is_empty()) {
            Some("provider_default") => Ok(Self::Explicit(ToolPolicy::ProviderDefault)),
            Some("read_only") => Ok(Self::Explicit(ToolPolicy::ReadOnly)),
            Some("allowlist") => {
                if tools.is_empty() {
                    return Err("allowlist mode requires a non-empty allowed_tools list".into());
                }
                Ok(Self::Explicit(ToolPolicy::AllowListed(
                    tools.into_iter().collect(),
                )))
            }
            Some(other) => Err(format!("unknown tool_policy_mode: {other}")),
            None if tools.is_empty() => Ok(Self::Explicit(ToolPolicy::ProviderDefault)),
            None => Ok(Self::LegacyAllowedTools(tools)),
        }
    }

    pub fn for_new_stream_json_provider() -> Self {
        Self::Explicit(ToolPolicy::ProviderDefault)
    }

    pub fn effective_for_legacy_provider(&self) -> ToolPolicy {
        match self {
            Self::Explicit(policy) => policy.clone(),
            Self::LegacyAllowedTools(tools) if is_readonly_tool_names(tools) => {
                ToolPolicy::ReadOnly
            }
            Self::LegacyAllowedTools(tools) => {
                ToolPolicy::AllowListed(tools.iter().cloned().collect())
            }
        }
    }

    pub fn effective_for_stream_json(&self) -> ToolPolicy {
        match self {
            Self::Explicit(policy) => policy.clone(),
            // Never treat a materialized default vector as an allowlist.
            Self::LegacyAllowedTools(_) => ToolPolicy::ProviderDefault,
        }
    }
}

impl Default for ConfiguredToolPolicy {
    fn default() -> Self {
        Self::for_new_stream_json_provider()
    }
}

pub fn is_readonly_tool_names(tools: &[AgentTool]) -> bool {
    !tools.is_empty()
        && tools
            .iter()
            .all(|tool| matches!(tool.canonical_key().as_str(), "read" | "grep" | "glob"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omitted_mode_and_tools_are_provider_default() {
        let policy = ConfiguredToolPolicy::from_raw(None, None).unwrap();
        assert_eq!(
            policy,
            ConfiguredToolPolicy::Explicit(ToolPolicy::ProviderDefault)
        );
    }

    #[test]
    fn mode_less_vector_is_legacy() {
        let tools = ["Read".to_string(), "Bash".to_string()];
        let policy = ConfiguredToolPolicy::from_raw(None, Some(&tools)).unwrap();
        match policy {
            ConfiguredToolPolicy::LegacyAllowedTools(items) => assert_eq!(items.len(), 2),
            other => panic!("expected legacy, got {other:?}"),
        }
    }

    #[test]
    fn empty_allowlist_mode_is_error() {
        assert!(ConfiguredToolPolicy::from_raw(Some("allowlist"), Some(&[])).is_err());
    }

    #[test]
    fn stream_json_legacy_vector_does_not_become_allowlist() {
        let tools = vec![AgentTool::new("Bash"), AgentTool::new("Write")];
        let policy = ConfiguredToolPolicy::LegacyAllowedTools(tools);
        assert_eq!(
            policy.effective_for_stream_json(),
            ToolPolicy::ProviderDefault
        );
    }
}
