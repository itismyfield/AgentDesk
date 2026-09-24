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
        tmux::session_presence(session.name).into()
    }

    // Same probe as the sync `tmux_diagnostics::tmux_session_pane_liveness`.
    fn liveness(&self, session: HostSessionRef<'_>) -> HostLiveness {
        tmux::pane_liveness(session.name).into()
    }

    fn send_text(
        &self,
        session: HostSessionRef<'_>,
        text: &str,
    ) -> Result<HostMutation, HostError> {
        map_output(tmux::send_literal(session.name, text))
    }

    fn send_keys(
        &self,
        session: HostSessionRef<'_>,
        keys: &[&str],
    ) -> Result<HostMutation, HostError> {
        map_output(tmux::send_keys(session.name, keys))
    }

    fn interrupt(&self, session: HostSessionRef<'_>) -> Result<HostMutation, HostError> {
        self.send_keys(session, &["C-c"])
    }

    fn capture_screen(
        &self,
        session: HostSessionRef<'_>,
        scroll_back: i32,
    ) -> Result<String, HostError> {
        tmux::capture_pane(session.name, scroll_back)
            .ok_or_else(|| HostError::Transport("tmux capture-pane failed".to_string()))
    }

    fn current_working_dir(
        &self,
        session: HostSessionRef<'_>,
    ) -> Result<Option<PathBuf>, HostError> {
        Ok(tmux::pane_current_path(session.name).map(PathBuf::from))
    }

    fn execution_pid(&self, session: HostSessionRef<'_>) -> Result<Option<u32>, HostError> {
        Ok(tmux::pane_pid(session.name))
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
}
