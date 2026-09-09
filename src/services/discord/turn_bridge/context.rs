use super::*;

pub(in crate::services::discord) struct TurnBridgeContext {
    pub(in crate::services::discord) provider: ProviderKind,
    pub(in crate::services::discord) gateway: Arc<dyn TurnGateway>,
    pub(in crate::services::discord) channel_id: ChannelId,
    /// `None` for a recovery turn with no anchored Discord user message
    /// (user_msg_id == 0, e.g. a TUI-direct turn). All Discord-message side
    /// effects keyed on it (reactions, analytics row, voice link) are skipped.
    pub(in crate::services::discord) user_msg_id: Option<MessageId>,
    pub(in crate::services::discord) user_text_owned: String,
    pub(in crate::services::discord) request_owner_name: String,
    pub(in crate::services::discord) role_binding: Option<RoleBinding>,
    pub(in crate::services::discord) adk_session_key: Option<String>,
    pub(in crate::services::discord) adk_session_name: Option<String>,
    pub(in crate::services::discord) adk_session_info: Option<String>,
    pub(in crate::services::discord) adk_cwd: Option<String>,
    pub(in crate::services::discord) dispatch_id: Option<String>,
    pub(in crate::services::discord) dispatch_kind: Option<String>,
    pub(in crate::services::discord) memory_recall_usage: TokenUsage,
    pub(in crate::services::discord) context_window_tokens: u64,
    pub(in crate::services::discord) context_compact_percent: u64,
    /// `None` for a recovery turn that never anchored a Discord placeholder
    /// (current_msg_id == 0, e.g. a TUI-direct turn). The bridge then creates a
    /// fresh placeholder on first output instead of editing a nonexistent one.
    pub(in crate::services::discord) current_msg_id: Option<MessageId>,
    pub(in crate::services::discord) response_sent_offset: usize,
    pub(in crate::services::discord) full_response: String,
    pub(in crate::services::discord) tmux_last_offset: Option<u64>,
    pub(in crate::services::discord) new_session_id: Option<String>,
    pub(in crate::services::discord) defer_watcher_resume: bool,
    /// Reuse the persisted V2 status panel only when resuming the same
    /// in-flight turn. Fresh turns must allocate a new panel near the new
    /// response instead of editing an old panel buried in scrollback.
    pub(in crate::services::discord) reuse_status_panel_message: bool,
    pub(in crate::services::discord) completion_tx: Option<tokio::sync::oneshot::Sender<()>>,
    /// `true` ONLY at the two TUI external-input idle callers. Default `false`
    /// for every other bridge caller; used by footer/chrome decisions that need
    /// the origin without a `request_owner_name` string compare.
    pub(in crate::services::discord) is_external_input_tui_direct: bool,
    pub(in crate::services::discord) inflight_state: InflightTurnState,
}
