//! Mutation gate for one recorded pane: admission, endpoint policy, exact execution.
//! Every write stays pinned to the server that the policy permitted.

#[cfg(unix)]
#[path = "herdr_gate/root_replaced.rs"]
pub(crate) mod root_replaced;

#[path = "herdr_gate/legacy_return.rs"]
pub(crate) mod legacy_return;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, UNIX_EPOCH};

use super::herdr::contract::{
    self, CloseEffect, ForegroundProcesses, HerdrTransport, ServerWitness,
};
use super::herdr::model::{HerdrCall, HerdrEndpoint, HerdrRequest};
use super::herdr::observe::{self, RestoreResume, RestoreUnverified};
use super::herdr::pane_probe::{self, EvidenceGap, HostOs, PaneProcesses, ProbeRequest, ProcessOs};
use super::herdr::policy::Unverified;
use super::herdr::policy::permit::{ExistingMutationEvidence, ExistingMutationPermit};
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
    PolicyUnverified(Unverified),
    /// The row holds no Pending or Bound execution with a location and launch evidence.
    NoStoredExecution,
    /// The logical key's `.host_kind` marker names another host.
    MarkerOtherHost,
    /// The marker is absent, unrecognized or unreadable.
    MarkerUnverified,
    /// A pane reading came from another server than the permitted witness.
    ServerChanged,
    /// The pane did not read as one root shell running one provider.
    PaneUnverified,
    OtherNonce,
    RootReplaced,
    ProviderReplaced,
    TerminateUnsupportedServer,
    MutationBusy,
    ExecutionClosing,
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

/// How long a read-only confirmation reads a pane again past a gap a short-lived process leaves.
const CONFIRM_WINDOW: Duration = Duration::from_secs(2);

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
        HostKey::CtrlS => "ctrl+s",
        HostKey::Left => "left",
        HostKey::Right => "right",
        HostKey::Backspace => "backspace",
    }
}

#[derive(Default)]
struct ExecutionFence {
    state: Mutex<(bool, bool)>, // active permit, closing
}

struct MutationPermit {
    fence: Arc<ExecutionFence>,
}
impl Drop for MutationPermit {
    fn drop(&mut self) {
        self.fence
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .0 = false;
    }
}

type FenceKey = (String, String, String, String, String, String);

fn execution_fences() -> &'static Mutex<HashMap<FenceKey, Arc<ExecutionFence>>> {
    static FENCES: OnceLock<Mutex<HashMap<FenceKey, Arc<ExecutionFence>>>> = OnceLock::new();
    FENCES.get_or_init(Default::default)
}

fn execution_fence(endpoint: &HerdrEndpoint, pane: &str, nonce: &str) -> Arc<ExecutionFence> {
    fence_at((
        endpoint.execution_node().to_string(),
        endpoint.config_key().to_string(),
        endpoint.socket_path().to_string_lossy().into_owned(),
        endpoint.herdr_session().to_string(),
        pane.to_string(),
        nonce.to_string(),
    ))
}

fn fence_at(key: FenceKey) -> Arc<ExecutionFence> {
    // Closing tombstones must outlive targets so a fresh target cannot reuse an uncertain nonce.
    execution_fences()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(key)
        .or_default()
        .clone()
}

/// Restores the fence a restart lost for a Bound execution a reconnect matched exactly, so a
/// notice admission finds it; an existing entry keeps its closing tombstone.
pub(crate) fn restore_execution_fence(stored: &HostedExecution) -> bool {
    let (Some(location), Some(expected)) = (&stored.location, &stored.expected) else {
        return false;
    };
    if !matches!(stored.state, HostedState::Bound)
        || expected.binding_nonce != stored.execution_nonce
    {
        return false;
    }
    fence_at((
        location.execution_node.clone(),
        location.endpoint_config_key.clone(),
        std::path::Path::new(&location.socket_addr)
            .to_string_lossy()
            .into_owned(),
        location.named_session.clone(),
        location.pane_id.clone(),
        stored.execution_nonce.clone(),
    ));
    true
}

