//! Compatibility vocabulary for relay verdict composition.
//! The retired external input contributes `Unknown`, never health evidence.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum ExternalRelayVerdict {
    /// The tier said nothing usable — §-1.5 ①. Composition must leave the
    /// in-band verdict exactly as it was.
    Unknown,
    /// Compatibility no-loss claim, not positive health evidence.
    NoLoss,
    /// Compatibility claim that unmatched blocks are lagging.
    Lagging { lost_blocks: u32 },
    /// Compatibility claim that unmatched blocks exceeded the gap bound.
    Unreachable { lost_blocks: u32 },
}
