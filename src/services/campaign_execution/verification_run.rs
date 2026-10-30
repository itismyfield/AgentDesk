use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::definition::AcceptanceCondition;
use super::model::{Action, ActionKind, ActionState, AttemptBinding, HostGate, Repository, Stage};
use super::profile::{
    Candidate, CheckKind, EnvironmentComponent, ExecutedTest, ExecutionEnvironment, PlannedTest,
    ResourceIdentity, TestEvidence, TestOutcome, VerificationCheck, VerificationPlan,
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

// The plan identity scope the helper records for each selection, so a bound plan names its policy.
pub fn plan_scope(selection: SelectionRule) -> &'static str {
    match selection {
        SelectionRule::NonPgClassifier => "campaign:non-pg",
        SelectionRule::PgClassifier => "campaign:pg",
        SelectionRule::ListedUniverse => "campaign:universe",
    }
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

// plan_digest is the helper's semantic plan identity; plan_artifact_digest is the plan file's bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActualPlan {
    pub archive_digest: Digest,
    pub manifest: Vec<PlannedTest>,
    pub plan_digest: Digest,
    pub plan_artifact_digest: Digest,
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
    ReconciledUnstarted,
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
    // The persistent id names this lost start, so a later observation resolves exactly it.
    Indeterminate {
        initiation_id: String,
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
        plan_artifact_digest: Digest,
        config_digest: Digest,
        scope: String,
        selected: Vec<ListedTest>,
    },
    Tests {
        executed: Vec<ExecutedTest>,
    },
}

// A result names the Started receipt it belongs to, so a late or foreign result cannot fill a slot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseResult {
    pub receipt: String,
    pub record: CommandRecord,
    pub termination: Termination,
    pub artifacts: Vec<ArtifactRef>,
    pub output: PhaseOutput,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ReconciledInitiation {
    Started {
        receipt: String,
        process: ProcessIdentity,
    },
    Unstarted,
}

// A direct observation of one phase child, named by the execution, action and phase it resolves.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitiationObservation {
    pub execution_id: String,
    pub action_id: String,
    pub phase_id: String,
    pub initiation_id: String,
    pub outcome: ReconciledInitiation,
    pub evidence: String,
    pub observed_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TreeState {
    Alive,
    Gone,
    Unknown,
}

// A direct observation of a Started phase child's process tree once no runner handle remains.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminationObservation {
    pub execution_id: String,
    pub action_id: String,
    pub phase_id: String,
    pub receipt: String,
    pub process: ProcessIdentity,
    pub tree: TreeState,
    pub evidence: String,
    pub observed_at: DateTime<Utc>,
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

impl VerificationPhase {
    // An Indeterminate start stays unresolved until a later direct observation is appended.
    pub fn unresolved(&self) -> bool {
        self.unresolved_initiation().is_some()
    }

    // The id a direct observation must name to resolve the current Indeterminate start.
    pub fn unresolved_initiation(&self) -> Option<&str> {
        match self.initiations.last() {
            Some(PhaseInitiation::Indeterminate { initiation_id, .. }) => Some(initiation_id),
            _ => None,
        }
    }

    pub fn started(&self) -> bool {
        self.initiations
            .iter()
            .any(|i| matches!(i, PhaseInitiation::Started { .. }))
    }
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
    pub extra: Vec<String>,
    pub exit_code: Option<i32>,
    pub audit_exit_code: Option<i32>,
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
        evidence: Box<TestEvidence>,
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

