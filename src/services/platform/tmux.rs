//! Tmux command abstraction layer.
//!
//! All tmux binary invocations MUST go through this module.
//! Callers in async contexts should use `tokio::task::spawn_blocking`.

use super::binary_resolver;
use crate::services::process::{configure_child_process_group, wait_with_output_timeout};
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

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

fn is_blank_session_name(session_name: &str) -> bool {
    session_name.trim().is_empty()
}

fn tmux_command() -> Command {
    let mut cmd = Command::new("tmux");
    binary_resolver::apply_runtime_path(&mut cmd);
    cmd
}

#[cfg(unix)]
fn failed_output(stderr: &str) -> Output {
    use std::os::unix::process::ExitStatusExt;

    Output {
        status: std::process::ExitStatus::from_raw(64 << 8),
        stdout: Vec::new(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

#[cfg(windows)]
fn failed_output(stderr: &str) -> Output {
    use std::os::windows::process::ExitStatusExt;

    Output {
        status: std::process::ExitStatus::from_raw(64),
        stdout: Vec::new(),
        stderr: stderr.as_bytes().to_vec(),
    }
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

mod availability;

pub use availability::{
    cached_unavailable_due_to_missing, invalidate_cache as invalidate_availability_cache,
    is_available, mark_available_from_live_session,
};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionPresence {
    Present,
    Missing,
    ProbeFailed,
}

/// Check whether a named tmux session definitely exists or definitely does not.
/// Transport, socket, permission, timeout, and unexpected tmux errors remain
/// distinguishable from a confirmed missing session so destructive callers can
/// fail closed.
pub(crate) fn session_presence(session_name: &str) -> SessionPresence {
    if is_blank_session_name(session_name) {
        return SessionPresence::ProbeFailed;
    }

    let mut command = tmux_command();
    command.args(["has-session", "-t", &exact_target(session_name)]);
    let Ok(output) = wait_for_tmux_output(command, Duration::from_secs(3), "tmux has-session")
    else {
        return SessionPresence::ProbeFailed;
    };
    if output.status.success() {
        return SessionPresence::Present;
    }

    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    if stderr.contains("can't find session")
        || stderr.contains("no such session")
        || stderr.contains("no server running")
    {
        SessionPresence::Missing
    } else {
        SessionPresence::ProbeFailed
    }
}

/// Compatibility boolean for non-destructive callers. Probe failures continue
/// to read as unavailable; destructive recovery must use [`session_presence`].
pub fn has_session(session_name: &str) -> bool {
    session_presence(session_name) == SessionPresence::Present
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
    let output = cmd
        .output()
        .map_err(|e| format!("Failed to create tmux session: {e}"))?;
    if output.status.success() {
        install_dead_marker_hooks(session_name);
    }
    Ok(output)
}

fn sh_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn tmux_double_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn dead_marker_shell_command(session_name: &str, cleanup_hook_index: Option<u64>) -> String {
    let marker_path = crate::services::tmux_common::session_dead_marker_path(session_name);
    let parent = std::path::Path::new(&marker_path)
        .parent()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| std::env::temp_dir().display().to_string());
    let mut shell = format!(
        "mkdir -p {}; : > {}",
        sh_single_quote(&parent),
        sh_single_quote(&marker_path)
    );
    if let Some(index) = cleanup_hook_index {
        shell.push_str(&format!(
            "; tmux set-hook -g -u {} >/dev/null 2>&1; tmux set-hook -g -u {} >/dev/null 2>&1",
            sh_single_quote(&format!("pane-exited[{index}]")),
            sh_single_quote(&format!("session-closed[{index}]"))
        ));
    }
    shell
}

fn dead_marker_hook_command(session_name: &str, cleanup_hook_index: Option<u64>) -> String {
    let shell = dead_marker_shell_command(session_name, cleanup_hook_index);
    format!("run-shell -b {}", sh_single_quote(&shell))
}

fn dead_marker_global_hook_index(session_name: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    session_name.hash(&mut hasher);
    // tmux hook arrays accept numeric indexes, but older builds are less happy
    // with full-width u64 values. Keep the deterministic namespace compact.
    hasher.finish() % 1_000_000_000
}

fn active_pane_id(session_name: &str) -> Option<String> {
    tmux_command()
        .args([
            "display-message",
            "-p",
            "-t",
            &exact_target(session_name),
            "#{pane_id}",
        ])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|pane_id| !pane_id.is_empty())
}

fn install_dead_marker_hooks(session_name: &str) {
    let target = exact_target(session_name);
    let command = dead_marker_hook_command(session_name, None);
    for hook in ["pane-exited", "session-closed"] {
        let output = tmux_command()
            .args(["set-hook", "-a", "-t", &target, hook, &command])
            .output();
        match output {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::warn!(
                    "tmux dead-marker hook install failed: session={session_name} hook={hook} status={} stderr={}",
                    output.status,
                    stderr.trim()
                );
            }
            Err(error) => {
                tracing::warn!(
                    "tmux dead-marker hook install failed: session={session_name} hook={hook} error={error}"
                );
            }
        }
    }

    let Some(pane_id) = active_pane_id(session_name) else {
        return;
    };
    let index = dead_marker_global_hook_index(session_name);
    let command = dead_marker_hook_command(session_name, Some(index));
    let hooks = [
        (
            format!("pane-exited[{index}]"),
            format!("#{{==:#{{hook_pane}},{pane_id}}}"),
        ),
        (
            format!("session-closed[{index}]"),
            format!("#{{==:#{{hook_session_name}},{session_name}}}"),
        ),
    ];
    for (hook_name, condition) in hooks {
        let guarded_command = format!(
            "if-shell -F {} {}",
            sh_single_quote(&condition),
            tmux_double_quote(&command)
        );
        let output = tmux_command()
            .args(["set-hook", "-g", &hook_name, &guarded_command])
            .output();
        match output {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::warn!(
                    "tmux dead-marker global hook install failed: session={session_name} hook={hook_name} status={} stderr={}",
                    output.status,
                    stderr.trim()
                );
            }
            Err(error) => {
                tracing::warn!(
                    "tmux dead-marker global hook install failed: session={session_name} hook={hook_name} error={error}"
                );
            }
        }
    }
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
    if is_blank_session_name(session_name) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!("  [{ts}] ⚠ refusing tmux kill for blank session name reason={reason}");
        return Ok(failed_output("refusing tmux kill for blank session name\n"));
    }
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
    if is_blank_session_name(session_name) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!("  [{ts}] ⚠ refusing tmux kill for blank session name reason={reason}");
        return Err("refusing tmux kill for blank session name".to_string());
    }
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

