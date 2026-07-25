/// Stale-resume error detection helpers.
///
/// These functions detect "resume target gone" errors that fire when a
/// provider session has expired, been GC'd, or otherwise become unknown to
/// the CLI on the other side of `--resume` / `--resume-session-id`. The
/// caller — `turn_bridge::mod` — uses the signal to clear the cached
/// `claude_session_id` / `raw_provider_session_id` and re-dispatch the same
/// user turn with a fresh session (issue #2090).
///
/// Two provider surfaces are covered:
///   - **claude CLI**: emits `"Error: No conversation found ..."` when its
///     local `~/.claude/projects/.../<id>.jsonl` session file is missing.
///   - **codex CLI**: emits `"session not found"`, `"could not find session"`,
///     `"Failed to resume"` and similar phrases when its rollout/session
///     store has GC'd the id.
///
/// The phrase set is deliberately narrow to avoid eating unrelated failures.
/// `is_valid_session_id` already rejects malformed ids before launch, so we
/// do NOT match `"invalid session"`-shaped strings — those belong to the
/// pre-flight format validator, not this post-flight resume-failure detector.
// Prefixes audited against the actual codex CLI binary (`@openai/codex`
// 0.130.0). Keep classification anchored to the provider payload's first
// meaningful position: broad substring matching lets quoted child failures
// masquerade as the direct resume-target-gone envelope.
const DIRECT_STALE_RESUME_PREFIXES: &[&str] = &[
    "no conversation found",
    "conversation not found",
    "could not find session",
    "could not find conversation",
    "session does not exist",
    "no session with id",
    "no saved session found with id",
    "no rollout found for conversation id",
    "timeout waiting for codex resumed rollout transcript",
];

fn is_direct_stale_resume_error_envelope(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    let payload = lower
        .strip_prefix("error:")
        .map(str::trim_start)
        .unwrap_or(lower.as_str());
    DIRECT_STALE_RESUME_PREFIXES
        .iter()
        .any(|prefix| payload.starts_with(prefix))
}

pub(in crate::services::discord) fn result_event_has_stale_resume_error(
    value: &serde_json::Value,
) -> bool {
    if value.get("type").and_then(|v| v.as_str()) != Some("result") {
        return false;
    }

    let subtype = value.get("subtype").and_then(|v| v.as_str()).unwrap_or("");
    let is_error = value
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || subtype.starts_with("error");
    if !is_error {
        return false;
    }

    value
        .get("result")
        .and_then(|v| v.as_str())
        .into_iter()
        .chain(
            value
                .get("errors")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|error| error.as_str()),
        )
        .any(is_direct_stale_resume_error_envelope)
}

pub(super) fn output_file_has_stale_resume_error_after_offset(
    output_path: &str,
    start_offset: u64,
) -> bool {
    let Ok(bytes) = std::fs::read(output_path) else {
        return false;
    };
    let start = usize::try_from(start_offset)
        .ok()
        .map(|offset| offset.min(bytes.len()))
        .unwrap_or(bytes.len());

    String::from_utf8_lossy(&bytes[start..])
        .lines()
        .any(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return false;
            }
            serde_json::from_str::<serde_json::Value>(trimmed)
                .ok()
                .map(|value| result_event_has_stale_resume_error(&value))
                .unwrap_or(false)
        })
}

pub(super) fn stream_error_has_stale_resume_error(message: &str, stderr: &str) -> bool {
    message
        .lines()
        .chain(stderr.lines())
        .any(is_direct_stale_resume_error_envelope)
}

