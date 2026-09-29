use crate::services::platform::tmux::{PaneLiveness, SessionPresence};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum HostKind {
    Tmux,
    Process,
    Herdr,
}

impl HostKind {
    /// Name written to disk (inflight locator, `.host_kind` marker).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Tmux => "tmux",
            Self::Process => "process",
            Self::Herdr => "herdr",
        }
    }

    /// Exact inverse of `as_str`; any other text is `None`, never a tmux default.
    pub(crate) fn from_persisted(value: &str) -> Option<Self> {
        match value {
            "tmux" => Some(Self::Tmux),
            "process" => Some(Self::Process),
            "herdr" => Some(Self::Herdr),
            _ => None,
        }
    }
}

/// Key a host finds a session by. The tmux name doubles as the process-registry
/// key; a Herdr ref holds only the pane id, its endpoint lives on the host.
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

    pub(crate) fn herdr_pane(pane_id: &'a str) -> Self {
        Self {
            kind: HostKind::Herdr,
            name: pane_id,
        }
    }

    /// The name for a tmux/process-registry lookup; a Herdr pane id is never one.
    pub(crate) fn legacy_name(self) -> Result<&'a str, HostError> {
        match self.kind {
            HostKind::Tmux | HostKind::Process => Ok(self.name),
            HostKind::Herdr => Err(HostError::Unsupported(HostKind::Herdr, "legacy_name")),
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
/// delivered; multi-step callers and socket hosts after a write produce it.
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
    /// Error body returned by a socket host; no exit status is invented.
    Remote {
        code: String,
        message: String,
    },
    /// Reply that breaks the typed contract (wrong id, tag, target, truncation).
    Protocol(String),
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

/// Where a session is hosted. It carries no execution or provider-source identity
/// (nonce, generation, provider session); those stay with their own owners.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostedRuntimeLocator {
    pub execution_node: Option<String>,
    pub host_kind: HostKind,
    pub host_session_id: String,
    pub pane: Option<String>,
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

    #[test]
    fn herdr_pane_ref_is_never_a_legacy_name() {
        let herdr = HostSessionRef::herdr_pane("AgentDesk-claude-x");
        assert_eq!(herdr.kind, HostKind::Herdr);
        assert_eq!(
            herdr.legacy_name(),
            Err(HostError::Unsupported(HostKind::Herdr, "legacy_name"))
        );
        assert_eq!(HostSessionRef::tmux("s").legacy_name(), Ok("s"));
        assert_eq!(HostSessionRef::process("s").legacy_name(), Ok("s"));
    }
}
