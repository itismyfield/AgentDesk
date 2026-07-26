pub(super) fn is_prompt_too_long_message(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("prompt is too long")
        || lower.contains("prompt too long")
        || lower.contains("context_length_exceeded")
        || lower.contains("conversation too long")
        || lower.contains("context window")
}

pub(super) fn is_auth_error_message(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("not logged in")
        || lower.contains("authentication error")
        || lower.contains("unauthorized")
        || lower.contains("please run /login")
        || lower.contains("oauth")
        || lower.contains("access token could not be refreshed")
        || (lower.contains("refresh token")
            && (lower.contains("expired")
                || lower.contains("invalid")
                || lower.contains("revoked")
                || lower.contains("already used")))
        || lower.contains("please log out and sign in again")
        || lower.contains("token expired")
        || lower.contains("invalid api key")
        || (lower.contains("api key")
            && (lower.contains("missing")
                || lower.contains("invalid")
                || lower.contains("expired")))
}

#[allow(dead_code)]
pub(super) fn detect_provider_overload_message(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let lower = trimmed.to_lowercase();
    let looks_overloaded = lower.contains("selected model is at capacity")
        || lower.contains("model is at capacity")
        || (lower.contains("at capacity") && lower.contains("model"))
        || lower.contains("try a different model")
        || lower.contains("rate limit")
        || lower.contains("hit your limit")
        || lower.contains("usage limit")
        || lower.contains("limit to reset")
        || lower.contains("too many requests")
        || lower.contains("provider overloaded")
        || lower.contains("server overloaded")
        || lower.contains("service overloaded")
        || lower.contains("overloaded")
        || lower.contains("please try again later");

    if looks_overloaded {
        Some(trimmed.to_string())
    } else {
        None
    }
}

const OVERLOAD_STATUS_CODES: [u64; 2] = [429, 529];
const OVERLOAD_ERROR_CODES: [&str; 7] = [
    "rate_limit",
    "rate_limited",
    "rate_limit_error",
    "too_many_requests",
    "overloaded",
    "overloaded_error",
    "provider_overloaded",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProviderOverloadReason {
    HttpStatus(u16),
    ErrorCode(&'static str),
}

impl ProviderOverloadReason {
    pub(super) fn public_reason(self) -> &'static str {
        match self {
            Self::HttpStatus(429) => "provider_http_429_capacity_response",
            Self::HttpStatus(529) => "provider_http_529_capacity_response",
            Self::ErrorCode("rate_limit")
            | Self::ErrorCode("rate_limited")
            | Self::ErrorCode("rate_limit_error")
            | Self::ErrorCode("too_many_requests") => "provider_rate_limit_response",
            Self::ErrorCode("overloaded")
            | Self::ErrorCode("overloaded_error")
            | Self::ErrorCode("provider_overloaded") => "provider_overload_response",
            _ => "provider_capacity_response",
        }
    }
}

pub(super) fn detect_structured_provider_overload(
    value: &serde_json::Value,
) -> Option<ProviderOverloadReason> {
    if value.get("type").and_then(serde_json::Value::as_str) != Some("result")
        || value.get("is_error").and_then(serde_json::Value::as_bool) != Some(true)
    {
        return None;
    }

    structured_error_objects(value)
        .find_map(|object| {
            for key in ["status", "status_code", "statusCode", "http_status"] {
                if let Some(status) = object.get(key).and_then(structured_status_code)
                    && OVERLOAD_STATUS_CODES.contains(&status)
                {
                    return u16::try_from(status)
                        .ok()
                        .map(ProviderOverloadReason::HttpStatus);
                }
            }
            for key in ["code", "error_code", "errorCode", "type"] {
                let Some(code) = object.get(key).and_then(serde_json::Value::as_str) else {
                    continue;
                };
                let normalized = code.trim().to_ascii_lowercase().replace(['-', ' '], "_");
                if let Some(code) = OVERLOAD_ERROR_CODES
                    .iter()
                    .copied()
                    .find(|candidate| *candidate == normalized)
                {
                    return Some(ProviderOverloadReason::ErrorCode(code));
                }
            }
            None
        })
        .or_else(|| result_error_texts(value).find_map(detect_api_error_overload_envelope))
}

fn structured_status_code(value: &serde_json::Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str()?.trim().parse::<u64>().ok())
}

