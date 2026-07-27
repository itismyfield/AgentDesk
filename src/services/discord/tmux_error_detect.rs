pub(super) fn is_prompt_too_long_message(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("prompt is too long")
        || lower.contains("prompt too long")
        || lower.contains("context_length_exceeded")
        || lower.contains("conversation too long")
        || lower.contains("context window")
}

/// A bounded diagnosis category for untyped provider prose.
///
/// This type intentionally carries no source text and no terminal authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderProseDiagnostic {
    Authentication,
    Overload,
}

impl ProviderProseDiagnostic {
    pub(crate) const COUNT: usize = 2;

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Authentication => 0,
            Self::Overload => 1,
        }
    }

    pub(crate) const fn summary(self) -> &'static str {
        match self {
            Self::Authentication => "authentication-like provider diagnostic",
            Self::Overload => "overload-like provider diagnostic",
        }
    }

    pub(crate) const fn redacted_content(self) -> &'static str {
        match self {
            Self::Authentication => {
                "Provider emitted authentication-like untyped prose; details redacted."
            }
            Self::Overload => "Provider emitted overload-like untyped prose; details redacted.",
        }
    }
}

pub(crate) fn classify_provider_prose_diagnostic(text: &str) -> Option<ProviderProseDiagnostic> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    if is_auth_like_prose(trimmed) {
        Some(ProviderProseDiagnostic::Authentication)
    } else if is_overload_like_prose(trimmed) {
        Some(ProviderProseDiagnostic::Overload)
    } else {
        None
    }
}

fn is_auth_like_prose(text: &str) -> bool {
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

fn is_overload_like_prose(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("selected model is at capacity")
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
        || lower.contains("please try again later")
}

#[cfg(test)]
mod pure_tests {
    use super::{ProviderProseDiagnostic, classify_provider_prose_diagnostic};

    #[test]
    fn classifies_auth_and_overload_prose_without_retaining_source_text() {
        assert_eq!(
            classify_provider_prose_diagnostic("refresh token was already used"),
            Some(ProviderProseDiagnostic::Authentication)
        );
        assert_eq!(
            classify_provider_prose_diagnostic("Please log out and sign in again"),
            Some(ProviderProseDiagnostic::Authentication)
        );
        assert_eq!(
            classify_provider_prose_diagnostic("529 server overloaded"),
            Some(ProviderProseDiagnostic::Overload)
        );
    }
}
