//! The Codex Direct TUI launch script, defined once; `codex` names it only inside itself.

/// Paths and timing produced while preparing a Codex Direct TUI launch script.
pub(crate) struct CodexTuiLaunchScript {
    pub(super) fence: crate::services::tmux_common::runtime_hint_fence::RuntimeHintFence,
    pub(super) prepared: crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation,
    pub(super) script_path: String,
    pub(super) owner_path: String,
    pub(super) rollout_modified_since: std::time::SystemTime,
    /// The Discord handoff reserved before launch, held until the tail returns.
    pub(super) handoff: crate::services::tui_prompt_dedupe::HandoffGuard,
}
