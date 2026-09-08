//! Session markers.

use super::*;

/// Return whether a provider wrapper was created for the requested auth
/// profile.  A legacy wrapper without a marker is safe to reuse only for the
/// implicit default profile; named profiles must start a fresh provider
/// session rather than silently continue under another account.
pub(crate) fn tmux_session_auth_profile_matches(
    session_name: &str,
    requested_profile_id: &str,
) -> bool {
    let marker = resolve_session_temp_path(session_name, TMUX_AUTH_PROFILE_TEMP_EXT)
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|value| value.trim().to_string());
    match marker.as_deref() {
        Some(profile_id) => profile_id == requested_profile_id,
        None => requested_profile_id == crate::services::provider_auth_profile::DEFAULT_PROFILE_ID,
    }
}

/// Persist the selected profile beside a newly-created provider wrapper so warm
/// follow-ups are never routed across account boundaries.
pub(crate) fn write_tmux_session_auth_profile(
    session_name: &str,
    profile_id: &str,
) -> Result<(), String> {
    std::fs::write(
        session_temp_path(session_name, TMUX_AUTH_PROFILE_TEMP_EXT),
        profile_id.trim(),
    )
    .map_err(|error| format!("write tmux auth profile marker: {error}"))
}

pub(crate) fn write_tmux_runtime_kind_marker(
    tmux_session_name: &str,
    runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind,
) -> Result<(), String> {
    let path = session_temp_path(tmux_session_name, TMUX_RUNTIME_KIND_TEMP_EXT);
    std::fs::write(&path, runtime_kind.as_str())
        .map_err(|e| format!("Failed to write tmux runtime kind marker: {}", e))
}

pub(crate) fn resolve_tmux_runtime_kind_marker(
    tmux_session_name: &str,
) -> Option<crate::services::agent_protocol::RuntimeHandoffKind> {
    let path = resolve_session_temp_path(tmux_session_name, TMUX_RUNTIME_KIND_TEMP_EXT)?;
    let raw = std::fs::read_to_string(path).ok()?;
    crate::services::agent_protocol::RuntimeHandoffKind::from_str(&raw)
}