impl ExecutionFence {
    fn acquire(self: &Arc<Self>, mutation: Mutation) -> Result<MutationPermit, HerdrGateRefusal> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.0 {
            return Err(HerdrGateRefusal::MutationBusy);
        }
        if state.1 && mutation != Mutation::Terminate {
            return Err(HerdrGateRefusal::ExecutionClosing);
        }
        state.0 = true;
        Ok(MutationPermit {
            fence: self.clone(),
        })
    }
}

pub(crate) fn try_notice_fence<R>(
    endpoint: &crate::config::session_hosts::ChannelEndpoint,
    nonce: &str,
    start: impl FnOnce() -> R,
) -> Result<R, HerdrGateRefusal> {
    let fences = execution_fences()
        .try_lock()
        .map_err(|_| HerdrGateRefusal::NoStoredExecution)?;
    let mut candidates = fences
        .iter()
        .filter(|((node, key, socket, session, _, execution), _)| {
            node == &endpoint.execution_node
                && key == &endpoint.key
                && std::path::Path::new(socket) == endpoint.socket_path
                && session == &endpoint.herdr_session
                && execution == nonce
        });
    let fence = candidates
        .next()
        .map(|(_, fence)| fence.clone())
        .ok_or(HerdrGateRefusal::NoStoredExecution)?;
    if candidates.next().is_some() {
        return Err(HerdrGateRefusal::NoStoredExecution);
    }
    drop(fences);
    let state = fence.state.try_lock().map_err(|error| match error {
        std::sync::TryLockError::WouldBlock => HerdrGateRefusal::MutationBusy,
        std::sync::TryLockError::Poisoned(_) => HerdrGateRefusal::NoStoredExecution,
    })?;
    if state.1 {
        return Err(HerdrGateRefusal::ExecutionClosing);
    }
    if state.0 {
        return Err(HerdrGateRefusal::MutationBusy);
    }
    let result = start();
    drop(state);
    Ok(result)
}

pub(crate) struct TerminationFence(MutationPermit, bool);
impl TerminationFence {
    /// A pre-write refusal can reopen only this execution, never a successor nonce.
    pub(crate) fn reopen(&self) {
        self.0
            .fence
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .1 = self.1;
    }
}

/// One gate decision; only the gate makes one, and one send consumes it.
struct Judged {
    witness: ServerWitness,
    mutation: Mutation,
    _permit: Option<MutationPermit>,
}

