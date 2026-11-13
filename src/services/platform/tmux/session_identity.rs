//! The final ownership check before tmux input or termination.

use super::*;
use crate::domain::session_identity::managed::{IDENTITY_ENV, ManagedIdentity};
use std::path::Path;

pub(crate) fn require_managed(session: &str) -> Result<(), String> {
    require_managed_with_program(session, Path::new("tmux"), Duration::from_secs(3))
}

pub(crate) fn require_managed_with_program(
    session: &str,
    program: &Path,
    budget: Duration,
) -> Result<(), String> {
    validate_target(session).map_err(|e| e.to_string())?;
    if session.starts_with("AgentDesk-") {
        return Ok(());
    }
    let refused = || format!("NotSent(\"unverified tmux session owner: {session}\")");
    if session.trim().is_empty() {
        return Err(refused());
    }
    let mut command = binary_resolver::runtime_command(program).map_err(|_| refused())?;
    crate::config::test_instance::configure_tmux_command(&mut command).map_err(|_| refused())?;
    command.args([
        "-u",
        "show-environment",
        "-t",
        &format!("={session}"),
        IDENTITY_ENV,
    ]);
    let output =
        wait_for_tmux_output(command, budget, "tmux session identity").map_err(|_| refused())?;
    let identity = output
        .status
        .success()
        .then(|| std::str::from_utf8(&output.stdout).ok())
        .flatten()
        .and_then(ManagedIdentity::decode_environment)
        .ok_or_else(refused)?;
    let scope = crate::services::session_identity_metadata::current_owner_scope()?;
    if identity.owner_scope != scope {
        return Err(refused());
    }
    Ok(())
}
