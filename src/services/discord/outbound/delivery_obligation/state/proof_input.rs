use super::super::schema::{ChunkReceipt, Publication};

#[derive(Debug, Clone)]
pub(in crate::services::discord) struct ProbedAttempt {
    pub publication: Publication,
    pub attempt_key: String,
    pub chunks: Vec<ProbedChunk>,
}

#[derive(Debug, Clone)]
pub(in crate::services::discord) struct ProbedChunk {
    pub nonce: String,
    pub receipt: ChunkReceipt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum ProofRejection {
    EpochMismatch,
    RevisionMismatch,
    PublicationMismatch,
    AttemptMismatch,
    InvalidPlan,
    EmptyProof,
    DuplicateChunk,
    MissingChunk,
    ChunkOutOfRange,
    NonceMismatch,
    CleanupUnproven,
    InvalidMessageId,
}