fn structured_error_objects(
    value: &serde_json::Value,
) -> impl Iterator<Item = &serde_json::Map<String, serde_json::Value>> {
    std::iter::once(value.as_object())
        .chain(
            ["error", "provider_error", "response", "cause"]
                .into_iter()
                .filter_map(|key| value.get(key).and_then(serde_json::Value::as_object))
                .map(Some),
        )
        .flatten()
}

fn result_error_texts(value: &serde_json::Value) -> impl Iterator<Item = &str> {
    value
        .get("errors")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .chain(value.get("result").and_then(serde_json::Value::as_str))
}

fn detect_api_error_overload_envelope(text: &str) -> Option<ProviderOverloadReason> {
    let trimmed = text.trim();
    let inner = trimmed.strip_prefix('[')?.strip_suffix(']')?;
    let detail = inner
        .strip_prefix("API Error:")
        .or_else(|| inner.strip_prefix("api error:"))?
        .trim();
    if detail.is_empty()
        || detail
            .chars()
            .any(|character| matches!(character, '[' | ']' | '\n' | '\r'))
    {
        return None;
    }
    let lower = detail.to_ascii_lowercase();
    if let Some(status) = lower
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .find_map(|token| token.parse::<u64>().ok())
        .filter(|status| OVERLOAD_STATUS_CODES.contains(status))
    {
        return u16::try_from(status)
            .ok()
            .map(ProviderOverloadReason::HttpStatus);
    }
    lower
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .find_map(|token| {
            OVERLOAD_ERROR_CODES
                .iter()
                .copied()
                .find(|candidate| *candidate == token)
                .map(ProviderOverloadReason::ErrorCode)
        })
}

#[cfg(test)]
mod pure_tests {
    use super::{
        ProviderOverloadReason, detect_structured_provider_overload, is_auth_error_message,
    };

    #[test]
    fn auth_error_detects_expired_refresh_token_variants() {
        assert!(is_auth_error_message("refresh token was already used"));
        assert!(is_auth_error_message("Please log out and sign in again"));
    }

    #[test]
    fn overload_requires_structured_error_provenance() {
        let prose = serde_json::json!({
            "type": "system",
            "subtype": "task_notification",
            "content": "review says rate limit and overloaded are ordinary prose"
        });
        let normal_result = serde_json::json!({
            "type": "result",
            "is_error": false,
            "result": "rate limit and overloaded are ordinary review prose",
            "status": 429
        });
        assert_eq!(detect_structured_provider_overload(&prose), None);
        assert_eq!(detect_structured_provider_overload(&normal_result), None);
    }

    #[test]
    fn overload_accepts_structured_429_and_529() {
        let status = serde_json::json!({
            "type": "result",
            "is_error": true,
            "error": {"status": 429}
        });
        let envelope = serde_json::json!({
            "type": "result",
            "is_error": true,
            "errors": ["[API Error: 529 overloaded_error]"]
        });
        assert_eq!(
            detect_structured_provider_overload(&status),
            Some(ProviderOverloadReason::HttpStatus(429))
        );
        assert_eq!(
            detect_structured_provider_overload(&envelope),
            Some(ProviderOverloadReason::HttpStatus(529))
        );
    }

    #[test]
    fn overload_rejects_envelope_embedded_in_prose() {
        let quoted = serde_json::json!({
            "type": "result",
            "is_error": true,
            "errors": ["review quoted [API Error: 529 overloaded_error] before recovery"]
        });
        assert_eq!(detect_structured_provider_overload(&quoted), None);
    }
}