    // Only what the runner can execute faithfully registers: one worker, and a classifier-based
    // selection only when a Classify phase feeds the Plan; the PG classifier has no helper path yet.
    pub fn validate(&self) -> Result<(), VerificationError> {
        if self.version.trim().is_empty() || !self.checks.iter().any(|c| c.required) {
            return Err(VerificationError::InvalidSpec("version or required".into()));
        }
        if self.command_policy.workers != 1 {
            return Err(VerificationError::InvalidSpec("workers".into()));
        }
        let mut ids = BTreeSet::new();
        for check in &self.checks {
            if !ids.insert(&check.id) {
                return Err(VerificationError::InvalidSpec(check.id.clone()));
            }
            let mut seen = BTreeSet::new();
            for phase in &check.phases {
                if !seen.insert(&phase.id) || phase.requires.iter().any(|r| !seen.contains(r)) {
                    return Err(VerificationError::InvalidSpec(phase.id.clone()));
                }
            }
            if check.kind == CheckKind::Test
                && !check.phases.iter().any(|p| p.kind == PhaseKind::Test)
            {
                return Err(VerificationError::InvalidSpec(check.id.clone()));
            }
            let kind_of = |id: &str| check.phases.iter().find(|p| p.id == id).map(|p| p.kind);
            let plan_follows_classify = check
                .phases
                .iter()
                .filter(|p| p.kind == PhaseKind::Plan)
                .all(|p| {
                    p.requires
                        .iter()
                        .any(|r| kind_of(r) == Some(PhaseKind::Classify))
                });
            match check.selection {
                SelectionRule::PgClassifier => {
                    return Err(VerificationError::InvalidSpec(format!(
                        "{}: pg selection has no plan path yet",
                        check.id
                    )));
                }
                SelectionRule::NonPgClassifier
                    if !plan_follows_classify
                        || !check.phases.iter().any(|p| p.kind == PhaseKind::Classify) =>
                {
                    return Err(VerificationError::InvalidSpec(format!(
                        "{}: classifier selection needs a classify phase before plan",
                        check.id
                    )));
                }
                _ => {}
            }
        }
        Ok(())
    }

