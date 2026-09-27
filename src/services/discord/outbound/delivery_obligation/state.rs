use super::schema::{ChunkReceipt, ExactRange, Publication, SourceToken};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum SourceIdentityState {
    LegacyUnbound,
    BoundAndCurrent(SourceToken),
    BoundButChanged(SourceToken),
    SourceUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum SourceObs {
    Unavailable,
    Available {
        token: SourceToken,
        size: u64,
        publication: Option<Publication>,
    },
}

/// Advance under the coord mutex on safety-state changes and successful U publications.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::services::discord) struct URevision(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum HandoffState {
    Admitted { intent_written: bool },
    Issuing,
    Settled,
    Released,
}

/// Subordinate state owned by an existing delivery lease guard.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct FlightHandoff {
    pub token: SourceToken,
    pub range: ExactRange,
    pub attempt_key: Option<String>,
    pub ledger_was_empty: bool,
    pub transport_nonce: Option<String>,
    pub state: HandoffState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum TransportOutcome {
    NotIssued,
    FirstRejected,
    MaybePosted { receipts: Vec<ChunkReceipt> },
    Confirmed { receipts: Vec<ChunkReceipt> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct WholeAttemptProof {
    publication: Publication,
    attempt_key: String,
    range: ExactRange,
    receipts: Vec<ChunkReceipt>,
}

/// WholeProof requires every planned chunk and no cleanup, then a publication recheck.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum Evidence {
    WholeProof(WholeAttemptProof),
    ChunkProof { receipts: Vec<ChunkReceipt> },
    Ambiguous,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct EvidenceSnapshot {
    publication: Publication,
    attempt_key: String,
    evidence: Evidence,
}

/// Protocol one retains ambiguous errors, including HTTP rejections, as Unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum SettlementOutcome {
    Confirmed,
    Unknown,
    Withdrawn,
    LandedStale,
    LandedUnrecorded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum BlockedReason {
    Unavailable,
    Corrupt,
    Incompatible,
    UnknownSchema,
    UnknownProtocol,
    IdentityIncomplete,
    SourceUnavailable,
    PublicationMismatch,
}
