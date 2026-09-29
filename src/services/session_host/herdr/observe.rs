//! Read-side policy: the ping/pong handshake check, bounded read-only
//! retries, and the rule that one observation never spans two connections.
#![cfg_attr(not(test), allow(dead_code))]

use std::thread;
use std::time::{Duration, Instant};

use super::contract::HerdrOutcome;
use super::model::{
    ExecutionState, HERDR_PROTOCOL, HerdrCall, HerdrObservation, HerdrRequest, HerdrResult,
};
use crate::services::session_host::model::HostError;

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
