use sha2::{Digest, Sha256};

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

fn host_temp_namespace() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "unknown-host".to_string())
}

fn session_temp_prefix(session_name: &str) -> String {
    let host = host_temp_namespace();
    let mut hasher = Sha256::new();
    hasher.update(current_tmux_owner_marker().as_bytes());
    hasher.update(b"|");
    hasher.update(host.as_bytes());
    let digest = hasher.finalize();
    let runtime_hash = format!("{:x}", digest);
    format!(
        "agentdesk-{}-{}-{}",
        &runtime_hash[..12],
        host,
        session_name
    )
}

/// Build a path for an AgentDesk runtime temp file.
/// Example: session_temp_path("mySession", "jsonl") -> "/tmp/agentdesk-<runtime>-<host>-mySession.jsonl"
pub fn session_temp_path(session_name: &str, extension: &str) -> String {
    format!(
        "{}/{}.{}",
        agentdesk_temp_dir(),
        session_temp_prefix(session_name),
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

#[cfg(test)]
mod tests {
    use super::session_temp_path;

    #[test]
    fn session_temp_path_is_namespaced_by_runtime_root() {
        let _lock = crate::services::discord::runtime_store::lock_test_env();
        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
        let previous_host = std::env::var_os("HOSTNAME");

        unsafe {
            std::env::set_var("HOSTNAME", "test-host");
            std::env::set_var("AGENTDESK_ROOT_DIR", "/tmp/adk-runtime-a");
        }
        let path_a = session_temp_path("tmux-a", "jsonl");

        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", "/tmp/adk-runtime-b") };
        let path_b = session_temp_path("tmux-a", "jsonl");

        match previous_root {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
        match previous_host {
            Some(value) => unsafe { std::env::set_var("HOSTNAME", value) },
            None => unsafe { std::env::remove_var("HOSTNAME") },
        }

        assert_ne!(path_a, path_b);
        assert!(path_a.contains("tmux-a"));
        assert!(path_b.contains("tmux-a"));
    }
}
