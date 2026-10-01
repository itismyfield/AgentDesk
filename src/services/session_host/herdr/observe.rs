//! Read-side policy: the ping/pong handshake check, bounded read-only retries, the rule
//! that one observation never spans two connections, and the E7 restore-resume reading.
#![cfg_attr(not(test), allow(dead_code))]

use std::thread;
use std::time::{Duration, Instant};

use super::contract::HerdrOutcome;
use super::model::{
    ExecutionState, HERDR_PROTOCOL, HerdrCall, HerdrObservation, HerdrRequest, HerdrResult,
};
use crate::services::session_host::model::HostError;

pub(crate) const RESTORE_RESUME_NOT_OFF: &str = "restore_resume_not_off";

/// The running server's effective `[session] resume_agents_on_restore`. Only a read-only
/// answer from that server, on one connection, can say `Off`; a default or a file cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestoreResume {
    Off {
        generation: u64,
    },
    On,
    /// No read, or an answer that does not state exactly `false`.
    Unverified,
}

impl RestoreResume {
    /// The connection a create or input may use; anything but `Off` admits none.
    pub(crate) fn admitted_generation(self) -> Option<u64> {
        match self {
            Self::Off { generation } => Some(generation),
            Self::On | Self::Unverified => None,
        }
    }
}

/// Protocol 22 has no read of the server's effective settings (`server.reload_config`
/// only reloads), so no server is verified and Herdr create and input stay refused.
pub(crate) fn read_restore_resume<T: ?Sized>(_transport: &T) -> RestoreResume {
    RestoreResume::Unverified
}

/// What a verified pong reported. The version string is informational only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HerdrHello {
    pub version: String,
    pub protocol: u32,
}

/// A pong must echo the ping id and speak our protocol; the version may differ.
pub(crate) fn hello_result(
    call: &HerdrCall,
    outcome: HerdrOutcome,
) -> Result<HerdrHello, HostError> {
    if call.request != (HerdrRequest::Ping {}) {
        return Err(HostError::Protocol("handshake without ping".to_string()));
    }
    let reply = outcome.map_err(|error| HostError::Transport(format!("{error:?}")))?;
    if reply.id != call.id {
        return Err(HostError::Protocol(format!(
            "pong id {} for ping {}",
            reply.id, call.id
        )));
    }
    match reply.body {
        Ok(HerdrResult::Pong { version, protocol }) if protocol == HERDR_PROTOCOL => {
            Ok(HerdrHello { version, protocol })
        }
        Ok(HerdrResult::Pong { protocol, .. }) => Err(HostError::Protocol(format!(
            "herdr protocol {protocol}, expected {HERDR_PROTOCOL}"
        ))),
        Ok(other) => Err(HostError::Protocol(format!("ping answered with {other:?}"))),
        Err(body) => Err(HostError::Remote {
            code: body.code,
            message: body.message,
        }),
    }
}

/// Repeats a read-only attempt until it gets any reply or `deadline` passes.
pub(crate) fn retry_read(
    deadline: Instant,
    backoff: Duration,
    mut attempt: impl FnMut() -> (HerdrOutcome, u64),
) -> (HerdrOutcome, u64) {
    loop {
        let (outcome, generation) = attempt();
        if outcome.is_ok() || Instant::now() + backoff >= deadline {
            return (outcome, generation);
        }
        thread::sleep(backoff);
    }
}

/// Execution evidence read on another connection than the snapshot is dropped.
pub(crate) fn fence_generation(
    observation: HerdrObservation,
    snapshot_generation: u64,
    process_generation: u64,
) -> HerdrObservation {
    if snapshot_generation == process_generation {
        return observation;
    }
    HerdrObservation {
        execution: ExecutionState::Unknown,
        shell_pid: None,
        ..observation
    }
}
