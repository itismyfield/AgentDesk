//! Keep warm process sessions and resume tokens inside their selected account.

use super::{SessionHandle, remove_process_session, terminate_process_handle};
use crate::services::tmux_common::{
    tmux_session_auth_profile_matches, write_tmux_session_auth_profile,
};

/// Fence a wrapper from a different account before checking warm-session reuse.
/// The return value also governs whether its previous resume token is safe.
pub(crate) fn prepare(session_name: &str, profile_id: &str) -> bool {
    let matches = tmux_session_auth_profile_matches(session_name, profile_id);
    if !matches && let Some(handle) = remove_process_session(session_name) {
        terminate_process_handle(handle);
    }
    matches
}

/// Publish account identity only after launch succeeds. If recording it fails,
/// stop the new process instead of leaving an untracked account in the registry.
pub(crate) fn record_launch(
    session_name: &str,
    profile_id: &str,
    handle: SessionHandle,
) -> Result<SessionHandle, String> {
    if let Err(error) = write_tmux_session_auth_profile(session_name, profile_id) {
        terminate_process_handle(handle);
        return Err(error);
    }
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::session_backend::{insert_process_session, process_session_is_alive};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    #[test]
    fn account_switch_fences_warm_process_and_retains_old_resume_identity_until_launch() {
        let name = format!("auth-switch-{}", uuid::Uuid::new_v4());
        let alive = Arc::new(AtomicBool::new(true));
        let handle = record_launch(
            &name,
            "work",
            SessionHandle::TestProcess {
                pid: 424_290,
                alive: alive.clone(),
            },
        )
        .unwrap();
        insert_process_session(name.clone(), handle);
        assert!(prepare(&name, "work"));
        assert!(process_session_is_alive(&name));

        assert!(!prepare(&name, "default"));
        assert!(!alive.load(Ordering::Relaxed));
        assert!(!process_session_is_alive(&name));
        // A failed replacement must not make the old resume token look safe.
        assert!(!prepare(&name, "default"));
        assert!(tmux_session_auth_profile_matches(&name, "work"));
        let _ = std::fs::remove_file(crate::services::tmux_common::session_temp_path(
            &name,
            "auth_profile",
        ));
    }
}
