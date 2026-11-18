use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::model::HostGate;
use super::verification_run::Digest;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandingTarget {
    pub repository: String,
    pub head_sha: String,
    pub head_ref: String,
    pub base_ref: String,
    pub pin_hash: Digest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreflightMode {
    Full,
    PreTlc,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandingPreflight {
    pub head_sha: String,
    pub mode: PreflightMode,
    pub transcript_digest: Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenedPr {
    pub repository: String,
    pub number: u64,
    pub head_sha: String,
    pub head_ref: String,
    pub base_ref: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteCi {
    Idle,
    Active,
    Unknown,
}

pub struct RemoteLanding {
    pub branch_head: Option<String>,
    pub pull_request: Option<OpenedPr>,
    pub ci: RemoteCi,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Publication {
    Wait,
    PushThenOpen,
    Open,
    Refused,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LandingStatus {
    Preparing,
    Failed { reason: String, uncertain: bool },
    CiHold { pr: OpenedPr },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Landing {
    pub target: LandingTarget,
    pub epoch: u64,
    pub control_version: u64,
    pub host: HostGate,
    pub action_id: String,
    pub deadline: DateTime<Utc>,
    pub preflight: Option<LandingPreflight>,
    pub status: LandingStatus,
}

impl Landing {
    pub fn publication(&self, remote: &RemoteLanding) -> Publication {
        if !matches!(self.status, LandingStatus::Preparing)
            || !self.verified()
            || remote.pull_request.is_some()
        {
            return Publication::Refused;
        }
        if remote.ci != RemoteCi::Idle {
            return Publication::Wait;
        }
        match remote.branch_head.as_deref() {
            None => Publication::PushThenOpen,
            Some(head) if head == self.target.head_sha => Publication::Open,
            Some(_) => Publication::Refused,
        }
    }

    pub fn matches_pr(&self, pr: &OpenedPr) -> bool {
        pr.number > 0
            && pr.repository == self.target.repository
            && pr.base_ref == self.target.base_ref
            && pr.head_ref == self.target.head_ref
            && pr.head_sha == self.target.head_sha
    }

    pub fn verified(&self) -> bool {
        self.preflight.as_ref().is_some_and(|check| {
            check.head_sha == self.target.head_sha && !check.transcript_digest.trim().is_empty()
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "fact", rename_all = "snake_case", deny_unknown_fields)]
pub enum LandingFact {
    Prepare {
        host: HostGate,
        deadline: DateTime<Utc>,
    },
    Preflight(LandingPreflight),
    Opened(OpenedPr),
    Failed {
        reason: String,
        uncertain: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossRunJob {
    pub job_id: u64,
    pub log_ref: String,
    pub run_id: u64,
    pub run_attempt: u32,
    pub successful_job_id: u64,
    pub inputs_digest: Digest,
    pub input_references: [String; 2],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TlcEvidence {
    pub head_sha: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub reference: String,
    pub outcome: TlcOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum TlcOutcome {
    Passed { selected: u32, passed: u32 },
    NoApplicableConfigs { selection_ref: String },
    Failed,
}

impl TlcEvidence {
    pub fn covers(&self, head: &str, now: DateTime<Utc>, limit: chrono::TimeDelta) -> bool {
        self.head_sha == head
            && self.started_at <= self.finished_at
            && self.finished_at <= now
            && self.finished_at - self.started_at < limit
            && !self.reference.trim().is_empty()
            && match &self.outcome {
                TlcOutcome::Passed { selected, passed } => *selected > 0 && passed == selected,
                TlcOutcome::NoApplicableConfigs { selection_ref } => {
                    !selection_ref.trim().is_empty()
                }
                TlcOutcome::Failed => false,
            }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeReceipt {
    pub artifact: super::profile::NodeArtifact,
    pub pr_number: u64,
    pub merge_sha: String,
    pub ci_run_id: u64,
    pub ci_run_attempt: u32,
    pub tlc: TlcEvidence,
    pub review_refs: Vec<String>,
    pub runner_losses: Vec<CrossRunJob>,
}

impl MergeReceipt {
    pub fn matches(&self, pr: &OpenedPr, work: &super::model::AttemptBinding) -> bool {
        self.pr_number == pr.number
            && self.artifact.node_id == work.node_id
            && self.artifact.hashes == work.hashes
            && self.artifact.candidate.repository == work.repository
            && self.artifact.candidate.base_sha == work.base_sha
            && self.artifact.candidate.head_sha == pr.head_sha
            && self.artifact.review.head_sha == pr.head_sha
            && self.tlc.head_sha == pr.head_sha
            && self.ci_run_id > 0
            && self.ci_run_attempt > 0
            && !self.review_refs.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeJob {
    pub deadline: DateTime<Utc>,
    pub status: MergeStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MergeStatus {
    Waiting,
    WaitingForMain(Box<MergeReceipt>),
    Adopted(Box<super::profile::MainIntegrationEvidence>),
    Failed { reason: String, uncertain: bool },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "fact", rename_all = "snake_case", deny_unknown_fields)]
pub enum MergeFact {
    Prepare { opened_at: Option<DateTime<Utc>> },
    Started(Box<MergeReceipt>),
    Adopted(Box<super::profile::MainIntegrationEvidence>),
    Failed { reason: String, uncertain: bool },
}

#[cfg(test)]
#[path = "landing_tests.rs"]
mod tests;
