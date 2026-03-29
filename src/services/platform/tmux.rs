//! Tmux command abstraction layer.
//!
//! All tmux binary invocations MUST go through this module.
//! Callers in async contexts should use `tokio::task::spawn_blocking`.

use std::process::{Command, Output, Stdio};

/// Format session name as exact-match target (prefix with `=`).
fn exact_target(session_name: &str) -> String {
    format!("={session_name}")
}

/// Check if tmux is available on the system.
pub fn is_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Get tmux version string (e.g. "tmux 3.4").
pub fn version() -> Result<String, String> {
    let out = Command::new("tmux")
        .arg("-V")
        .output()
        .map_err(|e| format!("tmux not found: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err("tmux -V failed".to_string())
    }
}

/// Check if a named tmux session exists.
pub fn has_session(session_name: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", &exact_target(session_name)])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Create a new detached tmux session.
///
/// Returns the raw `Output` so callers can inspect stderr on failure.
/// If `working_dir` is Some, the session starts in that directory.
pub fn create_session(
    session_name: &str,
    working_dir: Option<&str>,
    shell_command: &str,
) -> Result<Output, String> {
    let mut cmd = Command::new("tmux");
    cmd.args(["new-session", "-d", "-s", session_name]);
    if let Some(dir) = working_dir {
        cmd.args(["-c", dir]);
    }
    cmd.arg(shell_command);
    cmd.env_remove("CLAUDECODE");
    cmd.output()
        .map_err(|e| format!("Failed to create tmux session: {e}"))
}

/// Kill a tmux session. Returns true if the kill command succeeded.
pub fn kill_session(session_name: &str) -> bool {
    Command::new("tmux")
        .args(["kill-session", "-t", &exact_target(session_name)])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Kill a tmux session, returning full Output for error inspection.
pub fn kill_session_output(session_name: &str) -> std::io::Result<Output> {
    Command::new("tmux")
        .args(["kill-session", "-t", &exact_target(session_name)])
        .output()
}

/// Kill a tmux session, returning an error on failure (for anyhow contexts).
pub fn kill_session_checked(session_name: &str) -> Result<(), String> {
    let status = Command::new("tmux")
        .args(["kill-session", "-t", &exact_target(session_name)])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("tmux kill-session spawn failed: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("tmux kill-session failed for {session_name}"))
    }
}

/// Send keys to a tmux session.
///
/// `keys` are passed as separate arguments — typically `["some-text", "Enter"]`.
pub fn send_keys(session_name: &str, keys: &[&str]) -> Result<Output, String> {
    let target = exact_target(session_name);
    let mut args = vec!["send-keys", "-t", &target];
    args.extend(keys);
    Command::new("tmux")
        .args(&args)
        .output()
        .map_err(|e| format!("tmux send-keys failed: {e}"))
}

/// Capture pane content from a tmux session.
///
/// `scroll_back` is the number of lines to capture (negative = from bottom).
pub fn capture_pane(session_name: &str, scroll_back: i32) -> Option<String> {
    let scroll = scroll_back.to_string();
    Command::new("tmux")
        .args([
            "capture-pane",
            "-p",
            "-t",
            &exact_target(session_name),
            "-S",
            &scroll,
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
}

/// List all tmux session names.
pub fn list_session_names() -> Result<Vec<String>, String> {
    let out = Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
        .map_err(|e| format!("tmux list-sessions failed: {e}"))?;
    if !out.status.success() {
        return Err("tmux list-sessions returned non-zero".to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect())
}

/// Check if a session has any live (non-dead) panes.
pub fn has_live_pane(session_name: &str) -> bool {
    if !has_session(session_name) {
        return false;
    }
    Command::new("tmux")
        .args([
            "list-panes",
            "-t",
            &exact_target(session_name),
            "-F",
            "#{pane_dead}",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|line| line.trim() == "0")
        })
        .unwrap_or(false)
}

/// Set a tmux session option. Errors are silently ignored (fire-and-forget).
pub fn set_option(session_name: &str, key: &str, value: &str) {
    let _ = Command::new("tmux")
        .args(["set-option", "-t", &exact_target(session_name), key, value])
        .output();
}
