//! Tmux command abstraction layer.
//!
//! All tmux binary invocations MUST go through this module.
//! Callers in async contexts should use `tokio::task::spawn_blocking`.

use super::binary_resolver;
use crate::services::process::{configure_child_process_group, wait_with_output_timeout};
use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

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

fn wait_for_tmux_output(
    mut command: Command,
    timeout: Duration,
    label: &str,
) -> Result<Output, String> {
    configure_child_process_group(&mut command);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = command
        .spawn()
        .map_err(|e| format!("{label} spawn failed: {e}"))?;
    wait_with_output_timeout(child, timeout, label)
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

fn kill_session_output_internal_with_timeout(
    session_name: &str,
    reason: &str,
    timeout: Duration,
) -> Result<Output, String> {
    log_kill_request(session_name, reason);
    let mut command = tmux_command();
    command.args(["kill-session", "-t", &exact_target(session_name)]);
    let output = wait_for_tmux_output(command, timeout, "tmux kill-session");
    match &output {
        Ok(output) => log_kill_result(session_name, reason, output),
        Err(error) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ tmux kill failed: session={session_name} reason={reason} error={error}"
            );
        }
    }
    output
}

/// Kill a tmux session. Returns true if the kill command succeeded.
pub fn kill_session(session_name: &str, reason: &str) -> bool {
    kill_session_output_internal(session_name, reason)
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Kill a tmux session, returning full Output for error inspection.
pub fn kill_session_output(session_name: &str, reason: &str) -> std::io::Result<Output> {
    kill_session_output_internal(session_name, reason)
}

/// Kill a tmux session, enforcing a caller-supplied timeout.
pub fn kill_session_output_timeout(
    session_name: &str,
    reason: &str,
    timeout: Duration,
) -> Result<Output, String> {
    kill_session_output_internal_with_timeout(session_name, reason, timeout)
}

/// Kill a tmux session, returning an error on failure (for anyhow contexts).
#[allow(dead_code)]
pub fn kill_session_checked(session_name: &str, reason: &str) -> Result<(), String> {
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

/// Send keys to a tmux session, enforcing a caller-supplied timeout.
pub fn send_keys_timeout(
    session_name: &str,
    keys: &[&str],
    timeout: Duration,
) -> Result<Output, String> {
    let target = exact_target(session_name);
    let mut args = vec!["send-keys", "-t", &target];
    args.extend(keys);
    let mut command = tmux_command();
    command.args(&args);
    wait_for_tmux_output(command, timeout, "tmux send-keys")
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

/// A single tmux session as enumerated by [`list_sessions_with_pane_command`].
/// Field order mirrors the tmux `-F` format string in that helper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumeratedSession {
    pub session_name: String,
    /// `#{pane_current_command}` of the session's active pane — empty when
    /// tmux couldn't resolve a pane (rare; treated as "unknown" by callers).
    pub pane_current_command: String,
    /// `#{pane_pid}` of the session's active pane. `0` when tmux couldn't
    /// resolve a pane (e.g. dying session) — callers must treat zero as
    /// "no fallback available". Lets the matcher fall back to the live
    /// process argv when `pane_current_command` is unreliable (e.g.
    /// providers that rewrite their own process title — see #2470).
    pub pane_pid: u32,
}

/// A tmux session and the PID of the tmux server that owns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionServer {
    pub session_name: String,
    /// `#{pid}` is the tmux server PID. It is shared by every session on the
    /// same socket and may be 0 if tmux cannot resolve it.
    pub server_pid: u32,
}

/// List every tmux session along with its active pane's `pane_current_command`.
/// Used by `SessionDiscovery` (Epic #2285 / E2) to feed the `SessionMatcher`
/// with both the session name *and* the live provider fingerprint in a single
/// tmux invocation, avoiding a follow-up `display-message` per session.
///
/// The `-F` format uses `|` as a field separator. Session names produced by
/// [`build_tmux_session_name`](crate::services::provider::ProviderKind::build_tmux_session_name)
/// only contain `[A-Za-z0-9_-]`, so `|` is safe. Defensive callers should
/// nonetheless `splitn(2, '|')` to avoid surprises from operator-created
/// sessions.
pub fn list_sessions_with_pane_command() -> Result<Vec<EnumeratedSession>, String> {
    let out = tmux_command()
        .args([
            "list-sessions",
            "-F",
            "#{session_name}|#{pane_current_command}|#{pane_pid}",
        ])
        .output()
        .map_err(|e| format!("tmux list-sessions failed: {e}"))?;
    if !out.status.success() {
        return Err("tmux list-sessions returned non-zero".to_string());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut sessions = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(3, '|');
        let session_name = parts.next().unwrap_or("").trim().to_string();
        if session_name.is_empty() {
            continue;
        }
        let pane_current_command = parts.next().unwrap_or("").trim().to_string();
        let pane_pid = parts
            .next()
            .unwrap_or("")
            .trim()
            .parse::<u32>()
            .unwrap_or(0);
        sessions.push(EnumeratedSession {
            session_name,
            pane_current_command,
            pane_pid,
        });
    }
    Ok(sessions)
}

/// List every tmux session along with its owning tmux server PID.
pub fn list_sessions_with_server_pid() -> Result<Vec<SessionServer>, String> {
    let mut command = tmux_command();
    command.env_remove("TMUX");
    let out = command
        .args(["list-sessions", "-F", "#{session_name}|#{pid}"])
        .output()
        .map_err(|e| format!("tmux list-sessions failed: {e}"))?;
    if !out.status.success() {
        return Err("tmux list-sessions returned non-zero".to_string());
    }
    Ok(parse_sessions_with_server_pid(&String::from_utf8_lossy(
        &out.stdout,
    )))
}

fn parse_sessions_with_server_pid(text: &str) -> Vec<SessionServer> {
    let mut sessions = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, '|');
        let session_name = parts.next().unwrap_or("").trim().to_string();
        if session_name.is_empty() {
            continue;
        }
        let server_pid = parts
            .next()
            .unwrap_or("")
            .trim()
            .parse::<u32>()
            .unwrap_or(0);
        sessions.push(SessionServer {
            session_name,
            server_pid,
        });
    }
    sessions
}

