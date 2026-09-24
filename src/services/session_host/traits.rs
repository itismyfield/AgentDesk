use std::path::PathBuf;

use super::model::{
    HostCapabilities, HostError, HostKind, HostLiveness, HostMutation, HostPresence, HostSessionRef,
};

/// Observation and non-destructive input on one hosted session. Synchronous:
/// callers keep their own `spawn_blocking`, timeouts and fallback values.
pub(crate) trait InteractiveSessionHost: Send + Sync {
    fn kind(&self) -> HostKind;
    fn capabilities(&self) -> HostCapabilities;
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
