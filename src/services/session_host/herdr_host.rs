#![cfg_attr(not(test), allow(dead_code))]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use super::herdr::contract::{self, HerdrTransport};
use super::herdr::model::{
    ControlPlane, ENDPOINT_MISSING, HerdrCall, HerdrEndpoint, HerdrObservation, HerdrRequest,
};
use super::herdr::observe::{self, RESTORE_RESUME_NOT_OFF, RestoreResume};
use super::model::{
    HostCapabilities, HostError, HostKind, HostLiveness, HostMutation, HostPresence, HostRefusal,
    HostSessionRef,
};
use super::traits::InteractiveSessionHost;

/// Herdr pane host over an injected transport. It exists only with a
/// validated endpoint; nothing in production constructs one yet.
pub(crate) struct HerdrHost<T: HerdrTransport> {
    endpoint: HerdrEndpoint,
    transport: T,
    next_id: AtomicU64,
    /// E7 reading, taken afresh before every input.
    read_restore: fn(&T) -> RestoreResume,
}

fn pane_id(session: HostSessionRef<'_>) -> Result<&str, HostError> {
    if session.kind != HostKind::Herdr || session.name.trim().is_empty() {
        return Err(HostError::Unsupported(session.kind, "herdr_pane_target"));
    }
    Ok(session.name)
}

fn refused(op: &'static str) -> Result<HostMutation, HostError> {
    Ok(HostMutation::Refused(HostRefusal::Unsupported {
        kind: HostKind::Herdr,
        op,
    }))
}

