use std::path::PathBuf;
use std::process::Output;

use super::model::{
    HostCapabilities, HostError, HostKind, HostLiveness, HostMutation, HostPresence, HostSessionRef,
};
use super::traits::InteractiveSessionHost;
use crate::services::platform::tmux;

/// Thin adapter over `platform::tmux`, which stays the only tmux binary caller
/// and owns its per-platform behaviour.
pub(crate) struct TmuxHost;

fn map_output(result: Result<Output, String>) -> Result<HostMutation, HostError> {
    match result {
        Ok(output) if output.status.success() => Ok(HostMutation::Confirmed),
        Ok(output) => Err(HostError::Transport(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        )),
        Err(error) => Err(HostError::Transport(error)),
    }
}

impl TmuxHost {
    pub(crate) fn liveness_within(
        &self,
        session: HostSessionRef<'_>,
        budget: std::time::Duration,
    ) -> HostLiveness {
        let probe = |name| tmux::pane_liveness_within(name, budget).into();
        session
            .legacy_name()
            .map_or(HostLiveness::ProbeError, probe)
    }
}

impl InteractiveSessionHost for TmuxHost {
    fn kind(&self) -> HostKind {
        HostKind::Tmux
    }

    fn capabilities(&self) -> HostCapabilities {
        HostCapabilities {
            send_text: true,
            send_keys: true,
            interrupt: true,
            capture_screen: true,
            current_working_dir: true,
            execution_pid: true,
        }
    }

    fn presence(&self, session: HostSessionRef<'_>) -> HostPresence {
        let probe = |name| tmux::session_presence(name).into();
        session
            .legacy_name()
            .map_or(HostPresence::ProbeFailed, probe)
    }

    // Same probe as the sync `tmux_diagnostics::tmux_session_pane_liveness`.
    fn liveness(&self, session: HostSessionRef<'_>) -> HostLiveness {
        let probe = |name| tmux::pane_liveness(name).into();
        session
            .legacy_name()
            .map_or(HostLiveness::ProbeError, probe)
    }

    fn send_text(
        &self,
        session: HostSessionRef<'_>,
        text: &str,
    ) -> Result<HostMutation, HostError> {
        map_output(tmux::send_literal(session.legacy_name()?, text))
    }

    fn send_keys(
        &self,
        session: HostSessionRef<'_>,
        keys: &[&str],
    ) -> Result<HostMutation, HostError> {
        map_output(tmux::send_keys(session.legacy_name()?, keys))
    }

    fn interrupt(&self, session: HostSessionRef<'_>) -> Result<HostMutation, HostError> {
        self.send_keys(session, &["C-c"])
    }

    fn capture_screen(
        &self,
        session: HostSessionRef<'_>,
        scroll_back: i32,
    ) -> Result<String, HostError> {
        tmux::capture_pane(session.legacy_name()?, scroll_back)
            .ok_or_else(|| HostError::Transport("tmux capture-pane failed".to_string()))
    }

    fn current_working_dir(
        &self,
        session: HostSessionRef<'_>,
    ) -> Result<Option<PathBuf>, HostError> {
        Ok(tmux::pane_current_path(session.legacy_name()?).map(PathBuf::from))
    }

    fn execution_pid(&self, session: HostSessionRef<'_>) -> Result<Option<u32>, HostError> {
        Ok(tmux::pane_pid(session.legacy_name()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_are_the_platform_probes() {
        // A blank name short-circuits both platform probes without spawning tmux.
        let blank = HostSessionRef::tmux("");
        assert_eq!(TmuxHost.presence(blank), tmux::session_presence("").into());
        assert_eq!(TmuxHost.presence(blank), HostPresence::ProbeFailed);
        assert_eq!(TmuxHost.liveness(blank), tmux::pane_liveness("").into());
        assert_eq!(TmuxHost.liveness(blank), HostLiveness::DeadOrAbsent);
        assert_eq!(TmuxHost.kind(), HostKind::Tmux);
        assert!(TmuxHost.capabilities().interrupt);
    }

    #[cfg(unix)]
    #[test]
    fn mutation_outcome_follows_the_tmux_exit_status() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::ExitStatus;
        let output = |code: i32, stderr: &str| Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        };
        assert_eq!(map_output(Ok(output(0, ""))), Ok(HostMutation::Confirmed));
        assert_eq!(
            map_output(Ok(output(1, "can't find pane\n"))),
            Err(HostError::Transport("can't find pane".to_string()))
        );
        assert_eq!(
            map_output(Err("spawn failed".to_string())),
            Err(HostError::Transport("spawn failed".to_string()))
        );
    }

    #[test]
    fn herdr_ref_never_reaches_a_tmux_probe() {
        // A real probe of an absent session would read DeadOrAbsent.
        let herdr = HostSessionRef::herdr_pane("session-host-herdr-no-such-tmux");
        assert_eq!(
            TmuxHost.liveness(herdr),
            HostLiveness::ProbeError,
            "a same-named Herdr pane must never reach a tmux probe"
        );
        assert_eq!(TmuxHost.presence(herdr), HostPresence::ProbeFailed);
        let refused = Err(HostError::Unsupported(HostKind::Herdr, "legacy_name"));
        assert_eq!(TmuxHost.send_text(herdr, "x"), refused);
        assert_eq!(
            TmuxHost.execution_pid(herdr),
            Err(HostError::Unsupported(HostKind::Herdr, "legacy_name"))
        );
    }
}
