//! Verified source selection is pinned only for the designated test channel.

use super::session::source_observation::{CodexSourceMode, codex_source_mode_snapshot};

pub(crate) const CANARY_CHANNEL: u64 = 1_509_350_778_043_895_902;
pub(crate) const CANARY_TMUX: &str = "AgentDesk-codex-adk-codex-tui-e2e";

pub(crate) fn enabled_for(tmux: &str) -> bool {
    tmux == CANARY_TMUX && codex_source_mode_snapshot() == CodexSourceMode::Verified
}

pub(crate) fn launch_policy(
    tmux: &str,
    channel: Option<u64>,
) -> Result<&'static str, &'static str> {
    match codex_source_mode_snapshot() {
        CodexSourceMode::Verified if tmux == CANARY_TMUX && channel == Some(CANARY_CHANNEL) => {
            Ok("verified")
        }
        CodexSourceMode::Verified => Ok("legacy"),
        mode => mode.launch_policy(),
    }
}
