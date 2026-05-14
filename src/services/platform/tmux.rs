//! Tmux command abstraction layer.
//!
//! All tmux binary invocations MUST go through this module.
//! Callers in async contexts should use `tokio::task::spawn_blocking`.

use super::binary_resolver;
use std::io::Write;
use std::process::{Command, Output, Stdio};

/// Format session name as an exact-match target (prefix with `=`, suffix with
/// `:` so the target also resolves in pane-context commands).
///
/// `=name` alone matches any object with that exact name, but `send-keys` and
/// `display-message` require a target that resolves to a pane. Without the
/// trailing colon, `tmux send-keys -t =SESSION C-c` errors with "can't find
/// pane" and `display-message -t =SESSION '#{pane_pid}'` returns empty —
/// which made user-initiated turn-stop (⏳ reaction, !stop, /stop, watchdog)
/// silently fail to abort the running provider. `=SESSION:` reads as "exact
/// session named SESSION, default window/pane" and works for every tmux
/// command we use.
fn exact_target(session_name: &str) -> String {
    format!("={session_name}:")
}

fn tmux_command() -> Command {
    let mut cmd = Command::new("tmux");
    binary_resolver::apply_runtime_path(&mut cmd);
    cmd
}

/// Check if tmux is available on the system.
pub fn is_available() -> bool {
    tmux_command()
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Get tmux version string (e.g. "tmux 3.4").
pub fn version() -> Result<String, String> {
    let out = tmux_command()
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
    tmux_command()
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
    let mut cmd = tmux_command();
    cmd.args(["new-session", "-d", "-s", session_name]);
    if let Some(dir) = working_dir {
        cmd.args(["-c", dir]);
    }
    cmd.arg(shell_command);
    cmd.env_remove("CLAUDECODE");
    cmd.output()
        .map_err(|e| format!("Failed to create tmux session: {e}"))
}

fn log_kill_request(session_name: &str, reason: &str) {
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] ✂ tmux kill requested: session={session_name} reason={reason}");
}

fn log_kill_result(session_name: &str, reason: &str, output: &Output) {
    let ts = chrono::Local::now().format("%H:%M:%S");
    if output.status.success() {
        tracing::info!("  [{ts}] ✂ tmux kill succeeded: session={session_name} reason={reason}");
        return;
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        tracing::warn!(
            "  [{ts}] ⚠ tmux kill failed: session={session_name} reason={reason} status={}",
            output.status
        );
    } else {
        tracing::warn!(
            "  [{ts}] ⚠ tmux kill failed: session={session_name} reason={reason} status={} stderr={}",
            output.status,
            stderr
        );
    }
}

fn kill_session_output_internal(session_name: &str, reason: &str) -> std::io::Result<Output> {
    log_kill_request(session_name, reason);
    let output = tmux_command()
        .args(["kill-session", "-t", &exact_target(session_name)])
        .output();
    match &output {
        Ok(output) => log_kill_result(session_name, reason, output),
        Err(error) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ tmux kill spawn failed: session={session_name} reason={reason} error={error}"
            );
        }
    }
    output
}

