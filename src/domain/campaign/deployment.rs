use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::model::HostGate;
use super::verification_run::Digest;

/// One host that must restart on the target source; the incarnation is the boot it replaces.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployHost {
    pub host: String,
    pub previous_incarnation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_binary_digest: Option<String>,
}

/// The server's request: exactly this main commit on exactly these hosts, settled by the deadline.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployIntent {
    pub target_sha: String,
    pub hosts: Vec<DeployHost>,
    pub requested_at: DateTime<Utc>,
    pub deadline: DateTime<Utc>,
}

/// What one host's running binary reported after the deployment started.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployWitness {
    pub host: String,
    pub source_sha: String,
    pub boot_incarnation: String,
    pub binary_identity: Option<String>,
    pub boot_started_at_ms: i64,
    pub observed_at: DateTime<Utc>,
}

impl DeployIntent {
    pub fn valid(&self) -> bool {
        let mut hosts = BTreeSet::new();
        let sha = &self.target_sha;
        sha.len() == 40
            && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            && self.requested_at < self.deadline
            && !self.hosts.is_empty()
            && self.hosts.iter().all(|target| {
                !target.host.trim().is_empty()
                    && !target.previous_incarnation.trim().is_empty()
                    && hosts.insert(&target.host)
            })
    }

    // Every requested host runs a new boot of the target source. A missing, older or
    // different-source report leaves the deployment unconfirmed.
    pub fn witnessed_by(&self, witnesses: &[DeployWitness], now: DateTime<Utc>) -> bool {
        witnesses.len() == self.hosts.len()
            && self.hosts.iter().all(|target| {
                witnesses.iter().any(|seen| {
                    seen.host == target.host
                        && seen.source_sha == self.target_sha
                        && seen.boot_incarnation != target.previous_incarnation
                        && seen.boot_started_at_ms > self.requested_at.timestamp_millis()
                        && seen.observed_at <= now
                        && target
                            .expected_binary_digest
                            .as_ref()
                            .is_none_or(|digest| seen.binary_identity.as_ref() == Some(digest))
                })
            })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DeployStatus {
    Waiting,
    Started { receipt: String },
    // The final gate cannot prove whether the executor started; only a witness settles it.
    Indeterminate { evidence: String },
    Confirmed { witnesses: Vec<DeployWitness> },
    Failed { reason: String, uncertain: bool },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployJob {
    pub intent: DeployIntent,
    pub action_id: String,
    pub epoch: u64,
    pub control_version: u64,
    pub host: HostGate,
    pub pin_hash: Digest,
    pub status: DeployStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "fact", rename_all = "snake_case", deny_unknown_fields)]
pub enum DeployFact {
    // Also replaces an unstarted intent whose main head moved; the deadline never moves.
    Request {
        target_sha: String,
        hosts: Vec<DeployHost>,
    },
    Started {
        receipt: String,
    },
    Indeterminate {
        evidence: String,
    },
    Confirmed {
        witnesses: Vec<DeployWitness>,
    },
    Failed {
        reason: String,
        uncertain: bool,
    },
}

/// The owning agent's check of one operational condition against the deployed commit. Server-observed
/// conditions are judged from the run's own records and never stored here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceCheck {
    pub condition_id: String,
    pub reference: String,
    pub deployed_sha: String,
    pub observed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "fact", rename_all = "snake_case", deny_unknown_fields)]
pub enum AcceptanceFact {
    Checked(AcceptanceCheck),
    Judge,
}
