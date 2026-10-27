//! The tmux queries health may spawn: a closed command set, built from the prepared PATH only,
//! with `-N` on session queries so an observation never starts a tmux server.

use super::{
    SessionPresence, classify_has_session_output, exact_target, is_blank_session_name, liveness,
};
use std::process::{Command, Output};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HealthTmuxQuery {
    HasSession,
    PanePid,
    /// `tmux -V`, which prints the client version and starts no server.
    Version,
}

/// Why no command was built; no process exists for any of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HealthTmuxRefusal {
    BlankSession,
    RuntimePathUnready,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HealthTmuxAnswer {
    Present,
    Missing,
    PanePid(u32),
    Failed,
}

pub(crate) fn health_tmux_command(
    query: HealthTmuxQuery,
    session: &str,
) -> Result<Command, HealthTmuxRefusal> {
    health_tmux_command_using(query, session, liveness::prepared_tmux_command)
}

/// `prepare` yields the `tmux -u` command; a cold PATH yields `None` and nothing is spawned.
pub(crate) fn health_tmux_command_using(
    query: HealthTmuxQuery,
    session: &str,
    prepare: impl FnOnce() -> Option<Command>,
) -> Result<Command, HealthTmuxRefusal> {
    if query != HealthTmuxQuery::Version && is_blank_session_name(session) {
        return Err(HealthTmuxRefusal::BlankSession);
    }
    if cfg!(not(unix)) && query == HealthTmuxQuery::PanePid {
        return Err(HealthTmuxRefusal::Unsupported);
    }
    let mut command = prepare().ok_or(HealthTmuxRefusal::RuntimePathUnready)?;
    let target = exact_target(session);
    match query {
        HealthTmuxQuery::HasSession => command.args(["-N", "has-session", "-t", &target]),
        HealthTmuxQuery::PanePid => {
            command.args(["-N", "display-message", "-p", "-t", &target, "#{pane_pid}"])
        }
        HealthTmuxQuery::Version => command.arg("-V"),
    };
    Ok(command)
}

/// Only a completed command reaches here; a missing session needs tmux's own missing answer.
pub(crate) fn classify_health_tmux_output(
    query: HealthTmuxQuery,
    output: &Output,
) -> HealthTmuxAnswer {
    match query {
        HealthTmuxQuery::HasSession => match classify_has_session_output(output) {
            SessionPresence::Present => HealthTmuxAnswer::Present,
            SessionPresence::Missing => HealthTmuxAnswer::Missing,
            SessionPresence::ProbeFailed => HealthTmuxAnswer::Failed,
        },
        HealthTmuxQuery::PanePid => std::str::from_utf8(&output.stdout)
            .ok()
            .filter(|_| output.status.success())
            .and_then(|text| text.trim().parse().ok())
            .map_or(HealthTmuxAnswer::Failed, HealthTmuxAnswer::PanePid),
        // Any version line will do: the probe asks whether the client runs, not which one.
        HealthTmuxQuery::Version if output.status.success() && !output.stdout.is_empty() => {
            HealthTmuxAnswer::Present
        }
        HealthTmuxQuery::Version => HealthTmuxAnswer::Failed,
    }
}
