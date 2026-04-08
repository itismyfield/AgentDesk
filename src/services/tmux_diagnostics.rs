use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use crate::utils::format::safe_prefix;

fn tmux_exit_reason_path(tmux_session_name: &str) -> String {
    crate::services::tmux_common::session_temp_path(tmux_session_name, "exit_reason")
}

pub fn tmux_session_exists(tmux_session_name: &str) -> bool {
    crate::services::platform::tmux::has_session(tmux_session_name)
}

pub fn tmux_session_has_live_pane(tmux_session_name: &str) -> bool {
    crate::services::platform::tmux::has_live_pane(tmux_session_name)
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

fn stale_resume_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("no conversation found") || lower.contains("error: no conversation")
}

fn tail_jsonl_result_hint(buf: &str) -> Option<String> {
    for line in buf.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if value.get("type").and_then(|v| v.as_str()) != Some("result") {
            continue;
        }

        let subtype = value.get("subtype").and_then(|v| v.as_str()).unwrap_or("");
        let is_error = value
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            || subtype.starts_with("error");

        let stale_resume = value
            .get("result")
            .and_then(|v| v.as_str())
            .map(stale_resume_text)
            .unwrap_or(false)
            || value
                .get("errors")
                .and_then(|v| v.as_array())
                .map(|errors| {
                    errors
                        .iter()
                        .filter_map(|err| err.as_str())
                        .any(stale_resume_text)
                })
                .unwrap_or(false);

        if stale_resume {
            return Some("recent_output=stale_resume_error".to_string());
        }
        if is_error {
            return Some("recent_output=result_error_present".to_string());
        }
        return Some("recent_output=completed_result_present".to_string());
    }

    None
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
    if let Some(hint) = tail_jsonl_result_hint(&buf) {
        return Some(hint);
    }

    let last_line = buf.lines().rev().find(|line| !line.trim().is_empty())?;
    let compact = last_line.replace('\n', " ").replace('\r', " ");
    Some(format!(
        "recent_output_tail={}",
        safe_prefix(compact.trim(), 160)
    ))
}

/// Whether a follow-up FIFO error indicates the session should be killed and
/// recreated.  Returns `true` for infrastructure failures (broken pipe, file
/// not found, bad descriptor) but *not* for permission errors or unrelated I/O
/// failures — those indicate a deeper issue that blind retry won't fix.
pub fn should_recreate_session_after_followup_fifo_error(err: &str) -> bool {
    if err.is_empty() {
        return false;
    }
    // Broken pipe on write/flush — process on the other end is dead
    if err.contains("Broken pipe") {
        return true;
    }
    // FIFO file was deleted or doesn't exist
    if err.contains("No such file or directory") || err.contains("entity not found") {
        return true;
    }
    // Bad file descriptor — FIFO was closed or became invalid
    if err.contains("Bad file descriptor") {
        return true;
    }
    // No such device — FIFO target disappeared
    if err.contains("No such device") {
        return true;
    }
    false
}

/// #170: Classify ProcessBackend stdin pipe errors as recoverable (session recreation)
/// or unrecoverable. Similar to `should_recreate_session_after_followup_fifo_error`
/// but for stdin pipe errors instead of FIFO errors.
pub fn should_recreate_session_after_stdin_error(err: &str) -> bool {
    if err.is_empty() {
        return false;
    }
    // Broken pipe — child process exited
    if err.contains("Broken pipe") {
        return true;
    }
    // Child stdin was already closed/taken
    if err.contains("stdin already closed") {
        return true;
    }
    // Write/flush failures that indicate dead process
    if err.contains("Failed to write to child stdin") || err.contains("Failed to flush child stdin")
    {
        return true;
    }
    false
}