    // The policy becomes a candidate plan; the test manifest is bound later from the sealed output.
    pub fn plan(
        &self,
        stage: Stage,
        candidate: &Candidate,
        acceptance: Vec<AcceptanceCondition>,
    ) -> VerificationPlan {
        VerificationPlan {
            candidate: candidate.clone(),
            stage,
            acceptance,
            checks: self
                .checks
                .iter()
                .map(|c| VerificationCheck {
                    id: c.id.clone(),
                    kind: c.kind,
                    required: c.required,
                    acceptance_ids: c.acceptance_ids.clone(),
                })
                .collect(),
            environment_requirements: self.environment.clone(),
        }
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

impl VerificationRun {
    // Registration pre-creates every fixed phase and its Planned action; nothing starts here.
    pub fn register(input: RegistrationInput) -> Result<(Self, Vec<Action>), VerificationError> {
        input.spec.validate()?;
        let check = input
            .spec
            .check(&input.check_id)
            .ok_or_else(|| VerificationError::UnknownPhase(input.check_id.clone()))?;
        let binding = input.owner_binding.clone();
        let execution_id = execution_id(&binding, input.repair_cycle, &check.id);
        let phases: Vec<VerificationPhase> = check
            .phases
            .iter()
            .map(|template| VerificationPhase {
                id: template.id.clone(),
                action_id: phase_action_id(&binding.run_id, &execution_id, &template.id),
                kind: template.kind,
                target: input.gate.clone(),
                requires: template.requires.clone(),
                input: Slot::Unbound,
                initiations: Vec::new(),
                result: None,
            })
            .collect();
        let actions = phases
            .iter()
            .map(|phase| Action {
                id: phase.action_id.clone(),
                binding: binding.clone(),
                kind: ActionKind::VerificationPhase,
                target: input.gate.clone(),
                state: ActionState::Planned,
                initiation_receipt: None,
                reconciliation: None,
            })
            .collect();
        let origin = VerificationOrigin {
            execution_id,
            campaign_id: binding.campaign_id.clone(),
            run_id: binding.run_id.clone(),
            node_id: binding.node_id.clone(),
            series: binding.series,
            repair_cycle: input.repair_cycle,
            owner_attempt: binding.attempt_id.clone(),
            binding_digest: digest_of(&binding).map_err(|_| VerificationError::Encoding)?,
            owner_binding: binding,
            registered_node_epoch: input.registered_node_epoch,
            stage: input.owner_binding.stage,
            pin_hash: input.pin_hash,
            spec_hash: input.spec.hash()?,
            check_id: check.id.clone(),
            gate: input.gate,
            phase_ids: phases.iter().map(|p| p.id.clone()).collect(),
            registered_at: input.registered_at,
        };
        Ok((
            Self {
                origin,
                subject: Slot::Unbound,
                plan: Slot::Unbound,
                phases,
                collection: None,
                cancellation: None,
            },
            actions,
        ))
    }

    pub fn phase(&self, id: &str) -> Result<&VerificationPhase, VerificationError> {
        self.phases
            .iter()
            .find(|p| p.id == id)
            .ok_or_else(|| VerificationError::UnknownPhase(id.to_owned()))
    }

    fn phase_mut(&mut self, id: &str) -> Result<&mut VerificationPhase, VerificationError> {
        self.phases
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or_else(|| VerificationError::UnknownPhase(id.to_owned()))
    }

    // The subject slot takes only the original owner's output: another attempt's seal or candidate
    // with the same repository and base is a conflict, not a first binding.
    pub fn bind_subject(&mut self, subject: BoundSubject) -> Result<(), VerificationError> {
        let owner = &self.origin.owner_binding;
        let owned = match &subject.reference {
            VerificationSubjectRef::SealedOutput { owner_attempt, .. } => {
                *owner_attempt == owner.attempt_id
            }
            VerificationSubjectRef::BoundCandidate { attempt_id } => {
                *attempt_id == owner.attempt_id && subject.output_sha == owner.candidate_sha
            }
        };
        if !owned || subject.repository != owner.repository || subject.base_sha != owner.base_sha {
            return Err(VerificationError::Conflict);
        }
        self.subject.bind(subject)
    }

    pub fn bind_phase_input(
        &mut self,
        id: &str,
        input: PhaseInput,
    ) -> Result<(), VerificationError> {
        let subject = self
            .subject
            .bound()
            .ok_or_else(|| VerificationError::Unbound("subject".into()))?;
        if input.subject_sha != subject.output_sha {
            return Err(VerificationError::Conflict);
        }
        let phase = self.phase(id)?;
        for required in &phase.requires {
            let digest = self.predecessor_digest(phase, required)?;
            if input.predecessors.get(required) != Some(&digest) {
                return Err(VerificationError::PredecessorPending(required.clone()));
            }
        }
        // The test phase pins the archive, plan file and worker config this V produced as file
        // digests for its preflight; the semantic plan identity is re-checked by the helper.
        if phase.kind == PhaseKind::Test {
            let plan = self
                .plan
                .bound()
                .ok_or_else(|| VerificationError::Unbound("plan".into()))?;
            let pinned = |digest: &str| input.preflight.iter().any(|a| a.digest == digest);
            if !pinned(&plan.archive_digest)
                || !pinned(&plan.plan_artifact_digest)
                || !pinned(&plan.config_digest)
            {
                return Err(VerificationError::Unbound("plan".into()));
            }
        }
        let bound = BoundInput::new(input)?;
        self.phase_mut(id)?.input.bind(bound)
    }

    // Preparation outputs are consumed only after a clean exit; an audit consumes the test phase's
    // raw result however the tests exited.
    fn predecessor_digest(
        &self,
        consumer: &VerificationPhase,
        id: &str,
    ) -> Result<Digest, VerificationError> {
        let required = self.phase(id)?;
        let raw = consumer.kind == PhaseKind::Audit && required.kind == PhaseKind::Test;
        self.output_digest(id, raw)
    }

    fn output_digest(&self, id: &str, raw: bool) -> Result<Digest, VerificationError> {
        let result = self
            .phase(id)?
            .result
            .as_ref()
            .filter(|r| {
                r.termination == Termination::Exited && (raw || r.record.exit_code == Some(0))
            })
            .ok_or_else(|| VerificationError::PredecessorPending(id.to_owned()))?;
        digest_of(&result.output).map_err(|_| VerificationError::Encoding)
    }

    // The actual plan binds once from this V's own archive, list and plan outputs: the manifest is
    // what the plan selected, each entry listed and not ignored, under the registered check policy.
    pub fn bind_plan(&mut self, spec: &VerificationSpec) -> Result<(), VerificationError> {
        if spec.hash()? != self.origin.spec_hash {
            return Err(VerificationError::Conflict);
        }
        let check = spec
            .check(&self.origin.check_id)
            .ok_or_else(|| VerificationError::UnknownPhase(self.origin.check_id.clone()))?;
        let archive = self.output_of(PhaseKind::Archive)?;
        let universe = self.output_of(PhaseKind::List)?;
        let plan = self.output_of(PhaseKind::Plan)?;
        let (PhaseOutput::Archive { digest }, PhaseOutput::Universe { tests }) =
            (archive, universe)
        else {
            return Err(VerificationError::Conflict);
        };
        let PhaseOutput::Plan {
            plan_digest,
            plan_artifact_digest,
            config_digest,
            scope,
            selected,
        } = plan
        else {
            return Err(VerificationError::Conflict);
        };
        if scope != plan_scope(check.selection) {
            return Err(VerificationError::Conflict);
        }
        let listed: BTreeSet<(&str, &str)> = tests
            .iter()
            .filter(|t| !t.ignored)
            .map(|t| (t.binary_id.as_str(), t.name.as_str()))
            .collect();
        let mut chosen = BTreeSet::new();
        let mut manifest = Vec::with_capacity(selected.len());
        for test in selected {
            let key = (test.binary_id.as_str(), test.name.as_str());
            if test.ignored || !listed.contains(&key) || !chosen.insert(key) {
                return Err(VerificationError::Conflict);
            }
            manifest.push(PlannedTest {
                id: format!("{} {}", test.binary_id, test.name),
                check_id: check.id.clone(),
                required: check.required,
            });
        }
        // The listed universe means every runnable listed test; a partial plan cannot claim it.
        if check.selection == SelectionRule::ListedUniverse && chosen != listed {
            return Err(VerificationError::Conflict);
        }
        let actual = ActualPlan {
            archive_digest: digest.clone(),
            manifest,
            plan_digest: plan_digest.clone(),
            plan_artifact_digest: plan_artifact_digest.clone(),
            config_digest: config_digest.clone(),
        };
        self.plan.bind(actual)
    }

    fn output_of(&self, kind: PhaseKind) -> Result<&PhaseOutput, VerificationError> {
        let phase = self
            .phases
            .iter()
            .find(|p| p.kind == kind)
            .ok_or_else(|| VerificationError::UnknownPhase(format!("{kind:?}")))?;
        self.output_digest(&phase.id, false)?;
        phase
            .result
            .as_ref()
            .map(|r| &r.output)
            .ok_or_else(|| VerificationError::PredecessorPending(phase.id.clone()))
    }

    // Common start checks: exact identity, bound inputs, finished predecessors, current fence.
    // Which consumer holds the authority is decided by the caller from stored state.
    pub fn admit_phase_start(
        &self,
        id: &str,
        action: &Action,
        authority: &VerificationStartAuthority,
        fence: CurrentFence,
    ) -> Result<PhaseAdmission, VerificationError> {
        let phase = self.phase(id)?;
        if action.id != phase.action_id
            || action.binding != self.origin.owner_binding
            || action.kind != ActionKind::VerificationPhase
            || action.target != phase.target
        {
            return Err(VerificationError::ActionMismatch(action.id.clone()));
        }
        if authority.execution_id != self.origin.execution_id
            || authority.original_binding_digest != self.origin.binding_digest
            || authority.run_control_version != fence.run_control_version
            || authority.consumer_node_epoch != fence.node_epoch
        {
            return Err(VerificationError::StaleAuthority);
        }
        if self.cancellation.is_some() || action.state == ActionState::Cancelled {
            return Err(VerificationError::Cancelled);
        }
        match action.state {
            ActionState::Started => {
                return Ok(PhaseAdmission::AlreadyStarted {
                    receipt: action.initiation_receipt.clone().unwrap_or_default(),
                });
            }
            ActionState::Reconciled => return Ok(PhaseAdmission::AlreadyFinished),
            ActionState::Unknown => return Ok(PhaseAdmission::Reconcile),
            _ => {}
        }
        if phase.started() || phase.unresolved() {
            return Ok(PhaseAdmission::Reconcile);
        }
        let subject = self
            .subject
            .bound()
            .ok_or_else(|| VerificationError::Unbound("subject".into()))?;
        let input = phase
            .input
            .bound()
            .ok_or_else(|| VerificationError::Unbound(phase.id.clone()))?;
        for required in &phase.requires {
            self.predecessor_digest(phase, required)?;
        }
        // Consumer epoch and control version stay out of the payload: handover keeps the same effect.
        let payload = (
            &self.origin.execution_id,
            &phase.id,
            &self.origin.binding_digest,
            self.origin.registered_node_epoch,
            &self.origin.spec_hash,
            &self.origin.pin_hash,
            &self.origin.gate,
            &subject.output_sha,
            &input.digest,
        );
        Ok(PhaseAdmission::Start {
            action_id: phase.action_id.clone(),
            payload_digest: digest_of(&payload).map_err(|_| VerificationError::Encoding)?,
        })
    }

    pub fn record_initiation(
        &mut self,
        id: &str,
        initiation: PhaseInitiation,
    ) -> Result<(), VerificationError> {
        let phase = self.phase_mut(id)?;
        if phase.initiations.contains(&initiation) {
            return Ok(());
        }
        if phase.started() || phase.unresolved() {
            return Err(VerificationError::Conflict);
        }
        if let PhaseInitiation::Indeterminate { initiation_id, .. } = &initiation {
            let reused = phase.initiations.iter().any(|i| {
                matches!(i, PhaseInitiation::Indeterminate { initiation_id: id, .. } if id == initiation_id)
            });
            if reused {
                return Err(VerificationError::Conflict);
            }
        }
        phase.initiations.push(initiation);
        Ok(())
    }

    // Only an observation naming the execution, action, phase and lost start resolves it; replaying
    // the latest resolution answers with the phase's current state, an older start conflicts.
    pub fn reconcile_initiation(
        &mut self,
        observation: InitiationObservation,
    ) -> Result<ActionState, VerificationError> {
        let execution_id = self.origin.execution_id.clone();
        let phase = self.phase_mut(&observation.phase_id)?;
        if observation.execution_id != execution_id || observation.action_id != phase.action_id {
            return Err(VerificationError::ActionMismatch(observation.action_id));
        }
        let index = phase
            .initiations
            .iter()
            .position(|i| {
                matches!(i, PhaseInitiation::Indeterminate { initiation_id, .. }
                    if *initiation_id == observation.initiation_id)
            })
            .ok_or(VerificationError::Conflict)?;
        let (entry, state) = match observation.outcome {
            ReconciledInitiation::Started { receipt, process } => {
                if receipt != format!("{}:{}:{}", phase.id, process.pid, process.nonce) {
                    return Err(VerificationError::Conflict);
                }
                (
                    PhaseInitiation::Started { receipt, process },
                    ActionState::Started,
                )
            }
            ReconciledInitiation::Unstarted => (
                PhaseInitiation::NotSent {
                    reason: NotSentReason::ReconciledUnstarted,
                    evidence: observation.evidence,
                    observed_at: observation.observed_at,
                },
                ActionState::Planned,
            ),
        };
        let Some(resolution) = phase.initiations.get(index + 1) else {
            phase.initiations.push(entry);
            return Ok(state);
        };
        if *resolution != entry || index + 2 != phase.initiations.len() {
            return Err(VerificationError::Conflict);
        }
        let PhaseInitiation::Started { receipt, .. } = entry else {
            return Ok(state);
        };
        match &phase.result {
            None => Ok(state),
            Some(result) if result.receipt == receipt => Ok(ActionState::Reconciled),
            Some(_) => Err(VerificationError::Conflict),
        }
    }

    // After a restart no runner handle remains: a Started phase closes only once its tree is seen
    // Gone, and then as an interruption, because the exit status cannot be recovered.
    pub fn reconcile_termination(
        &mut self,
        action: &Action,
        observation: TerminationObservation,
    ) -> Result<(), VerificationError> {
        let phase = self.phase(&observation.phase_id)?;
        if observation.execution_id != self.origin.execution_id
            || observation.action_id != phase.action_id
        {
            return Err(VerificationError::ActionMismatch(observation.action_id));
        }
        let started = phase.initiations.iter().any(|i| {
            matches!(i, PhaseInitiation::Started { receipt, process }
                if *receipt == observation.receipt && *process == observation.process)
        });
        if !started {
            return Err(VerificationError::ActionMismatch(observation.receipt));
        }
        if observation.tree != TreeState::Gone {
            return Err(VerificationError::NotFinished);
        }
        let command = &phase
            .input
            .bound()
            .ok_or_else(|| VerificationError::Unbound(phase.id.clone()))?
            .input
            .command;
        let result = PhaseResult {
            receipt: observation.receipt,
            record: CommandRecord {
                executable: command.program.clone(),
                cwd: command.cwd.clone(),
                argv: command.argv.clone(),
                env_identity: command.env_identity.clone(),
                phase_id: phase.id.clone(),
                started_at: observation.process.started_at,
                finished_at: observation.observed_at,
                exit_code: None,
                signal: None,
                stdout_ref: String::new(),
                stderr_ref: String::new(),
            },
            termination: Termination::Interrupted {
                evidence: observation.evidence,
            },
            artifacts: Vec::new(),
            output: PhaseOutput::None,
        };
        self.record_result(&observation.phase_id, action, result)
    }

    // A result is accepted once for the phase, action and Started receipt it names.
    pub fn record_result(
        &mut self,
        id: &str,
        action: &Action,
        result: PhaseResult,
    ) -> Result<(), VerificationError> {
        let phase = self.phase_mut(id)?;
        if !phase.started() {
            return Err(VerificationError::NotFinished);
        }
        let receipt_known = phase.initiations.iter().any(
            |i| matches!(i, PhaseInitiation::Started { receipt, .. } if *receipt == result.receipt),
        );
        if result.record.phase_id != phase.id
            || action.id != phase.action_id
            || action.initiation_receipt.as_deref() != Some(result.receipt.as_str())
            || !receipt_known
        {
            return Err(VerificationError::ActionMismatch(result.receipt));
        }
        match &phase.result {
            None => phase.result = Some(result),
            Some(existing) if *existing == result => {}
            Some(_) => return Err(VerificationError::Conflict),
        }
        Ok(())
    }

    pub fn state<'a>(&self, action: impl Fn(&str) -> Option<&'a Action>) -> VerificationRunState {
        if self.collection.is_some() {
            return VerificationRunState::Collected;
        }
        if self.cancellation.is_some() {
            return VerificationRunState::Cancelled;
        }
        let states: Vec<ActionState> = self
            .phases
            .iter()
            .map(|p| action(&p.action_id).map_or(ActionState::Unknown, |a| a.state))
            .collect();
        if states.contains(&ActionState::Unknown) || self.phases.iter().any(|p| p.unresolved()) {
            return VerificationRunState::Unknown;
        }
        let running = self
            .phases
            .iter()
            .zip(&states)
            .find(|(p, s)| **s == ActionState::Started && p.result.is_none());
        if let Some((phase, _)) = running {
            return if phase.kind == PhaseKind::Test || phase.kind == PhaseKind::Audit {
                VerificationRunState::Running
            } else {
                VerificationRunState::Preparing
            };
        }
        if states.contains(&ActionState::StartPending) {
            return if self.plan.bound().is_some() {
                VerificationRunState::StartPending
            } else {
                VerificationRunState::Preparing
            };
        }
        if self.plan.bound().is_some() {
            return VerificationRunState::Materialized;
        }
        if self.phases.iter().any(|p| p.result.is_some()) {
            return VerificationRunState::Preparing;
        }
        VerificationRunState::Registered
    }

    // Unstarted cancellation: refused while any phase started and its termination is unconfirmed.
    pub fn cancel(
        &mut self,
        actions: &BTreeMap<String, Action>,
        evidence: String,
        cancelled_at: DateTime<Utc>,
    ) -> Result<Vec<String>, VerificationError> {
        if self.collection.is_some() {
            return Err(VerificationError::NotFinished);
        }
        let mut to_cancel = Vec::new();
        for phase in &self.phases {
            let state = actions.get(&phase.action_id).map(|a| a.state);
            let started = phase.started()
                || phase.unresolved()
                || matches!(
                    state,
                    Some(ActionState::Started | ActionState::Unknown | ActionState::StartPending)
                );
            if started && phase.result.is_none() {
                return Err(VerificationError::Started);
            }
            if !started && state != Some(ActionState::Reconciled) {
                to_cancel.push(phase.action_id.clone());
            }
        }
        if self.cancellation.is_none() {
            self.cancellation = Some(Cancellation {
                evidence,
                cancelled_at,
            });
        }
        Ok(to_cancel)
    }

    // Raw collection: every started or unresolved phase must have terminated before anything is
    // classified; the complete/all_passed split is kept and the stage verdict is not decided here.
    pub fn collect<'a>(
        &self,
        action: impl Fn(&str) -> Option<&'a Action>,
        environment: ExecutionEnvironment,
        log_refs: Vec<String>,
    ) -> Result<VerificationCollection, VerificationError> {
        for phase in &self.phases {
            let state = action(&phase.action_id).map(|a| a.state);
            let live = phase.started()
                || phase.unresolved()
                || !matches!(
                    state,
                    Some(ActionState::Planned | ActionState::Reconciled | ActionState::Cancelled)
                );
            if live && phase.result.is_none() {
                return Err(VerificationError::NotFinished);
            }
        }
        for phase in &self.phases {
            let Some(result) = &phase.result else {
                continue;
            };
            if let Termination::Interrupted { evidence } = &result.termination {
                return Ok(VerificationCollection::Interrupted {
                    phase_id: phase.id.clone(),
                    evidence: evidence.clone(),
                });
            }
            let preparation = !matches!(phase.kind, PhaseKind::Test | PhaseKind::Audit);
            if preparation && result.record.exit_code != Some(0) {
                return Ok(VerificationCollection::PreparationFailed {
                    phase_id: phase.id.clone(),
                    record: result.record.clone(),
                });
            }
        }
        // Executed needs every fixed phase finished, including an audit that followed failed tests.
        if self.phases.iter().any(|p| p.result.is_none()) {
            return Err(VerificationError::NotFinished);
        }
        let result_of = |kind: PhaseKind| {
            self.phases
                .iter()
                .find(|p| p.kind == kind)
                .and_then(|p| p.result.as_ref())
        };
        let (Some(result), Some(plan)) = (result_of(PhaseKind::Test), self.plan.bound()) else {
            return Err(VerificationError::NotFinished);
        };
        let PhaseOutput::Tests { executed } = &result.output else {
            return Err(VerificationError::Conflict);
        };
        let mut audit = audit_execution(&plan.manifest, executed, result.record.exit_code);
        if let Some(audited) = result_of(PhaseKind::Audit) {
            audit.audit_exit_code = audited.record.exit_code;
            audit.all_passed = audit.all_passed
                && audited.termination == Termination::Exited
                && audited.record.exit_code == Some(0);
        }
        Ok(VerificationCollection::Executed {
            evidence: Box::new(TestEvidence {
                execution_id: self.origin.execution_id.clone(),
                binding: self.origin.owner_binding.clone(),
                environment,
                planned: plan.manifest.clone(),
                actual: executed.clone(),
                log_refs,
            }),
            audit,
        })
    }
}

