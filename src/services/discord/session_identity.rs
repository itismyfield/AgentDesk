//! Session identity parsing (SSoT, issue #1074).
//!
//! The Discord control plane expresses "session identity" through three
//! related primitives:
//!
//! 1. **tmux session name** — `AgentDesk-<provider>-<channel>[<suffix>]`, built
//!    by `ProviderKind::build_tmux_session_name` and parsed back by
//!    [`crate::services::provider::parse_provider_and_channel_from_tmux_name`].
//!
//! 2. **legacy session key** — `<hostname>:<tmux_name>`. Historical wire format
//!    still accepted by external API callers and by DB rows migrated from the
//!    pre-namespaced era.
//!
//! 3. **namespaced session key** — `<provider>/<token_hash>/<hostname>:<tmux_name>`.
//!    Current canonical wire format.
//!
//! Parsing these strings was previously scattered across:
//!   - `db/session_agent_resolution.rs`
//!   - `services/queue.rs`
//!   - `server/routes/session_activity.rs`
//!   - `server/routes/agents.rs`
//!   - `server/routes/dispatched_sessions.rs`
//!   - `services/discord/adk_session.rs`
//!   - `services/discord/tmux.rs`
//!
//! This module consolidates the **parse** half into one unit-tested surface.
//! The **build** half already lives in `ProviderKind::build_tmux_session_name`
//! and `adk_session::build_namespaced_session_key`; those stay put. Production
//! call sites that parse session keys should route through
//! [`SessionIdentity::parse`] or [`tmux_name_from_session_key`], leaving raw
//! colon splitting only for this parser or non-session string formats.
//!
//! See `docs/recovery-paths.md` for the broader recovery-module SSoT plan.

use crate::services::provider::{ProviderKind, parse_provider_and_channel_from_tmux_name};

/// Parsed view of a session key (legacy or namespaced) plus the tmux name
/// embedded in it and any provider/channel derivable from the tmux name.
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
    /// Parse a legacy (`host:tmux`) or namespaced
    /// (`provider/token_hash/host:tmux`) session key.
    ///
    /// Returns `None` when the input is missing the mandatory `:` separator or
    /// when either side of the separator is empty.
    pub fn parse(session_key: &str) -> Option<Self> {
        let trimmed = session_key.trim();
        if trimmed.is_empty() {
            return None;
        }

        // Split on the LAST `:` for compatibility with old provider/channel
        // prefixes such as `claude:<channel_id>:<tmux>`. The canonical forms
        // still have exactly one colon.
        let (prefix, tmux_raw) = trimmed.rsplit_once(':')?;
        let tmux_name = tmux_raw.trim();
        if tmux_name.is_empty() {
            return None;
        }

        // `prefix` has 0 or 2 '/' separators for canonical keys. Legacy audit
        // rows also used `host/alias:tmux`; preserve that by taking the final
        // slash segment as the host unless the prefix starts with a known
        // provider and is therefore a malformed namespaced key.
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

    /// Returns the derived provider/channel tuple from the tmux name, if it
    /// can be parsed.
    // #3034: SessionIdentity accessor surface — no live caller yet; kept as part
    // of the identity-parsing API.
    #[allow(dead_code)]
    pub fn provider_and_channel(&self) -> Option<(ProviderKind, String)> {
        parse_provider_and_channel_from_tmux_name(&self.tmux_name)
    }

    /// Render back to the legacy `host:tmux` form.
    #[allow(dead_code)] // #3034: legacy-form compat accessor, see note above.
    pub fn legacy_key(&self) -> String {
        format!("{}:{}", self.host, self.tmux_name)
    }
}

/// Extract just the tmux session name from a session key in either form.
///
/// Convenience wrapper around [`SessionIdentity::parse`] for call sites that
/// only need the tmux tail. New code should prefer this over ad hoc separator
/// parsing.
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
