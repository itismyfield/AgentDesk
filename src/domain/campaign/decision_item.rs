use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::definition::valid_id;
use super::verification_run::{Digest, digest_of};
use super::workflow_options::ApprovalGate;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ApprovalTarget {
    Candidate { base_sha: String, head_sha: String },
    PullRequest { number: u64, head_sha: String },
    Main { sha: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalBinding {
    pub run_id: String,
    pub node_id: String,
    pub gate: ApprovalGate,
    pub definition_revision: u64,
    pub target: ApprovalTarget,
    pub policy_hash: Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForumApproval {
    pub decision_ref: String,
    pub response_ref: String,
    pub response_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalSource {
    CoordinatorRecordedForum,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRecord {
    pub source: ApprovalSource,
    pub evidence: ForumApproval,
    pub recorded_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionItem {
    pub binding: ApprovalBinding,
    pub approval: Option<ApprovalRecord>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalBook {
    pub current: BTreeMap<Digest, Digest>,
    pub items: BTreeMap<Digest, DecisionItem>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ApprovalError {
    #[error("approval input is incomplete")]
    Invalid,
    #[error("approval does not match the current gate")]
    Stale,
    #[error("approval conflicts with the saved gate or provenance")]
    Conflict,
    #[error("approval encoding failed")]
    Encoding,
}

impl ApprovalBinding {
    pub fn validate(&self) -> Result<(), ApprovalError> {
        let target_valid = match (self.gate, &self.target) {
            (
                ApprovalGate::Start
                | ApprovalGate::Implementation
                | ApprovalGate::Review
                | ApprovalGate::Landing,
                ApprovalTarget::Candidate { base_sha, head_sha },
            ) => !base_sha.trim().is_empty() && !head_sha.trim().is_empty(),
            (ApprovalGate::Merge, ApprovalTarget::PullRequest { number, head_sha }) => {
                *number > 0 && !head_sha.trim().is_empty()
            }
            (ApprovalGate::Deploy, ApprovalTarget::Main { sha }) => !sha.trim().is_empty(),
            _ => false,
        };
        if !valid_id(&self.run_id)
            || !valid_id(&self.node_id)
            || self.definition_revision == 0
            || self.policy_hash.trim().is_empty()
            || !target_valid
        {
            return Err(ApprovalError::Invalid);
        }
        Ok(())
    }

    pub fn id(&self) -> Result<Digest, ApprovalError> {
        digest_of(self).map_err(|_| ApprovalError::Encoding)
    }

    pub fn slot_id(&self) -> Result<Digest, ApprovalError> {
        digest_of(&(&self.node_id, self.gate)).map_err(|_| ApprovalError::Encoding)
    }
}

impl ForumApproval {
    pub fn validate(&self) -> Result<(), ApprovalError> {
        let response_valid = url::Url::parse(&self.response_ref)
            .is_ok_and(|url| url.scheme() == "https" && url.host_str().is_some());
        if self.decision_ref.trim().is_empty() || !response_valid {
            return Err(ApprovalError::Invalid);
        }
        Ok(())
    }
}

impl ApprovalBook {
    pub fn is_empty(&self) -> bool {
        self.current.is_empty() && self.items.is_empty()
    }

    pub fn open(&mut self, binding: ApprovalBinding) -> Result<bool, ApprovalError> {
        binding.validate()?;
        let id = binding.id()?;
        let slot = binding.slot_id()?;
        if let Some(saved) = self.items.get(&id)
            && saved.binding != binding
        {
            return Err(ApprovalError::Conflict);
        }
        if self.current.get(&slot) == Some(&id) {
            return Ok(false);
        }
        self.items.entry(id.clone()).or_insert(DecisionItem {
            binding,
            approval: None,
        });
        self.current.insert(slot, id);
        Ok(true)
    }

    pub fn approve(
        &mut self,
        binding: &ApprovalBinding,
        evidence: ForumApproval,
        recorded_at: DateTime<Utc>,
    ) -> Result<bool, ApprovalError> {
        binding.validate()?;
        evidence.validate()?;
        let id = binding.id()?;
        if self.current.get(&binding.slot_id()?) != Some(&id) {
            return Err(ApprovalError::Stale);
        }
        let item = self.items.get_mut(&id).ok_or(ApprovalError::Stale)?;
        if item.binding != *binding {
            return Err(ApprovalError::Conflict);
        }
        if let Some(saved) = &item.approval {
            return if saved.evidence == evidence {
                Ok(false)
            } else {
                Err(ApprovalError::Conflict)
            };
        }
        item.approval = Some(ApprovalRecord {
            source: ApprovalSource::CoordinatorRecordedForum,
            evidence,
            recorded_at,
        });
        Ok(true)
    }
}
