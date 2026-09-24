use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::platform::tmux::{PaneLiveness, SessionPresence};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum HostKind {
    Tmux,
    Process,
}

/// Key a host finds a session by. The tmux session name doubles as the
/// process-registry key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HostSessionRef<'a> {
    pub kind: HostKind,
    pub name: &'a str,
}

impl<'a> HostSessionRef<'a> {
    pub(crate) fn tmux(name: &'a str) -> Self {
        Self {
            kind: HostKind::Tmux,
            name,
        }
    }

    pub(crate) fn process(name: &'a str) -> Self {
        Self {
            kind: HostKind::Process,
            name,
        }
    }
}

/// One-to-one with `platform::tmux::SessionPresence`; no reverse mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostPresence {
    Present,
    Missing,
    ProbeFailed,
}

/// One-to-one with `platform::tmux::PaneLiveness`; no reverse mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostLiveness {
    Live,
    DeadOrAbsent,
    ProbeError,
}

impl From<SessionPresence> for HostPresence {
    fn from(value: SessionPresence) -> Self {
        match value {
            SessionPresence::Present => Self::Present,
            SessionPresence::Missing => Self::Missing,
            SessionPresence::ProbeFailed => Self::ProbeFailed,
        }
    }
}

impl From<PaneLiveness> for HostLiveness {
    fn from(value: PaneLiveness) -> Self {
        match value {
            PaneLiveness::Live => Self::Live,
            PaneLiveness::DeadOrAbsent => Self::DeadOrAbsent,
            PaneLiveness::ProbeError => Self::ProbeError,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostRefusal {
    Unsupported { kind: HostKind, op: &'static str },
    TargetMissing,
    Precondition(String),
}

/// Outcome of a mutating call. `Indeterminate` means some input may have been
/// delivered; hosts never produce it themselves, multi-step callers do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostMutation {
    Confirmed,
    Refused(HostRefusal),
    Indeterminate(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostError {
    Transport(String),
    Timeout,
    Unsupported(HostKind, &'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct HostCapabilities {
    pub send_text: bool,
    pub send_keys: bool,
    pub interrupt: bool,
    pub capture_screen: bool,
    pub current_working_dir: bool,
    pub execution_pid: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostKindSource {
    DurableRuntimeKind,
    RuntimeKindMarker,
    ProcessRegistry,
}

/// `Unknown` and `Conflict` must never admit a destroy, recreate or finalize path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostKindResolution {
    Known {
        kind: HostKind,
        source: HostKindSource,
    },
    Unknown,
    Conflict {
        first: (HostKind, HostKindSource),
        second: (HostKind, HostKindSource),
    },
}

/// In-memory only; it carries no execution identity (nonce or generation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostedRuntimeLocator {
    pub execution_node: Option<String>,
    pub host_kind: HostKind,
    pub host_session_id: String,
    pub pane: Option<String>,
    pub runtime_kind: Option<RuntimeHandoffKind>,
    pub provider_session_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_maps_one_to_one_with_session_presence() {
        for (platform, host) in [
            (SessionPresence::Present, HostPresence::Present),
            (SessionPresence::Missing, HostPresence::Missing),
            (SessionPresence::ProbeFailed, HostPresence::ProbeFailed),
        ] {
            assert_eq!(HostPresence::from(platform), host);
        }
    }

    #[test]
    fn liveness_maps_one_to_one_with_pane_liveness() {
        for (platform, host) in [
            (PaneLiveness::Live, HostLiveness::Live),
            (PaneLiveness::DeadOrAbsent, HostLiveness::DeadOrAbsent),
            (PaneLiveness::ProbeError, HostLiveness::ProbeError),
        ] {
            assert_eq!(HostLiveness::from(platform), host);
        }
    }

    #[test]
    fn session_ref_constructors_tag_the_host_kind() {
        assert_eq!(
            HostSessionRef::tmux("s"),
            HostSessionRef {
                kind: HostKind::Tmux,
                name: "s"
            }
        );
        assert_eq!(HostSessionRef::process("s").kind, HostKind::Process);
    }
}
