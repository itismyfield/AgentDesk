//! Pure provider/channel derivation from tmux session names.

use super::SessionIdentity;
use crate::domain::provider::naming::{tmux_env_suffix, tmux_session_prefix};
use crate::domain::provider::{ProviderKind, provider_catalog};

pub fn parse_provider_and_channel_from_tmux_name(
    session_name: &str,
) -> Option<(ProviderKind, String)> {
    let prefix = format!("{}-", tmux_session_prefix());
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

/// The provider a session records: its stored value, then a namespaced key, and only for a
/// legacy-prefixed tail the name decoder. A new-format name never supplies or defaults one.
pub fn recorded_provider(stored: Option<&str>, session_key: Option<&str>) -> Option<ProviderKind> {
    if let Some(stored) = stored.map(str::trim).filter(|value| !value.is_empty()) {
        return ProviderKind::from_str(stored);
    }
    let identity = SessionIdentity::parse(session_key?)?;
    match identity.provider_from_key.as_deref() {
        Some(named) => ProviderKind::from_str(named),
        None => identity
            .provider_and_channel()
            .map(|(provider, _)| provider),
    }
}

impl SessionIdentity {
    /// Whether everything this key records about its provider, a namespace and a legacy tail,
    /// names `provider`; a key that records neither proves nothing.
    pub fn records_provider(&self, provider: &ProviderKind) -> bool {
        let keyed = self
            .provider_from_key
            .as_deref()
            .map(ProviderKind::from_str);
        let named = self.provider_and_channel().map(|(named, _)| named);
        (keyed.is_some() || named.is_some())
            && keyed.is_none_or(|keyed| keyed.as_ref() == Some(provider))
            && named.is_none_or(|named| &named == provider)
    }

    /// Derive the provider and channel encoded in the tmux name.
    pub fn provider_and_channel(&self) -> Option<(ProviderKind, String)> {
        parse_provider_and_channel_from_tmux_name(&self.tmux_name)
    }
}
