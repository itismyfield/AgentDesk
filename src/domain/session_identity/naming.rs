//! Pure provider/channel derivation from tmux session names.

use super::SessionIdentity;
use crate::domain::provider::naming::{TMUX_SESSION_PREFIX, tmux_env_suffix};
use crate::domain::provider::{ProviderKind, provider_catalog};

pub fn parse_provider_and_channel_from_tmux_name(
    session_name: &str,
) -> Option<(ProviderKind, String)> {
    let prefix = format!("{}-", TMUX_SESSION_PREFIX);
    let stripped = session_name.strip_prefix(&prefix)?;
    let without_suffix = stripped.strip_suffix(tmux_env_suffix()).unwrap_or(stripped);
    for entry in provider_catalog() {
        let prefix = format!("{}-", entry.id);
        if let Some(rest) = without_suffix.strip_prefix(&prefix) {
            if let Some(provider) = ProviderKind::from_str(entry.id) {
                return Some((provider, rest.to_string()));
            }
        }
    }
    provider_catalog()
        .iter()
        .find(|entry| entry.default_channel_provider)
        .and_then(|entry| entry.kind())
        .map(|provider| (provider, without_suffix.to_string()))
}

impl SessionIdentity {
    /// Derive the provider and channel encoded in the tmux name.
    pub fn provider_and_channel(&self) -> Option<(ProviderKind, String)> {
        parse_provider_and_channel_from_tmux_name(&self.tmux_name)
    }
}
