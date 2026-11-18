use serde::{Deserialize, Serialize};

use super::model::{Repository, Role};
use super::verification_run::{Digest, digest_of};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalSlot {
    Start,
    Implementation,
    Review,
    Landing,
    Merge,
    Deploy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeferredCapability {
    Dag,
    MultiHost,
    AutomaticApproval,
    ClaudeFallback,
    Tracker,
    Ui,
    GeneralOwnership,
    DesignStages,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MinimalPipelineScope {
    pub repository: Repository,
    pub issue_number: u64,
    pub host: String,
    pub decision_refs: [String; 2],
    pub single_issue: bool,
    pub fixed_host: bool,
    pub individual_pr: bool,
    pub automatic_approval: bool,
    pub approval_default_required: bool,
    pub approval_slots: [ApprovalSlot; 6],
    pub deferred: Vec<DeferredCapability>,
}

impl MinimalPipelineScope {
    pub fn minimal(issue_number: u64, host: String, decision_refs: [String; 2]) -> Self {
        Self {
            repository: Repository::Adk,
            issue_number,
            host,
            decision_refs,
            single_issue: true,
            fixed_host: true,
            individual_pr: true,
            automatic_approval: false,
            approval_default_required: false,
            approval_slots: [
                ApprovalSlot::Start,
                ApprovalSlot::Implementation,
                ApprovalSlot::Review,
                ApprovalSlot::Landing,
                ApprovalSlot::Merge,
                ApprovalSlot::Deploy,
            ],
            deferred: vec![
                DeferredCapability::Dag,
                DeferredCapability::MultiHost,
                DeferredCapability::AutomaticApproval,
                DeferredCapability::ClaudeFallback,
                DeferredCapability::Tracker,
                DeferredCapability::Ui,
                DeferredCapability::GeneralOwnership,
                DeferredCapability::DesignStages,
            ],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSourceDigest {
    pub identity: String,
    pub digest: Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineRole {
    pub role: Role,
    pub provider: String,
    pub model: String,
    pub effort: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineRulesSnapshot {
    pub sources: Vec<RuleSourceDigest>,
    pub scope: MinimalPipelineScope,
    pub roles: Vec<PipelineRole>,
}

impl PipelineRulesSnapshot {
    pub fn hash(&self) -> Result<Digest, serde_json::Error> {
        digest_of(&(&self.sources, &self.scope, &self.roles))
    }
}
