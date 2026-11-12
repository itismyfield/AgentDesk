//! Session-key values and parsing for legacy and namespaced wire formats.

pub(crate) mod managed;
pub(crate) mod naming;

use crate::domain::provider::ProviderKind;

/// Parsed legacy or namespaced session key and its embedded tmux name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionIdentity {
    /// Present for namespaced keys only (`<provider>/<token_hash>/...`).
    pub provider_from_key: Option<String>,
    /// Present for namespaced keys only.
    pub token_hash: Option<String>,
    /// Hostname prefix before the `:` separator.
    pub host: String,
    /// The tmux session name suffix (everything after the final `:`).
    pub tmux_name: String,
    /// Whether this key used the namespaced triple format.
    pub namespaced: bool,
}

impl SessionIdentity {
    /// Parse legacy `host:tmux` or namespaced `provider/token_hash/host:tmux`.
    /// Missing separators and empty mandatory parts return `None`.
    pub fn parse(session_key: &str) -> Option<Self> {
        let trimmed = session_key.trim();
        if trimmed.is_empty() {
            return None;
        }

        // Preserve old `provider:channel:tmux` prefixes by splitting at the last colon.
        let (prefix, tmux_raw) = trimmed.rsplit_once(':')?;
        let tmux_name = tmux_raw.trim();
        if tmux_name.is_empty() {
            return None;
        }

        // Legacy host aliases use the final slash segment; known provider prefixes
        // must instead contain every namespaced part.
        let prefix_parts: Vec<&str> = prefix.splitn(3, '/').collect();
        let (host, provider_from_key, token_hash, namespaced) = match prefix_parts.as_slice() {
            [provider, token, host_part] if ProviderKind::from_str(provider).is_some() => {
                if provider.is_empty() || token.is_empty() || host_part.trim().is_empty() {
                    return None;
                }
                (
                    host_part.trim().to_string(),
                    Some((*provider).to_string()),
                    Some((*token).to_string()),
                    true,
                )
            }
            [provider, ..]
                if ProviderKind::from_str(provider).is_some() && prefix.contains('/') =>
            {
                return None;
            }
            _ => (
                prefix.rsplit('/').next()?.trim().to_string(),
                None,
                None,
                false,
            ),
        };

        if host.is_empty() {
            return None;
        }

        Some(Self {
            provider_from_key,
            token_hash,
            host,
            tmux_name: tmux_name.to_string(),
            namespaced,
        })
    }
}

/// Extract the tmux tail from either session-key format.
pub fn tmux_name_from_session_key(session_key: &str) -> Option<String> {
    SessionIdentity::parse(session_key).map(|id| id.tmux_name)
}

#[cfg(test)]
mod tests {
    use super::{SessionIdentity, tmux_name_from_session_key};

    #[test]
    fn parses_legacy_session_key() {
        let identity = SessionIdentity::parse(" mac-mini:AgentDesk-codex-adk-cdx ").unwrap();

        assert!(!identity.namespaced);
        assert_eq!(identity.provider_from_key, None);
        assert_eq!(identity.token_hash, None);
        assert_eq!(identity.host, "mac-mini");
        assert_eq!(identity.tmux_name, "AgentDesk-codex-adk-cdx");
        assert_eq!(identity.legacy_key(), "mac-mini:AgentDesk-codex-adk-cdx");
    }

    #[test]
    fn parses_namespaced_session_key() {
        let identity =
            SessionIdentity::parse("codex/hash123/mac-mini:AgentDesk-codex-adk-cdx-t123").unwrap();

        assert!(identity.namespaced);
        assert_eq!(identity.provider_from_key.as_deref(), Some("codex"));
        assert_eq!(identity.token_hash.as_deref(), Some("hash123"));
        assert_eq!(identity.host, "mac-mini");
        assert_eq!(identity.tmux_name, "AgentDesk-codex-adk-cdx-t123");
        assert_eq!(
            identity.legacy_key(),
            "mac-mini:AgentDesk-codex-adk-cdx-t123"
        );
        assert_eq!(
            tmux_name_from_session_key("codex/hash123/mac-mini:AgentDesk-codex-adk-cdx-t123")
                .as_deref(),
            Some("AgentDesk-codex-adk-cdx-t123")
        );
    }

    #[test]
    fn legacy_parser_preserves_last_colon_tail_and_host_aliases() {
        let multi_colon = SessionIdentity::parse(
            "claude:1473922824350601297:agentdesk-claude-channel-1473922824350601297",
        )
        .unwrap();
        assert!(!multi_colon.namespaced);
        assert_eq!(multi_colon.host, "claude:1473922824350601297");
        assert_eq!(
            multi_colon.tmux_name,
            "agentdesk-claude-channel-1473922824350601297"
        );
        assert_eq!(
            tmux_name_from_session_key(
                "claude:1473922824350601297:agentdesk-claude-channel-1473922824350601297"
            )
            .as_deref(),
            Some("agentdesk-claude-channel-1473922824350601297")
        );

        let host_alias = SessionIdentity::parse("remote-host/farbox:codex-1234").unwrap();
        assert!(!host_alias.namespaced);
        assert_eq!(host_alias.host, "farbox");
        assert_eq!(host_alias.tmux_name, "codex-1234");
    }

    #[test]
    fn rejects_missing_or_empty_parts() {
        assert!(SessionIdentity::parse("").is_none());
        assert!(SessionIdentity::parse("mac-mini").is_none());
        assert!(SessionIdentity::parse(":AgentDesk-codex-adk-cdx").is_none());
        assert!(SessionIdentity::parse("mac-mini: ").is_none());
        assert!(SessionIdentity::parse("codex//mac-mini:AgentDesk-codex-adk-cdx").is_none());
        assert!(SessionIdentity::parse("codex/hash123/:AgentDesk-codex-adk-cdx").is_none());
    }
}
