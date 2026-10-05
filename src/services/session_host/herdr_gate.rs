//! Herdr input gate for one recorded pane: admission (cancel keys exempt), E7 and the stored
//! execution are checked on one server before every input, which goes only to that server.
#![cfg_attr(not(test), allow(dead_code))]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, UNIX_EPOCH};

use super::herdr::contract::{self, HerdrTransport, ServerWitness};
use super::herdr::model::{HerdrCall, HerdrEndpoint, HerdrRequest};
use super::herdr::observe::{self, RestoreResume, RestoreUnverified};
use super::herdr::pane_probe::{self, EvidenceGap, HostOs, PaneProcesses, ProbeRequest, ProcessOs};
use super::model::{HostError, HostKey, HostMutation, HostPresence, HostRefusal};
use crate::db::dispatched_sessions::hosted_execution::{
    ExpectedExecution, HostedExecution, HostedLocation, HostedState,
};
use crate::services::herdr_admission::{self, StopCause};

/// Why the gate let nothing through; every case is decided before any input is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HerdrGateRefusal {
    AdmissionStopped(StopCause),
    /// The server resumes agents when it restores.
    RestoreOn,
    RestoreUnverified(RestoreUnverified),
    /// The row holds no Pending or Bound execution with a location and launch evidence.
    NoStoredExecution,
    /// A pane reading came from another server than the one E7 named.
    ServerChanged,
    /// The pane did not read as one root shell running one provider.
    PaneUnverified,
    OtherNonce,
    RootReplaced,
    ProviderReplaced,
}

/// What an input does to the pane: a cancel key only stops work, so admission lets it through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mutation {
    Input,
    Cancel,
}

/// Herdr key names a cancel judgment may carry.
const CANCEL_KEYS: &[&str] = &["esc", "ctrl+c"];
const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";

/// Herdr's name for an executor key; Herdr writes `enter` as a carriage return.
fn herdr_key(key: HostKey) -> &'static str {
    match key {
        HostKey::Enter => "enter",
        HostKey::Escape => "esc",
        HostKey::CtrlU => "ctrl+u",
        HostKey::CtrlE => "ctrl+e",
        HostKey::Left => "left",
        HostKey::Right => "right",
        HostKey::Backspace => "backspace",
    }
}

/// One gate decision; only the gate makes one, and one send consumes it.
struct Judged {
    witness: ServerWitness,
    mutation: Mutation,
}

struct PaneGate {
    endpoint: HerdrEndpoint,
    transport: Arc<dyn HerdrTransport>,
    pane: String,
    nonce: String,
    expected: ExpectedExecution,
    read_restore: fn(&dyn HerdrTransport, &HerdrEndpoint) -> RestoreResume,
    os: Arc<dyn ProcessOs>,
    next_id: AtomicU64,
    /// The latest judgment, waiting for the send it admits.
    pinned: Mutex<Option<Judged>>,
}

/// A recorded Herdr pane on this node's registered endpoint; every input to it passes the gate.
/// Its calls block on the socket, so callers stay off the async runtime.
#[derive(Clone)]
pub(crate) struct HerdrTarget(Arc<PaneGate>);

impl std::fmt::Debug for HerdrTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HerdrTarget")
            .field("endpoint", &self.0.endpoint.config_key())
            .field("pane", &self.0.pane)
            .field("nonce", &self.0.nonce)
            .finish()
    }
}

impl PartialEq for HerdrTarget {
    fn eq(&self, other: &Self) -> bool {
        (&self.0.endpoint, &self.0.pane, &self.0.nonce)
            == (&other.0.endpoint, &other.0.pane, &other.0.nonce)
    }
}

impl Eq for HerdrTarget {}

/// E7 over the production OS reads.
fn read_restore_off(transport: &dyn HerdrTransport, endpoint: &HerdrEndpoint) -> RestoreResume {
    observe::read_restore_resume(transport, endpoint)
}

/// The stored location names exactly this endpoint.
fn on_endpoint(location: &HostedLocation, endpoint: &HerdrEndpoint) -> bool {
    location.execution_node == endpoint.execution_node()
        && location.endpoint_config_key == endpoint.config_key()
        && std::path::Path::new(&location.socket_addr) == endpoint.socket_path()
        && location.named_session == endpoint.herdr_session()
}

