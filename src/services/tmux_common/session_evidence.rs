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

pub(crate) fn retire_account(
    authority: &super::TmuxSourceAuthority<'_>,
    session: &str,
    other_proof: bool,
) -> Result<(), String> {
    use super::{legacy_tmp_session_path, session_temp_path};
    let tmux = authority.session();
    let mut result = Ok(());
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
            if !other_proof
                && paired_profile(&profile)
                && let Err(error) = authority.remove_session_file(&profile)
            {
                result = result.and(Err(error));
                continue;
            }
            let removed = authority.remove_session_file(auth);
            result = result.and(removed);
        }
    }
    result
}
