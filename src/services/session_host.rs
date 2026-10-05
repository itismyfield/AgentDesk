//! Session host boundary: where an interactive provider session lives (tmux
//! pane or child process) and the observation/input operations on it. Creation,
//! destruction, execution identity and cancel routing stay with their owners.

mod herdr {
    pub(crate) mod contract;
    #[cfg(unix)]
    pub(crate) mod launch_host;
    pub(crate) mod model;
    pub(crate) mod observe;
    pub(crate) mod pane_probe;
    pub(crate) mod provenance;
    // Unix-socket only; no Windows transport exists.
    #[cfg(unix)]
    pub(crate) mod transport;
    pub(crate) mod wire;
}
mod consumer_guard;
mod herdr_gate;
mod herdr_host;
mod herdr_registry;
#[cfg(test)]
pub(crate) mod herdr_socket_rig_tests;
pub(crate) mod legacy_collapse;
mod model;
mod process_host;
mod resolve;
mod session_record;
#[cfg(test)]
pub(crate) mod test_support;
mod tmux_host;
mod traits;

pub(crate) use herdr::contract::ServerWitness;
// Dormant: the Herdr turn executor builds targets from the boot registry.
#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use herdr_gate::{HerdrGateRefusal, HerdrTarget, Mutation as HerdrMutation};
#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use herdr_registry::registry as herdr_endpoints;
// Dormant: activation constructs it for a configured endpoint.
#[cfg(unix)]
#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use herdr::launch_host::SocketHerdrLaunchHost;
pub(crate) use herdr::observe::{RESTORE_RESUME_NOT_OFF, RestoreResume, RestoreUnverified};
pub(crate) use herdr::pane_probe::EvidenceGap;
pub(crate) use model::{
    HostCapabilities, HostError, HostKey, HostKind, HostKindResolution, HostKindSource,
    HostLiveness, HostMutation, HostPresence, HostRefusal, HostSessionRef, HostedRuntimeLocator,
};
pub(crate) use process_host::ProcessHost;
pub(crate) use resolve::{
    HostEvidence, HostWitness, SessionTargetEvidence, host_for, resolve_host_kind,
};
pub(crate) use tmux_host::{TmuxHost, tmux_key_name};
pub(crate) use traits::InteractiveSessionHost;
// Only the keyed teardown gate consumes the resolver and guard so far.
#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use {
    consumer_guard::{
        AutomaticEffect, ClearedHostSession, DeferReason, GuardRefusal, GuardVerdict, PolicyProbe,
        StateChange, clear_legacy_session, guard_first_state_change, probe_for_policy,
    },
    resolve::{
        ResolvedSessionTarget, SessionTargetEvidenceSource, SessionTargetInput, TargetHost,
        TargetSource, UnknownHost, resolve_session_target,
    },
    session_record::session_record_witness,
};