// Completeness is judged against the bound manifest in both directions, never from the exit
// code or pass count; the audit phase's own exit is folded in by collect().
pub fn audit_execution(
    planned: &[PlannedTest],
    executed: &[ExecutedTest],
    exit_code: Option<i32>,
) -> ExecutionAudit {
    let mut seen = BTreeSet::new();
    let mut duplicate = Vec::new();
    for test in executed {
        if !seen.insert((&test.check_id, &test.id)) {
            duplicate.push(test.id.clone());
        }
    }
    let failed: Vec<String> = executed
        .iter()
        .filter(|t| t.outcome == TestOutcome::Failed)
        .map(|t| t.id.clone())
        .collect();
    let skipped: Vec<String> = executed
        .iter()
        .filter(|t| {
            matches!(
                t.outcome,
                TestOutcome::Skipped { .. } | TestOutcome::Unknown { .. }
            )
        })
        .map(|t| t.id.clone())
        .collect();
    let missing: Vec<String> = planned
        .iter()
        .filter(|p| {
            !executed
                .iter()
                .any(|t| t.id == p.id && t.check_id == p.check_id)
        })
        .map(|p| p.id.clone())
        .collect();
    let extra: Vec<String> = executed
        .iter()
        .filter(|t| {
            !planned
                .iter()
                .any(|p| p.id == t.id && p.check_id == t.check_id)
        })
        .map(|t| t.id.clone())
        .collect();
    let complete = !planned.is_empty()
        && !executed.is_empty()
        && missing.is_empty()
        && duplicate.is_empty()
        && extra.is_empty()
        && planned
            .iter()
            .filter(|p| p.required)
            .all(|p| !skipped.contains(&p.id));
    ExecutionAudit {
        planned: planned.len(),
        executed: executed.len(),
        all_passed: complete && failed.is_empty() && exit_code == Some(0),
        failed,
        skipped,
        missing,
        duplicate,
        extra,
        exit_code,
        audit_exit_code: None,
        complete,
    }
}

#[cfg(test)]
#[path = "verification_run_tests.rs"]
pub(crate) mod tests;
