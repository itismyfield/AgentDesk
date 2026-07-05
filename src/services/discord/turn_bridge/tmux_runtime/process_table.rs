//! Process-table discovery for the tmux turn runtime (#3479 split).
//!
//! Behavior-preserving extraction from `tmux_runtime.rs`: the `ps`-backed
//! process table (`ProcessRow`), the provider-CLI PID resolution that walks the
//! tmux pane's descendant tree, the pane-foreground classification, the
//! wrapper-FIFO writer, and the `SIGINT` primitive. These helpers depend only
//! on `crate::services::platform::tmux::pane_pid`, `ProviderKind`, and
//! `libc`/`std`, so they form a cohesive leaf module. The interrupt/cancel
//! orchestration that consumes them stays in the parent module.
//!
//! The handful of helpers the parent calls are `pub(super)`; the intra-module
//! table/scoring helpers keep their original module-private visibility.

use super::*;

#[cfg(test)]
static SIGINT_TEST_EVENTS: std::sync::LazyLock<std::sync::Mutex<Vec<u32>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

#[cfg(test)]
pub(super) fn take_sigint_test_events() -> Vec<u32> {
    let mut events = SIGINT_TEST_EVENTS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    std::mem::take(&mut *events)
}

#[cfg(unix)]
#[derive(Debug, Clone, Eq, PartialEq)]
struct ProcessRow {
    pid: u32,
    ppid: u32,
    command: String,
}

#[cfg(unix)]
pub(super) fn provider_cli_pid_in_tmux(
    tmux_session_name: &str,
    provider: &ProviderKind,
    tracked_child_pid: Option<u32>,
) -> Option<u32> {
    let pane_pid = crate::services::platform::tmux::pane_pid(tmux_session_name)?;
    let rows = process_table();
    select_provider_pid_in_pane(pane_pid, &rows, provider, tracked_child_pid)
}

/// #3207 (part 1): is the tmux pane foreground the `agentdesk tmux-wrapper`
/// (stream-json host) rather than the bare claude TUI? Drives
/// [`super::claude_turn_interrupt_delivery`]. Returns `false` when the pane /
/// process table cannot be read, defaulting the caller to the live `TuiEscape`
/// path.
#[cfg(unix)]
pub(super) fn pane_foreground_is_provider_wrapper(tmux_session_name: &str) -> bool {
    let Some(pane_pid) = crate::services::platform::tmux::pane_pid(tmux_session_name) else {
        return false;
    };
    let rows = process_table();
    rows.iter()
        .find(|row| row.pid == pane_pid)
        .map(|row| command_is_agentdesk_provider_wrapper(&row.command))
        .unwrap_or(false)
}

#[cfg(not(unix))]
pub(super) fn pane_foreground_is_provider_wrapper(_tmux_session_name: &str) -> bool {
    false
}

/// #3207 (part 1): write a pre-formatted (newline-terminated) line to the
/// wrapper input FIFO without blocking. The wrapper holds the FIFO open
/// `O_RDWR`, so a writer never blocks while it is alive; if the wrapper is gone
/// the non-blocking open fails with `ENXIO` and we report the miss instead of
/// hanging a blocking-pool thread forever.
#[cfg(unix)]
pub(super) fn write_line_to_wrapper_fifo(input_fifo_path: &str, line: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(input_fifo_path)
        .map_err(|error| format!("open input fifo {input_fifo_path}: {error}"))?;
    file.write_all(line.as_bytes())
        .and_then(|()| {
            if line.ends_with('\n') {
                Ok(())
            } else {
                file.write_all(b"\n")
            }
        })
        .and_then(|()| file.flush())
        .map_err(|error| format!("write input fifo {input_fifo_path}: {error}"))
}

#[cfg(not(unix))]
pub(super) fn write_line_to_wrapper_fifo(
    _input_fifo_path: &str,
    _line: &str,
) -> Result<(), String> {
    Err("wrapper FIFO interrupt is only supported on Unix".to_string())
}