/// Helper: returns true if tmux pane list output indicates at least one live pane.
#[allow(dead_code)]
pub fn pane_list_has_live_pane(output: &str) -> bool {
    output.lines().any(|line| line.trim() == "0")
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
        read_recent_output_hint, record_tmux_exit_reason,
        should_recreate_session_after_followup_fifo_error,
        should_recreate_session_after_stdin_error,
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

    #[test]
    fn test_should_recreate_session_after_followup_fifo_error() {
        // FIFO write broken pipe
        assert!(should_recreate_session_after_followup_fifo_error(
            "Failed to write to input FIFO: Broken pipe (os error 32)"
        ));
        // FIFO flush broken pipe
        assert!(should_recreate_session_after_followup_fifo_error(
            "Failed to flush input FIFO: Broken pipe (os error 32)"
        ));
        // FIFO open — file not found
        assert!(should_recreate_session_after_followup_fifo_error(
            "Failed to open input FIFO: No such file or directory (os error 2)"
        ));
        // FIFO open — not found (alternative wording)
        assert!(should_recreate_session_after_followup_fifo_error(
            "Failed to open input FIFO: entity not found"
        ));
        // FIFO open — bad file descriptor
        assert!(should_recreate_session_after_followup_fifo_error(
            "Failed to open input FIFO: Bad file descriptor (os error 9)"
        ));
        // FIFO open — no such device
        assert!(should_recreate_session_after_followup_fifo_error(
            "Failed to open input FIFO: No such device or address (os error 6)"
        ));
        // Unrelated error should NOT trigger recreation
        assert!(!should_recreate_session_after_followup_fifo_error(
            "Failed to read Codex output: unexpected EOF"
        ));
        // Permission error should NOT trigger recreation
        assert!(!should_recreate_session_after_followup_fifo_error(
            "Failed to open input FIFO: Permission denied (os error 13)"
        ));
        // Empty string should NOT trigger
        assert!(!should_recreate_session_after_followup_fifo_error(""));
    }

    #[test]
    fn test_should_recreate_session_after_stdin_error() {
        // Broken pipe on stdin write
        assert!(should_recreate_session_after_stdin_error(
            "Failed to write to child stdin: Broken pipe (os error 32)"
        ));
        // Broken pipe on stdin flush
        assert!(should_recreate_session_after_stdin_error(
            "Failed to flush child stdin: Broken pipe (os error 32)"
        ));
        // stdin already closed
        assert!(should_recreate_session_after_stdin_error(
            "Child stdin already closed"
        ));
        // Generic write failure (includes BrokenPipe as inner cause)
        assert!(should_recreate_session_after_stdin_error(
            "Failed to write to child stdin: connection reset"
        ));
        // Lock poisoned — NOT recoverable via recreation
        assert!(!should_recreate_session_after_stdin_error(
            "stdin lock poisoned: PoisonError"
        ));
        // Unrelated error
        assert!(!should_recreate_session_after_stdin_error(
            "unexpected EOF in output"
        ));
        // Empty string
        assert!(!should_recreate_session_after_stdin_error(""));
    }

    #[test]
    fn test_read_recent_output_hint_ignores_embedded_result_string() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"literal \"type\":\"result\" in code"}]}}"#,
        )
        .unwrap();
        let hint = read_recent_output_hint(file.path().to_str().unwrap()).unwrap();
        assert!(hint.starts_with("recent_output_tail="));
    }

    #[test]
    fn test_read_recent_output_hint_detects_success_result_line() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            r#"{"type":"result","subtype":"success","result":"ok"}"#,
        )
        .unwrap();
        assert_eq!(
            read_recent_output_hint(file.path().to_str().unwrap()).as_deref(),
            Some("recent_output=completed_result_present")
        );
    }

    #[test]
    fn test_read_recent_output_hint_detects_stale_resume_error() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["No conversation found"]}"#,
        )
        .unwrap();
        assert_eq!(
            read_recent_output_hint(file.path().to_str().unwrap()).as_deref(),
            Some("recent_output=stale_resume_error")
        );
    }
}
