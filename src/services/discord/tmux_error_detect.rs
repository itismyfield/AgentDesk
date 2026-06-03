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

#[cfg(test)]
mod pure_tests {
    use super::is_auth_error_message;

    #[test]
    fn auth_error_detects_expired_refresh_token_variants() {
        assert!(is_auth_error_message("refresh token was already used"));
        assert!(is_auth_error_message("Please log out and sign in again"));
    }
}
