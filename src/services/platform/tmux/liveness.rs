use super::*;

#[cfg(test)]
pub(crate) mod tests;

pub(super) fn pane_liveness_using(
    session_name: &str,
    mut prepare: impl FnMut() -> Option<Command>,
    mut run: impl FnMut(Command, Duration, &str) -> Result<Output, String>,
) -> PaneLiveness {
    probe_pane_liveness(session_name, |args| {
        let mut command = prepare().ok_or("tmux is not on the runtime PATH")?;
        command.args(args);
        run(
            command,
            PANE_LIVENESS_PROBE_TIMEOUT,
            &format!("tmux {}", args[0]),
        )
    })
}

pub(super) fn prepared_tmux_command() -> Option<Command> {
    let path = binary_resolver::prepared_runtime_path()?;
    let mut command = binary_resolver::command_with_path("tmux".as_ref(), Some(path)).ok()?;
    command.arg("-u");
    Some(command)
}

pub(super) fn pane_liveness_within_using(
    session_name: &str,
    budget: Duration,
    mut prepare: impl FnMut() -> Option<Command>,
    mut run: impl FnMut(Command, Duration, &str) -> Result<Output, String>,
) -> PaneLiveness {
    let deadline = Instant::now() + budget.min(PANE_LIVENESS_PROBE_TIMEOUT);
    if is_blank_session_name(session_name) {
        return PaneLiveness::ProbeError;
    }
    probe_pane_liveness(session_name, |args| {
        if Instant::now() >= deadline {
            return Err("pane liveness budget exhausted".into());
        }
        let mut command = prepare().ok_or("tmux is not on the ready runtime PATH")?;
        command.args(args);
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("pane liveness budget exhausted".into());
        }
        run(command, remaining, &format!("tmux {}", args[0]))
    })
}

fn probe_pane_liveness(
    session_name: &str,
    mut run: impl FnMut(&[&str]) -> Result<Output, String>,
) -> PaneLiveness {
    match run(&["has-session", "-t", &exact_target(session_name)]) {
        // Spawn/exec failure ⇒ we never reached tmux: unknown, not dead.
        Err(_) => return PaneLiveness::ProbeError,
        Ok(output) => match classify_has_session_output(&output) {
            SessionPresence::Present => {}
            SessionPresence::Missing => return PaneLiveness::DeadOrAbsent,
            SessionPresence::ProbeFailed => return PaneLiveness::ProbeError,
        },
    }
    match run(&[
        "list-panes",
        "-t",
        &exact_target(session_name),
        "-F",
        "#{pane_dead}",
    ]) {
        // list-panes failed on a session we just confirmed present ⇒ unknown.
        Err(_) => PaneLiveness::ProbeError,
        Ok(output) if !output.status.success() => PaneLiveness::ProbeError,
        Ok(output) => {
            if String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line.trim() == "0")
            {
                PaneLiveness::Live
            } else {
                // Present but every pane is dead ⇒ the process exited.
                PaneLiveness::DeadOrAbsent
            }
        }
    }
}
