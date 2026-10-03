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
    read_restore: fn(&T, &HerdrEndpoint) -> RestoreResume,
}

/// Paste end marker; a body holding it would close the paste early.
const PASTE_END: &str = "\x1b[201~";
const PASTE_START: &str = "\x1b[200~";

/// The executor's tmux key names mapped to Herdr's; anything else is never sent.
fn herdr_key_name(key: &str) -> Option<&'static str> {
    Some(match key {
        "Enter" => "enter",
        "Escape" => "esc",
        "C-u" => "ctrl+u",
        "C-e" => "ctrl+e",
        "Left" => "left",
        "Right" => "right",
        "BSpace" => "backspace",
        _ => return None,
    })
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
    pub(crate) fn with_restore_reader(
        self,
        read_restore: fn(&T, &HerdrEndpoint) -> RestoreResume,
    ) -> Self {
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
        (self.read_restore)(&self.transport, &self.endpoint)
            .admitted_generation()
            .ok_or_else(|| {
                HostMutation::Refused(HostRefusal::Precondition(
                    RESTORE_RESUME_NOT_OFF.to_string(),
                ))
            })
    }

    /// One mutation, after E7, on the connection that read it; `request` is checked first.
    fn mutate(&self, pane: &str, request: HerdrRequest) -> Result<HostMutation, HostError> {
        let generation = match self.restore_off() {
            Ok(generation) => generation,
            Err(refusal) => return Ok(refusal),
        };
        let call = self.next_call(request);
        let (outcome, _) = self.transport.call_on(&call, generation);
        contract::mutation_result(&call, outcome, pane)
    }

    /// Prompt text as one bracketed paste, as tmux `paste-buffer -p` delivers it; Herdr
    /// does not bracket `send_text` itself. A body holding the end marker is refused.
    pub(crate) fn send_paste(
        &self,
        session: HostSessionRef<'_>,
        text: &str,
    ) -> Result<HostMutation, HostError> {
        let pane = pane_id(session)?;
        if text.contains(PASTE_END) {
            return Ok(HostMutation::Refused(HostRefusal::Precondition(
                "paste_end_marker_in_text".to_string(),
            )));
        }
        let text = format!("{PASTE_START}{text}{PASTE_END}");
        let pane_id = pane.to_string();
        self.mutate(pane, HerdrRequest::PaneSendText { pane_id, text })
    }

    /// One line and Enter in a single input, so the text never lands without its Enter.
    pub(crate) fn send_line(
        &self,
        session: HostSessionRef<'_>,
        text: &str,
    ) -> Result<HostMutation, HostError> {
        let pane = pane_id(session)?;
        if text.is_empty() || text.chars().any(char::is_control) {
            return Ok(HostMutation::Refused(HostRefusal::Precondition(
                "line_not_single".to_string(),
            )));
        }
        let request = HerdrRequest::PaneSendInput {
            pane_id: pane.to_string(),
            text: text.to_string(),
            keys: vec!["enter".to_string()],
        };
        self.mutate(pane, request)
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

    // Supported operations, not admission: no production path selects Herdr.
    fn capabilities(&self) -> HostCapabilities {
        HostCapabilities {
            send_text: true,
            send_keys: true,
            interrupt: true,
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
        let request = HerdrRequest::PaneSendText {
            pane_id: pane.to_string(),
            text: text.to_string(),
        };
        self.mutate(pane, request)
    }

    /// Every key is mapped before any I/O; one unknown key sends none of them.
    fn send_keys(
        &self,
        session: HostSessionRef<'_>,
        keys: &[&str],
    ) -> Result<HostMutation, HostError> {
        let pane = pane_id(session)?;
        let mapped: Option<Vec<String>> = keys
            .iter()
            .map(|key| herdr_key_name(key).map(str::to_string))
            .collect();
        let Some(keys) = mapped.filter(|keys| !keys.is_empty()) else {
            return refused("send_keys");
        };
        let pane_id = pane.to_string();
        self.mutate(pane, HerdrRequest::PaneSendKeys { pane_id, keys })
    }

    fn interrupt(&self, session: HostSessionRef<'_>) -> Result<HostMutation, HostError> {
        let pane = pane_id(session)?;
        let request = HerdrRequest::PaneSendKeys {
            pane_id: pane.to_string(),
            keys: vec!["ctrl+c".to_string()],
        };
        self.mutate(pane, request)
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
