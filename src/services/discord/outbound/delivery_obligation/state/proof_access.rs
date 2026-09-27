use super::super::schema::{ChunkReceipt, ExactRange, Publication};
use super::{Evidence, EvidenceSnapshot, WholeAttemptProof};

impl EvidenceSnapshot {
    pub(in crate::services::discord) fn publication(&self) -> Publication {
        self.publication
    }
    pub(in crate::services::discord) fn attempt_key(&self) -> &str {
        &self.attempt_key
    }
    pub(in crate::services::discord) fn evidence(&self) -> &Evidence {
        &self.evidence
    }
}

impl WholeAttemptProof {
    pub(in crate::services::discord) fn publication(&self) -> Publication {
        self.publication
    }
    pub(in crate::services::discord) fn attempt_key(&self) -> &str {
        &self.attempt_key
    }
    pub(in crate::services::discord) fn range(&self) -> ExactRange {
        self.range
    }
    pub(in crate::services::discord) fn receipts(&self) -> &[ChunkReceipt] {
        &self.receipts
    }
}
