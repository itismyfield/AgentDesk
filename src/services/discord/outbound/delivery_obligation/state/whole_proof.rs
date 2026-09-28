use std::collections::BTreeSet;

use super::super::schema::{CleanupState, ObligationLedger};
use super::proof_input::{ProbedAttempt, ProofRejection};
use super::{Evidence, EvidenceSnapshot, WholeAttemptProof};

impl EvidenceSnapshot {
    /// Validate captured probe facts; settlement must recheck the publication under its lock.
    pub(in crate::services::discord) fn verify_whole(
        ledger: &ObligationLedger,
        probe: ProbedAttempt,
    ) -> Result<Self, ProofRejection> {
        use ProofRejection::*;
        if probe.publication.epoch != ledger.publication.epoch {
            return Err(EpochMismatch);
        }
        if probe.publication.rev != ledger.publication.rev {
            return Err(RevisionMismatch);
        }
        if probe.publication != ledger.publication {
            return Err(PublicationMismatch);
        }
        let attempt = ledger
            .open
            .iter()
            .filter_map(|open| open.attempt.as_ref())
            .find(|attempt| !probe.attempt_key.is_empty() && attempt.key == probe.attempt_key)
            .ok_or(AttemptMismatch)?;
        if ledger
            .open
            .iter()
            .filter_map(|open| open.attempt.as_ref())
            .any(|other| {
                other.key == attempt.key
                    && (other.range != attempt.range
                        || other.chunk_total != attempt.chunk_total
                        || other.chunk_nonces != attempt.chunk_nonces)
            })
        {
            return Err(InvalidPlan);
        }
        let (Some(total), Some(nonces)) = (attempt.chunk_total, &attempt.chunk_nonces) else {
            return Err(InvalidPlan);
        };
        let unique: BTreeSet<_> = nonces.iter().collect();
        if total == 0
            || nonces.len() != total as usize
            || unique.len() != nonces.len()
            || nonces.iter().any(String::is_empty)
            || attempt.range.0 >= attempt.range.1
            || attempt.range.1 > ledger.publication.extent_end
        {
            return Err(InvalidPlan);
        }
        if probe.chunks.is_empty() {
            return Err(EmptyProof);
        }
        let mut seen = BTreeSet::new();
        let mut messages = BTreeSet::new();
        let mut receipts = Vec::with_capacity(probe.chunks.len());
        for chunk in probe.chunks {
            let receipt = chunk.receipt;
            if receipt.chunk >= total {
                return Err(ChunkOutOfRange);
            }
            if !seen.insert(receipt.chunk) {
                return Err(DuplicateChunk);
            }
            if chunk.nonce != nonces[receipt.chunk as usize] {
                return Err(NonceMismatch);
            }
            if receipt.message_id == 0 || !messages.insert(receipt.message_id) {
                return Err(InvalidMessageId);
            }
            if receipt.cleanup != CleanupState::NotRemoved {
                return Err(CleanupUnproven);
            }
            receipts.push(receipt);
        }
        if seen.len() != total as usize {
            return Err(MissingChunk);
        }
        receipts.sort_by_key(|receipt| receipt.chunk);
        let proof = WholeAttemptProof {
            publication: probe.publication,
            attempt_key: probe.attempt_key.clone(),
            range: attempt.range,
            receipts,
        };
        Ok(Self {
            publication: probe.publication,
            attempt_key: probe.attempt_key,
            evidence: Evidence::WholeProof(proof),
        })
    }
}
