use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::model::{AttemptBinding, HostGate, Repository, Stage};
use super::profile::{
    CheckKind, EnvironmentComponent, ExecutedTest, PlannedTest, ResourceIdentity, TestEvidence,
};

pub type Digest = String;

pub fn digest_of<T: Serialize>(value: &T) -> Result<Digest, serde_json::Error> {
    serde_json::to_vec(value).map(|bytes| blake3::hash(&bytes).to_hex().to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseKind {
    Archive,
    List,
    SourceCompare,
    Classify,
    Plan,
    Test,
    Audit,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseTemplate {
    pub id: String,
    pub kind: PhaseKind,
    pub requires: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionRule {
    NonPgClassifier,
    PgClassifier,
    ListedUniverse,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandPolicy {
    pub nextest_profile: String,
    pub no_tests_fail: bool,
    pub workers: u32,
}

// Candidate-independent check policy; the actual test list comes from the sealed output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecCheck {
    pub id: String,
    pub kind: CheckKind,
    pub required: bool,
    pub acceptance_ids: Vec<String>,
    pub selection: SelectionRule,
    pub phases: Vec<PhaseTemplate>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationSpec {
    pub version: String,
    pub repository: Repository,
    pub checks: Vec<SpecCheck>,
    pub environment: Vec<EnvironmentComponent>,
    pub command_policy: CommandPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VerificationSubjectRef {
    BoundCandidate {
        attempt_id: String,
    },
    SealedOutput {
        owner_attempt: String,
        seal_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundSubject {
    pub reference: VerificationSubjectRef,
    pub repository: Repository,
    pub base_sha: String,
    pub output_sha: String,
}

// A fill-once slot: Unbound until bound exactly once; re-binding the same value is idempotent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Slot<T> {
    Unbound,
    Bound { value: T },
}

impl<T: PartialEq> Slot<T> {
    pub fn bound(&self) -> Option<&T> {
        match self {
            Self::Bound { value } => Some(value),
            Self::Unbound => None,
        }
    }
    pub fn bind(&mut self, value: T) -> Result<(), VerificationError> {
        match self {
            Self::Unbound => {
                *self = Self::Bound { value };
                Ok(())
            }
            Self::Bound { value: bound } if *bound == value => Ok(()),
            Self::Bound { .. } => Err(VerificationError::AlreadyBound),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationOrigin {
    pub execution_id: String,
    pub campaign_id: String,
    pub run_id: String,
    pub node_id: String,
    pub series: u32,
    pub repair_cycle: u32,
    pub owner_attempt: String,
    pub owner_binding: AttemptBinding,
    pub binding_digest: Digest,
    pub registered_node_epoch: u64,
    pub stage: Stage,
    pub pin_hash: Digest,
    pub spec_hash: Digest,
    pub check_id: String,
    pub gate: HostGate,
    pub phase_ids: Vec<String>,
    pub registered_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActualPlan {
    pub archive_digest: Digest,
    pub manifest: Vec<PlannedTest>,
    pub plan_digest: Digest,
    pub config_digest: Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec {
    pub program: String,
    pub argv: Vec<String>,
    pub cwd: String,
    pub env_identity: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub name: String,
    pub path: String,
    pub digest: Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedArtifact {
    pub name: String,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseInput {
    pub subject_sha: String,
    pub checkout_path: String,
    pub command: CommandSpec,
    pub predecessors: BTreeMap<String, Digest>,
    pub preflight: Vec<ArtifactRef>,
    pub outputs: Vec<ExpectedArtifact>,
    pub resources: Vec<ResourceIdentity>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundInput {
    pub input: PhaseInput,
    pub digest: Digest,
}

impl BoundInput {
    pub fn new(input: PhaseInput) -> Result<Self, VerificationError> {
        let digest = digest_of(&input).map_err(|_| VerificationError::Encoding)?;
        Ok(Self { input, digest })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub host: String,
    pub pid: u32,
    pub process_group: Option<u32>,
    pub started_at: DateTime<Utc>,
    pub nonce: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandRecord {
    pub executable: String,
    pub cwd: String,
    pub argv: Vec<String>,
    pub env_identity: BTreeMap<String, String>,
    pub phase_id: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout_ref: String,
    pub stderr_ref: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum NotSentReason {
    ResourceWait { resource: ResourceIdentity },
    PreflightMismatch { artifact: String },
    StaleAuthority,
    AuthorityRevoked,
    PhaseCancelled,
}

// Resource waits and preflight mismatches leave the phase unstarted; only Started carries a receipt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PhaseInitiation {
    NotSent {
        reason: NotSentReason,
        evidence: String,
        observed_at: DateTime<Utc>,
    },
    Started {
        receipt: String,
        process: ProcessIdentity,
    },
    Indeterminate {
        evidence: String,
        observed_at: DateTime<Utc>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Termination {
    Exited,
    Signalled,
    Interrupted { evidence: String },
    TimedOut,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListedTest {
    pub binary_id: String,
    pub name: String,
    pub ignored: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PhaseOutput {
    None,
    Archive {
        digest: Digest,
    },
    Universe {
        tests: Vec<ListedTest>,
    },
    Plan {
        plan_digest: Digest,
        config_digest: Digest,
    },
    Tests {
        executed: Vec<ExecutedTest>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseResult {
    pub record: CommandRecord,
    pub termination: Termination,
    pub artifacts: Vec<ArtifactRef>,
    pub output: PhaseOutput,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationPhase {
    pub id: String,
    pub action_id: String,
    pub kind: PhaseKind,
    pub target: HostGate,
    pub requires: Vec<String>,
    pub input: Slot<BoundInput>,
    pub initiations: Vec<PhaseInitiation>,
    pub result: Option<PhaseResult>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cancellation {
    pub evidence: String,
    pub cancelled_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationRun {
    pub origin: VerificationOrigin,
    pub subject: Slot<BoundSubject>,
    pub plan: Slot<ActualPlan>,
    pub phases: Vec<VerificationPhase>,
    pub collection: Option<VerificationCollection>,
    pub cancellation: Option<Cancellation>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationRunState {
    Registered,
    Preparing,
    Materialized,
    StartPending,
    Running,
    Collected,
    Cancelled,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum AuthoritySource {
    CurrentWork,
    ReviewPrerequisite {
        prerequisite_id: String,
    },
    FinishPlan {
        stop_control_version: u64,
        grant_ref: String,
    },
}

// Who may start the registered V now; the V's original owner and epoch are never overwritten by it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationStartAuthority {
    pub execution_id: String,
    pub original_binding_digest: Digest,
    pub consumer_attempt: String,
    pub consumer_binding_digest: Digest,
    pub consumer_stage: Stage,
    pub consumer_node_epoch: u64,
    pub run_control_version: u64,
    pub source: AuthoritySource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentFence {
    pub run_control_version: u64,
    pub node_epoch: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionAudit {
    pub planned: usize,
    pub executed: usize,
    pub failed: Vec<String>,
    pub skipped: Vec<String>,
    pub missing: Vec<String>,
    pub duplicate: Vec<String>,
    pub exit_code: Option<i32>,
    pub complete: bool,
    pub all_passed: bool,
}

// Collected is not PASS: preparation failures and interruptions are collected as such.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VerificationCollection {
    PreparationFailed {
        phase_id: String,
        record: CommandRecord,
    },
    Interrupted {
        phase_id: String,
        evidence: String,
    },
    Executed {
        evidence: TestEvidence,
        audit: ExecutionAudit,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistrationInput {
    pub owner_binding: AttemptBinding,
    pub repair_cycle: u32,
    pub registered_node_epoch: u64,
    pub pin_hash: Digest,
    pub spec: VerificationSpec,
    pub check_id: String,
    pub gate: HostGate,
    pub registered_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PhaseAdmission {
    Start {
        action_id: String,
        payload_digest: Digest,
    },
    AlreadyStarted {
        receipt: String,
    },
    AlreadyFinished,
    Reconcile,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum VerificationError {
    #[error("slot already bound to a different value")]
    AlreadyBound,
    #[error("subject, plan or phase input is still unbound: {0}")]
    Unbound(String),
    #[error("unknown phase or check: {0}")]
    UnknownPhase(String),
    #[error("phase action does not match the registered phase: {0}")]
    ActionMismatch(String),
    #[error("predecessor phase has not finished: {0}")]
    PredecessorPending(String),
    #[error("authority is stale or for another execution")]
    StaleAuthority,
    #[error("phase or verification was cancelled")]
    Cancelled,
    #[error("phase already started; cancel only after termination")]
    Started,
    #[error("execution lifecycle has not finished")]
    NotFinished,
    #[error("recorded value conflicts with the stored record")]
    Conflict,
    #[error("encoding failed")]
    Encoding,
    #[error("spec invalid: {0}")]
    InvalidSpec(String),
}

impl VerificationSpec {
    pub fn hash(&self) -> Result<Digest, VerificationError> {
        digest_of(self).map_err(|_| VerificationError::Encoding)
    }

    pub fn check(&self, id: &str) -> Option<&SpecCheck> {
        self.checks.iter().find(|c| c.id == id)
    }
}

pub fn execution_id(binding: &AttemptBinding, repair_cycle: u32, check_id: &str) -> Digest {
    let key = (
        &binding.campaign_id,
        &binding.run_id,
        &binding.node_id,
        binding.series,
        repair_cycle,
        &binding.attempt_id,
        check_id,
    );
    format!(
        "v-{}",
        blake3::hash(&serde_json::to_vec(&key).unwrap_or_default()).to_hex()
    )
}

pub fn phase_action_id(run_id: &str, execution_id: &str, phase_id: &str) -> String {
    format!("{run_id}:{execution_id}:{phase_id}")
}

#[cfg(test)]
#[path = "verification_run_tests.rs"]
mod tests;
