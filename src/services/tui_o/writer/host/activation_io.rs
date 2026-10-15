//! What a first activation reads from the gateway: fenced intake facts and Legacy's local custody.

use crate::services::tui_o::writer::activation::ActivationFacts;

/// The facts and queued Legacy bodies read while intake writes are fenced.
pub struct FencedFacts {
    pub hold: crate::db::o_channel_activation::IntakeFence,
    pub facts: ActivationFacts,
    pub queued_bodies: i64,
}

/// Legacy's local hold on a channel as the gateway reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Custody {
    Free,
    /// Only an inflight row, which outlives its turn when Legacy never clears it.
    Row,
    /// A pending start naming Legacy's current turn, with no terminal delivery held.
    PendingStart,
    /// A terminal delivery Legacy still owns, or one unreadable beside a pending start.
    Active,
}

impl Custody {
    /// A pending start or a terminal delivery; strict callers read both as the former `Active`.
    pub fn active(self) -> bool {
        matches!(self, Self::PendingStart | Self::Active)
    }

    /// A Codex takeover leaves Legacy its turn identity, so only a held terminal delivery stops it.
    pub fn blocks_takeover(self) -> bool {
        self == Self::Active
    }
}
