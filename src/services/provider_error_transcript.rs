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
    api_error_envelope_detail(trimmed, lower).is_some()
}

fn api_error_envelope_detail<'a>(trimmed: &'a str, lower: &str) -> Option<&'a str> {
    let inner = trimmed.strip_prefix('[')?.strip_suffix(']')?;
    if !lower.starts_with("[api error:")
        || inner
            .chars()
            .any(|character| matches!(character, '[' | ']' | '\n' | '\r'))
    {
        return None;
    }
    let (_, detail) = inner.split_once(':')?;
    (!detail.trim().is_empty()).then_some(detail.trim())
}

pub(crate) fn provider_overload_metadata(message: &str) -> Option<serde_json::Value> {
    let trimmed = message.trim();
    let lower = trimmed.to_ascii_lowercase();
    let detail = api_error_envelope_detail(trimmed, &lower)?.to_ascii_lowercase();
    detail
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .find_map(|token| match token {
            "429" | "529" => token
                .parse::<u16>()
                .ok()
                .map(|status| serde_json::json!({ "status": status })),
            "rate_limit"
            | "rate_limited"
            | "rate_limit_error"
            | "too_many_requests"
            | "overloaded"
            | "overloaded_error"
            | "provider_overloaded" => Some(serde_json::json!({ "code": token })),
            _ => None,
        })
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
    use super::{is_strong_provider_error_transcript, provider_overload_metadata};

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

    #[test]
    fn overload_metadata_requires_closed_api_error_envelope() {
        assert_eq!(
            provider_overload_metadata("[API Error: 429 too many requests]"),
            Some(serde_json::json!({"status": 429}))
        );
        assert_eq!(
            provider_overload_metadata("[API Error: overloaded_error]"),
            Some(serde_json::json!({"code": "overloaded_error"}))
        );
        assert_eq!(
            provider_overload_metadata("[API Error: OVERLOADED_ERROR]"),
            Some(serde_json::json!({"code": "overloaded_error"}))
        );
        assert_eq!(
            provider_overload_metadata("review says rate limit and 529 overloaded_error"),
            None
        );
        assert_eq!(
            provider_overload_metadata(
                "[API Error: 529 overloaded_error]\nrecovered on the next attempt"
            ),
            None
        );
    }
}
