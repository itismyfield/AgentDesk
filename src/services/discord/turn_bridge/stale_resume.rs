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
pub(super) fn contains_stale_resume_error_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    // Phrase set audited against the actual codex CLI binary
    // (`@openai/codex` 0.130.0). Several seemingly-relevant fragments
    // collide with non-stale codex internal log lines and must NOT be
    // re-added — they are listed here as a "do not add" guard:
    //   - `failed to resume` (un-narrowed) → matches `failed to resume
    //      descendant thread / local thread recorder / live thread for
    //      selection`.
    //   - `session not found` → matches `fuzzy file search session not
    //      found`, `Session not found for request_id`, `Session not found
    //      for thread_id`.
    //   - `unknown session id` → matches codex's exec-server
    //      `file_system_handler.rs` error path.
    //   - `cannot resume thread` / `error resuming thread` → both live in
    //      codex's busy-state cluster (`cannot resume thread ... with history
    //      while it is already running`) and surface concurrency conflicts,
    //      not stale-resume failures.
    //   - `failed to resume session` → matches codex's
    //      `Failed to resume session from <path>` config/app-server load
    //      failure, which is unrelated to a GC'd session id.
    // The shortlist below is the residue that grepped clean against the
    // codex binary while still covering the real resume-target-gone surface
    // for both providers.
    const STALE_PHRASES: &[&str] = &[
        // claude CLI surface (current + a couple of defensive variants).
        "no conversation found",
        "error: no conversation",
        "conversation not found",
        // Generic / codex resume-target-gone phrases — tight enough that
        // codex internals don't false-match.
        "could not find session",
        "could not find conversation",
        "session does not exist",
        "no session with id",
        // Additional codex 0.130.0 user-facing strings that surface when the
        // rollout/session/thread store has GC'd the resume target.
        "no saved session found with id",
        "no rollout found for conversation id",
    ];
    STALE_PHRASES.iter().any(|p| lower.contains(p))
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

    if value
        .get("result")
        .and_then(|v| v.as_str())
        .map(contains_stale_resume_error_text)
        .unwrap_or(false)
    {
        return true;
    }

    value
        .get("errors")
        .and_then(|v| v.as_array())
        .map(|errors| {
            errors.iter().any(|err| {
                err.as_str()
                    .map(contains_stale_resume_error_text)
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
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
    contains_stale_resume_error_text(message) || contains_stale_resume_error_text(stderr)
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
        assert!(contains_stale_resume_error_text(
            "Error: No conversation found for session abc-123"
        ));
        assert!(contains_stale_resume_error_text(
            "error: no conversation matching that id"
        ));
        assert!(contains_stale_resume_error_text("Conversation not found."));
    }

    #[test]
    fn matches_codex_stale_phrases() {
        // Codex phrasing varies across releases. The matcher captures the
        // tight substrings that don't collide with codex internal strings
        // (verified by codex review against the 0.130.0 binary).
        assert!(contains_stale_resume_error_text(
            "could not find session with id xyz"
        ));
        assert!(contains_stale_resume_error_text(
            "could not find conversation for resume target"
        ));
        assert!(contains_stale_resume_error_text(
            "session does not exist on this host"
        ));
        assert!(contains_stale_resume_error_text(
            "no session with id matching the resume request"
        ));
        // codex 0.130.0 user-facing surface strings.
        assert!(contains_stale_resume_error_text(
            "No saved session found with ID aaaa-bbbb-cccc"
        ));
        assert!(contains_stale_resume_error_text(
            "no rollout found for conversation id 42"
        ));
    }

    #[test]
    fn does_not_match_unrelated_errors() {
        assert!(!contains_stale_resume_error_text(""));
        assert!(!contains_stale_resume_error_text("Permission denied"));
        assert!(!contains_stale_resume_error_text(
            "valid session resumed successfully"
        ));
        // `is_valid_session_id` already rejects malformed ids before launch;
        // this matcher must not double-claim that pre-flight failure.
        assert!(!contains_stale_resume_error_text(
            "Invalid session ID format"
        ));
        assert!(!contains_stale_resume_error_text(
            "Process exited with code Some(1)"
        ));
        // Codex 0.130.0 internal strings that look stale-resume-ish but are
        // not — these used to false-match the broader phrase set before
        // codex review narrowed it.
        assert!(!contains_stale_resume_error_text(
            "failed to resume descendant thread"
        ));
        assert!(!contains_stale_resume_error_text(
            "failed to resume local thread recorder"
        ));
        assert!(!contains_stale_resume_error_text(
            "failed to resume live thread for selection"
        ));
        assert!(!contains_stale_resume_error_text(
            "Agent resume failed: spawn refused"
        ));
        assert!(!contains_stale_resume_error_text(
            "fuzzy file search session not found"
        ));
        assert!(!contains_stale_resume_error_text(
            "Session not found for request_id rpc-42"
        ));
        assert!(!contains_stale_resume_error_text(
            "Session not found for thread_id t-7"
        ));
        // codex exec-server `file_system_handler.rs` uses "unknown session id"
        // in a non-stale-resume context — must not auto-clear the user's
        // provider session because of it.
        assert!(!contains_stale_resume_error_text(
            "file_system_handler.rs: unknown session id 'fs-1'"
        ));
        // codex emits `cannot resume thread ... with history while it is
        // already running` for the busy-state error path — that's a
        // concurrency conflict, not a stale-resume failure. `error resuming
        // thread` sits in the same busy-state cluster in the 0.130.0 binary.
        assert!(!contains_stale_resume_error_text(
            "cannot resume thread abc with history while it is already running"
        ));
        assert!(!contains_stale_resume_error_text(
            "error resuming thread abc: already running"
        ));
        // codex emits `Failed to resume session from <path>` for unrelated
        // config/app-server load failures.
        assert!(!contains_stale_resume_error_text(
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
    fn result_event_ignores_unrelated_errors() {
        let value: serde_json::Value = serde_json::from_str(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"Permission denied"}"#,
        )
        .unwrap();
        assert!(!result_event_has_stale_resume_error(&value));
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
