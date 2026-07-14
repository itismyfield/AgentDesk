use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::utils::format::safe_prefix;

fn tmux_exit_reason_path(tmux_session_name: &str) -> String {
    crate::services::tmux_common::session_temp_path(tmux_session_name, "exit_reason")
}

pub fn tmux_session_exists(tmux_session_name: &str) -> bool {
    crate::services::platform::tmux::has_session(tmux_session_name)
}

/// Async-safe exact session-existence probe for request/reaper paths.
///
/// `tmux_session_exists` invokes the tmux CLI and is therefore blocking. Keep
/// it off Tokio workers and bound the join so a wedged tmux server cannot
/// wedge Discord intake. A timeout or join failure is treated conservatively
/// as "present": stale-busy recovery must never release a turn unless absence
/// was positively observed.
pub async fn probe_tmux_session_exists(tmux_session_name: &str) -> bool {
    let name = tmux_session_name.to_string();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(move || tmux_session_exists(&name)),
    )
    .await
    .unwrap_or(Ok(true))
    .unwrap_or(true)
}

pub fn tmux_session_has_live_pane(tmux_session_name: &str) -> bool {
    crate::services::platform::tmux::has_live_pane(tmux_session_name)
}

/// #3635: three-state pane liveness (`Live` / `DeadOrAbsent` / `ProbeError`).
/// Use when a transient tmux probe failure must NOT be mistaken for death.
pub fn tmux_session_pane_liveness(
    tmux_session_name: &str,
) -> crate::services::platform::tmux::PaneLiveness {
    crate::services::platform::tmux::pane_liveness(tmux_session_name)
}

/// #3208: the current working directory of the live pane for a tmux session.
/// Used by the follow-up readiness path to resolve the running Claude session's
/// JSONL transcript when it lives in a rotating worktree that differs from the
/// channel's configured workspace cwd.
pub fn tmux_session_pane_cwd(tmux_session_name: &str) -> Option<String> {
    crate::services::platform::tmux::pane_current_path(tmux_session_name)
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
    let path = tmux_exit_reason_path(tmux_session_name);
    let path = Path::new(&path);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp_path = path.with_extension(format!(
        "exit_reason.tmp-{}-{}",
        std::process::id(),
        chrono::Local::now()
            .timestamp_nanos_opt()
            .unwrap_or_default()
    ));
    if std::fs::write(&tmp_path, stamped).is_ok() {
        let _ = std::fs::rename(&tmp_path, path);
    }
    let _ = std::fs::remove_file(tmp_path);
}

pub fn tmux_exit_reason_is_normal_completion(reason: &str) -> bool {
    let lower = reason.trim().to_ascii_lowercase();
    lower.contains("turn completed")
        || lower.contains("dispatch turn completed")
        || lower.contains("unified-thread run completed")
        || lower == "exit:0"
        // Routine fresh-session teardown is an intentional, quiet cleanup the
        // runtime performs after a fresh routine reaches a terminal state
        // (agent turn finished/timed out, or a terminal JS-script action
        // completed/skipped/paused the run). `force_kill_turn` records the
        // exit reason as `explicit cleanup via routine fresh ...`, so without
        // this branch the lifecycle watcher would treat the deliberate kill as
        // an abnormal pane death and emit a false "session ended" notice for a
        // session that already delivered its terminal response (#3006).
        || lower.contains("routine fresh")
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
    // ProcessBackend stopped markers intentionally force a cold start on the
    // next message instead of trying to reuse a cancelled wrapper pipe.
    if err.contains("Process session ") && err.contains(" was stopped") {
        return true;
    }
    // Write/flush failures that indicate dead process
    if err.contains("Failed to write to child stdin") || err.contains("Failed to flush child stdin")
    {
        return true;
    }
    false
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
    use super::should_recreate_session_after_stdin_error;

    #[test]
    fn process_backend_stopped_error_forces_recreate() {
        assert!(should_recreate_session_after_stdin_error(
            "Process session AgentDesk-claude-123 was stopped"
        ));
    }
}
