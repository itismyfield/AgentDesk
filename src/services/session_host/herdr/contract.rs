//! Transport contract and the pure reply → host-result adapter. No socket
//! here: a transport is injected, and tests use a fake one.
#![cfg_attr(not(test), allow(dead_code))]

use std::path::PathBuf;

use super::model::{
    ControlPlane, ExecutionState, HERDR_PROTOCOL, HerdrCall, HerdrErrorBody, HerdrObservation,
    HerdrReadSource, HerdrReply, HerdrRequest, HerdrResult, PaneState,
};
use crate::services::session_host::model::{HostError, HostMutation};

/// Upper bound on history lines one capture may request.
pub(crate) const CAPTURE_MAX_LINES: u32 = 10_000;

/// Why no typed reply came back. `AfterWrite` means the server may have acted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HerdrTransportError {
    NotSent(String),
    AfterWrite(String),
}

pub(crate) type HerdrOutcome = Result<HerdrReply, HerdrTransportError>;

/// One request, one reply; framing and connection reuse belong to the transport.
pub(crate) trait HerdrTransport: Send + Sync {
    /// The outcome and the generation of the connection that carried it, read
    /// together so a concurrent reconnect cannot relabel the reply.
    fn call(&self, call: &HerdrCall) -> (HerdrOutcome, u64);
    /// Sends `call` only on the open connection with `generation`; otherwise `NotSent`.
    fn call_on(&self, call: &HerdrCall, generation: u64) -> (HerdrOutcome, u64);
}

enum Fault {
    Transport(HerdrTransportError),
    Remote(HerdrErrorBody),
    Contract(String),
}

impl From<Fault> for HostError {
    fn from(fault: Fault) -> Self {
        match fault {
            Fault::Transport(
                HerdrTransportError::NotSent(message) | HerdrTransportError::AfterWrite(message),
            ) => HostError::Transport(message),
            Fault::Remote(body) => HostError::Remote {
                code: body.code,
                message: body.message,
            },
            Fault::Contract(detail) => HostError::Protocol(detail),
        }
    }
}

fn reply_result(call: &HerdrCall, outcome: HerdrOutcome) -> Result<HerdrResult, Fault> {
    let reply = outcome.map_err(Fault::Transport)?;
    if reply.id != call.id {
        return Err(Fault::Contract(format!(
            "reply id {} for request {}",
            reply.id, call.id
        )));
    }
    reply.body.map_err(Fault::Remote)
}

fn unexpected(result: &HerdrResult) -> Fault {
    Fault::Contract(format!("unexpected result {result:?}"))
}

fn same_pane(expected: &str, actual: &str) -> Result<(), Fault> {
    if expected == actual {
        Ok(())
    } else {
        Err(Fault::Contract(format!(
            "pane {actual} answered for {expected}"
        )))
    }
}

/// Only a complete, protocol-matching snapshot can say the pane is missing.
pub(crate) fn snapshot_observation(
    call: &HerdrCall,
    outcome: HerdrOutcome,
    pane_id: &str,
) -> HerdrObservation {
    let snapshot = match reply_result(call, outcome) {
        Ok(HerdrResult::SessionSnapshot { snapshot }) => snapshot,
        Ok(_) | Err(Fault::Contract(_)) => {
            return HerdrObservation::failed(ControlPlane::Incompatible);
        }
        Err(Fault::Transport(_)) => return HerdrObservation::failed(ControlPlane::Unreachable),
        Err(Fault::Remote(_)) => return HerdrObservation::failed(ControlPlane::Reachable),
    };
    if snapshot.protocol != HERDR_PROTOCOL {
        return HerdrObservation::failed(ControlPlane::Incompatible);
    }
    let pane = snapshot.panes.iter().find(|pane| pane.pane_id == pane_id);
    HerdrObservation {
        pane: if pane.is_some() {
            PaneState::Present
        } else {
            PaneState::Missing
        },
        revision: pane.map(|pane| pane.revision),
        ..HerdrObservation::failed(ControlPlane::Reachable)
    }
}