struct PaneGate {
    endpoint: HerdrEndpoint,
    transport: Arc<dyn HerdrTransport>,
    pane: String,
    nonce: String,
    logical_key: String,
    expected: ExpectedExecution,
    read_restore: fn(&dyn HerdrTransport, &HerdrEndpoint) -> RestoreResume,
    evidence: Result<ExistingMutationEvidence, Unverified>,
    os: Arc<dyn ProcessOs>,
    next_id: AtomicU64,
    /// The latest judgment, waiting for the send it admits.
    pinned: Mutex<Option<Judged>>,
    fence: Arc<ExecutionFence>,
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
        Self::new_with_evidence(
            endpoint,
            transport,
            stored,
            ExistingMutationEvidence::default(),
        )
    }

    /// Evidence is loaded by the async owner; judging never queries its store.
    pub(crate) fn new_with_evidence(
        endpoint: HerdrEndpoint,
        transport: Arc<dyn HerdrTransport>,
        stored: &HostedExecution,
        evidence: ExistingMutationEvidence,
    ) -> Option<Self> {
        Self::new_with_evidence_result(endpoint, transport, stored, Ok(evidence))
    }

    pub(crate) fn new_with_evidence_result(
        endpoint: HerdrEndpoint,
        transport: Arc<dyn HerdrTransport>,
        stored: &HostedExecution,
        evidence: Result<ExistingMutationEvidence, Unverified>,
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
            fence: execution_fence(&endpoint, &location.pane_id, &stored.execution_nonce),
            endpoint,
            transport,
            pane: location.pane_id.clone(),
            nonce: stored.execution_nonce.clone(),
            logical_key: stored.owner.logical_key.clone(),
            expected: expected.clone(),
            read_restore: read_restore_off,
            evidence,
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

    /// Keeps this nonce closed and excludes all pane writes through settlement.
    pub(crate) fn termination_fence(&self) -> Result<TerminationFence, HerdrGateRefusal> {
        let permit = self.0.fence.acquire(Mutation::Terminate)?;
        let mut state = self
            .0
            .fence
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let was_closing = state.1;
        state.1 = true;
        Ok(TerminationFence(permit, was_closing))
    }

    pub(crate) fn revalidate_termination_reads(&self) -> Result<ServerWitness, HerdrGateRefusal> {
        self.0.restore_witness(Mutation::Terminate)
    }

    pub(crate) fn provider_absent(&self) -> Result<bool, String> {
        match pane_probe::recorded_process(self.0.os.as_ref(), &self.0.expected.provider_process) {
            pane_probe::RecordedProcess::ExactAlive => Ok(false),
            pane_probe::RecordedProcess::ProvenAbsent | pane_probe::RecordedProcess::Replaced => {
                Ok(true)
            }
            pane_probe::RecordedProcess::Unreadable => Err("provider unreadable".into()),
        }
    }

    pub(crate) fn pane_id(&self) -> &str {
        &self.0.pane
    }

    /// Judges the next input and keeps the judgment for the send that follows it.
    pub(crate) fn pin(&self, mutation: Mutation) -> Result<(), HerdrGateRefusal> {
        let mut pinned = self.0.pinned.lock().unwrap_or_else(PoisonError::into_inner);
        *pinned = None;
        let judged = self.0.fence.acquire(mutation).and_then(|permit| {
            self.0.judge(mutation).map(|mut judged| {
                judged._permit = Some(permit);
                judged
            })
        });
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

    pub(crate) fn pin_terminate_fenced(
        &self,
        fence: &TerminationFence,
    ) -> Result<(), HerdrGateRefusal> {
        if !Arc::ptr_eq(&self.0.fence, &fence.0.fence) {
            return Err(HerdrGateRefusal::OtherNonce);
        }
        let judged = self.0.judge(Mutation::Terminate)?;
        *self.0.pinned.lock().unwrap_or_else(PoisonError::into_inner) = Some(judged);
        Ok(())
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
        let call = close_call(&self.0.next_id, &self.0.pane);
        if !request_matches(judged.mutation, &call.request) {
            return CloseEffect::NotSent("herdr_close_without_terminate_judgment".into());
        }
        if !crate::config_live_reload::current()
            .is_some_and(|config| config.runtime.herdr_terminate_enabled == Some(true))
        {
            return CloseEffect::NotSent("herdr_terminate_disabled".into());
        }
        #[cfg(test)]
        if self.0.endpoint.mode() == crate::config::session_hosts::EndpointMode::UserDefault {
            super::herdr::policy::permit::EVENTS.with(|events| events.borrow_mut().push("Close"));
        }
        let result = deliver_close(self.0.transport.as_ref(), &judged.witness, &call);
        if matches!(
            result,
            CloseEffect::Acknowledged | CloseEffect::Indeterminate(_)
        ) {
            self.0
                .fence
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .1 = true;
        }
        result
    }

    /// Drops a judgment no send will use, so a later send without its own judgment writes nothing.
    pub(crate) fn discard_pin(&self) {
        let mut pinned = self.0.pinned.lock().unwrap_or_else(PoisonError::into_inner);
        *pinned = None;
    }

    pub(crate) fn send_text(&self, text: &str) -> Result<HostMutation, HostError> {
        let pane_id = self.0.pane.clone();
        let text = text.to_string();
        self.send_content(HerdrRequest::PaneSendText { pane_id, text })
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
        self.send_content(HerdrRequest::PaneSendInput {
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

    /// Content passes the owner fence first; a refused write drops its judgment unsent.
    fn send_content(&self, request: HerdrRequest) -> Result<HostMutation, HostError> {
        let mut sent = None;
        let physical = format!(
            "{}#{}",
            self.0.endpoint.socket_path().display(),
            self.0.pane
        );
        let fence = crate::services::tui_input::writer_lease::content("herdr", &physical, || {
            sent = Some(self.send_pinned(request));
            Ok(())
        });
        match (fence, sent) {
            (Ok(()), Some(sent)) => sent,
            _ => {
                self.discard_pin();
                Ok(refused("owner_content_fence"))
            }
        }
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
        self.read(contract::capture_request(&self.0.pane, scroll_back))
    }

    /// The same read with ANSI styling kept, under the same source and truncation checks.
    pub(crate) fn capture_ansi(&self, scroll_back: i32) -> Option<String> {
        self.read(contract::capture_ansi_request(&self.0.pane, scroll_back))
    }

    fn read(&self, request: HerdrRequest) -> Option<String> {
        let call = self.0.call(request);
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

    /// The same read for an attach or reconnect: a gap a short-lived pane process leaves, as the
    /// launch probe allows, is read again for a moment on the same server; each attempt is the whole proof.
    pub(crate) fn execution_confirmed(&self) -> bool {
        let witness = self.0.transport.server_witness();
        let confirmed = witness
            .map_err(|why| format!("{why:?}"))
            .and_then(|witness| {
                let verified = self.0.verify_pane_within(&witness, CONFIRM_WINDOW);
                verified.map_err(|refusal| format!("{refusal:?}"))
            });
        if let Err(why) = &confirmed {
            tracing::info!(pane = %self.0.pane, %why, "herdr gate: execution not confirmed");
        }
        confirmed.is_ok()
    }
}

/// The close of `pane`; every close this gate sends is built here.
fn close_call(next_id: &AtomicU64, pane: &str) -> HerdrCall {
    next_call(
        next_id,
        HerdrRequest::PaneClose {
            pane_id: pane.to_string(),
        },
    )
}

/// Sends a close only on a connection to `witness`'s server.
fn deliver_close(
    transport: &dyn HerdrTransport,
    witness: &ServerWitness,
    call: &HerdrCall,
) -> CloseEffect {
    let outcome = transport.call_with_witness(call, witness);
    contract::close_result(call, outcome)
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
        window: Duration,
    ) -> Result<ExpectedExecution, EvidenceGap> {
        let request = ProbeRequest {
            provider,
            nonce,
            launched_at: UNIX_EPOCH,
            witness,
            window,
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
        let witness = match self.transport.server_witness() {
            Ok(witness) => witness,
            Err(why) => return unreadable(&why),
        };
        self.read_execution_on(&witness)
    }

    fn read_execution_on(&self, witness: &ServerWitness) -> PaneReading {
        let unreadable = |why: &dyn std::fmt::Debug| PaneReading::Unreadable(format!("{why:?}"));
        let reads = Reads {
            transport: self.transport.as_ref(),
            next_id: &self.next_id,
            pane: &self.pane,
        };
        let call = next_call(&self.next_id, HerdrRequest::SessionSnapshot {});
        let (outcome, answered) = self.transport.call(&call);
        if answered.as_ref() != Ok(witness) {
            return unreadable(&"snapshot from another server");
        }
        match contract::snapshot_observation(&call, outcome, &self.pane).presence() {
            HostPresence::Present => {}
            HostPresence::Missing => return PaneReading::Missing,
            other => return unreadable(&other),
        }
        let (read, answered) = reads.processes();
        if answered.as_ref() != Ok(witness) {
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
            match reads.provider_on(
                &self.provider,
                &self.nonce,
                self.os.as_ref(),
                witness,
                Duration::ZERO,
            ) {
                Ok(now) if now.root == root => PaneProvider::Execution(now.provider_process),
                Ok(_) => PaneProvider::Unverified("root shell changed between readings".into()),
                Err(gap) => PaneProvider::Unverified(format!("{gap:?}")),
            }
        };
        PaneReading::Present { root, provider }
    }

    /// `confirmed_ended` of `reading` over this view's process reads.
    pub(crate) fn ended(&self, record: &HostedExecution, reading: &PaneReading) -> bool {
        confirmed_ended(self.os.as_ref(), record, reading)
    }
}

/// `record`'s execution is over: its pane is gone, or a lone shell is left that is its recorded
/// root or another shell outliving both recorded processes; only the latter reads the OS.
fn confirmed_ended(os: &dyn ProcessOs, record: &HostedExecution, reading: &PaneReading) -> bool {
    let absent = |stamp: &ProcessStamp| {
        pane_probe::recorded_process(os, stamp) == pane_probe::RecordedProcess::ProvenAbsent
    };
    match reading {
        PaneReading::Missing => true,
        PaneReading::Present {
            root,
            provider: PaneProvider::Exited,
        } => record.expected.as_ref().is_some_and(|expected| {
            expected.root == *root || (absent(&expected.root) && absent(&expected.provider_process))
        }),
        PaneReading::Present { .. } | PaneReading::Unreadable(_) => false,
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

    /// Admission first (no I/O), the `.host_kind` marker, then endpoint policy and the exact
    /// pane execution on its permitted server.
    fn judge(&self, mutation: Mutation) -> Result<Judged, HerdrGateRefusal> {
        if mutation == Mutation::Input {
            herdr_admission::check().map_err(HerdrGateRefusal::AdmissionStopped)?;
        }
        let witness = self.restore_witness(mutation)?;
        self.verify_pane(&witness)?;
        Ok(Judged {
            witness,
            mutation,
            _permit: None,
        })
    }

    /// The host marker and endpoint policy select the server every later read and write must reach.
    fn restore_witness(&self, mutation: Mutation) -> Result<ServerWitness, HerdrGateRefusal> {
        match read_host_kind_marker(&self.logical_key) {
            HostKindMarker::Known(HostKind::Herdr) => {}
            HostKindMarker::Known(_) => return Err(HerdrGateRefusal::MarkerOtherHost),
            _ => return Err(HerdrGateRefusal::MarkerUnverified),
        }
        if self.endpoint.mode() == crate::config::session_hosts::EndpointMode::UserDefault {
            let evidence = self
                .evidence
                .as_ref()
                .map_err(|why| HerdrGateRefusal::PolicyUnverified(*why))?;
            let witness = self
                .transport
                .server_witness()
                .map_err(|_| HerdrGateRefusal::PolicyUnverified(Unverified::Peer))?;
            let endpoint = crate::services::herdr_launch::HerdrLaunchEndpoint {
                execution_node: self.endpoint.execution_node().into(),
                config_key: self.endpoint.config_key().into(),
                socket_addr: self.endpoint.socket_path().to_string_lossy().into_owned(),
                herdr_session: self.endpoint.herdr_session().into(),
            };
            return ExistingMutationPermit::issue(&endpoint, &self.nonce, witness, evidence)
                .map(|permit| permit.witness().clone())
                .map_err(HerdrGateRefusal::PolicyUnverified);
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
        Ok(witness)
    }

    /// The launch's provenance rule read once more on `witness`'s server, within `window`.
    fn provider_on(
        &self,
        witness: &ServerWitness,
        window: Duration,
    ) -> Result<ExpectedExecution, EvidenceGap> {
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
            window,
        )
    }

    /// The provenance rule on `witness`'s server must name the recorded root shell and provider
    /// processes.
    fn verify_pane(&self, witness: &ServerWitness) -> Result<(), HerdrGateRefusal> {
        self.verify_pane_within(witness, Duration::ZERO)
    }

    fn verify_pane_within(
        &self,
        witness: &ServerWitness,
        window: Duration,
    ) -> Result<(), HerdrGateRefusal> {
        #[cfg(test)]
        if self.endpoint.mode() == crate::config::session_hosts::EndpointMode::UserDefault {
            super::herdr::policy::permit::EVENTS
                .with(|events| events.borrow_mut().push("exact_identity_probe"));
        }
        let now = self.provider_on(witness, window).map_err(|gap| {
            match window.is_zero() {
                true => {
                    tracing::debug!(pane = %self.pane, ?gap, "herdr input gate: pane unverified")
                }
                false => tracing::info!(pane = %self.pane, ?gap, "herdr gate: pane unconfirmed"),
            }
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
        #[cfg(test)]
        if self.endpoint.mode() == crate::config::session_hosts::EndpointMode::UserDefault {
            super::herdr::policy::permit::EVENTS.with(|events| {
                events.borrow_mut().push(match judged.mutation {
                    Mutation::Input => "Input",
                    Mutation::Cancel => "Cancel",
                    Mutation::Terminate => "Close",
                })
            });
        }
        let outcome = self.transport.call_with_witness(&call, &judged.witness);
        contract::mutation_result(&call, outcome, &self.pane)
    }
}

#[cfg(unix)]
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

/// The bare shell a create left when its reply was lost: removed only while it is still the
/// untouched workspace the create made, and only on the server it was made on.
#[cfg(unix)]
pub(crate) mod residue {
    use std::sync::atomic::AtomicU64;
    use std::thread;
    use std::time::Duration;

    use super::super::herdr::contract::{self, CloseEffect, HerdrTransport, ServerWitness};
    use super::super::herdr::model::{HerdrRequest, HerdrSnapshot};
    use super::super::herdr::pane_probe::{self, PaneOccupant, ProcessOs, RecordedProcess};
    use super::{close_call, deliver_close, next_call};
    use crate::db::dispatched_sessions::hosted_execution::ProcessStamp;

    const GONE_POLLS: u32 = 15;
    const GONE_POLL: Duration = Duration::from_millis(200);

    /// The shell a create left: its pane, the workspace label the create gave it, and its root.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct CreateResidue {
        pub pane_id: String,
        pub label: String,
        pub root: ProcessStamp,
    }

    /// Reads the residue again on `witness`'s server, closes it, and returns once the pane and its
    /// root shell are both gone; any other reading closes nothing.
    pub(crate) fn dispose_create_residue(
        transport: &dyn HerdrTransport,
        os: &dyn ProcessOs,
        witness: &ServerWitness,
        next_id: &AtomicU64,
        residue: &CreateResidue,
    ) -> Result<(), String> {
        let snapshot = snapshot_on(transport, witness, next_id)?;
        if !untouched(&snapshot, residue) {
            return Err("the residue workspace changed".into());
        }
        let call = next_call(
            next_id,
            HerdrRequest::PaneProcessInfo {
                pane_id: residue.pane_id.clone(),
            },
        );
        let (outcome, answered) = transport.call(&call);
        let read = contract::foreground_result(&call, outcome, &residue.pane_id);
        let reading = (read.map_err(|error| format!("{error:?}")), answered);
        match pane_probe::occupant(reading, os, witness) {
            PaneOccupant::LoneShell { root, .. } if root == residue.root => {}
            other => return Err(format!("the residue pane now reads {other:?}")),
        }
        let closed = deliver_close(transport, witness, &close_call(next_id, &residue.pane_id));
        if let CloseEffect::NotSent(detail) = &closed {
            return Err(format!("close not sent: {detail}"));
        }
        for _ in 0..GONE_POLLS {
            let snapshot = snapshot_on(transport, witness, next_id)?;
            let pane_gone = !snapshot
                .panes
                .iter()
                .any(|pane| pane.pane_id == residue.pane_id);
            if pane_gone
                && pane_probe::recorded_process(os, &residue.root) == RecordedProcess::ProvenAbsent
            {
                return Ok(());
            }
            thread::sleep(GONE_POLL);
        }
        Err(format!("residue not confirmed gone after {closed:?}"))
    }

    /// A labeled snapshot that `witness`'s server answered.
    pub(crate) fn snapshot_on(
        transport: &dyn HerdrTransport,
        witness: &ServerWitness,
        next_id: &AtomicU64,
    ) -> Result<HerdrSnapshot, String> {
        let call = next_call(next_id, HerdrRequest::SessionSnapshot {});
        let (outcome, answered) = transport.call(&call);
        if answered.as_ref() != Ok(witness) {
            return Err(format!("snapshot answered by {answered:?}"));
        }
        contract::labeled_snapshot(&call, outcome)
    }

    /// Exactly one workspace carries the label, with one tab and one pane, and that pane is the residue.
    fn untouched(snapshot: &HerdrSnapshot, residue: &CreateResidue) -> bool {
        let Some(workspaces) = &snapshot.workspaces else {
            return false;
        };
        let labeled: Vec<_> = workspaces
            .iter()
            .filter(|workspace| workspace.label == residue.label)
            .collect();
        let [workspace] = labeled.as_slice() else {
            return false;
        };
        workspace.pane_count == 1
            && workspace.tab_count == 1
            && snapshot.panes.iter().any(|pane| {
                pane.pane_id == residue.pane_id && pane.workspace_id == workspace.workspace_id
            })
    }
}

#[cfg(test)]
#[path = "herdr_gate_tests.rs"]
pub(super) mod tests;
