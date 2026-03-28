use crate::services::tmux_diagnostics::clear_tmux_exit_reason;

/// Format a tmux session name as an exact-match target.
///
/// tmux `-t` flags perform prefix matching by default: `-t foo` matches
/// both `foo` and `foo-bar`.  Prefixing with `=` forces exact matching,
/// preventing the wrong session from being targeted when session names
/// share a common prefix (e.g. main vs thread sessions).
pub fn tmux_exact_target(session_name: &str) -> String {
    format!("={}", session_name)
}

/// Get the platform-appropriate temp directory for AgentDesk runtime files.
pub fn agentdesk_temp_dir() -> String {
    std::env::temp_dir().display().to_string()
}

/// Build a path for an AgentDesk runtime temp file.
/// Example: session_temp_path("mySession", "jsonl") -> "/tmp/agentdesk-mySession.jsonl"
pub fn session_temp_path(session_name: &str, extension: &str) -> String {
    format!(
        "{}/agentdesk-{}.{}",
        agentdesk_temp_dir(),
        session_name,
        extension
    )
}

/// Get the current AgentDesk runtime root marker for tmux session ownership.
pub fn current_tmux_owner_marker() -> String {
    crate::config::runtime_root()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| ".adk/release".to_string())
}

/// Path to the owner marker file for a tmux session.
pub fn tmux_owner_path(tmux_session_name: &str) -> String {
    session_temp_path(tmux_session_name, "owner")
}

/// Write the owner marker file so this runtime claims the tmux session.
pub fn write_tmux_owner_marker(tmux_session_name: &str) -> Result<(), String> {
    clear_tmux_exit_reason(tmux_session_name);
    let owner_path = tmux_owner_path(tmux_session_name);
    std::fs::write(&owner_path, current_tmux_owner_marker())
        .map_err(|e| format!("Failed to write tmux owner marker: {}", e))
}