/// Kill a tmux session. Returns true if the kill command succeeded.
pub fn kill_session_with_reason(session_name: &str, reason: &str) -> bool {
    kill_session_output_internal(session_name, reason)
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Kill a tmux session. Returns true if the kill command succeeded.
pub fn kill_session(session_name: &str) -> bool {
    kill_session_with_reason(session_name, "unspecified")
}

/// Kill a tmux session, returning full Output for error inspection.
pub fn kill_session_output_with_reason(
    session_name: &str,
    reason: &str,
) -> std::io::Result<Output> {
    kill_session_output_internal(session_name, reason)
}

/// Kill a tmux session, returning full Output for error inspection.
pub fn kill_session_output(session_name: &str) -> std::io::Result<Output> {
    kill_session_output_with_reason(session_name, "unspecified")
}

/// Kill a tmux session, returning an error on failure (for anyhow contexts).
#[allow(dead_code)]
pub fn kill_session_checked_with_reason(session_name: &str, reason: &str) -> Result<(), String> {
    let output = kill_session_output_internal(session_name, reason)
        .map_err(|e| format!("tmux kill-session spawn failed: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            Err(format!("tmux kill-session failed for {session_name}"))
        } else {
            Err(format!(
                "tmux kill-session failed for {session_name}: {stderr}"
            ))
        }
    }
}

/// Kill a tmux session, returning an error on failure (for anyhow contexts).
#[allow(dead_code)]
pub fn kill_session_checked(session_name: &str) -> Result<(), String> {
    kill_session_checked_with_reason(session_name, "unspecified")
}

/// Send keys to a tmux session.
///
/// `keys` are passed as separate arguments — typically `["some-text", "Enter"]`.
pub fn send_keys(session_name: &str, keys: &[&str]) -> Result<Output, String> {
    let target = exact_target(session_name);
    let mut args = vec!["send-keys", "-t", &target];
    args.extend(keys);
    tmux_command()
        .args(&args)
        .output()
        .map_err(|e| format!("tmux send-keys failed: {e}"))
}

/// Send literal text to a tmux session without interpreting tmux key names.
pub fn send_literal(session_name: &str, text: &str) -> Result<Output, String> {
    let target = exact_target(session_name);
    tmux_command()
        .args(["send-keys", "-t", &target, "-l", "--", text])
        .output()
        .map_err(|e| format!("tmux send-keys -l failed: {e}"))
}

/// Load literal text into a named tmux buffer via stdin.
pub fn load_buffer(buffer_name: &str, text: &str) -> Result<Output, String> {
    let mut child = tmux_command()
        .args(["load-buffer", "-b", buffer_name, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("tmux load-buffer failed: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| format!("tmux load-buffer stdin failed: {e}"))?;
    }
    child
        .wait_with_output()
        .map_err(|e| format!("tmux load-buffer wait failed: {e}"))
}

/// Paste a named tmux buffer to the active pane.
pub fn paste_buffer(session_name: &str, buffer_name: &str, delete: bool) -> Result<Output, String> {
    let target = exact_target(session_name);
    let args = paste_buffer_args(buffer_name, &target, delete);
    tmux_command()
        .args(&args)
        .output()
        .map_err(|e| format!("tmux paste-buffer failed: {e}"))
}

fn paste_buffer_args<'a>(buffer_name: &'a str, target: &'a str, delete: bool) -> Vec<&'a str> {
    // `-p` requests bracketed paste and `-r` keeps LF as LF instead of tmux's
    // default LF -> CR replacement, which can look like Enter in TUIs.
    let mut args = vec!["paste-buffer", "-p", "-r"];
    if delete {
        args.push("-d");
    }
    args.extend(["-b", buffer_name, "-t", target]);
    args
}

/// Return the PID of the active pane process for a tmux session.
#[cfg(unix)]
pub fn pane_pid(session_name: &str) -> Option<u32> {
    let output = tmux_command()
        .args([
            "display-message",
            "-p",
            "-t",
            &exact_target(session_name),
            "#{pane_pid}",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

/// Return the PID of the active pane process for a tmux session.
#[cfg(not(unix))]
pub fn pane_pid(_session_name: &str) -> Option<u32> {
    None
}

/// Capture pane content from a tmux session.
///
/// `scroll_back` is the number of lines to capture (negative = from bottom).
pub fn capture_pane(session_name: &str, scroll_back: i32) -> Option<String> {
    let scroll = scroll_back.to_string();
    tmux_command()
        .args([
            "capture-pane",
            "-p",
            "-t",
            // `capture-pane` expects a session target here, not an exact-match
            // pane target, so pass the plain session name.
            session_name,
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
    let out = tmux_command()
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
    tmux_command()
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
    let _ = tmux_command()
        .args(["set-option", "-t", &exact_target(session_name), key, value])
        .output();
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;

    #[test]
    fn exact_target_includes_trailing_colon_for_pane_context_commands() {
        // `=name` alone fails for `send-keys` / `display-message` with
        // "can't find pane"; the trailing colon makes the target resolve to
        // the default pane in the matched session. Pin this format here so a
        // future "simplification" doesn't silently re-break user-initiated
        // turn-stop again.
        assert_eq!(
            exact_target("AgentDesk-claude-adk-cc"),
            "=AgentDesk-claude-adk-cc:"
        );
    }
}

#[cfg(test)]
mod paste_tests {
    use super::*;

    #[test]
    fn paste_buffer_args_request_bracketed_lf_preserving_paste() {
        assert_eq!(
            paste_buffer_args("agentdesk-buffer", "=AgentDesk-claude-adk-cc:", true),
            vec![
                "paste-buffer",
                "-p",
                "-r",
                "-d",
                "-b",
                "agentdesk-buffer",
                "-t",
                "=AgentDesk-claude-adk-cc:"
            ]
        );
    }
}
