//! Session host boundary: where an interactive provider session lives (tmux
//! pane or child process) and the observation/input operations on it. Creation,
//! destruction, execution identity and cancel routing stay with their owners.

pub(crate) mod legacy_collapse;
mod model;
mod process_host;
mod resolve;
mod tmux_host;
mod traits;

pub(crate) use model::{
    HostCapabilities, HostError, HostKind, HostKindResolution, HostKindSource, HostLiveness,
    HostMutation, HostPresence, HostRefusal, HostSessionRef, HostedRuntimeLocator,
};
pub(crate) use process_host::ProcessHost;
pub(crate) use resolve::{HostEvidence, host_for, resolve_host_kind};
pub(crate) use tmux_host::TmuxHost;
pub(crate) use traits::InteractiveSessionHost;