// #3034: raw (non-bracketed) paste arg builder; exercised only by the tmux
// arg-builder unit tests below now that the raw-paste entrypoint is unused.
#[allow(dead_code)]
fn paste_buffer_raw_args<'a>(buffer_name: &'a str, target: &'a str, delete: bool) -> Vec<&'a str> {
    let mut args = vec!["paste-buffer", "-r"];
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

/// Return the current working directory of the active pane for a tmux session
/// (`#{pane_current_path}`).
///
/// #3208: a Discord-hosted Claude TUI may run in a rotating worktree
/// (`~/.adk/release/worktrees/claude-adk-cc-<ts>`) whose cwd differs from the
/// channel's configured workspace (the DB-restored cwd is sometimes ignored at
/// turn start). The follow-up readiness path needs the *actual* running cwd to
/// resolve the live JSONL transcript; the configured workspace path resolves to
/// a stale/empty Claude project dir, so the structured turn-state probe reports
/// `Unknown` and the screen-marker fallback then false-flags a genuinely-idle
/// (background-agents-running) turn as busy. Returns `None` when the session has
/// no live pane or tmux is unavailable.
pub fn pane_current_path(session_name: &str) -> Option<String> {
    if is_blank_session_name(session_name) {
        return None;
    }
    let output = tmux_command()
        .args([
            "display-message",
            "-p",
            "-t",
            &exact_target(session_name),
            "#{pane_current_path}",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() { None } else { Some(path) }
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

/// Capture pane content from a tmux session while preserving ANSI attributes.
///
/// `scroll_back` is the number of lines to capture (negative = from bottom).
pub fn capture_pane_with_escapes(session_name: &str, scroll_back: i32) -> Option<String> {
    let scroll = scroll_back.to_string();
    tmux_command()
        .args([
            "capture-pane",
            "-e",
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

/// #3212 (codex P1): the wall-clock start time of a process, as a
/// [`std::time::SystemTime`]. The follow-up readiness resolver uses this as the
/// launch-mtime cutoff for the cwd-mtime transcript fallback: only transcripts
/// modified at/after the live Claude session's launch may be adopted, so a
/// finished prior session's stale same-cwd transcript can never be mistaken for
/// the live one (false-ready).
///
/// Derived from elapsed-since-start (`ps -o etime=`) rather than the absolute
/// `lstart` to dodge locale/timezone parsing of the human date. `etime` format
/// is `[[DD-]HH:]MM:SS`. Returns `None` when the PID is zero/missing or the OS
/// call fails — callers MUST treat `None` conservatively (no fallback adoption).
#[cfg(unix)]
pub fn process_start_time(pid: u32) -> Option<std::time::SystemTime> {
    if pid == 0 {
        return None;
    }
    let out = std::process::Command::new("ps")
        .args(["-o", "etime=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let elapsed = parse_ps_etime(String::from_utf8_lossy(&out.stdout).trim())?;
    std::time::SystemTime::now().checked_sub(elapsed)
}

/// Return the start time of a process. No implementation on non-unix hosts.
#[cfg(not(unix))]
pub fn process_start_time(_pid: u32) -> Option<std::time::SystemTime> {
    None
}

/// Parse a `ps -o etime=` field (`[[DD-]HH:]MM:SS`) into an elapsed
/// [`std::time::Duration`]. Returns `None` for empty/malformed input so the
/// caller falls back to the conservative no-cutoff path.
#[cfg(unix)]
fn parse_ps_etime(raw: &str) -> Option<std::time::Duration> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let (days, hms) = match raw.split_once('-') {
        Some((d, rest)) => (d.parse::<u64>().ok()?, rest),
        None => (0, raw),
    };
    let parts: Vec<&str> = hms.split(':').collect();
    let (hours, minutes, seconds) = match parts.as_slice() {
        [h, m, s] => (
            h.parse::<u64>().ok()?,
            m.parse::<u64>().ok()?,
            s.parse::<u64>().ok()?,
        ),
        [m, s] => (0, m.parse::<u64>().ok()?, s.parse::<u64>().ok()?),
        _ => return None,
    };
    Some(std::time::Duration::from_secs(
        days * 86_400 + hours * 3_600 + minutes * 60 + seconds,
    ))
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

/// #3635: three-state liveness of a tmux session's panes. Unlike [`has_live_pane`]
/// (which collapses both "session absent" and "probe failed" to `false`), this
/// distinguishes a *definitive* negative (`DeadOrAbsent`) from a *probe failure*
/// (`ProbeError`). Callers deciding to destroy state on death MUST treat
/// `ProbeError` as "unknown ⇒ preserve" — a transient tmux hiccup is not proof
/// the owner died.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneLiveness {
    /// Session exists and has at least one non-dead pane.
    Live,
    /// Session is definitively gone, or exists with only dead panes (the owning
    /// process has exited). A clean negative answer from tmux.
    DeadOrAbsent,
    /// tmux could not be queried (binary spawn failure / non-success status on a
    /// present session). The answer is unknown — callers must not treat this as
    /// death.
    ProbeError,
}

const PANE_LIVENESS_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Probe a session's pane liveness as a three-state answer (see [`PaneLiveness`]).
pub fn pane_liveness(session_name: &str) -> PaneLiveness {
    if is_blank_session_name(session_name) {
        return PaneLiveness::DeadOrAbsent;
    }
    let mut has_session = tmux_command();
    has_session.args(["has-session", "-t", &exact_target(session_name)]);
    match wait_for_tmux_output(has_session, PANE_LIVENESS_PROBE_TIMEOUT, "tmux has-session") {
        // Spawn/exec failure ⇒ we never reached tmux: unknown, not dead.
        Err(_) => return PaneLiveness::ProbeError,
        // Clean non-zero exit ⇒ no such session (or no server running): the
        // session — and the process that lived in it — is gone.
        Ok(output) if !output.status.success() => return PaneLiveness::DeadOrAbsent,
        Ok(_) => {}
    }
    let mut list_panes = tmux_command();
    list_panes.args([
        "list-panes",
        "-t",
        &exact_target(session_name),
        "-F",
        "#{pane_dead}",
    ]);
    match wait_for_tmux_output(list_panes, PANE_LIVENESS_PROBE_TIMEOUT, "tmux list-panes") {
        // list-panes failed on a session we just confirmed present ⇒ unknown.
        Err(_) => PaneLiveness::ProbeError,
        Ok(output) if !output.status.success() => PaneLiveness::ProbeError,
        Ok(output) => {
            if String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line.trim() == "0")
            {
                PaneLiveness::Live
            } else {
                // Present but every pane is dead ⇒ the process exited.
                PaneLiveness::DeadOrAbsent
            }
        }
    }
}

/// Set a tmux session option. Errors are silently ignored (fire-and-forget).
pub fn set_option(session_name: &str, key: &str, value: &str) {
    let _ = tmux_command()
        .args(["set-option", "-t", &exact_target(session_name), key, value])
        .output();
}

/// Read one tmux session option without treating a missing session/option as a
/// fatal condition. Callers that rehydrate best-effort runtime metadata should
/// use this instead of shelling out themselves: malformed targets, absent tmux,
/// and non-success responses all resolve to `None`.
pub fn get_option(session_name: &str, key: &str) -> Option<String> {
    if is_blank_session_name(session_name) || key.trim().is_empty() {
        return None;
    }
    let output = tmux_command()
        .args([
            "show-options",
            "-qv",
            "-t",
            &exact_target(session_name),
            key,
        ])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod target_safety_tests {
    use super::*;

    #[test]
    fn blank_session_name_is_not_a_valid_target() {
        assert!(!has_session(""));
        assert_eq!(get_option("", "@agentdesk_claude_compact_provenance"), None);
        assert!(
            kill_session_output_internal_with_timeout(
                "",
                "unit test blank guard",
                Duration::from_millis(1)
            )
            .is_err()
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

    #[test]
    fn paste_buffer_raw_args_do_not_request_bracketed_paste() {
        let args = paste_buffer_raw_args("agentdesk-buffer", "=AgentDesk-codex-adk-cdx:", true);

        assert_eq!(
            args,
            vec![
                "paste-buffer",
                "-r",
                "-d",
                "-b",
                "agentdesk-buffer",
                "-t",
                "=AgentDesk-codex-adk-cdx:"
            ]
        );
        assert!(!args.contains(&"-p"));
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

    #[test]
    fn dead_marker_hook_writes_marker_on_pane_exit() {
        let _guard = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());

        if !is_available() {
            eprintln!("skipping dead marker hook test: tmux is not available");
            return;
        }

        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
        let previous_host = std::env::var_os("HOSTNAME");
        let root = std::env::temp_dir().join(format!("adk-issue-2424-hook-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", &root);
            std::env::set_var("HOSTNAME", "issue-2424-hook-host");
        }

        let session = unique_test_session_name();
        let marker_path = crate::services::tmux_common::session_dead_marker_path(&session);
        let output = create_session(&session, None, "sleep 60")
            .expect("temporary tmux session should be created");
        if !output.status.success() {
            panic!(
                "temporary tmux session should be created: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let _ = kill_session(&session, "dead marker hook test trigger");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline && !std::path::Path::new(&marker_path).exists() {
            std::thread::sleep(Duration::from_millis(100));
        }
        let marker_exists = std::path::Path::new(&marker_path).exists();

        crate::services::tmux_common::cleanup_session_temp_files(&session);
        let _ = std::fs::remove_dir_all(&root);
        match previous_root {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
        match previous_host {
            Some(value) => unsafe { std::env::set_var("HOSTNAME", value) },
            None => unsafe { std::env::remove_var("HOSTNAME") },
        }
        // CI keeps the strict assertion (this is the regression signal); only
        // local hosts where the pane-exit hook is environment-dependent skip.
        if !marker_exists {
            if std::env::var_os("CI").is_some() {
                panic!("tmux pane-exit hook did not create dead marker at {marker_path}");
            }
            eprintln!(
                "skipping dead marker hook assertion: tmux hook did not create marker at {marker_path}"
            );
        }
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
mod etime_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn parse_ps_etime_handles_all_field_widths() {
        assert_eq!(parse_ps_etime("00:05"), Some(Duration::from_secs(5)));
        assert_eq!(parse_ps_etime("01:02"), Some(Duration::from_secs(62)));
        assert_eq!(
            parse_ps_etime("01:02:03"),
            Some(Duration::from_secs(3_600 + 120 + 3))
        );
        assert_eq!(
            parse_ps_etime("2-03:04:05"),
            Some(Duration::from_secs(2 * 86_400 + 3 * 3_600 + 4 * 60 + 5))
        );
        assert_eq!(parse_ps_etime(""), None);
        assert_eq!(parse_ps_etime("garbage"), None);
        assert_eq!(parse_ps_etime("1:2:3:4"), None);
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
    fn session_presence_distinguishes_missing_from_probe_failure() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        write_fake_tmux(
            temp.path(),
            "case \"$FAKE_TMUX_MODE\" in missing) echo \"can't find session: test\" >&2; exit 1;; failed) echo 'permission denied' >&2; exit 1;; *) exit 0;; esac",
        );
        let _path = PathOverride::prepend(temp.path());

        unsafe { std::env::set_var("FAKE_TMUX_MODE", "missing") };
        assert_eq!(session_presence("agentdesk-test"), SessionPresence::Missing);
        unsafe { std::env::set_var("FAKE_TMUX_MODE", "failed") };
        assert_eq!(
            session_presence("agentdesk-test"),
            SessionPresence::ProbeFailed
        );
        unsafe { std::env::set_var("FAKE_TMUX_MODE", "present") };
        assert_eq!(session_presence("agentdesk-test"), SessionPresence::Present);
        unsafe { std::env::remove_var("FAKE_TMUX_MODE") };
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
