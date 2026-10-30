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

/// Persist the account a Claude session launches under beside its id, so a launch that never ran
/// leaves a record of its own session only and vouches for no other.
pub(crate) fn write_claude_session_auth(
    session_name: &str,
    session_id: &str,
    profile_id: &str,
) -> Result<(), String> {
    std::fs::write(
        session_temp_path(session_name, CLAUDE_SESSION_AUTH_TEMP_EXT),
        format!("{session_id}\n{}", profile_id.trim()),
    )
    .map_err(|error| format!("write claude session auth marker: {error}"))
}

/// Whether `session_id` ran under `profile_id`: by its own record, or by the profile marker while
/// no launch recorded one. A record of another session vouches for nothing.
#[cfg(unix)]
pub(crate) fn claude_session_auth_matches(
    session_name: &str,
    session_id: &str,
    profile_id: &str,
) -> Result<bool, String> {
    if resolve_session_temp_path(session_name, CLAUDE_SESSION_AUTH_TEMP_EXT).is_none() {
        return Ok(tmux_session_auth_profile_matches(session_name, profile_id));
    }
    Ok(claude_session_auth(session_name, session_id)?.as_deref() == Some(profile_id.trim()))
}

/// Carries the account recorded for `from` over to `to` once the same execution verifiably moved on
/// from `from` to `to`; a record of any other session stays as it is and vouches for neither.
pub(crate) fn advance_claude_session_auth(
    session_name: &str,
    from: &str,
    to: &str,
) -> Result<(), String> {
    match claude_session_auth(session_name, from)? {
        Some(profile) => write_claude_session_auth(session_name, to, &profile),
        None => Ok(()),
    }
}

/// The account `session_id` ran under, recorded by its launch or by the session it moved on from;
/// `None` when the record names another session or none exists.
pub(crate) fn claude_session_auth(
    session_name: &str,
    session_id: &str,
) -> Result<Option<String>, String> {
    let Some(path) = resolve_session_temp_path(session_name, CLAUDE_SESSION_AUTH_TEMP_EXT) else {
        return Ok(None);
    };
    let body = std::fs::read_to_string(&path)
        .map_err(|error| format!("read claude session auth marker: {error}"))?;
    let own = body.split_once('\n').filter(|(id, _)| *id == session_id);
    Ok(own.map(|(_, profile)| profile.to_owned()))
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
