//! Session host boundary: where an interactive provider session lives and the observation, input,
//! create, terminate and restart-read effects on it. Admission, identity and cancel routing stay
//! with owners.

mod herdr {
    #[cfg(unix)]
    pub(crate) mod autostart;
    pub(crate) mod contract;
    #[cfg(unix)]
    pub(crate) mod launch_host;
    #[cfg(unix)]
    pub(crate) mod managed_config;
    pub(crate) mod model;
    pub(crate) mod observe;
    pub(crate) mod pane_probe;
    pub(crate) mod policy;
    pub(crate) mod provenance;
    // Unix-socket only; no Windows transport exists.
    #[cfg(unix)]
    pub(crate) mod transport;
    pub(crate) mod wire;
}
mod consumer_guard;
mod create;
pub(crate) use create::HostCreate;
#[cfg(any(test, feature = "test-support"))]
pub(crate) use create::create_in_effect;
mod herdr_gate;
mod herdr_host;
mod herdr_registry;
pub(crate) mod host_decision;
pub(crate) mod surface_policy;
pub(crate) mod thread_host;
#[cfg(all(any(test, feature = "test-support"), unix))]
pub(crate) use herdr_registry::HerdrRegistry;
#[cfg(any(test, feature = "test-support"))]
pub(crate) mod herdr_socket_rig_tests;
pub(crate) mod legacy_collapse;
mod model;
pub(crate) mod operation_policy;
mod process_host;
// Only unix keeps a Herdr execution to read again after a restart.
#[cfg(unix)]
mod recover;
#[cfg(all(any(test, feature = "test-support"), unix))]
pub(crate) use recover::{
    HerdrCurrentExecution, HerdrMarkerEvidence, compare_herdr_execution, recover_in_effect,
};
#[cfg(unix)]
pub(crate) use recover::{
    HerdrEndpointId, HerdrExecutionMatch, HerdrExecutionReader, HerdrMismatch, HerdrPaneEvidence,
    HerdrPaneReading, HerdrUnknown, HostRecover, HostRecovery, NoHerdrEndpoint, SocketHerdrReader,
};
mod resolve;
mod session_record;
mod submit;
#[cfg(test)]
pub(crate) use submit::SubmitSpy;
#[cfg(unix)]
pub(crate) use submit::WrapperFrame;
mod termination;
#[cfg(any(test, feature = "test-support"))]
pub(crate) mod test_support;
mod tmux_host;
mod traits;
// Test seams: the Herdr internals the session-host tests reach from their upper-crate hosts.
#[cfg(all(any(test, feature = "test-support"), unix))]
pub(crate) mod herdr_for_test {
    pub(crate) use super::herdr::*;
}
#[cfg(all(any(test, feature = "test-support"), unix))]
pub(crate) mod herdr_gate_for_test {
    pub(crate) use super::herdr_gate::*;
}
#[cfg(all(any(test, feature = "test-support"), unix))]
pub(crate) mod herdr_registry_for_test {
    pub(crate) use super::herdr_registry::*;
}
#[cfg(all(any(test, feature = "test-support"), unix))]
pub(crate) use herdr_host::HerdrHost;

pub(crate) use herdr::contract::ServerWitness;
// Dedicated Herdr config upkeep: the boot pass, the 30-second task, doctor and health.
#[cfg(unix)]
pub(crate) use herdr::autostart;
#[cfg(unix)]
pub(crate) use herdr::managed_config;
#[cfg(all(any(test, feature = "test-support"), unix))]
pub(crate) use herdr::model::HerdrEndpoint;
pub(crate) use herdr::policy;
pub(crate) use herdr_gate::legacy_return;
#[cfg(test)]
pub(crate) use herdr_gate::shut_execution_for_test;
pub(crate) use herdr_gate::{execution_closing, restore_execution_fence, try_notice_fence};
// The unix Herdr turn executor, behind its off-by-default switch, builds targets here.
#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use herdr_gate::{
    HerdrGateRefusal, HerdrPaneView, HerdrTarget, Mutation as HerdrMutation, PaneProvider,
    PaneReading, TerminationFence,
};
#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use herdr_registry::registry as herdr_endpoints;
// Dormant: activation constructs it for a configured endpoint.
#[cfg(unix)]
#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use herdr::launch_host::SocketHerdrLaunchHost;
#[cfg(unix)]
pub(crate) use herdr::launch_host::{GrantedServer, ReplacedServer};
pub(crate) use herdr::observe::{RESTORE_RESUME_NOT_OFF, RestoreResume, RestoreUnverified};
pub(crate) use herdr::pane_probe::{EvidenceGap, PaneOccupant, START_SLACK};
#[cfg(unix)]
pub(crate) use herdr::provenance::{StartIdentity, boot_time};
pub(crate) use model::{
    HostCapabilities, HostError, HostKey, HostKind, HostKindResolution, HostKindSource,
    HostLiveness, HostMutation, HostPresence, HostRefusal, HostSessionRef, HostedRuntimeLocator,
};
pub(crate) use process_host::ProcessHost;
pub(crate) use resolve::{
    HostEvidence, HostWitness, SessionTargetEvidence, host_for, resolve_host_kind,
};
pub(crate) use submit::{
    AuthorizedHostTarget, InputSource, KeyOutcome, PinnedInput, SubmitOutcome, SubmitPayload,
};
pub(crate) use tmux_host::{TmuxHost, tmux_key_name};
pub(crate) use traits::InteractiveSessionHost;
// Only the keyed teardown gate consumes the resolver and guard so far.
#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use {
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
