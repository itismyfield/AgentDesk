use serde::{Deserialize, Serialize};

/// Execution context passed from a Discord turn, one-shot dispatch, or other
/// call sites through to the provider binary resolver. Enables per-agent
/// canary selection without requiring global PATH manipulation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderExecutionContext {
    /// Provider identifier (e.g. "codex", "claude", "gemini", "qwen").
    pub provider: String,
    /// Discord agent role id or logical agent identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Discord channel id the turn is executing in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    /// Session key linking provider session to this execution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_key: Option<String>,
    /// Actual tmux session name used for the provider process, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tmux_session: Option<String>,
    /// Human-readable Discord channel name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_name: Option<String>,
    /// How this execution was initiated (e.g. "discord_turn", "one_shot", "meeting").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_mode: Option<String>,
}

impl ProviderExecutionContext {
    pub fn for_provider(provider: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_serializes_round_trip() {
        let ctx = ProviderExecutionContext {
            provider: "codex".to_string(),
            agent_id: Some("codex-agent".to_string()),
            channel_id: Some("123456789".to_string()),
            session_key: Some("sess-abc".to_string()),
            tmux_session: Some("agentdesk-codex-agent-control".to_string()),
            channel_name: Some("agent-control".to_string()),
            execution_mode: Some("discord_turn".to_string()),
        };
        let json = serde_json::to_string(&ctx).unwrap();
        let decoded: ProviderExecutionContext = serde_json::from_str(&json).unwrap();
        assert_eq!(ctx, decoded);
    }

    #[test]
    fn context_omits_none_fields() {
        let ctx = ProviderExecutionContext::for_provider("claude");
        let json = serde_json::to_string(&ctx).unwrap();
        assert!(!json.contains("agent_id"));
        assert!(!json.contains("channel_id"));
    }

    #[test]
    fn context_for_provider_sets_only_provider() {
        let ctx = ProviderExecutionContext::for_provider("gemini");
        assert_eq!(ctx.provider, "gemini");
        assert!(ctx.agent_id.is_none());
        assert!(ctx.execution_mode.is_none());
    }
}
