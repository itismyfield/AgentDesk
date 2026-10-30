use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::adk_workspace::WorkspaceObservation;
use super::evidence::{EvidenceEnvelope, EvidencePayload, EvidenceSource, TuiPayload};
use super::model::{AttemptBinding, Repository, Role};
use super::profile::{Candidate, ServerResolvedSubject};
use super::verification_run::{
    BoundSubject, Digest, VerificationRun, VerificationSubjectRef, digest_of,
};

// One immutable seal per Work attempt: the produced output, never a rewrite of the input binding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateSeal {
    pub seal_id: String,
    pub owner_attempt: String,
    pub binding_digest: Digest,
    pub repository: Repository,
    pub base_sha: String,
    pub output_sha: String,
    pub claim_id: String,
    pub workspace: WorkspaceObservation,
    pub pin_hash: Digest,
    pub sealed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum SealError {
    #[error("output claim is not this Work attempt's bound submission")]
    ClaimMismatch,
    #[error("observed checkout belongs to another repository")]
    RepositoryMismatch,
    #[error("observed checkout is not at the claimed output")]
    WrongHead,
    #[error("observed checkout has tracked or untracked changes")]
    Dirty,
    #[error("attempt already sealed a different output")]
    AlreadySealed,
    #[error("encoding failed")]
    Encoding,
}

impl CandidateSeal {
    // The claim says which output; only the server's own checkout observation makes it sealable.
    pub fn seal(
        binding: &AttemptBinding,
        claim: &EvidenceEnvelope,
        repository_identity: &str,
        workspace: &WorkspaceObservation,
        pin_hash: &str,
        sealed_at: DateTime<Utc>,
    ) -> Result<Self, SealError> {
        let EvidencePayload::Submitted(TuiPayload::OutputClaim { output_sha }) = &claim.payload
        else {
            return Err(SealError::ClaimMismatch);
        };
        if binding.role != Role::Implementation
            || claim.source != EvidenceSource::BoundTui
            || &claim.binding != binding
        {
            return Err(SealError::ClaimMismatch);
        }
        if workspace.repository != binding.repository
            || workspace.repository_identity != repository_identity
        {
            return Err(SealError::RepositoryMismatch);
        }
        if &workspace.head_sha != output_sha {
            return Err(SealError::WrongHead);
        }
        if !workspace.clean() {
            return Err(SealError::Dirty);
        }
        Ok(Self {
            seal_id: format!("seal:{}", binding.attempt_id),
            owner_attempt: binding.attempt_id.clone(),
            binding_digest: digest_of(binding).map_err(|_| SealError::Encoding)?,
            repository: binding.repository,
            base_sha: binding.base_sha.clone(),
            output_sha: output_sha.clone(),
            claim_id: claim.evidence_id.clone(),
            workspace: workspace.clone(),
            pin_hash: pin_hash.into(),
            sealed_at,
        })
    }

    // Observation and seal times are excluded so a retried seal of the same output is idempotent.
    pub fn identity_digest(&self) -> Result<Digest, SealError> {
        let w = &self.workspace;
        digest_of(&(
            (&self.seal_id, &self.owner_attempt, &self.binding_digest),
            (self.repository, &self.base_sha, &self.output_sha),
            (&w.repository_identity, &w.host, &w.path, &self.pin_hash),
        ))
        .map_err(|_| SealError::Encoding)
    }

    pub fn confirm_retry(&self, stored: &CandidateSeal) -> Result<(), SealError> {
        if self.identity_digest()? != stored.identity_digest()? {
            return Err(SealError::AlreadySealed);
        }
        Ok(())
    }

    pub fn bound_subject(&self) -> BoundSubject {
        BoundSubject {
            reference: VerificationSubjectRef::SealedOutput {
                owner_attempt: self.owner_attempt.clone(),
                seal_id: self.seal_id.clone(),
            },
            repository: self.repository,
            base_sha: self.base_sha.clone(),
            output_sha: self.output_sha.clone(),
        }
    }
}

// The candidate a stored V verifies comes from its own subject slot and original owner only.
pub fn resolved_subject(run: &VerificationRun) -> Option<ServerResolvedSubject> {
    let subject = run.subject.bound()?;
    let owner = &run.origin.owner_binding;
    match &subject.reference {
        VerificationSubjectRef::BoundCandidate { attempt_id } => {
            (attempt_id == &run.origin.owner_attempt && subject.output_sha == owner.candidate_sha)
                .then(|| ServerResolvedSubject::bound_candidate(owner))
        }
        VerificationSubjectRef::SealedOutput { owner_attempt, .. } => (owner_attempt
            == &run.origin.owner_attempt
            && subject.repository == owner.repository
            && subject.base_sha == owner.base_sha)
            .then(|| {
                ServerResolvedSubject::sealed(Candidate {
                    repository: subject.repository,
                    base_sha: subject.base_sha.clone(),
                    head_sha: subject.output_sha.clone(),
                })
            }),
    }
}

#[cfg(test)]
#[path = "candidate_seal_tests.rs"]
mod tests;
