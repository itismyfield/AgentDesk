//! Session host boundary: where an interactive provider session lives (tmux
//! pane or child process) and the observation/input operations on it. Creation,
//! destruction, execution identity and cancel routing stay with their owners.

mod herdr {
    pub(crate) mod contract;
    pub(crate) mod model;
    pub(crate) mod observe;
    // Unix-socket only; no Windows transport exists.
    #[cfg(unix)]
    pub(crate) mod transport;
    pub(crate) mod wire;
}
mod consumer_guard;
mod herdr_host;
pub(crate) mod legacy_collapse;
mod model;
mod process_host;
mod resolve;
#[cfg(test)]
pub(crate) mod test_support;
mod tmux_host;
mod traits;

pub(crate) use model::{
    HostCapabilities, HostError, HostKind, HostKindResolution, HostKindSource, HostLiveness,
    HostMutation, HostPresence, HostRefusal, HostSessionRef, HostedRuntimeLocator,
};
pub(crate) use process_host::ProcessHost;
pub(crate) use resolve::{
    HostEvidence, HostWitness, SessionTargetEvidence, host_for, resolve_host_kind,
};
// The target resolver and guard have no production consumer yet.
pub(crate) use tmux_host::TmuxHost;
pub(crate) use traits::InteractiveSessionHost;
#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use {
    consumer_guard::{
        AutomaticEffect, DeferReason, GuardRefusal, GuardVerdict, PolicyProbe, StateChange,
        guard_first_state_change, probe_for_policy,
    },
    resolve::{
        ResolvedSessionTarget, SessionTargetEvidenceSource, SessionTargetInput, TargetHost,
        TargetSource, UnknownHost, resolve_session_target,
    },
};