impl HerdrTarget {
    /// A Pending or Bound execution with launch evidence, located on `endpoint`; no I/O.
    pub(crate) fn new(
        endpoint: HerdrEndpoint,
        transport: Arc<dyn HerdrTransport>,
        stored: &HostedExecution,
    ) -> Option<Self> {
        let live = matches!(stored.state, HostedState::Pending | HostedState::Bound);
        let (Some(location), Some(expected)) = (&stored.location, &stored.expected) else {
            return None;
        };
        if !live
            || expected.binding_nonce != stored.execution_nonce
            || !on_endpoint(location, &endpoint)
        {
            return None;
        }
        Some(Self(Arc::new(PaneGate {
            endpoint,
            transport,
            pane: location.pane_id.clone(),
            nonce: stored.execution_nonce.clone(),
            expected: expected.clone(),
            read_restore: read_restore_off,
            os: Arc::new(HostOs),
            next_id: AtomicU64::new(1),
            pinned: Mutex::new(None),
        })))
    }

    #[cfg(test)]
    pub(crate) fn with_reads(
        self,
        read_restore: fn(&dyn HerdrTransport, &HerdrEndpoint) -> RestoreResume,
        os: Arc<dyn ProcessOs>,
    ) -> Self {
        let gate = Arc::try_unwrap(self.0).unwrap_or_else(|_| panic!("unshared target"));
        Self(Arc::new(PaneGate {
            read_restore,
            os,
            ..gate
        }))
    }

    pub(crate) fn pane_id(&self) -> &str {
        &self.0.pane
    }

    /// Judges the next input and keeps the judgment for the send that follows it.
    pub(crate) fn pin(&self, mutation: Mutation) -> Result<(), HerdrGateRefusal> {
        let judged = self.0.judge(mutation);
        let mut pinned = self.0.pinned.lock().unwrap_or_else(PoisonError::into_inner);
        // A refusal also drops an older judgment, so no later send can use it.
        match judged {
            Ok(judged) => {
                *pinned = Some(judged);
                Ok(())
            }
            Err(refusal) => {
                *pinned = None;
                Err(refusal)
            }
        }
    }

    pub(crate) fn send_text(&self, text: &str) -> Result<HostMutation, HostError> {
        let pane_id = self.0.pane.clone();
        let text = text.to_string();
        self.send_pinned(HerdrRequest::PaneSendText { pane_id, text })
    }

    /// One bracketed paste, as tmux `paste-buffer -p` delivers it; a body holding the end
    /// marker would close the paste early, so it is refused unsent.
    pub(crate) fn send_paste(&self, text: &str) -> Result<HostMutation, HostError> {
        if text.contains(PASTE_END) {
            return Ok(refused("paste_end_marker_in_text"));
        }
        self.send_text(&format!("{PASTE_START}{text}{PASTE_END}"))
    }

    pub(crate) fn send_keys(&self, keys: &[HostKey]) -> Result<HostMutation, HostError> {
        let pane_id = self.0.pane.clone();
        let keys = keys.iter().map(|key| herdr_key(*key).to_string()).collect();
        self.send_pinned(HerdrRequest::PaneSendKeys { pane_id, keys })
    }

    /// Sends `request` to the server the pinned judgment named, consuming it; without one
    /// nothing is written.
    fn send_pinned(&self, request: HerdrRequest) -> Result<HostMutation, HostError> {
        let judged = self
            .0
            .pinned
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        match judged {
            Some(judged) => self.0.send(judged, request),
            None => Ok(refused("herdr_input_without_gate_judgment")),
        }
    }

    /// A read of the pane's screen; `scroll_back` follows the tmux capture convention.
    pub(crate) fn capture(&self, scroll_back: i32) -> Option<String> {
        let call = self
            .0
            .call(contract::capture_request(&self.0.pane, scroll_back));
        let (outcome, _) = self.0.transport.call(&call);
        contract::capture_result(&call, outcome, &self.0.pane).ok()
    }

    /// Whether a complete snapshot of the endpoint shows the pane.
    pub(crate) fn present(&self) -> bool {
        let call = self.0.call(HerdrRequest::SessionSnapshot {});
        let (outcome, _) = self.0.transport.call(&call);
        contract::snapshot_observation(&call, outcome, &self.0.pane).presence()
            == HostPresence::Present
    }