impl<T: HerdrTransport> HerdrHost<T> {
    pub(crate) fn new(endpoint: HerdrEndpoint, transport: T) -> Self {
        Self {
            endpoint,
            transport,
            next_id: AtomicU64::new(1),
            read_restore: observe::read_restore_resume,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_restore_reader(self, read_restore: fn(&T) -> RestoreResume) -> Self {
        Self {
            read_restore,
            ..self
        }
    }

    fn next_call(&self, request: HerdrRequest) -> HerdrCall {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        HerdrCall {
            id: format!("adk-{id}"),
            request,
        }
    }

    /// The call, its outcome and the generation of the connection that answered.
    fn call(&self, request: HerdrRequest) -> (HerdrCall, contract::HerdrOutcome, u64) {
        let call = self.next_call(request);
        let (outcome, generation) = self.transport.call(&call);
        (call, outcome, generation)
    }

    /// E7 before any input: only a fresh read of resume-on-restore off admits it, and
    /// only on the connection that read it, so a reconnect drops the earlier reading.
    fn restore_off(&self) -> Result<u64, HostMutation> {
        (self.read_restore)(&self.transport)
            .admitted_generation()
            .ok_or_else(|| {
                HostMutation::Refused(HostRefusal::Precondition(
                    RESTORE_RESUME_NOT_OFF.to_string(),
                ))
            })
    }

    fn exchange<R>(
        &self,
        session: HostSessionRef<'_>,
        request: impl FnOnce(String) -> HerdrRequest,
        adapt: fn(&HerdrCall, contract::HerdrOutcome, &str) -> Result<R, HostError>,
    ) -> Result<R, HostError> {
        let pane = pane_id(session)?;
        let (call, outcome, _) = self.call(request(pane.to_string()));
        adapt(&call, outcome, pane)
    }

    /// The snapshot observation and the generation of the connection it came from.
    fn observe_pane(&self, session: HostSessionRef<'_>) -> (HerdrObservation, u64) {
        let Ok(pane) = pane_id(session) else {
            return (HerdrObservation::failed(ControlPlane::Reachable), 0);
        };
        let (call, outcome, generation) = self.call(HerdrRequest::SessionSnapshot {});
        (
            contract::snapshot_observation(&call, outcome, pane),
            generation,
        )
    }

    pub(crate) fn observe(&self, session: HostSessionRef<'_>) -> HerdrObservation {
        let (observation, snapshot_generation) = self.observe_pane(session);
        let (Ok(pane), HostPresence::Present) = (pane_id(session), observation.presence()) else {
            return observation;
        };
        let (call, outcome, process_generation) = self.call(HerdrRequest::PaneProcessInfo {
            pane_id: pane.to_string(),
        });
        let observation = contract::with_process_info(observation, &call, outcome, pane);
        observe::fence_generation(observation, snapshot_generation, process_generation)
    }
}

impl<T: HerdrTransport> InteractiveSessionHost for HerdrHost<T> {
    fn kind(&self) -> HostKind {
        HostKind::Herdr
    }

    // Key grammar is unverified, so keys and interrupt stay refused.
    fn capabilities(&self) -> HostCapabilities {
        HostCapabilities {
            send_text: true,
            capture_screen: true,
            current_working_dir: true,
            execution_pid: true,
            ..HostCapabilities::default()
        }
    }

    fn presence(&self, session: HostSessionRef<'_>) -> HostPresence {
        self.observe_pane(session).0.presence()
    }

    fn liveness(&self, session: HostSessionRef<'_>) -> HostLiveness {
        self.observe(session).liveness()
    }

    fn send_text(
        &self,
        session: HostSessionRef<'_>,
        text: &str,
    ) -> Result<HostMutation, HostError> {
        let pane = pane_id(session)?;
        let generation = match self.restore_off() {
            Ok(generation) => generation,
            Err(refusal) => return Ok(refusal),
        };
        let call = self.next_call(HerdrRequest::PaneSendText {
            pane_id: pane.to_string(),
            text: text.to_string(),
        });
        let (outcome, _) = self.transport.call_on(&call, generation);
        contract::mutation_result(&call, outcome, pane)
    }

    fn send_keys(
        &self,
        _session: HostSessionRef<'_>,
        _keys: &[&str],
    ) -> Result<HostMutation, HostError> {
        self.restore_off().map_or_else(Ok, |_| refused("send_keys"))
    }

    fn interrupt(&self, _session: HostSessionRef<'_>) -> Result<HostMutation, HostError> {
        self.restore_off().map_or_else(Ok, |_| refused("interrupt"))
    }

    fn capture_screen(
        &self,
        session: HostSessionRef<'_>,
        scroll_back: i32,
    ) -> Result<String, HostError> {
        let request = |pane_id: String| contract::capture_request(&pane_id, scroll_back);
        self.exchange(session, request, contract::capture_result)
    }

    fn current_working_dir(
        &self,
        session: HostSessionRef<'_>,
    ) -> Result<Option<PathBuf>, HostError> {
        let request = |pane_id| HerdrRequest::PaneGet { pane_id };
        self.exchange(session, request, contract::working_dir_result)
    }

    fn execution_pid(&self, session: HostSessionRef<'_>) -> Result<Option<u32>, HostError> {
        let request = |pane_id| HerdrRequest::PaneProcessInfo { pane_id };
        self.exchange(session, request, contract::execution_pid_result)
    }
}

/// What `host_for(Herdr)` returns: no endpoint, so every call fails without I/O.
pub(crate) struct UnconfiguredHerdrHost;

fn no_endpoint<R>() -> Result<R, HostError> {
    Err(HostError::Unsupported(HostKind::Herdr, ENDPOINT_MISSING))
}

impl InteractiveSessionHost for UnconfiguredHerdrHost {
    fn kind(&self) -> HostKind {
        HostKind::Herdr
    }

    fn capabilities(&self) -> HostCapabilities {
        HostCapabilities::default()
    }

    fn presence(&self, _session: HostSessionRef<'_>) -> HostPresence {
        HostPresence::ProbeFailed
    }

    fn liveness(&self, _session: HostSessionRef<'_>) -> HostLiveness {
        HostLiveness::ProbeError
    }

    fn send_text(
        &self,
        _session: HostSessionRef<'_>,
        _text: &str,
    ) -> Result<HostMutation, HostError> {
        no_endpoint()
    }

    fn send_keys(
        &self,
        _session: HostSessionRef<'_>,
        _keys: &[&str],
    ) -> Result<HostMutation, HostError> {
        no_endpoint()
    }

    fn interrupt(&self, _session: HostSessionRef<'_>) -> Result<HostMutation, HostError> {
        no_endpoint()
    }

    fn capture_screen(
        &self,
        _session: HostSessionRef<'_>,
        _scroll_back: i32,
    ) -> Result<String, HostError> {
        no_endpoint()
    }

    fn current_working_dir(
        &self,
        _session: HostSessionRef<'_>,
    ) -> Result<Option<PathBuf>, HostError> {
        no_endpoint()
    }

    fn execution_pid(&self, _session: HostSessionRef<'_>) -> Result<Option<u32>, HostError> {
        no_endpoint()
    }
}

#[cfg(test)]
#[path = "herdr_host_tests.rs"]
mod tests;

#[cfg(test)]
#[cfg(windows)]
mod windows_tests {
    use super::*;

    #[test]
    fn an_unconfigured_herdr_host_refuses_every_operation() {
        let host = UnconfiguredHerdrHost;
        let pane = HostSessionRef::herdr_pane("w1-1");
        let missing = || HostError::Unsupported(HostKind::Herdr, ENDPOINT_MISSING);
        assert_eq!(host.capabilities(), HostCapabilities::default());
        assert_eq!(host.presence(pane), HostPresence::ProbeFailed);
        assert_eq!(host.liveness(pane), HostLiveness::ProbeError);
        assert_eq!(host.send_text(pane, "x"), Err(missing()));
        assert_eq!(host.send_keys(pane, &["C-c"]), Err(missing()));
        assert_eq!(host.interrupt(pane), Err(missing()));
        assert_eq!(host.capture_screen(pane, 0), Err(missing()));
        assert_eq!(host.current_working_dir(pane), Err(missing()));
        assert_eq!(host.execution_pid(pane), Err(missing()));
    }
}
