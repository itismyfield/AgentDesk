//! Each decision names the effect it permits; metadata never grants consumption.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EvidenceReason {
    MissingEvidence,
    IdentityMismatch,
    SourceChanged,
    PrefixChanged,
    RangeGap,
    ForeignTurn,
    IncompleteRecord,
    FiniteCloseExceeded,
    Cancelled,
    Denied,
    PermissionUnknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PersistenceReason {
    MissingEvidence,
    IdentityMismatch,
    SourceChanged,
    PrefixChanged,
    ActorUnavailable,
    ActorStale,
    WrongReply,
    StorageFailed,
    ReadBackMismatch,
    Denied,
}

impl PersistenceReason {
    pub(crate) fn from_store(error: &crate::services::tui_o::store::StoreError) -> Self {
        use crate::services::tui_o::store::StoreError;
        match error {
            StoreError::Rejected(_) => Self::IdentityMismatch,
            StoreError::Io(_) | StoreError::Halt(_) | StoreError::SpoolFull => Self::StorageFailed,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PublicationReason {
    Evidence(EvidenceReason),
    Persistence(PersistenceReason),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CheckpointReason {
    Evidence(EvidenceReason),
    FloorAheadOfCursor,
    NoProcessedWitness,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RestoreReason {
    Evidence(EvidenceReason),
    Persistence(PersistenceReason),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LiveReadReason {
    Evidence(EvidenceReason),
    Persistence(PersistenceReason),
    TemporarilyUnavailable,
}
impl From<EvidenceReason> for LiveReadReason {
    fn from(reason: EvidenceReason) -> Self {
        Self::Evidence(reason)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HistoricalReason {
    Evidence(EvidenceReason),
    Persistence(PersistenceReason),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PostReason {
    Evidence(EvidenceReason),
    MissingExactPermit,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PublicationDecision<T> {
    MetadataPublished(T),
    Unavailable(PublicationReason),
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CheckpointDecision<T> {
    Ready(T),
    MetadataOnly(CheckpointReason),
    Unavailable(RestoreReason),
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LiveReadDecision<T> {
    Granted(T),
    Held(LiveReadReason),
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HistoricalDecision<T> {
    Readable(T),
    Unavailable(HistoricalReason),
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PostDecision<T> {
    Permitted(T),
    Held(PostReason),
}
