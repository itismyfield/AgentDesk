use std::path::PathBuf;

use super::model::{
    HostCapabilities, HostError, HostKind, HostLiveness, HostMutation, HostPresence, HostSessionRef,
};

/// Observation and non-destructive input on one hosted session. Synchronous:
/// callers keep their own `spawn_blocking`, timeouts and fallback values.
pub trait InteractiveSessionHost: Send + Sync {
    fn submit(
        &self,
        target: super::submit::AuthorizedHostTarget<'_>,
        payload: super::submit::SubmitPayload<'_>,
    ) -> super::submit::SubmitOutcome {
        super::submit::submit(self.kind(), target, payload)
    }
    /// Starts a launch its owner prepared and admitted; the reply is the create's, never a bind.
    fn create(&self, request: super::create::HostCreate<'_>) -> super::create::CreateOutcome {
        super::create::create(self.kind(), request)
    }
    fn kind(&self) -> HostKind;
    fn capabilities(&self) -> HostCapabilities;
    fn idle_cleanup_support(&self) -> Result<(), &'static str> {
        super::idle_cleanup::support(self.kind(), self.capabilities())
    }
    /// Closes only the proved idle execution; a new input after proof must veto the close.
    /// Repeating a close for the same execution must not close again.
    fn cleanup_idle(
        &self,
        request: super::IdleCleanupRequest<'_>,
    ) -> Result<bool, super::IdleCleanupPreserved> {
        self.idle_cleanup_support()
            .map_err(|reason| (reason, None))?;
        super::idle_cleanup::cleanup(self.kind(), request)
    }
    fn presence(&self, session: HostSessionRef<'_>) -> HostPresence;
    fn liveness(&self, session: HostSessionRef<'_>) -> HostLiveness;
    fn send_text(&self, session: HostSessionRef<'_>, text: &str)
    -> Result<HostMutation, HostError>;
    fn send_keys(
        &self,
        session: HostSessionRef<'_>,
        keys: &[&str],
    ) -> Result<HostMutation, HostError>;
    fn interrupt(&self, session: HostSessionRef<'_>) -> Result<HostMutation, HostError>;
    /// Consumes the existing owner's warrant; close replies remain distinct from proven exit.
    fn terminate(
        &self,
        warrant: crate::services::termination_audit::host_terminate::HostTerminateWarrant<'_>,
    ) -> crate::services::termination_audit::host_terminate::HostTerminateEffect {
        super::termination::terminate(self.kind(), warrant)
    }
    /// Reads a durable execution on its stored endpoint again; nothing is created, sent or closed.
    #[cfg(unix)]
    fn recover(&self, request: super::recover::HostRecover<'_>) -> super::recover::HostRecovery {
        super::recover::recover(self.kind(), request)
    }
    /// `scroll_back` follows `platform::tmux::capture_pane` (negative = history).
    fn capture_screen(
        &self,
        session: HostSessionRef<'_>,
        scroll_back: i32,
    ) -> Result<String, HostError>;
    fn current_working_dir(
        &self,
        session: HostSessionRef<'_>,
    ) -> Result<Option<PathBuf>, HostError>;
    fn execution_pid(&self, session: HostSessionRef<'_>) -> Result<Option<u32>, HostError>;
}
