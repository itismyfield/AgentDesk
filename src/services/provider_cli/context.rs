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
