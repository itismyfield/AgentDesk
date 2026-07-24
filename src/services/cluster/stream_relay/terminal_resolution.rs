//! Typed terminal-delivery resolutions shared by relay sinks and ACK consumers.

/// The exact, per-frame terminal resolution retained by the relay ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryOutcome {
    Delivered,
    /// A fresh-message transport was confirmed. The optional frontier and
    /// persistence bit describe post-transport authority; neither revokes the
    /// transport confirmation or authorizes an immediate duplicate POST.
    FreshDelivered {
        committed_to: Option<u64>,
        persistence_recorded: bool,
    },
    NotDelivered,
    Unknown,
}

/// Result of accepting a relay frame into a sink.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelaySinkOutcome {
    FrameAccepted,
    TerminalDelivered,
    /// A confirmed fresh-message POST kept outside the sink-error taxonomy so
    /// the exact-sequence ACK can suppress a same-process fallback POST.
    TerminalFreshDelivered {
        committed_to: Option<u64>,
        persistence_recorded: bool,
    },
    TerminalNotDelivered,
    #[allow(dead_code)]
    TerminalUnknown,
}

impl RelaySinkOutcome {
    pub fn terminal_delivered(self) -> bool {
        matches!(self, Self::TerminalDelivered)
    }

    pub fn terminal_fresh_delivered(self) -> Option<(Option<u64>, bool)> {
        match self {
            Self::TerminalFreshDelivered {
                committed_to,
                persistence_recorded,
            } => Some((committed_to, persistence_recorded)),
            _ => None,
        }
    }

    pub fn terminal_not_delivered(self) -> bool {
        matches!(self, Self::TerminalNotDelivered)
    }

    pub fn terminal_unknown(self) -> bool {
        matches!(self, Self::TerminalUnknown)
    }
}
