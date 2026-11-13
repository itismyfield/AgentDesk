use crate::domain::session_identity::SessionIdentity;
use crate::services::provider::ProviderKind;
use std::collections::HashSet;

pub(super) fn live_name(
    key: &str,
    provider: Option<&str>,
    live: &HashSet<String>,
) -> Option<(ProviderKind, String)> {
    let provider = ProviderKind::from_str(provider?)?;
    let parsed = SessionIdentity::parse(key)?;
    if !live.contains(&parsed.tmux_name)
        || parsed
            .provider_from_key
            .as_deref()
            .is_some_and(|value| value != provider.as_str())
    {
        return None;
    }
    Some((provider, parsed.tmux_name))
}
