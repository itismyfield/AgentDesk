//! The provider of a session from structured evidence; a display name is not decoded.

use super::ProviderKind;
use crate::domain::session_identity::naming::recorded_provider;

/// Stored provider, namespaced key and legacy tail first; a name none of them covers answers
/// from the identity marker its tmux session was created with.
pub(crate) fn session_provider(
    stored: Option<&str>,
    session_key: Option<&str>,
    tmux_name: &str,
) -> Option<ProviderKind> {
    recorded_provider(stored, session_key).or_else(|| tmux_session_provider(tmux_name))
}

/// The provider of a live tmux session known only by name: the legacy decoder for a legacy
/// name, otherwise the marker this runtime scope wrote at creation. Unreadable is unknown.
pub(crate) fn tmux_session_provider(tmux_name: &str) -> Option<ProviderKind> {
    if let Some((provider, _)) = super::parse_provider_and_channel_from_tmux_name(tmux_name) {
        return Some(provider);
    }
    let marker = crate::services::platform::tmux::session_identity::managed_identity(tmux_name);
    marker
        .ok()?
        .provider
        .as_deref()
        .and_then(ProviderKind::from_str)
}