/// Read the command line of a live process by PID, used as a fallback
/// fingerprint source when `pane_current_command` is unreliable. Providers that
/// rewrite their own process title (e.g. claude code 2.1.143 sets the title
/// to its version string) cause tmux's `#{pane_current_command}` to report
/// the rewritten value, hiding the underlying binary. Wrapper-based panes can
/// also report a generic runner such as `node` while the provider-specific
/// companion path appears later in argv.
///
/// Returns `None` when the PID is zero/missing, the OS call fails, or the
/// output is empty. Callers should treat `None` as "no fallback available"
/// and propagate the original pane-command result.
///
/// Implementation notes:
/// - macOS: `ps -ww -p PID -o args=` — `args` includes the full command line
///   and `-ww` avoids width truncation.
/// - Linux: `/proc/{PID}/exe` plus `/proc/{PID}/cmdline`; `cmdline` can be
///   overwritten by process-title tricks, so include the kernel executable
///   symlink as a stable first candidate.
/// - Windows: no implementation today (the discovery loop runs on macOS/Linux
///   hosts; if a Windows operator host materialises we'll add `wmic` later).
pub fn read_process_args(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }

    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("ps")
            .args(["-ww", "-p", &pid.to_string(), "-o", "args="])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if line.is_empty() {
            return None;
        }
        Some(line)
    }

    #[cfg(target_os = "linux")]
    {
        let exe = std::fs::read_link(format!("/proc/{}/exe", pid))
            .ok()
            .map(|path| path.display().to_string());
        let raw = std::fs::read(format!("/proc/{}/cmdline", pid)).ok()?;
        let mut args: Vec<String> = raw
            .split(|&b| b == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).to_string())
            .collect();
        if let Some(exe) = exe {
            args.insert(0, exe);
        }
        if args.is_empty() {
            return None;
        }
        Some(args.join(" "))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = pid;
        None
    }
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

