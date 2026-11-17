//! Trust repair has only bounded observations and a single menu Enter.
use crate::services::session_host::{HostKey, SubmitPayload, TmuxHost};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

pub(crate) struct Pane {
    pub(crate) cwd: PathBuf,
    pub(crate) text: String,
}
pub(crate) trait TrustTransport {
    fn observe(&mut self, session: &str, budget: Duration) -> Option<Pane>;
    fn enter(&mut self, session: &str, budget: Duration) -> bool;
}
pub(crate) struct TrustHost;
impl TrustTransport for TrustHost {
    fn observe(&mut self, session: &str, budget: Duration) -> Option<Pane> {
        let target = format!("={session}:");
        let output = TmuxHost
            .run_actor(
                Path::new("tmux"),
                session,
                budget / 2,
                &[
                    "display-message",
                    "-p",
                    "-t",
                    &target,
                    "#{pane_dead}\n#{pane_current_path}",
                ],
            )
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let metadata = String::from_utf8(output.stdout).ok()?;
        let (alive, cwd) = metadata.trim_end().split_once('\n')?;
        if alive != "0" || cwd.is_empty() {
            return None;
        }
        let text = TmuxHost.capture_bounded(session, -80, budget / 2)?;
        Some(Pane {
            cwd: cwd.into(),
            text,
        })
    }
    fn enter(&mut self, session: &str, budget: Duration) -> bool {
        TmuxHost
            .actor_input(
                Path::new("tmux"),
                session,
                budget / 2,
                SubmitPayload::Keys(&[HostKey::Enter]),
            )
            .into_output()
            .is_ok_and(|output| output.status.success())
    }
}
