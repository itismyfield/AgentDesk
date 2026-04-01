//! SessionRuntime trait — abstract adapter for session backends (#123).
//!
//! Currently the only implementation is `TmuxRuntime`. A future `ProcessRuntime`
//! can be added without touching business logic in turn_bridge.rs / recovery.rs.

use anyhow::Result;

/// Trait abstracting session runtime operations.
///
/// Business logic (recovery, turn_bridge) should call these methods
/// instead of invoking tmux commands directly.
pub trait SessionRuntime: Send + Sync {
    /// Build a session name from the channel/agent identifier.
    fn build_session_name(&self, channel_name: &str) -> String;

    /// Get output and input paths for a session.
    /// Returns (output_jsonl_path, input_fifo_path).
    fn runtime_paths(&self, session_name: &str) -> (String, String);

    /// Check if a session exists (may or may not be alive).
    fn session_exists(&self, session_name: &str) -> bool;

    /// Check if a session has a live, responsive pane.
    fn session_has_live_pane(&self, session_name: &str) -> bool;

    /// Check if a session is ready to accept input.
    fn session_ready_for_input(&self, session_name: &str) -> bool;

    /// Kill/terminate a session.
    fn kill_session(&self, session_name: &str) -> Result<()>;

    /// Record why a session exited (for diagnostics).
    fn record_exit_reason(&self, session_name: &str, reason: &str);

    /// Build a diagnostic message for a dead session.
    fn death_diagnostic(&self, session_name: &str, output_path: Option<&str>) -> Option<String>;
}

/// Tmux-based implementation of SessionRuntime.
pub struct TmuxRuntime {
    /// Prefix for session names (e.g., "AgentDesk-claude-")
    pub prefix: String,
}

impl TmuxRuntime {
    pub fn new(prefix: &str) -> Self {
        Self {
            prefix: prefix.to_string(),
        }
    }
}

impl SessionRuntime for TmuxRuntime {
    fn build_session_name(&self, channel_name: &str) -> String {
        format!("{}{}", self.prefix, channel_name)
    }

    fn runtime_paths(&self, session_name: &str) -> (String, String) {
        #[cfg(unix)]
        {
            use crate::services::tmux_common::session_temp_path;
            (
                session_temp_path(session_name, "jsonl"),
                session_temp_path(session_name, "input"),
            )
        }
        #[cfg(not(unix))]
        {
            let tmp = std::env::temp_dir();
            (
                tmp.join(format!("agentdesk-{}.jsonl", session_name))
                    .display()
                    .to_string(),
                tmp.join(format!("agentdesk-{}.input", session_name))
                    .display()
                    .to_string(),
            )
        }
    }

    fn session_exists(&self, session_name: &str) -> bool {
        crate::services::tmux_diagnostics::tmux_session_exists(session_name)
    }

    fn session_has_live_pane(&self, session_name: &str) -> bool {
        crate::services::tmux_diagnostics::tmux_session_has_live_pane(session_name)
    }

    fn session_ready_for_input(&self, session_name: &str) -> bool {
        // Check if tmux session exists and has a responsive pane
        if !self.session_exists(session_name) {
            return false;
        }
        // Use the provider-specific readiness check
        crate::services::provider::tmux_session_ready_for_input(session_name)
    }

    fn kill_session(&self, session_name: &str) -> Result<()> {
        crate::services::platform::tmux::kill_session_checked(session_name)
            .map_err(|e| anyhow::anyhow!(e))
    }

    fn record_exit_reason(&self, session_name: &str, reason: &str) {
        crate::services::tmux_diagnostics::record_tmux_exit_reason(session_name, reason);
    }

    fn death_diagnostic(&self, session_name: &str, output_path: Option<&str>) -> Option<String> {
        crate::services::tmux_diagnostics::build_tmux_death_diagnostic(session_name, output_path)
    }
}