pub(super) fn stream_error_requires_terminal_session_reset(message: &str, stderr: &str) -> bool {
    let lower = format!("{} {}", message, stderr).to_ascii_lowercase();
    lower.contains("gemini session could not be recovered after retry")
        || lower.contains("gemini stream ended without a terminal result")
        || lower.contains("invalidargument: gemini resume selector must be")
        || lower.contains("qwen session could not be recovered after retry")
        || lower.contains("qwen stream ended without a terminal result")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_claude_stale_phrases() {
        assert!(is_direct_stale_resume_error_envelope(
            "Error: No conversation found for session abc-123"
        ));
        assert!(is_direct_stale_resume_error_envelope(
            "error: no conversation matching that id"
        ));
        assert!(is_direct_stale_resume_error_envelope(
            "Conversation not found."
        ));
    }

    #[test]
    fn matches_codex_stale_phrases() {
        // Codex phrasing varies across releases. The matcher captures the
        // tight substrings that don't collide with codex internal strings
        // (verified by codex review against the 0.130.0 binary).
        assert!(is_direct_stale_resume_error_envelope(
            "could not find session with id xyz"
        ));
        assert!(is_direct_stale_resume_error_envelope(
            "could not find conversation for resume target"
        ));
        assert!(is_direct_stale_resume_error_envelope(
            "session does not exist on this host"
        ));
        assert!(is_direct_stale_resume_error_envelope(
            "no session with id matching the resume request"
        ));
        // codex 0.130.0 user-facing surface strings.
        assert!(is_direct_stale_resume_error_envelope(
            "No saved session found with ID aaaa-bbbb-cccc"
        ));
        assert!(is_direct_stale_resume_error_envelope(
            "no rollout found for conversation id 42"
        ));
        assert!(is_direct_stale_resume_error_envelope(
            "Timeout waiting for Codex resumed rollout transcript under /Users/kunkun/.codex/sessions"
        ));
    }

    #[test]
    fn does_not_match_unrelated_errors() {
        assert!(!is_direct_stale_resume_error_envelope(""));
        assert!(!is_direct_stale_resume_error_envelope("Permission denied"));
        assert!(!is_direct_stale_resume_error_envelope(
            "valid session resumed successfully"
        ));
        // `is_valid_session_id` already rejects malformed ids before launch;
        // this matcher must not double-claim that pre-flight failure.
        assert!(!is_direct_stale_resume_error_envelope(
            "Invalid session ID format"
        ));
        assert!(!is_direct_stale_resume_error_envelope(
            "Process exited with code Some(1)"
        ));
        // Codex 0.130.0 internal strings that look stale-resume-ish but are
        // not — these used to false-match the broader phrase set before
        // codex review narrowed it.
        assert!(!is_direct_stale_resume_error_envelope(
            "failed to resume descendant thread"
        ));
        assert!(!is_direct_stale_resume_error_envelope(
            "failed to resume local thread recorder"
        ));
        assert!(!is_direct_stale_resume_error_envelope(
            "failed to resume live thread for selection"
        ));
        assert!(!is_direct_stale_resume_error_envelope(
            "Agent resume failed: spawn refused"
        ));
        assert!(!is_direct_stale_resume_error_envelope(
            "fuzzy file search session not found"
        ));
        assert!(!is_direct_stale_resume_error_envelope(
            "Session not found for request_id rpc-42"
        ));
        assert!(!is_direct_stale_resume_error_envelope(
            "Session not found for thread_id t-7"
        ));
        // codex exec-server `file_system_handler.rs` uses "unknown session id"
        // in a non-stale-resume context — must not auto-clear the user's
        // provider session because of it.
        assert!(!is_direct_stale_resume_error_envelope(
            "file_system_handler.rs: unknown session id 'fs-1'"
        ));
        // codex emits `cannot resume thread ... with history while it is
        // already running` for the busy-state error path — that's a
        // concurrency conflict, not a stale-resume failure. `error resuming
        // thread` sits in the same busy-state cluster in the 0.130.0 binary.
        assert!(!is_direct_stale_resume_error_envelope(
            "cannot resume thread abc with history while it is already running"
        ));
        assert!(!is_direct_stale_resume_error_envelope(
            "error resuming thread abc: already running"
        ));
        // codex emits `Failed to resume session from <path>` for unrelated
        // config/app-server load failures.
        assert!(!is_direct_stale_resume_error_envelope(
            "Failed to resume session from /tmp/codex-config.toml"
        ));
    }

    #[test]
    fn stream_error_helpers_compose_message_and_stderr() {
        assert!(stream_error_has_stale_resume_error(
            "",
            "could not find session matching the resume target"
        ));
        assert!(stream_error_has_stale_resume_error(
            "Error: No conversation found",
            ""
        ));
        assert!(!stream_error_has_stale_resume_error(
            "transport error",
            "tls handshake aborted"
        ));
        assert!(!stream_error_has_stale_resume_error(
            "Error: child failed: No conversation found with session ID abc",
            ""
        ));
    }

    #[test]
    fn result_event_detects_claude_resume_error_shape() {
        // Claude CLI emits `{"type":"result","subtype":"error_during_execution","result":"Error: No conversation found ..."}`.
        let value: serde_json::Value = serde_json::from_str(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"Error: No conversation found for session abc"}"#,
        )
        .unwrap();
        assert!(result_event_has_stale_resume_error(&value));
    }

    #[test]
    fn result_event_accepts_direct_errors_array_envelope() {
        let value = serde_json::json!({
            "type": "result",
            "subtype": "error_during_execution",
            "is_error": true,
            "errors": ["Error: No conversation found with session ID abc"]
        });
        assert!(result_event_has_stale_resume_error(&value));
    }

    #[test]
    fn result_event_ignores_unrelated_or_quoted_child_errors() {
        let unrelated: serde_json::Value = serde_json::from_str(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"Permission denied"}"#,
        )
        .unwrap();
        let quoted_child = serde_json::json!({
            "type": "result",
            "subtype": "error_during_execution",
            "is_error": true,
            "result": "child failed: No conversation found with session ID abc"
        });
        let prefixed_child = serde_json::json!({
            "type": "result",
            "subtype": "error_during_execution",
            "is_error": true,
            "result": "Error: child failed: No conversation found with session ID abc"
        });
        let wrong_provenance = serde_json::json!({
            "type": "system",
            "subtype": "task_notification",
            "is_error": true,
            "result": "Error: No conversation found with session ID abc"
        });
        assert!(!result_event_has_stale_resume_error(&unrelated));
        assert!(!result_event_has_stale_resume_error(&quoted_child));
        assert!(!result_event_has_stale_resume_error(&prefixed_child));
        assert!(!result_event_has_stale_resume_error(&wrong_provenance));
    }

    #[test]
    fn terminal_reset_matchers_unchanged_for_gemini_qwen() {
        assert!(stream_error_requires_terminal_session_reset(
            "Gemini session could not be recovered after retry",
            ""
        ));
        assert!(stream_error_requires_terminal_session_reset(
            "",
            "Qwen stream ended without a terminal result"
        ));
        assert!(!stream_error_requires_terminal_session_reset(
            "claude error: no conversation found",
            ""
        ));
    }
}
