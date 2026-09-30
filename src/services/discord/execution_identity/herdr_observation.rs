//! Stored-versus-current comparison for a Herdr-hosted execution. The stored record
//! is the expectation; a current observation is only ever compared against it.
#![cfg_attr(not(test), allow(dead_code))]

use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedLocation, ProcessStamp,
};
use crate::services::session_host::HostKind;
use crate::services::tmux_common::host_marker::HostKindMarker;

/// What a reader saw for the pane now. `None` means the value could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HerdrCurrentExecution {
    pub location: HostedLocation,
    pub binding_nonce: Option<String>,
    pub root: Option<ProcessStamp>,
    pub provider_process: Option<ProcessStamp>,
    pub marker: HostKindMarker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HerdrUnknown {
    NoStoredEvidence,
    EndpointChanged,
    MarkerLost,
    NotObserved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HerdrMismatch {
    OtherPane,
    OtherHostMarker,
    OtherNonce,
    RootReplaced,
    ProviderReplaced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HerdrExecutionMatch {
    Match,
    Mismatch(HerdrMismatch),
    Unknown(HerdrUnknown),
}

/// A root or provider process is the same only when pid and start both agree.
fn stamp(stored: &ProcessStamp, current: Option<&ProcessStamp>) -> Option<bool> {
    current.map(|current| current == stored)
}

/// Match needs every stored field confirmed; an unreadable value is Unknown, never Mismatch.
pub(crate) fn compare_herdr_execution(
    stored: &HostedExecution,
    current: &HerdrCurrentExecution,
) -> HerdrExecutionMatch {
    use HerdrExecutionMatch::{Match, Mismatch, Unknown};
    let (Some(location), Some(expected)) = (&stored.location, &stored.expected) else {
        return Unknown(HerdrUnknown::NoStoredEvidence);
    };
    let now = &current.location;
    let same_endpoint = (&location.host, &location.execution_node)
        == (&now.host, &now.execution_node)
        && (&location.endpoint_config_key, &location.socket_addr)
            == (&now.endpoint_config_key, &now.socket_addr)
        && location.named_session == now.named_session;
    if !same_endpoint {
        return Unknown(HerdrUnknown::EndpointChanged);
    }
    if location.pane_id != now.pane_id {
        return Mismatch(HerdrMismatch::OtherPane);
    }
    match &current.marker {
        HostKindMarker::Known(HostKind::Herdr) => {}
        HostKindMarker::Known(_) => return Mismatch(HerdrMismatch::OtherHostMarker),
        _ => return Unknown(HerdrUnknown::MarkerLost),
    }
    let checks = [
        (
            current
                .binding_nonce
                .as_deref()
                .map(|nonce| nonce == stored.execution_nonce),
            HerdrMismatch::OtherNonce,
        ),
        (
            stamp(&expected.root, current.root.as_ref()),
            HerdrMismatch::RootReplaced,
        ),
        (
            stamp(
                &expected.provider_process,
                current.provider_process.as_ref(),
            ),
            HerdrMismatch::ProviderReplaced,
        ),
    ];
    if let Some((_, mismatch)) = checks.iter().find(|(same, _)| *same == Some(false)) {
        return Mismatch(*mismatch);
    }
    if checks.iter().any(|(same, _)| same.is_none()) {
        return Unknown(HerdrUnknown::NotObserved);
    }
    Match
}

#[cfg(test)]
#[path = "herdr_observation_tests.rs"]
mod tests;
