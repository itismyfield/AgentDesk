/// Recognize provider-generated error-only transcript envelopes that can be
/// emitted without a typed terminal error event.
///
/// Keep this intentionally narrow: ordinary assistant prose such as
/// `Error summary: ...` is still a deliverable response.
const UNKNOWN_PROVIDER_ERROR_PREFIXES: &[&str] = &[
    "error: unknown opencode error",
    "error: unknown codex error",
    "error: unknown qwen error",
    "error: unknown gemini error",
    "error: unknown claude error",
    "error: unknown antigravity error",
];

pub(crate) fn is_strong_provider_error_transcript(message: &str) -> bool {
    let trimmed = message.trim();
    let lower = trimmed.to_ascii_lowercase();
    is_single_api_error_envelope(trimmed, &lower)
        || UNKNOWN_PROVIDER_ERROR_PREFIXES
            .iter()
            .any(|prefix| has_explicit_suffix_boundary(&lower, prefix))
        || is_provider_error_presentation(&lower)
        || is_explicit_provider_error_line(&lower)
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

/// Discord delivery wraps typed provider failures in a user-facing error
/// block. The wrapper is still an error-only response, not useful routine
/// work, so recognize it without matching ordinary prose that merely mentions
/// an error or a rate limit.
fn is_provider_error_presentation(lower: &str) -> bool {
    let framed = lower.contains("||") && lower.contains("```") && lower.contains("provider");
    if !framed {
        return false;
    }

    UNKNOWN_PROVIDER_ERROR_PREFIXES
        .iter()
        .any(|prefix| lower.contains(prefix))
        || has_rate_limit_marker(lower)
        || (lower.starts_with("⚠️ provider가 응답을 완료하지 못했어요.")
            && lower
                .lines()
                .any(|line| is_explicit_provider_error_line(line.trim())))
}

/// OpenCode may surface the provider failure directly as an `Error: ...`
/// transcript. Require a known transport/provider marker so a legitimate
/// report beginning with `Error:` remains deliverable.
fn is_explicit_provider_error_line(lower: &str) -> bool {
    let Some(rest) = lower.strip_prefix("error:") else {
        return false;
    };
    let rest = rest.trim_start();
    rest.chars().count() <= 2_000 && has_provider_error_marker_at_start(rest)
}

fn has_rate_limit_marker(lower: &str) -> bool {
    [
        "apierror",
        "ai_apicallerror",
        "too many requests",
        "rate limit",
        "rate-limit",
        "status code (429)",
        "status code: 429",
        "http 429",
        "statuscode: 429",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn has_provider_error_marker_at_start(lower: &str) -> bool {
    [
        "agy returned an empty response",
        "apierror",
        "ai_apicallerror",
        "too many requests",
        "rate limit",
        "rate-limit",
        "status code (429)",
        "status code: 429",
        "http 429",
        "statuscode: 429",
    ]
    .iter()
    .any(|marker| {
        let Some(suffix) = lower.strip_prefix(marker) else {
            return false;
        };
        suffix.is_empty()
            || matches!(
                suffix.chars().next(),
                Some(':' | '(' | '[' | '{' | ';' | ' ')
            )
    })
}

/// A fresh Qwen wrapper can remain alive after a terminal API error. Only
/// accept its complete latest send/error/ready envelope, never old scrollback
/// or an error quoted inside a useful response.
pub(crate) fn qwen_terminal_api_error(pane: &str) -> Option<String> {
    let (_, latest_send) = pane.rsplit_once("[sending...]")?;
    let lines: Vec<_> = latest_send
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if lines.len() != 2 || lines[1] != "▶ Ready for input (type message + Enter)" {
        return None;
    }
    let error = lines[0];
    (error.starts_with("[API Error:") && is_strong_provider_error_transcript(error))
        .then(|| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::{is_strong_provider_error_transcript, qwen_terminal_api_error};

    #[test]
    fn qwen_terminal_error_requires_latest_complete_wrapper_envelope() {
        let failed = "[sending...]\n[API Error: 410 status code (no body)]\n\n▶ Ready for input (type message + Enter)\n";
        assert_eq!(
            qwen_terminal_api_error(failed).as_deref(),
            Some("[API Error: 410 status code (no body)]")
        );
        assert!(qwen_terminal_api_error(&format!("{failed}\n[sending...]\nWorking...")).is_none());
        assert!(
            qwen_terminal_api_error(
                &failed.replace("▶ Ready for input (type message + Enter)", "Working...")
            )
            .is_none()
        );
        assert!(qwen_terminal_api_error(&failed.replace("[sending...]", "Report:")).is_none());
        assert!(
            qwen_terminal_api_error(&failed.replace("\n\n▶", "\nRecovered successfully\n▶"))
                .is_none()
        );
    }

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
            "Error: AI_APICallError: Too Many Requests (429)",
            "Error: APIError",
            "Error: APIError: upstream request failed",
            "Error: AGY returned an empty response because a tool permission was denied in headless mode.",
            "⚠️ provider가 응답을 완료하지 못했어요.\n||**상세**\n```text\nError: AGY returned an empty response because a tool permission was denied in headless mode.\n```||",
            "⚠️ provider가 응답을 완료하지 못했어요.\n||**상세**\n```text\nError: Unknown OpenCode error\n```||",
            "⚠️ provider가 응답을 완료하지 못했어요.\n||**상세**\n```text\nError: APIError\n```||",
            "⚠️ provider가 응답을 완료하지 못했어요.\n||**상세**\n```text\nAI_APICallError: statusCode: 429\n```||",
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
            "Error: a report about an APIError is ready.",
            "[API Error: 400 status code (no body)] follow-up explanation",
            "[API Error: 400 status code (no body)]\nretry succeeded",
            "[API Error: 400 status code (no body)",
            "Error: a report about an AI_APICallError is ready.",
            "The provider returned Too Many Requests; the retry succeeded.",
            "Provider error example: ||```text\nError: AGY returned an empty response\n```||\nThe retry succeeded.",
        ] {
            assert!(
                !is_strong_provider_error_transcript(message),
                "unexpected provider error envelope: {message}"
            );
        }
    }
}
