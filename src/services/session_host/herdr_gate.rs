//! Herdr input gate for one recorded pane: admission (cancel keys exempt), E7 and the stored
//! execution are checked on one server before every input, which goes only to that server.
#![cfg_attr(not(test), allow(dead_code))]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, UNIX_EPOCH};

use super::herdr::contract::{
    self, CloseEffect, ForegroundProcesses, HerdrTransport, ServerWitness,
};
use super::herdr::model::{HerdrCall, HerdrEndpoint, HerdrRequest};
use super::herdr::observe::{self, RestoreResume, RestoreUnverified};
use super::herdr::pane_probe::{self, EvidenceGap, HostOs, PaneProcesses, ProbeRequest, ProcessOs};
use super::model::{HostError, HostKey, HostKind, HostMutation, HostPresence, HostRefusal};
use crate::db::dispatched_sessions::hosted_execution::{
    ExpectedExecution, HostedExecution, HostedLocation, HostedState, ProcessStamp,
};
use crate::services::herdr_admission::{self, StopCause};
use crate::services::tmux_common::host_marker::{HostKindMarker, read_host_kind_marker};

/// Why the gate let nothing through; every case is decided before any input is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HerdrGateRefusal {
    AdmissionStopped(StopCause),
    /// The server resumes agents when it restores.
    RestoreOn,
    RestoreUnverified(RestoreUnverified),
    /// The row holds no Pending or Bound execution with a location and launch evidence.
    NoStoredExecution,
    /// The logical key's `.host_kind` marker names another host.
    MarkerOtherHost,
    /// The marker is absent, unrecognized or unreadable.
    MarkerUnverified,
    /// A pane reading came from another server than the one E7 named.
    ServerChanged,
    /// The pane did not read as one root shell running one provider.
    PaneUnverified,
    OtherNonce,
    RootReplaced,
    ProviderReplaced,
    TerminateUnsupportedServer,
}

/// What one server reports of the recorded pane: a reading for a reconnect or a retire, never an
/// input judgment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PaneReading {
    /// A complete snapshot of the server has no such pane.
    Missing,
    Unreadable(String),
    Present {
        root: ProcessStamp,
        provider: PaneProvider,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PaneProvider {
    /// The root shell is the only foreground process.
    Exited,
    /// The one foreground child of the root shell, its environment naming this execution.
    Execution(ProcessStamp),
    Unverified(String),
}

/// What an input does to the pane: a cancel key only stops work, so admission lets it through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mutation {
    Input,
    Cancel,
    Terminate,
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
    logical_key: String,
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
            logical_key: stored.owner.logical_key.clone(),
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

    /// Pins only a termination judgment; ordinary stop verdicts grant no close authority.
    pub(crate) fn pin_terminate(&self) -> Result<(), HerdrGateRefusal> {
        self.pin(Mutation::Terminate)
    }

    /// Consumes a termination judgment, reading the live switch immediately before the write.
    pub(crate) fn send_close_pinned(&self) -> CloseEffect {
        let judged = self
            .0
            .pinned
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(judged) = judged else {
            return CloseEffect::NotSent("herdr_close_without_gate_judgment".into());
        };
        let request = HerdrRequest::PaneClose {
            pane_id: self.0.pane.clone(),
        };
        if !request_matches(judged.mutation, &request) {
            return CloseEffect::NotSent("herdr_close_without_terminate_judgment".into());
        }
        let call = self.0.call(request);
        if !crate::config_live_reload::current()
            .is_some_and(|config| config.runtime.herdr_terminate_enabled == Some(true))
        {
            return CloseEffect::NotSent("herdr_terminate_disabled".into());
        }
        let outcome = self.0.transport.call_with_witness(&call, &judged.witness);
        contract::close_result(&call, outcome)
    }

    /// Drops a judgment no send will use, so a later send without its own judgment writes nothing.
    pub(crate) fn discard_pin(&self) {
        let mut pinned = self.0.pinned.lock().unwrap_or_else(PoisonError::into_inner);
        *pinned = None;
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
            self.discard_pin();
            return Ok(refused("paste_end_marker_in_text"));
        }
        self.send_text(&format!("{PASTE_START}{text}{PASTE_END}"))
    }

    /// One line and Enter in a single input, so the line never lands without its Enter; a line
    /// holding a control character is refused unsent.
    pub(crate) fn send_line(&self, text: &str) -> Result<HostMutation, HostError> {
        if text.is_empty() || text.chars().any(char::is_control) {
            self.discard_pin();
            return Ok(refused("line_not_single"));
        }
        let (pane_id, text) = (self.0.pane.clone(), text.to_string());
        let keys = vec![herdr_key(HostKey::Enter).to_string()];
        self.send_pinned(HerdrRequest::PaneSendInput {
            pane_id,
            text,
            keys,
        })
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

fn next_call(next_id: &AtomicU64, request: HerdrRequest) -> HerdrCall {
    let id = next_id.fetch_add(1, Ordering::Relaxed);
    HerdrCall {
        id: format!("adk-input-{id}"),
        request,
    }
}

/// The reads of one pane on one transport, shared by the input gate and the read-only view.
struct Reads<'a> {
    transport: &'a dyn HerdrTransport,
    next_id: &'a AtomicU64,
    pane: &'a str,
}

