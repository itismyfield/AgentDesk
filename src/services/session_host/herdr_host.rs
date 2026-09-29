#![cfg_attr(not(test), allow(dead_code))]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use super::herdr::contract::{self, HerdrTransport};
use super::herdr::model::{
    ControlPlane, ENDPOINT_MISSING, HerdrCall, HerdrEndpoint, HerdrObservation, HerdrRequest,
};
use super::herdr::observe;
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
        }
    }

    /// The call, its outcome and the generation of the connection that answered.
    fn call(&self, request: HerdrRequest) -> (HerdrCall, contract::HerdrOutcome, u64) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let call = HerdrCall {
            id: format!("adk-{id}"),
            request,
        };
        let (outcome, generation) = self.transport.call(&call);
        (call, outcome, generation)
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
        let text = text.to_string();
        let request = |pane_id| HerdrRequest::PaneSendText { pane_id, text };
        self.exchange(session, request, contract::mutation_result)
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
