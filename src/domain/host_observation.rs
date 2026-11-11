//! Host kinds and marker observations; filesystem reads stay with their adapters.

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostKindMarker {
    Absent,
    Known(HostKind),
    /// Present but empty, truncated or naming a host this binary does not know.
    Unrecognized(String),
    /// Present but unreadable; never reported as absent.
    ReadFailed(String),
}