// TUI mode regression: when the provider CLI itself is the tmux pane
// foreground (no wrapper in front — e.g. `claude --session-id …` running
// directly), `descendant_processes` excludes `pane_pid` and the search
// falls through to None. The interrupt path then silently no-ops: stop
// emoji marks the turn [Stopped] in the mailbox but no SIGINT ever
// reaches the claude TUI, so it keeps generating and the watcher keeps
// posting the response to Discord. Check `pane_pid` itself before walking
// descendants.
#[cfg(unix)]
fn select_provider_pid_in_pane(
    pane_pid: u32,
    rows: &[ProcessRow],
    provider: &ProviderKind,
    tracked_child_pid: Option<u32>,
) -> Option<u32> {
    if let Some(pane_row) = rows.iter().find(|row| row.pid == pane_pid)
        && !command_is_agentdesk_provider_wrapper(&pane_row.command)
        && command_matches_provider(&pane_row.command, provider)
    {
        return Some(pane_pid);
    }

    let descendants = descendant_processes(pane_pid, rows);

    if let Some(pid) = tracked_child_pid
        && descendants.iter().any(|row| {
            row.pid == pid
                && !command_is_agentdesk_provider_wrapper(&row.command)
                && command_matches_provider(&row.command, provider)
        })
    {
        return Some(pid);
    }

    descendants
        .into_iter()
        .filter(|row| !command_is_agentdesk_provider_wrapper(&row.command))
        .filter(|row| command_matches_provider(&row.command, provider))
        .max_by_key(|row| provider_command_match_score(&row.command, provider))
        .map(|row| row.pid)
}

#[cfg(not(unix))]
pub(super) fn provider_cli_pid_in_tmux(
    _tmux_session_name: &str,
    _provider: &ProviderKind,
    _tracked_child_pid: Option<u32>,
) -> Option<u32> {
    None
}

#[cfg(unix)]
fn process_table() -> Vec<ProcessRow> {
    let output = match std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,command="])
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return Vec::new(),
    };

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_process_row)
        .collect()
}

#[cfg(unix)]
fn parse_process_row(line: &str) -> Option<ProcessRow> {
    let mut parts = line.split_whitespace();
    let pid = parts.next()?.parse::<u32>().ok()?;
    let ppid = parts.next()?.parse::<u32>().ok()?;
    let command = parts.collect::<Vec<_>>().join(" ");
    if command.is_empty() {
        return None;
    }
    Some(ProcessRow { pid, ppid, command })
}

#[cfg(unix)]
fn descendant_processes(root_pid: u32, rows: &[ProcessRow]) -> Vec<ProcessRow> {
    let mut descendants = Vec::new();
    let mut stack = vec![root_pid];
    while let Some(parent) = stack.pop() {
        for row in rows.iter().filter(|row| row.ppid == parent) {
            descendants.push(row.clone());
            stack.push(row.pid);
        }
    }
    descendants
}

#[cfg(unix)]
fn command_matches_provider(command: &str, provider: &ProviderKind) -> bool {
    provider_command_match_score(command, provider) > 0
}

#[cfg(unix)]
fn provider_command_match_score(command: &str, provider: &ProviderKind) -> u8 {
    let Some(binary) = provider_cli_binary_name(provider) else {
        return 0;
    };
    let lower = command.to_ascii_lowercase();
    let first = lower
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(|ch| ch == '\'' || ch == '"');
    let first_basename = std::path::Path::new(first)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(first);

    if first_basename == binary {
        return 3;
    }
    if lower.contains(&format!("/{binary} "))
        || lower.ends_with(&format!("/{binary}"))
        || lower.contains(&format!(" {binary} "))
    {
        return 2;
    }
    if lower.contains(binary) { 1 } else { 0 }
}

#[cfg(unix)]
fn command_is_agentdesk_provider_wrapper(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    lower.contains(" codex-tmux-wrapper")
        || lower.contains(" qwen-tmux-wrapper")
        || lower.contains(" tmux-wrapper")
}

#[cfg(unix)]
fn provider_cli_binary_name(provider: &ProviderKind) -> Option<&'static str> {
    match provider {
        ProviderKind::Claude => Some("claude"),
        ProviderKind::Codex => Some("codex"),
        ProviderKind::Qwen => Some("qwen"),
        ProviderKind::Gemini | ProviderKind::OpenCode | ProviderKind::Unsupported(_) => None,
    }
}

#[cfg(all(unix, not(test)))]
pub(super) fn send_sigint(pid: u32) -> Result<(), String> {
    #[allow(unsafe_code)]
    let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGINT) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

#[cfg(all(unix, test))]
pub(super) fn send_sigint(pid: u32) -> Result<(), String> {
    SIGINT_TEST_EVENTS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .push(pid);
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn send_sigint(_pid: u32) -> Result<(), String> {
    Err("SIGINT fallback is only supported on Unix".to_string())
}
