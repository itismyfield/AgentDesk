//! Host-guard verdict a provider turn carries into its sync tmux teardown sites.

use crate::services::session_host::ClearedHostSession;

/// Whether the turn's tmux session may be torn down, judged before spawn.
#[derive(Debug)]
pub(crate) enum TeardownClearance {
    /// The turn's sessions row is a found legacy row with no host trace.
    Cleared(ClearedHostSession),
    /// The guard kept the session; every teardown effect is skipped.
    Refused(String),
    /// The turn has no session key, so the teardown stays name-only as before.
    Unkeyed,
}

/// Session name a teardown may act on; a clearance for another name, a refusal or a
/// missing clearance is `Err`.
#[cfg(unix)]
fn admitted<'a>(
    clearance: Option<&'a TeardownClearance>,
    tmux_name: &str,
) -> Result<Option<&'a ClearedHostSession>, String> {
    let refusal = match clearance {
        Some(TeardownClearance::Cleared(session)) if session.name() == tmux_name => {
            return Ok(Some(session));
        }
        Some(TeardownClearance::Unkeyed) => return Ok(None),
        Some(TeardownClearance::Cleared(session)) => format!("cleared {}", session.name()),
        Some(TeardownClearance::Refused(reason)) => reason.clone(),
        None => "no teardown clearance".to_string(),
    };
    tracing::warn!(tmux_name, refusal, "host guard kept the tmux session");
    Err(format!(
        "host guard kept tmux session {tmux_name}: {refusal}"
    ))
}

/// Termination audit for `tmux_name` once admitted; `Err` leaves it unrecorded.
#[cfg(unix)]
pub(crate) fn report_tmux_death(
    clearance: Option<&TeardownClearance>,
    tmux_name: &str,
    component: &str,
    code: &str,
    reason: &str,
    last_offset: Option<u64>,
) -> Result<(), String> {
    match admitted(clearance, tmux_name)? {
        Some(session) => crate::services::termination_audit::record_termination_for_cleared(
            session,
            None,
            component,
            code,
            Some(reason),
            last_offset,
        ),
        None => crate::services::termination_audit::record_termination_for_tmux(
            tmux_name,
            None,
            component,
            code,
            Some(reason),
            last_offset,
        ),
    }
    Ok(())
}

/// Audit, exit reason, then kill of `tmux_name`, in that order, once admitted.
#[cfg(unix)]
pub(crate) fn teardown_tmux(
    clearance: Option<&TeardownClearance>,
    tmux_name: &str,
    component: &str,
    code: &str,
    reason: &str,
) -> Result<(), String> {
    report_tmux_death(clearance, tmux_name, component, code, reason, None)?;
    crate::services::tmux_diagnostics::record_tmux_exit_reason(tmux_name, reason);
    crate::services::platform::tmux::kill_session(tmux_name, reason);
    Ok(())
}

#[cfg(test)]
#[path = "provider_teardown_tests.rs"]
pub(crate) mod tests;