/// A root shell pid reads as Live; a null pid or any failure stays Unknown.
pub(crate) fn with_process_info(
    observation: HerdrObservation,
    call: &HerdrCall,
    outcome: HerdrOutcome,
    pane_id: &str,
) -> HerdrObservation {
    let shell_pid = execution_pid_result(call, outcome, pane_id).ok().flatten();
    HerdrObservation {
        execution: if shell_pid.is_some() {
            ExecutionState::Live
        } else {
            ExecutionState::Unknown
        },
        shell_pid,
        ..observation
    }
}

/// `shell_pid` is the PTY root, like tmux `pane_pid`; never a foreground pid.
pub(crate) fn execution_pid_result(
    call: &HerdrCall,
    outcome: HerdrOutcome,
    pane_id: &str,
) -> Result<Option<u32>, HostError> {
    match reply_result(call, outcome)? {
        HerdrResult::PaneProcessInfo { process_info } => {
            same_pane(pane_id, &process_info.pane_id)?;
            Ok(process_info.shell_pid)
        }
        other => Err(unexpected(&other).into()),
    }
}

pub(crate) fn working_dir_result(
    call: &HerdrCall,
    outcome: HerdrOutcome,
    pane_id: &str,
) -> Result<Option<PathBuf>, HostError> {
    match reply_result(call, outcome)? {
        HerdrResult::PaneInfo { pane } => {
            same_pane(pane_id, &pane.pane_id)?;
            Ok(pane.foreground_cwd.or(pane.cwd).map(PathBuf::from))
        }
        other => Err(unexpected(&other).into()),
    }
}

/// `scroll_back` < 0 asks for that many unwrapped history lines, capped.
pub(crate) fn capture_request(pane_id: &str, scroll_back: i32) -> HerdrRequest {
    let (source, lines) = if scroll_back < 0 {
        let lines = scroll_back.unsigned_abs().min(CAPTURE_MAX_LINES);
        (HerdrReadSource::RecentUnwrapped, Some(lines))
    } else {
        (HerdrReadSource::Visible, None)
    };
    HerdrRequest::PaneRead {
        pane_id: pane_id.to_string(),
        source,
        lines,
        strip_ansi: true,
    }
}

/// A truncated read is an error, never a complete screen.
pub(crate) fn capture_result(
    call: &HerdrCall,
    outcome: HerdrOutcome,
    pane_id: &str,
) -> Result<String, HostError> {
    let HerdrRequest::PaneRead { source, .. } = &call.request else {
        return Err(HostError::Protocol("capture without pane.read".to_string()));
    };
    match reply_result(call, outcome)? {
        HerdrResult::PaneRead { read } => {
            same_pane(pane_id, &read.pane_id)?;
            if read.source != *source || read.truncated {
                return Err(HostError::Protocol(format!(
                    "read source {:?} truncated={}",
                    read.source, read.truncated
                )));
            }
            Ok(read.text)
        }
        other => Err(unexpected(&other).into()),
    }
}

/// Once bytes may have left, every non-`ok` answer is Indeterminate.
pub(crate) fn mutation_result(
    call: &HerdrCall,
    outcome: HerdrOutcome,
    _pane_id: &str,
) -> Result<HostMutation, HostError> {
    match reply_result(call, outcome) {
        Ok(HerdrResult::Ok) => Ok(HostMutation::Confirmed),
        Err(Fault::Transport(HerdrTransportError::NotSent(message))) => {
            Err(HostError::Transport(message))
        }
        Err(Fault::Transport(HerdrTransportError::AfterWrite(message))) => {
            Ok(HostMutation::Indeterminate(message))
        }
        Err(Fault::Remote(body)) => Ok(HostMutation::Indeterminate(format!(
            "remote {}: {}",
            body.code, body.message
        ))),
        Err(Fault::Contract(detail)) => Ok(HostMutation::Indeterminate(detail)),
        Ok(other) => Ok(HostMutation::Indeterminate(format!(
            "unexpected result {other:?}"
        ))),
    }
}