impl Reads<'_> {
    fn processes(&self) -> PaneProcesses {
        let call = next_call(
            self.next_id,
            HerdrRequest::PaneProcessInfo {
                pane_id: self.pane.to_string(),
            },
        );
        let (outcome, witness) = self.transport.call(&call);
        let read = contract::foreground_result(&call, outcome, self.pane);
        (read.map_err(|error| format!("{error:?}")), witness)
    }

    fn provider_on(
        &self,
        provider: &str,
        nonce: &str,
        os: &dyn ProcessOs,
        witness: &ServerWitness,
    ) -> Result<ExpectedExecution, EvidenceGap> {
        let request = ProbeRequest {
            provider,
            nonce,
            launched_at: UNIX_EPOCH,
            witness,
            window: Duration::ZERO,
        };
        pane_probe::probe(&|| self.processes(), os, &request)
    }
}

/// A recorded pane read, never written, on this node's registered endpoint. Unlike a target it
/// needs only the stored location, so an execution without launch evidence can still be read.
pub(crate) struct HerdrPaneView {
    transport: Arc<dyn HerdrTransport>,
    pane: String,
    nonce: String,
    provider: String,
    os: Arc<dyn ProcessOs>,
    next_id: AtomicU64,
}

impl HerdrPaneView {
    /// A Pending or Bound execution whose stored location names exactly `endpoint`; no I/O.
    pub(crate) fn new(
        endpoint: &HerdrEndpoint,
        transport: Arc<dyn HerdrTransport>,
        stored: &HostedExecution,
    ) -> Option<Self> {
        let live = matches!(stored.state, HostedState::Pending | HostedState::Bound);
        let location = stored.location.as_ref()?;
        if !live || !on_endpoint(location, endpoint) {
            return None;
        }
        Some(Self {
            transport,
            pane: location.pane_id.clone(),
            nonce: stored.execution_nonce.clone(),
            provider: stored.owner.provider.clone(),
            os: Arc::new(HostOs),
            next_id: AtomicU64::new(1),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_os(self, os: Arc<dyn ProcessOs>) -> Self {
        Self { os, ..self }
    }

    pub(crate) fn pane_id(&self) -> &str {
        &self.pane
    }

    /// The pane's presence, root shell and provider, every answer from one server; no input.
    pub(crate) fn read_execution(&self) -> PaneReading {
        let unreadable = |why: &dyn std::fmt::Debug| PaneReading::Unreadable(format!("{why:?}"));
        let reads = Reads {
            transport: self.transport.as_ref(),
            next_id: &self.next_id,
            pane: &self.pane,
        };
        let witness = match self.transport.server_witness() {
            Ok(witness) => witness,
            Err(why) => return unreadable(&why),
        };
        let call = next_call(&self.next_id, HerdrRequest::SessionSnapshot {});
        let (outcome, answered) = self.transport.call(&call);
        if answered.as_ref() != Ok(&witness) {
            return unreadable(&"snapshot from another server");
        }
        match contract::snapshot_observation(&call, outcome, &self.pane).presence() {
            HostPresence::Present => {}
            HostPresence::Missing => return PaneReading::Missing,
            other => return unreadable(&other),
        }
        let (read, answered) = reads.processes();
        if answered.as_ref() != Ok(&witness) {
            return unreadable(&"processes from another server");
        }
        let (root, foreground) = match read {
            Ok((Some(root), ForegroundProcesses::Listed(foreground))) => (root, foreground),
            other => return unreadable(&other),
        };
        let root = match self.os.start(root) {
            Ok(start) => ProcessStamp {
                pid: root,
                start: pane_probe::start_text(start.identity),
            },
            Err(why) => return unreadable(&why),
        };
        let provider = if foreground == [root.pid] {
            PaneProvider::Exited
        } else {
            match reads.provider_on(&self.provider, &self.nonce, self.os.as_ref(), &witness) {
                Ok(now) if now.root == root => PaneProvider::Execution(now.provider_process),
                Ok(_) => PaneProvider::Unverified("root shell changed between readings".into()),
                Err(gap) => PaneProvider::Unverified(format!("{gap:?}")),
            }
        };
        PaneReading::Present { root, provider }
    }
}

fn refused(why: &str) -> HostMutation {
    HostMutation::Refused(HostRefusal::Precondition(why.to_string()))
}

fn pane_of(request: &HerdrRequest) -> Option<&str> {
    match request {
        HerdrRequest::PaneSendText { pane_id, .. }
        | HerdrRequest::PaneSendKeys { pane_id, .. }
        | HerdrRequest::PaneSendInput { pane_id, .. }
        | HerdrRequest::PaneClose { pane_id } => Some(pane_id),
        _ => None,
    }
}

fn cancel_only(request: &HerdrRequest) -> bool {
    matches!(request, HerdrRequest::PaneSendKeys { keys, .. }
        if !keys.is_empty() && keys.iter().all(|key| CANCEL_KEYS.contains(&key.as_str())))
}

fn request_matches(mutation: Mutation, request: &HerdrRequest) -> bool {
    let close = matches!(request, HerdrRequest::PaneClose { .. });
    (mutation == Mutation::Terminate) == close
        && (mutation != Mutation::Cancel || cancel_only(request))
}

impl PaneGate {
    fn call(&self, request: HerdrRequest) -> HerdrCall {
        next_call(&self.next_id, request)
    }

    /// Admission first (no I/O), the `.host_kind` marker, then E7, then the pane on the server
    /// E7 named.
    fn judge(&self, mutation: Mutation) -> Result<Judged, HerdrGateRefusal> {
        if mutation == Mutation::Input {
            herdr_admission::check().map_err(HerdrGateRefusal::AdmissionStopped)?;
        }
        match read_host_kind_marker(&self.logical_key) {
            HostKindMarker::Known(HostKind::Herdr) => {}
            HostKindMarker::Known(_) => return Err(HerdrGateRefusal::MarkerOtherHost),
            _ => return Err(HerdrGateRefusal::MarkerUnverified),
        }
        let witness = match (self.read_restore)(self.transport.as_ref(), &self.endpoint) {
            RestoreResume::Off { witness } => witness,
            RestoreResume::On => return Err(HerdrGateRefusal::RestoreOn),
            RestoreResume::Unverified(RestoreUnverified::VersionNotVerified)
                if mutation == Mutation::Terminate =>
            {
                return Err(HerdrGateRefusal::TerminateUnsupportedServer);
            }
            RestoreResume::Unverified(why) => return Err(HerdrGateRefusal::RestoreUnverified(why)),
        };
        self.verify_pane(&witness)?;
        Ok(Judged { witness, mutation })
    }

    /// The launch's provenance rule read once more on `witness`'s server.
    fn provider_on(&self, witness: &ServerWitness) -> Result<ExpectedExecution, EvidenceGap> {
        let reads = Reads {
            transport: self.transport.as_ref(),
            next_id: &self.next_id,
            pane: &self.pane,
        };
        reads.provider_on(
            &self.expected.binding_provider,
            &self.nonce,
            self.os.as_ref(),
            witness,
        )
    }

    /// The provenance rule on `witness`'s server must name the recorded root shell and provider
    /// processes.
    fn verify_pane(&self, witness: &ServerWitness) -> Result<(), HerdrGateRefusal> {
        let now = self.provider_on(witness).map_err(|gap| {
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
        if !request_matches(judged.mutation, &request) || judged.mutation == Mutation::Terminate {
            return Ok(refused("herdr_judgment_request_mismatch"));
        }
        let call = self.call(request);
        let outcome = self.transport.call_with_witness(&call, &judged.witness);
        contract::mutation_result(&call, outcome, &self.pane)
    }
}

#[cfg(test)]
impl From<CloseEffect>
    for crate::services::termination_audit::host_terminate::herdr_terminate::HerdrTerminateResult
{
    fn from(effect: CloseEffect) -> Self {
        match effect {
            CloseEffect::Acknowledged => Self::Acknowledged,
            CloseEffect::NotSent(why) => Self::NotSent(why),
            CloseEffect::Indeterminate(why) => Self::Indeterminate(why),
            CloseEffect::ConfirmationRequired => Self::ConfirmationRequired,
        }
    }
}

#[cfg(test)]
#[path = "herdr_gate_tests.rs"]
mod tests;
