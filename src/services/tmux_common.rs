use crate::services::tmux_diagnostics::clear_tmux_exit_reason;

/// Get the platform-appropriate temp directory for RemoteCC runtime files.
pub fn remotecc_temp_dir() -> String {
    std::env::temp_dir().display().to_string()
}

/// Build a path for a RemoteCC runtime temp file.
/// Example: session_temp_path("mySession", "jsonl") → "/tmp/remotecc-mySession.jsonl"
pub fn session_temp_path(session_name: &str, extension: &str) -> String {
    format!("{}/remotecc-{}.{}", remotecc_temp_dir(), session_name, extension)
}

/// Get the current RemoteCC runtime root marker for tmux session ownership.
pub fn current_tmux_owner_marker() -> String {
    std::env::var("REMOTECC_ROOT_DIR")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| dirs::home_dir().map(|home| home.join(".remotecc").display().to_string()))
        .unwrap_or_else(|| ".remotecc".to_string())
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
