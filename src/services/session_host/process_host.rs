use std::path::PathBuf;

use super::model::{
    HostCapabilities, HostError, HostKind, HostLiveness, HostMutation, HostPresence, HostRefusal,
    HostSessionRef,
};
use super::traits::InteractiveSessionHost;
use crate::services::session_backend;

/// Observation and text input over the in-process session registry. Cancel,
/// stop and teardown keep their provider/child-pid aware owners.
pub(crate) struct ProcessHost;

fn refused(op: &'static str) -> Result<HostMutation, HostError> {
    Ok(HostMutation::Refused(HostRefusal::Unsupported {
        kind: HostKind::Process,
        op,
    }))
}

impl InteractiveSessionHost for ProcessHost {
    fn kind(&self) -> HostKind {
        HostKind::Process
    }

    fn capabilities(&self) -> HostCapabilities {
        HostCapabilities {
            send_text: true,
            execution_pid: true,
            ..HostCapabilities::default()
        }
    }

    // The registry is local memory, so there is no probe-failure state.
    fn presence(&self, session: HostSessionRef<'_>) -> HostPresence {
        if session_backend::process_session_pid(session.name).is_some() {
            HostPresence::Present
        } else {
            HostPresence::Missing
        }
    }

    fn liveness(&self, session: HostSessionRef<'_>) -> HostLiveness {
        if session_backend::process_session_is_alive(session.name) {
            HostLiveness::Live
        } else {
            HostLiveness::DeadOrAbsent
        }
    }

    fn send_text(
        &self,
        session: HostSessionRef<'_>,
        text: &str,
    ) -> Result<HostMutation, HostError> {
        session_backend::send_process_session_input(session.name, text, None)
            .map(|()| HostMutation::Confirmed)
            .map_err(HostError::Transport)
    }

    fn send_keys(
        &self,
        _session: HostSessionRef<'_>,
        _keys: &[&str],
    ) -> Result<HostMutation, HostError> {
        refused("send_keys")
    }

    fn interrupt(&self, _session: HostSessionRef<'_>) -> Result<HostMutation, HostError> {
        refused("interrupt")
    }

    fn capture_screen(
        &self,
        _session: HostSessionRef<'_>,
        _scroll_back: i32,
    ) -> Result<String, HostError> {
        Err(HostError::Unsupported(HostKind::Process, "capture_screen"))
    }

    fn current_working_dir(
        &self,
        _session: HostSessionRef<'_>,
    ) -> Result<Option<PathBuf>, HostError> {
        Ok(None)
    }

    fn execution_pid(&self, session: HostSessionRef<'_>) -> Result<Option<u32>, HostError> {
        Ok(session_backend::process_session_pid(session.name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNREGISTERED: &str = "session-host-process-host-test-unregistered";

    #[test]
    fn capabilities_cover_observation_and_text_input_only() {
        let caps = ProcessHost.capabilities();
        assert!(caps.send_text && caps.execution_pid);
        assert!(!caps.send_keys && !caps.interrupt && !caps.capture_screen);
        assert!(!caps.current_working_dir);
        assert_eq!(ProcessHost.kind(), HostKind::Process);
    }

    #[test]
    fn unregistered_session_reads_as_missing_and_dead() {
        let session = HostSessionRef::process(UNREGISTERED);
        assert_eq!(ProcessHost.presence(session), HostPresence::Missing);
        assert_eq!(ProcessHost.liveness(session), HostLiveness::DeadOrAbsent);
        assert_eq!(ProcessHost.execution_pid(session), Ok(None));
        assert_eq!(ProcessHost.current_working_dir(session), Ok(None));
    }

    #[test]
    fn send_text_surfaces_the_registry_error_as_transport() {
        let session = HostSessionRef::process(UNREGISTERED);
        let expected = session_backend::send_process_session_input(UNREGISTERED, "x", None)
            .expect_err("unregistered session must not accept input");
        assert_eq!(
            ProcessHost.send_text(session, "x"),
            Err(HostError::Transport(expected))
        );
    }

    #[test]
    fn destructive_and_screen_operations_are_refused() {
        let session = HostSessionRef::process(UNREGISTERED);
        let unsupported = |op| {
            Ok(HostMutation::Refused(HostRefusal::Unsupported {
                kind: HostKind::Process,
                op,
            }))
        };
        assert_eq!(ProcessHost.interrupt(session), unsupported("interrupt"));
        assert_eq!(
            ProcessHost.send_keys(session, &["C-c"]),
            unsupported("send_keys")
        );
        assert_eq!(
            ProcessHost.capture_screen(session, -50),
            Err(HostError::Unsupported(HostKind::Process, "capture_screen"))
        );
    }
}