#[cfg(test)]
mod live_pane_tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    fn unique_test_session_name() -> String {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        format!(
            "AgentDesk-live-pane-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )
    }

    #[test]
    fn has_live_pane_reports_live_tmux_session() {
        let _guard = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());

        if !is_available() {
            eprintln!("skipping has_live_pane test: tmux is not available");
            return;
        }

        let session = unique_test_session_name();
        let output = create_session(&session, None, "sleep 60")
            .expect("temporary tmux session should be created");

        if !output.status.success() {
            panic!(
                "temporary tmux session should be created: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let result = has_live_pane(&session);
        let _ = kill_session(&session, "live pane test cleanup");

        assert!(result);
    }
}

#[cfg(test)]
mod session_server_tests {
    use super::*;

    #[test]
    fn parse_sessions_with_server_pid_skips_empty_names_and_defaults_bad_pids() {
        let sessions =
            parse_sessions_with_server_pid("AgentDesk-a|123\n|456\noperator|not-a-pid\n");

        assert_eq!(
            sessions,
            vec![
                SessionServer {
                    session_name: "AgentDesk-a".to_string(),
                    server_pid: 123,
                },
                SessionServer {
                    session_name: "operator".to_string(),
                    server_pid: 0,
                },
            ]
        );
    }
}

#[cfg(all(test, unix))]
mod timeout_tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    struct PathOverride {
        _guard: std::sync::MutexGuard<'static, ()>,
        previous: Option<std::ffi::OsString>,
    }

    impl PathOverride {
        fn prepend(path: &std::path::Path) -> Self {
            let guard = crate::config::shared_test_env_lock()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let previous = std::env::var_os("PATH");
            let joined = match &previous {
                Some(old) => {
                    let mut paths = vec![path.to_path_buf()];
                    paths.extend(std::env::split_paths(old));
                    std::env::join_paths(paths).expect("join PATH")
                }
                None => path.as_os_str().to_os_string(),
            };
            unsafe { std::env::set_var("PATH", joined) };
            Self {
                _guard: guard,
                previous,
            }
        }
    }

    impl Drop for PathOverride {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var("PATH", value) },
                None => unsafe { std::env::remove_var("PATH") },
            }
        }
    }

    fn write_fake_tmux(dir: &std::path::Path, body: &str) {
        let path = dir.join("tmux");
        let mut file = std::fs::File::create(&path).expect("fake tmux");
        writeln!(file, "#!/bin/sh").expect("shebang");
        writeln!(file, "{body}").expect("body");
        let mut permissions = std::fs::metadata(&path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).expect("chmod");
    }

    #[test]
    fn send_keys_timeout_reports_hung_tmux() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        write_fake_tmux(temp.path(), "sleep 5");
        let _path = PathOverride::prepend(temp.path());

        let error = send_keys_timeout(
            "agentdesk-test",
            &["/compact", "Enter"],
            Duration::from_millis(20),
        )
        .expect_err("hung tmux should time out");

        assert!(error.contains("timed out"), "unexpected error: {error}");
    }

    #[test]
    fn send_keys_timeout_preserves_tmux_stderr_on_fast_failure() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        write_fake_tmux(temp.path(), "echo 'no such session' >&2; exit 2");
        let _path = PathOverride::prepend(temp.path());

        let output = send_keys_timeout(
            "agentdesk-test",
            &["/compact", "Enter"],
            Duration::from_secs(1),
        )
        .expect("fake tmux should exit before timeout");

        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("no such session"),
            "stderr should be preserved, got: {stderr:?}"
        );
    }

    #[test]
    fn kill_session_timeout_reports_hung_tmux() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        write_fake_tmux(temp.path(), "sleep 5");
        let _path = PathOverride::prepend(temp.path());

        let error = kill_session_output_timeout(
            "agentdesk-test",
            "test timeout",
            Duration::from_millis(20),
        )
        .expect_err("hung tmux should time out");

        assert!(error.contains("timed out"), "unexpected error: {error}");
    }
}
