/// Stale-resume error detection helpers.
///
/// These functions detect "no conversation found" errors that occur when a
/// Claude session has expired or been invalidated, so the caller can trigger
/// a fresh retry.
pub(super) fn contains_stale_resume_error_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("no conversation found") || lower.contains("error: no conversation")
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
