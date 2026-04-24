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
//! and `adk_session::build_namespaced_session_key`; those stay put. Existing
//! call sites can migrate to [`SessionIdentity::parse`] opportunistically —
//! the old inline `split_once(':')` patterns remain equivalent until each
//! site is touched.
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

        // Split on the FIRST `:` — everything before is the host (or the
        // provider/token/host triple), everything after is the tmux name.
        let (prefix, tmux_raw) = trimmed.split_once(':')?;
        let tmux_name = tmux_raw.trim();
        if tmux_name.is_empty() {
            return None;
        }

        // `prefix` has 0 or 2 '/' separators. 0 = legacy `host`;
        // 2 = namespaced `provider/token/host`.
        let prefix_parts: Vec<&str> = prefix.splitn(3, '/').collect();
        let (host, provider_from_key, token_hash, namespaced) = match prefix_parts.as_slice() {
            [provider, token, host_part] if !provider.is_empty() && !token.is_empty() => (
                host_part.trim().to_string(),
                Some((*provider).to_string()),
                Some((*token).to_string()),
                true,
            ),
            [host_only] => (host_only.trim().to_string(), None, None, false),
            _ => return None,
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
    pub fn provider_and_channel(&self) -> Option<(ProviderKind, String)> {
        parse_provider_and_channel_from_tmux_name(&self.tmux_name)
    }

    /// Render back to the legacy `host:tmux` form.
    pub fn legacy_key(&self) -> String {
        format!("{}:{}", self.host, self.tmux_name)
    }
}

/// Extract just the tmux session name from a session key in either form.
///
/// Convenience wrapper around [`SessionIdentity::parse`] for call sites that
/// only need the tmux tail. Previously duplicated across
/// `services/queue.rs`, `server/routes/agents.rs`, `db/session_agent_resolution.rs`,
/// and `server/routes/dispatched_sessions.rs`. New code should prefer this.
pub fn tmux_name_from_session_key(session_key: &str) -> Option<String> {
    SessionIdentity::parse(session_key).map(|id| id.tmux_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_legacy_session_key() {
        let id = SessionIdentity::parse("mac-mini:AgentDesk-codex-ch-cdx")
            .expect("legacy parse should succeed");
        assert_eq!(id.host, "mac-mini");
        assert_eq!(id.tmux_name, "AgentDesk-codex-ch-cdx");
        assert!(!id.namespaced);
        assert!(id.provider_from_key.is_none());
        assert!(id.token_hash.is_none());
        assert_eq!(id.legacy_key(), "mac-mini:AgentDesk-codex-ch-cdx");
    }

    #[test]
    fn parse_namespaced_session_key() {
        let id = SessionIdentity::parse("codex/deadbeef/mac-mini:AgentDesk-codex-ch-cdx")
            .expect("namespaced parse should succeed");
        assert_eq!(id.provider_from_key.as_deref(), Some("codex"));
        assert_eq!(id.token_hash.as_deref(), Some("deadbeef"));
        assert_eq!(id.host, "mac-mini");
        assert_eq!(id.tmux_name, "AgentDesk-codex-ch-cdx");
        assert!(id.namespaced);
        assert_eq!(id.legacy_key(), "mac-mini:AgentDesk-codex-ch-cdx");
    }

    #[test]
    fn parse_rejects_missing_colon() {
        assert!(SessionIdentity::parse("no-colon-here").is_none());
    }

    #[test]
    fn parse_rejects_empty_halves() {
        assert!(SessionIdentity::parse(":tmux-name").is_none());
        assert!(SessionIdentity::parse("host:").is_none());
        assert!(SessionIdentity::parse("").is_none());
        assert!(SessionIdentity::parse("   ").is_none());
    }

    #[test]
    fn tmux_name_helper_extracts_tail() {
        assert_eq!(
            tmux_name_from_session_key("mac-mini:AgentDesk-gemini-x-gm").as_deref(),
            Some("AgentDesk-gemini-x-gm")
        );
        assert_eq!(
            tmux_name_from_session_key("claude/hash/h:AgentDesk-claude-x-cc").as_deref(),
            Some("AgentDesk-claude-x-cc")
        );
        assert_eq!(tmux_name_from_session_key("garbage"), None);
    }

    #[test]
    fn provider_and_channel_roundtrips_through_provider_parser() {
        // Build a tmux name the same way ProviderKind would, then parse back.
        let tmux = ProviderKind::Codex.build_tmux_session_name("adk-cdx-t1485506232256168011");
        let key = format!("mac-mini:{}", tmux);
        let id = SessionIdentity::parse(&key).unwrap();
        let (provider, _channel) = id.provider_and_channel().expect("provider parseable");
        assert_eq!(provider, ProviderKind::Codex);
    }
}
