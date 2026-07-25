/// Recognize provider-generated error-only transcript envelopes that can be
/// emitted without a typed terminal error event.
///
/// Keep this intentionally narrow: ordinary assistant prose such as
/// `Error summary: ...` is still a deliverable response.
pub(crate) fn is_strong_provider_error_transcript(message: &str) -> bool {
    let trimmed = message.trim();
    let lower = trimmed.to_ascii_lowercase();
    single_api_error_envelope_detail(trimmed).is_some()
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

pub(crate) fn single_api_error_envelope_detail(message: &str) -> Option<&str> {
    let trimmed = message.trim();
    let inner = trimmed.strip_prefix('[')?.strip_suffix(']')?;
    let prefix_len = "api error:".len();
    let prefix = inner.get(..prefix_len)?;
    if !prefix.eq_ignore_ascii_case("api error:") {
        return None;
    }
    let detail = inner.get(prefix_len..)?.trim();
    (!detail.is_empty()
        && !detail
            .chars()
            .any(|character| matches!(character, '[' | ']' | '\n' | '\r')))
    .then_some(detail)
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
            "[api error: 529 overloaded_error]",
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
