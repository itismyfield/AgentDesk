//! Session host boundary: where an interactive provider session lives and the observation, input,
//! create, terminate and restart-read effects on it. Admission, identity and cancel routing stay
//! with owners.

pub mod herdr {
    #[cfg(unix)]
    pub mod autostart;
    pub mod contract;
    #[cfg(unix)]
    pub mod launch_host;
    #[cfg(unix)]
    pub mod managed_config;
    pub mod model;
    pub mod observe;
    pub mod pane_probe;
    pub mod policy;
    pub mod provenance;
    // Unix-socket only; no Windows transport exists.
    #[cfg(unix)]
    pub mod transport;
    pub mod wire;
}
mod consumer_guard;
mod create;
pub use create::HostCreate;
#[cfg(any(test, feature = "test-support"))]
pub use create::create_in_effect;
pub mod herdr_gate;
pub mod herdr_host;
pub mod herdr_registry;
pub mod host_decision;
pub mod surface_policy;
pub mod thread_host;
#[cfg(all(any(test, feature = "test-support"), unix))]
pub use herdr_registry::HerdrRegistry;
#[cfg(any(test, feature = "test-support"))]
pub mod herdr_socket_rig_tests;
pub mod legacy_collapse;
pub mod model;
pub mod operation_policy;
mod process_host;
// Only unix keeps a Herdr execution to read again after a restart.
#[cfg(unix)]
mod recover;
#[cfg(all(any(test, feature = "test-support"), unix))]
pub use recover::{
    HerdrCurrentExecution, HerdrMarkerEvidence, compare_herdr_execution, recover_in_effect,
};
#[cfg(unix)]
pub use recover::{
    HerdrEndpointId, HerdrExecutionMatch, HerdrExecutionReader, HerdrMismatch, HerdrPaneEvidence,
    HerdrPaneReading, HerdrUnknown, HostRecover, HostRecovery, NoHerdrEndpoint, SocketHerdrReader,
};
mod resolve;
mod session_record;
mod submit;
#[cfg(test)]
pub(crate) use submit::SubmitSpy;
#[cfg(unix)]
pub use submit::WrapperFrame;
mod termination;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
mod tmux_host;
pub mod traits;
// Test seams: the Herdr internals the session-host tests reach from their upper-crate hosts.
#[cfg(all(any(test, feature = "test-support"), unix))]
pub mod herdr_for_test {
    pub use super::herdr::{
        contract, launch_host, managed_config, model, observe, pane_probe, policy, provenance,
        transport, wire,
    };
}
#[cfg(all(any(test, feature = "test-support"), unix))]
pub mod herdr_gate_for_test {
    pub use super::herdr_gate::*;
}
#[cfg(all(any(test, feature = "test-support"), unix))]
pub mod herdr_registry_for_test {
    pub use super::herdr_registry::*;
}
#[cfg(all(any(test, feature = "test-support"), unix))]
pub use herdr_host::HerdrHost;

pub use herdr::contract::ServerWitness;
// Dedicated Herdr config upkeep: the boot pass, the 30-second task, doctor and health.
#[cfg(unix)]
pub use herdr::autostart;
#[cfg(unix)]
pub use herdr::managed_config;
#[cfg(all(any(test, feature = "test-support"), unix))]
pub use herdr::model::HerdrEndpoint;
pub use herdr::policy;
pub use herdr_gate::legacy_return;
#[cfg(any(test, feature = "test-support"))]
pub use herdr_gate::shut_execution_for_test;
pub use herdr_gate::{execution_closing, restore_execution_fence, try_notice_fence};
// The unix Herdr turn executor, behind its off-by-default switch, builds targets here.
#[cfg_attr(not(test), allow(unused_imports))]
pub use herdr_gate::{
    HerdrGateRefusal, HerdrPaneView, HerdrTarget, Mutation as HerdrMutation, PaneProvider,
    PaneReading, TerminationFence,
};
#[cfg_attr(not(test), allow(unused_imports))]
pub use herdr_registry::registry as herdr_endpoints;
// Dormant: activation constructs it for a configured endpoint.
#[cfg(unix)]
#[cfg_attr(not(test), allow(unused_imports))]
pub use herdr::launch_host::SocketHerdrLaunchHost;
#[cfg(unix)]
pub use herdr::launch_host::{GrantedServer, ReplacedServer};
pub use herdr::observe::{RESTORE_RESUME_NOT_OFF, RestoreResume, RestoreUnverified};
pub use herdr::pane_probe::{EvidenceGap, PaneOccupant, START_SLACK};
#[cfg(unix)]
pub use herdr::provenance::{StartIdentity, boot_time};
pub use model::{
    HostCapabilities, HostError, HostKey, HostKind, HostKindResolution, HostKindSource,
    HostLiveness, HostMutation, HostPresence, HostRefusal, HostSessionRef, HostedRuntimeLocator,
};
pub(crate) use process_host::ProcessHost;
pub use resolve::{HostEvidence, HostWitness, SessionTargetEvidence, host_for, resolve_host_kind};
pub use submit::{
    AuthorizedHostTarget, InputSource, KeyOutcome, PinnedInput, SubmitOutcome, SubmitPayload,
};
pub use tmux_host::{TmuxHost, tmux_key_name};
pub use traits::InteractiveSessionHost;
// Only the keyed teardown gate consumes the resolver and guard so far.
#[cfg_attr(not(test), allow(unused_imports))]
pub use {
    consumer_guard::{
        AutomaticEffect, ChannelTeardown, ClearedHostSession, DeferReason, GuardRefusal,
        GuardVerdict, PolicyProbe, StateChange, clear_legacy_session, guard_first_state_change,
        probe_for_policy,
    },
    resolve::{
        ResolvedSessionTarget, SessionTargetEvidenceSource, SessionTargetInput, TargetHost,
        TargetSource, UnknownHost, resolve_session_target,
    },
    session_record::session_record_witness,
};
