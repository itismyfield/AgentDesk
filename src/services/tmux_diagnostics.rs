use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use crate::utils::format::safe_prefix;

fn tmux_exit_reason_path(tmux_session_name: &str) -> String {
    crate::services::tmux_common::session_temp_path(tmux_session_name, "exit_reason")
}

#[cfg(unix)]
pub fn tmux_session_exists(tmux_session_name: &str) -> bool {
    let exact = crate::services::tmux_common::tmux_exact_target(tmux_session_name);
    std::process::Command::new("tmux")
        .args(["has-session", "-t", &exact])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(unix))]
pub fn tmux_session_exists(_tmux_session_name: &str) -> bool {
    false
}

fn pane_list_has_live_pane(stdout: &str) -> bool {
    stdout.lines().any(|line| line.trim() == "0")
}

#[cfg(unix)]
pub fn tmux_session_has_live_pane(tmux_session_name: &str) -> bool {
    if !tmux_session_exists(tmux_session_name) {
        return false;
    }

    let exact = crate::services::tmux_common::tmux_exact_target(tmux_session_name);
    std::process::Command::new("tmux")
        .args(["list-panes", "-t", &exact, "-F", "#{pane_dead}"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| pane_list_has_live_pane(&String::from_utf8_lossy(&output.stdout)))
        .unwrap_or(false)
}

#[cfg(not(unix))]
pub fn tmux_session_has_live_pane(_tmux_session_name: &str) -> bool {
    false
}

pub fn clear_tmux_exit_reason(tmux_session_name: &str) {
    let _ = std::fs::remove_file(tmux_exit_reason_path(tmux_session_name));
}

pub fn record_tmux_exit_reason(tmux_session_name: &str, reason: &str) {
    let stamped = format!(
        "[{}] {}",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        reason.trim()
    );
    let _ = std::fs::write(tmux_exit_reason_path(tmux_session_name), stamped);
}

pub fn read_tmux_exit_reason(tmux_session_name: &str) -> Option<String> {
    std::fs::read_to_string(tmux_exit_reason_path(tmux_session_name))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn read_recent_output_hint(output_path: &str) -> Option<String> {
    let mut file = File::open(output_path).ok()?;
    let len = file.metadata().ok()?.len();
    let tail_len = len.min(2048);
    if tail_len == 0 {
        return None;
    }

    file.seek(SeekFrom::Start(len.saturating_sub(tail_len)))
        .ok()?;
    let mut buf = String::new();
    file.read_to_string(&mut buf).ok()?;

    let lower = buf.to_lowercase();
    if lower.contains("authentication_failed") {
        return Some("recent_output=authentication_failed".to_string());
    }
    if lower.contains("prompt too long") || lower.contains("prompt is too long") {
        return Some("recent_output=prompt_too_long".to_string());
    }
    if lower.contains("\"type\":\"result\"") || lower.contains("\"type\": \"result\"") {
        return Some("recent_output=completed_result_present".to_string());
    }

    let last_line = buf.lines().rev().find(|line| !line.trim().is_empty())?;
    let compact = last_line.replace('\n', " ").replace('\r', " ");
    Some(format!(
        "recent_output_tail={}",
        safe_prefix(compact.trim(), 160)
    ))
}

pub fn build_tmux_death_diagnostic(
    tmux_session_name: &str,
    output_path: Option<&str>,
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(reason) = read_tmux_exit_reason(tmux_session_name) {
        parts.push(format!("last_exit_reason={reason}"));
    }
    if let Some(path) = output_path {
        if let Some(hint) = read_recent_output_hint(path) {
            parts.push(hint);
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_tmux_death_diagnostic, clear_tmux_exit_reason, pane_list_has_live_pane,
        record_tmux_exit_reason,
    };

    #[test]
    fn test_tmux_exit_reason_round_trip() {
        let session = format!("AgentDesk-test-{}", std::process::id());
        clear_tmux_exit_reason(&session);
        record_tmux_exit_reason(&session, "explicit cleanup: /stop");
        let diag = build_tmux_death_diagnostic(&session, None).unwrap();
        assert!(diag.contains("explicit cleanup: /stop"));
        clear_tmux_exit_reason(&session);
    }

    #[test]
    fn test_pane_list_has_live_pane() {
        assert!(pane_list_has_live_pane("1\n0\n"));
        assert!(!pane_list_has_live_pane("1\n1\n"));
        assert!(!pane_list_has_live_pane(""));
    }
}