    /// Whether the stored execution still runs in the pane, read on one server.
    pub(crate) fn execution_alive(&self) -> bool {
        self.0
            .transport
            .server_witness()
            .is_ok_and(|witness| self.0.verify_pane(&witness).is_ok())
    }
}

fn refused(why: &str) -> HostMutation {
    HostMutation::Refused(HostRefusal::Precondition(why.to_string()))
}

fn pane_of(request: &HerdrRequest) -> Option<&str> {
    match request {
        HerdrRequest::PaneSendText { pane_id, .. }
        | HerdrRequest::PaneSendKeys { pane_id, .. }
        | HerdrRequest::PaneSendInput { pane_id, .. } => Some(pane_id),
        _ => None,
    }
}

fn cancel_only(request: &HerdrRequest) -> bool {
    matches!(request, HerdrRequest::PaneSendKeys { keys, .. }
        if !keys.is_empty() && keys.iter().all(|key| CANCEL_KEYS.contains(&key.as_str())))
}

impl PaneGate {
    fn call(&self, request: HerdrRequest) -> HerdrCall {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        HerdrCall {
            id: format!("adk-input-{id}"),
            request,
        }
    }

    /// Admission first (no I/O), then E7, then the pane on the server E7 named.
    fn judge(&self, mutation: Mutation) -> Result<Judged, HerdrGateRefusal> {
        if mutation == Mutation::Input {
            herdr_admission::check().map_err(HerdrGateRefusal::AdmissionStopped)?;
        }
        let witness = match (self.read_restore)(self.transport.as_ref(), &self.endpoint) {
            RestoreResume::Off { witness } => witness,
            RestoreResume::On => return Err(HerdrGateRefusal::RestoreOn),
            RestoreResume::Unverified(why) => return Err(HerdrGateRefusal::RestoreUnverified(why)),
        };
        self.verify_pane(&witness)?;
        Ok(Judged { witness, mutation })
    }

    fn read_processes(&self) -> PaneProcesses {
        let call = self.call(HerdrRequest::PaneProcessInfo {
            pane_id: self.pane.clone(),
        });
        let (outcome, witness) = self.transport.call(&call);
        let read = contract::foreground_result(&call, outcome, &self.pane);
        (read.map_err(|error| format!("{error:?}")), witness)
    }

    /// The launch's provenance rule, read once more on `witness`'s server, must name the
    /// recorded root shell and provider processes.
    fn verify_pane(&self, witness: &ServerWitness) -> Result<(), HerdrGateRefusal> {
        let request = ProbeRequest {
            provider: &self.expected.binding_provider,
            nonce: &self.nonce,
            launched_at: UNIX_EPOCH,
            witness,
            window: Duration::ZERO,
        };
        let now = pane_probe::probe(&|| self.read_processes(), self.os.as_ref(), &request)
            .map_err(|gap| {
                tracing::debug!(pane = %self.pane, ?gap, "herdr input gate: pane unverified");
                match gap {
                    EvidenceGap::ServerChanged => HerdrGateRefusal::ServerChanged,
                    EvidenceGap::NonceMissing | EvidenceGap::OtherNonce(_) => {
                        HerdrGateRefusal::OtherNonce
                    }
                    _ => HerdrGateRefusal::PaneUnverified,
                }
            })?;
        if now.root != self.expected.root {
            return Err(HerdrGateRefusal::RootReplaced);
        }
        if now.provider_process != self.expected.provider_process {
            return Err(HerdrGateRefusal::ProviderReplaced);
        }
        Ok(())
    }

    /// Writes `request` only on a connection to the judged server.
    fn send(&self, judged: Judged, request: HerdrRequest) -> Result<HostMutation, HostError> {
        if pane_of(&request) != Some(self.pane.as_str()) {
            return Ok(refused("herdr_input_for_another_pane"));
        }
        if judged.mutation == Mutation::Cancel && !cancel_only(&request) {
            return Ok(refused("herdr_cancel_judgment_for_input"));
        }
        let call = self.call(request);
        let outcome = self.transport.call_with_witness(&call, &judged.witness);
        contract::mutation_result(&call, outcome, &self.pane)
    }
}

#[cfg(test)]
#[path = "herdr_gate_tests.rs"]
mod tests;
