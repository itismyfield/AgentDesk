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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DetectedProviderOverload {
    pub(super) reason: ProviderOverloadReason,
    pub(super) display_reason: String,
}

fn detected_status(status: u64) -> Option<DetectedProviderOverload> {
    let status = u16::try_from(status).ok()?;
    Some(DetectedProviderOverload {
        reason: ProviderOverloadReason::HttpStatus(status),
        display_reason: format!("Provider HTTP {status} capacity response"),
    })
}

fn detected_code(code: &str) -> Option<DetectedProviderOverload> {
    let code = OVERLOAD_ERROR_CODES
        .iter()
        .copied()
        .find(|candidate| *candidate == code)?;
    Some(DetectedProviderOverload {
        reason: ProviderOverloadReason::ErrorCode(code),
        display_reason: format!("Provider capacity error ({code})"),
    })
}

pub(super) fn detect_structured_provider_overload(
    value: &serde_json::Value,
) -> Option<DetectedProviderOverload> {
    if value.get("type").and_then(serde_json::Value::as_str) != Some("result") {
        return None;
    }
    let is_error = value
        .get("is_error")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !is_error {
        return None;
    }

    structured_error_objects(value)
        .find_map(|object| {
            for key in ["status", "status_code", "statusCode", "http_status"] {
                if let Some(status) = object.get(key).and_then(structured_status_code)
                    && OVERLOAD_STATUS_CODES.contains(&status)
                {
                    return detected_status(status);
                }
            }
            for key in ["code", "error_code", "errorCode", "type"] {
                if let Some(code) = object.get(key).and_then(serde_json::Value::as_str) {
                    let normalized = code.trim().to_ascii_lowercase().replace(['-', ' '], "_");
                    if OVERLOAD_ERROR_CODES.contains(&normalized.as_str()) {
                        return detected_code(&normalized);
                    }
                }
            }
            None
        })
        .or_else(|| result_error_texts(value).find_map(detect_api_error_overload_envelope))
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

fn detect_api_error_overload_envelope(text: &str) -> Option<DetectedProviderOverload> {
    let detail =
        crate::services::provider_error_transcript::single_api_error_envelope_detail(text)?;
    let lower = detail.to_ascii_lowercase();
    let status = lower
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .find_map(|token| token.parse::<u64>().ok())
        .filter(|status| OVERLOAD_STATUS_CODES.contains(status));
    if let Some(status) = status {
        return detected_status(status);
    }

    lower
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .find_map(detected_code)
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

#[cfg(test)]
mod pure_tests {
    use super::{
        DetectedProviderOverload, ProviderOverloadReason, detect_structured_provider_overload,
        is_auth_error_message,
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
            "content": "review says a transient edit failure (rate limit) uses the same path"
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
    fn overload_accepts_real_provider_error_envelopes() {
        let claude = serde_json::json!({
            "type": "result",
            "subtype": "error_during_execution",
            "is_error": true,
            "result": "[API Error: 529 overloaded_error]"
        });
        let wrapper = serde_json::json!({
            "type": "result",
            "subtype": "error_during_execution",
            "is_error": true,
            "errors": ["[API Error: 429 status code (no body)]"]
        });
        assert_eq!(
            detect_structured_provider_overload(&claude),
            Some(DetectedProviderOverload {
                reason: ProviderOverloadReason::HttpStatus(529),
                display_reason: "Provider HTTP 529 capacity response".to_string(),
            })
        );
        assert_eq!(
            detect_structured_provider_overload(&wrapper),
            Some(DetectedProviderOverload {
                reason: ProviderOverloadReason::HttpStatus(429),
                display_reason: "Provider HTTP 429 capacity response".to_string(),
            })
        );
    }

    #[test]
    fn overload_rejects_api_error_envelope_embedded_in_review_prose() {
        let quoted = serde_json::json!({
            "type": "result",
            "subtype": "error_during_execution",
            "is_error": true,
            "errors": ["child review quoted [API Error: 529 overloaded_error] before recovery"]
        });
        assert_eq!(detect_structured_provider_overload(&quoted), None);
    }

    #[test]
    fn overload_requires_result_provenance_even_for_exact_envelope() {
        let system = serde_json::json!({
            "type": "system",
            "subtype": "task_notification",
            "is_error": true,
            "result": "[API Error: 529 overloaded_error]"
        });
        assert_eq!(detect_structured_provider_overload(&system), None);
    }

    #[test]
    fn overload_api_error_prefix_is_case_insensitive() {
        let value = serde_json::json!({
            "type": "result",
            "subtype": "error_during_execution",
            "is_error": true,
            "errors": ["[api error: 529 overloaded_error]"]
        });
        assert_eq!(
            detect_structured_provider_overload(&value)
                .as_ref()
                .map(|detected| detected.reason),
            Some(ProviderOverloadReason::HttpStatus(529))
        );
    }
}
