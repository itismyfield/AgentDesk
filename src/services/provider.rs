use crate::utils::format::safe_prefix;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderKind {
    Claude,
    Codex,
    Unsupported(String),
}

impl ProviderKind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Unsupported(s) => s.as_str(),
        }
    }

    pub fn display_name(&self) -> &str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::Unsupported(s) => s.as_str(),
        }
    }

    pub fn counterpart(&self) -> Self {
        match self {
            Self::Claude => Self::Codex,
            Self::Codex => Self::Claude,
            Self::Unsupported(_) => self.clone(),
        }
    }

    /// Parse a known provider string. Returns None for unknown providers.
    pub fn from_str(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    /// Parse a provider string, returning Unsupported for unknown providers.
    pub fn from_str_or_unsupported(raw: &str) -> Self {
        Self::from_str(raw).unwrap_or_else(|| Self::Unsupported(raw.trim().to_string()))
    }

    /// Returns true if this is a known, supported provider (Claude or Codex).
    pub fn is_supported(&self) -> bool {
        !matches!(self, Self::Unsupported(_))
    }

    pub fn is_channel_supported(&self, channel_name: Option<&str>, is_dm: bool) -> bool {
        if is_dm {
            return self.is_supported();
        }

        let Some(channel_name) = channel_name else {
            return matches!(self, Self::Claude);
        };

        if channel_name.ends_with("-cdx") {
            return matches!(self, Self::Codex);
        }

        if channel_name.ends_with("-cc") {
            return matches!(self, Self::Claude);
        }

        matches!(self, Self::Claude)
    }

    pub fn build_tmux_session_name(&self, channel_name: &str) -> String {
        let sanitized: String = channel_name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let trimmed = safe_prefix(&sanitized, 44);
        format!("remoteCC-{}-{}", self.as_str(), trimmed)
    }
}

pub fn parse_provider_and_channel_from_tmux_name(
    session_name: &str,
) -> Option<(ProviderKind, String)> {
    let stripped = session_name.strip_prefix("remoteCC-")?;
    if let Some(rest) = stripped.strip_prefix("claude-") {
        return Some((ProviderKind::Claude, rest.to_string()));
    }
    if let Some(rest) = stripped.strip_prefix("codex-") {
        return Some((ProviderKind::Codex, rest.to_string()));
    }
    Some((ProviderKind::Claude, stripped.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{parse_provider_and_channel_from_tmux_name, ProviderKind};

    #[test]
    fn test_provider_channel_support() {
        assert!(ProviderKind::Claude.is_channel_supported(Some("mac-mini"), false));
        assert!(ProviderKind::Claude.is_channel_supported(Some("cookingheart-dev-cc"), false));
        assert!(!ProviderKind::Claude.is_channel_supported(Some("cookingheart-dev-cdx"), false));
        assert!(ProviderKind::Codex.is_channel_supported(Some("cookingheart-dev-cdx"), false));
        assert!(!ProviderKind::Codex.is_channel_supported(Some("cookingheart-dev-cc"), false));
        assert!(ProviderKind::Codex.is_channel_supported(None, true));
    }

    #[test]
    fn test_unsupported_provider() {
        let p = ProviderKind::from_str_or_unsupported("gemini");
        assert!(!p.is_supported());
        assert_eq!(p.as_str(), "gemini");
        assert_eq!(p.display_name(), "gemini");
        assert!(!p.is_channel_supported(Some("test-cc"), false));
        assert!(!p.is_channel_supported(Some("test"), false));
        assert!(!p.is_channel_supported(None, true)); // unsupported even in DM
    }

    #[test]
    fn test_from_str_or_unsupported_known() {
        assert_eq!(ProviderKind::from_str_or_unsupported("claude"), ProviderKind::Claude);
        assert_eq!(ProviderKind::from_str_or_unsupported("Codex"), ProviderKind::Codex);
    }

    #[test]
    fn test_tmux_name_parse_supports_legacy_and_provider_aware_names() {
        assert_eq!(
            parse_provider_and_channel_from_tmux_name("remoteCC-claude-cookingheart-dev-cc"),
            Some((ProviderKind::Claude, "cookingheart-dev-cc".to_string()))
        );
        assert_eq!(
            parse_provider_and_channel_from_tmux_name("remoteCC-codex-cookingheart-dev-cdx"),
            Some((ProviderKind::Codex, "cookingheart-dev-cdx".to_string()))
        );
        assert_eq!(
            parse_provider_and_channel_from_tmux_name("remoteCC-mac-mini"),
            Some((ProviderKind::Claude, "mac-mini".to_string()))
        );
    }
}
