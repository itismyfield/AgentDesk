//! Provider-specific session token validation.

use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderSessionToken(String);

impl ProviderSessionToken {
    /// Opaque session token. Dialects validate the provider-specific shape.
    pub fn new_opaque(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

pub fn parse_strict_uuid(raw: &str, provider: &str) -> Result<ProviderSessionToken, String> {
    let trimmed = raw.trim();
    Uuid::parse_str(trimmed).map_err(|_| {
        format!("InvalidArgument: {provider} session id must be a UUID, got {trimmed}")
    })?;
    Ok(ProviderSessionToken(trimmed.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_uuid() {
        assert!(parse_strict_uuid("latest", "grok").is_err());
        assert!(parse_strict_uuid("not-a-uuid", "antigravity").is_err());
    }

    #[test]
    fn accepts_uuid() {
        let token = parse_strict_uuid("01234567-89ab-cdef-0123-456789abcdef", "grok").unwrap();
        assert_eq!(token.as_str(), "01234567-89ab-cdef-0123-456789abcdef");
    }
}
