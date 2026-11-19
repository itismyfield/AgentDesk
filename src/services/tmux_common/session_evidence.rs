//! Session-scoped evidence shared by pane cleanup and selector retirement.

use super::{CLAUDE_SESSION_AUTH_TEMP_EXT, TMUX_AUTH_PROFILE_TEMP_EXT};
use std::path::Path;

pub(crate) const MODEL_PROOF_EXT: &str = "routine_model.json";

fn account_record(body: &str) -> Option<(&str, &str)> {
    body.split_once('\n')
        .filter(|(session, profile)| !session.is_empty() && !profile.trim().is_empty())
}

fn paired_profile(path: &Path) -> bool {
    let Ok(profile) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(auth) = std::fs::read_to_string(path.with_extension(CLAUDE_SESSION_AUTH_TEMP_EXT))
    else {
        return false;
    };
    account_record(&auth).is_some_and(|(_, account)| account == profile.trim())
}

/// Pane cleanup leaves evidence that may still be needed by the saved conversation.
pub(crate) fn survives_pane_cleanup(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.strip_suffix(MODEL_PROOF_EXT)
        .is_some_and(|stem| stem.ends_with('.'))
        || matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some(CLAUDE_SESSION_AUTH_TEMP_EXT)
        )
        || (matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some(TMUX_AUTH_PROFILE_TEMP_EXT)
        ) && paired_profile(path))
}

#[cfg(unix)]
pub(crate) fn require_native_account(
    tmux: &str,
    session: &str,
    profile: &str,
) -> Result<(), String> {
    use super::{
        claude_session_auth, resolve_session_temp_path, tmux_session_auth_profile_matches,
    };
    let account = claude_session_auth(tmux, session)?;
    if account.as_deref() == Some(profile)
        || (profile == crate::services::provider_auth_profile::DEFAULT_PROFILE_ID
            && resolve_session_temp_path(tmux, CLAUDE_SESSION_AUTH_TEMP_EXT).is_none()
            && tmux_session_auth_profile_matches(tmux, profile))
    {
        return Ok(());
    }
    Err("NotSent(\"routine_resume_account_changed_or_unproved\")".into())
}

#[cfg(unix)]
pub(crate) fn retire(
    authority: &super::TmuxSourceAuthority<'_>,
    provider: &str,
    session: &str,
) -> Result<(), String> {
    use super::{legacy_tmp_session_path, session_temp_path};
    let tmux = authority.session();
    let other_proof =
        crate::services::tui_input::routine_model::retire(authority, provider, session)?;
    let mut result = Ok(());
    if matches!(provider, "claude" | "codex") {
        for path in [
            session_temp_path(tmux, CLAUDE_SESSION_AUTH_TEMP_EXT),
            legacy_tmp_session_path(tmux, CLAUDE_SESSION_AUTH_TEMP_EXT),
        ] {
            let auth = Path::new(&path);
            let account = match std::fs::read_to_string(auth) {
                Ok(account) => account,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    result = result.and(Err(error.to_string()));
                    continue;
                }
            };
            if account_record(&account).is_some_and(|(id, _)| id == session) {
                let profile = auth.with_extension(TMUX_AUTH_PROFILE_TEMP_EXT);
                if !other_proof && paired_profile(&profile) {
                    let removed = remove(&profile);
                    result = result.and(removed);
                }
                let removed = remove(auth);
                result = result.and(removed);
            }
        }
    }
    result
}

#[cfg(unix)]
pub(crate) fn remove(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}
