/// Recognize provider-generated error-only transcript envelopes that can be
/// emitted without a typed terminal error event.
///
/// Keep this intentionally narrow: ordinary assistant prose such as
/// `Error summary: ...` is still a deliverable response.
pub(crate) fn is_strong_provider_error_transcript(message: &str) -> bool {
    let trimmed = message.trim();
    let lower = trimmed.to_ascii_lowercase();
    is_single_api_error_envelope(trimmed, &lower)
        || [
            "error: unknown opencode error",
            "error: unknown codex error",
            "error: unknown qwen error",
            "error: unknown gemini error",
            "error: unknown claude error",
        ]
        .iter()
        .any(|prefix| has_explicit_suffix_boundary(&lower, prefix))
}

fn is_single_api_error_envelope(trimmed: &str, lower: &str) -> bool {
    let Some(inner) = trimmed
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    else {
        return false;
    };
    lower.starts_with("[api error:")
        && !inner
            .chars()
            .any(|character| matches!(character, '[' | ']' | '\n' | '\r'))
        && !inner.trim().is_empty()
}

fn has_explicit_suffix_boundary(message: &str, prefix: &str) -> bool {
    let Some(suffix) = message.strip_prefix(prefix) else {
        return false;
    };
    if suffix.is_empty() {
        return true;
    }
    let suffix = suffix.trim_start_matches(|character| matches!(character, ' ' | '\t'));
    matches!(
        suffix.chars().next(),
        Some(':' | '\n' | '\r' | '(' | '[' | '{' | ';')
    )
}

#[cfg(test)]
mod tests {
    use super::is_strong_provider_error_transcript;

    #[test]
    fn recognizes_narrow_provider_error_envelopes() {
        for message in [
            "[API Error: 400 status code (no body)]",
            "Error: Unknown OpenCode error",
            "Error: Unknown Codex error: provider exited",
            "Error: Unknown Codex error (exit code 1)",
            "Error: Unknown Codex error\nstderr: provider exited",
            "Error: Unknown Qwen error",
            "Error: Unknown Gemini error",
            "Error: Unknown Claude error",
        ] {
            assert!(
                is_strong_provider_error_transcript(message),
                "expected provider error envelope: {message}"
            );
        }
    }

    #[test]
    fn ignores_normal_error_discussion() {
        for message in [
            "Error summary: CI failed in lint; the fix is ready.",
            "Error: Unknown Codex error handling is documented here.",
            "[API Error: 400 status code (no body)] follow-up explanation",
            "[API Error: 400 status code (no body)]\nretry succeeded",
            "[API Error: 400 status code (no body)",
        ] {
            assert!(
                !is_strong_provider_error_transcript(message),
                "unexpected provider error envelope: {message}"
            );
        }
    }
}
