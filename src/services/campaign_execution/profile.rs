use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::definition::{
    AcceptanceCondition, DefinitionError, DefinitionHashes, PROFILE_VERSION, profile_hash,
};
use super::model::{AttemptBinding, ExecutionReadiness, Repository, Role, Stage};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryProfile {
    pub repository: Repository,
    pub version: String,
    pub hash: String,
    pub models: Vec<RoleModelDefault>,
    pub limits: ProfileLimits,
    pub readiness: ExecutionReadiness,
    pub unavailable_reason: String,
    pub environment_requirements: Vec<EnvironmentComponent>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleModelDefault {
    pub role: Role,
    pub selection: ModelDefault,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ModelDefault {
    Pinned { model: String, effort: String },
    Unspecified { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileLimits {
    pub concurrent_tasks: u32,
    pub quality_repairs: u32,
    pub design_review_rounds: u32,
}

impl RepositoryProfile {
    // Defaults are code constants pinned by PROFILE_VERSION; changes require a version bump.
    pub fn for_repository(repository: Repository) -> Result<Self, DefinitionError> {
        use EnvironmentComponent as E;
        Ok(Self {
            repository,
            version: PROFILE_VERSION.into(),
            hash: profile_hash(repository, PROFILE_VERSION)?,
            models: [Role::Implementation, Role::Review]
                .into_iter()
                .map(|role| RoleModelDefault {
                    role,
                    selection: ModelDefault::Unspecified {
                        reason: "Role model defaults are not pinned in the approved contract"
                            .into(),
                    },
                })
                .collect(),
            limits: ProfileLimits {
                concurrent_tasks: 2,
                quality_repairs: 3,
                design_review_rounds: 2,
            },
            readiness: ExecutionReadiness::Unavailable,
            unavailable_reason:
                "Repository execution adapter and final host gate are not implemented".into(),
            environment_requirements: match repository {
                Repository::Adk => vec![E::Toolchain, E::RustTarget],
                Repository::CookingHeart => vec![
                    E::Toolchain,
                    E::Engine,
                    E::ResourcesLock,
                    E::LoreAssets,
                    E::MonolithSubmodule,
                    E::Mcp,
                    E::Editor,
                    E::Pie,
                ],
            },
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub repository: Repository,
    pub base_sha: String,
    pub head_sha: String,
}

impl Candidate {
    pub fn validate_binding(&self, binding: &AttemptBinding) -> Result<(), ContractError> {
        if self.repository != binding.repository
            || self.base_sha != binding.base_sha
            || self.head_sha != binding.candidate_sha
        {
            return Err(ContractError::CandidateMismatch);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkout {
    pub host: String,
    pub path: String,
    pub observed_sha: String,
    pub observed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub candidate: Candidate,
    pub editing: Checkout,
    pub verification: Checkout,
    pub verification_kind: VerificationCheckoutKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationCheckoutKind {
    Isolated,
    Shared,
}

impl Workspace {
    pub fn validate(&self, candidate: &Candidate) -> Result<(), ContractError> {
        if &self.candidate != candidate
            || self.editing.observed_sha != candidate.head_sha
            || self.verification.observed_sha != candidate.head_sha
        {
            return Err(ContractError::CandidateMismatch);
        }
        if candidate.repository == Repository::CookingHeart
            && (self.verification_kind != VerificationCheckoutKind::Shared
                || (self.editing.host == self.verification.host
                    && self.editing.path == self.verification.path))
        {
            return Err(ContractError::SharedCheckoutRequired);
        }
        if candidate.repository == Repository::Adk
            && self.verification_kind != VerificationCheckoutKind::Isolated
        {
            return Err(ContractError::IsolatedCheckoutRequired);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentComponent {
    Toolchain,
    RustTarget,
    Engine,
    ResourcesLock,
    LoreAssets,
    MonolithSubmodule,
    Mcp,
    Editor,
    Pie,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentFact {
    pub identity: String,
    pub version: Option<String>,
    pub path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentObservation {
    pub component: EnvironmentComponent,
    pub observation: Observation<EnvironmentFact>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionEnvironment {
    pub os: String,
    pub checkout: Checkout,
    pub components: Vec<ComponentObservation>,
    pub allocations: Vec<ResourceAllocation>,
}

// Installed engines/assets are environment facts; only contended use is an exclusive resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Host,
    Build,
    Editor,
    SharedCheckout,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceIdentity {
    pub kind: ResourceKind,
    pub host: String,
    pub identity: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceAllocation {
    pub resource: ResourceIdentity,
    pub allocation_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceRequirement {
    pub resource: ResourceIdentity,
    pub availability: Observation<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationPlan {
    pub candidate: Candidate,
    pub stage: Stage,
    pub acceptance: Vec<AcceptanceCondition>,
    pub checks: Vec<VerificationCheck>,
    pub environment_requirements: Vec<EnvironmentComponent>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationCheck {
    pub id: String,
    pub kind: CheckKind,
    pub required: bool,
    pub acceptance_ids: Vec<String>,
}

// Execution checks are distinct from the acceptance condition's VerificationKind classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    Test,
    Build,
    RequiredCi,
    MainIntegration,
    Deployment,
    Operational,
    Editor,
    Pie,
}

impl VerificationPlan {
    pub fn validate(&self) -> Result<(), ContractError> {
        if !self.checks.iter().any(|c| c.required) {
            return Err(ContractError::NoRequiredChecks);
        }
        let mut checks = BTreeSet::new();
        let mut acceptance = BTreeSet::new();
        for ac in &self.acceptance {
            if !acceptance.insert(ac.id.as_str()) {
                return Err(ContractError::DuplicateId(ac.id.clone()));
            }
        }
        for check in &self.checks {
            if check.id.trim().is_empty() || !checks.insert(&check.id) {
                return Err(ContractError::DuplicateId(check.id.clone()));
            }
            for ac in &check.acceptance_ids {
                if !acceptance.contains(ac.as_str()) {
                    return Err(ContractError::UnknownAcceptance(ac.clone()));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Observation<T> {
    Observed {
        evidence: T,
        observed_at: DateTime<Utc>,
    },
    NotYet {
        reason: String,
        observed_at: DateTime<Utc>,
    },
    Unknown {
        reason: String,
        last_observed_at: Option<DateTime<Utc>>,
    },
}

impl<T> Observation<T> {
    pub fn observed_evidence(&self) -> Option<&T> {
        match self {
            Self::Observed { evidence, .. } => Some(evidence),
            Self::NotYet { .. } | Self::Unknown { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObservationTarget {
    Pr {
        repository: Repository,
        number: u64,
    },
    Ci {
        repository: Repository,
        run_id: String,
        head_sha: String,
    },
    Main {
        repository: Repository,
        branch: String,
        target_sha: String,
    },
    Deploy {
        repository: Repository,
        host: String,
        deployment_id: String,
        expected_sha: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetObservation {
    pub target: ObservationTarget,
    pub observation: Observation<ExternalEvidence>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExternalEvidence {
    Pr {
        head_sha: String,
        merge_sha: Option<String>,
        reference: String,
    },
    Ci {
        conclusion: String,
        reference: String,
    },
    Main(MainIntegrationEvidence),
    Deploy {
        actual_sha: String,
        reference: String,
    },
}

impl TargetObservation {
    // Target identity validation does not establish external success.
    pub fn validate_target(&self, requested: &ObservationTarget) -> Result<(), ContractError> {
        if &self.target != requested {
            return Err(ContractError::ObservationTargetMismatch);
        }
        let Some(evidence) = self.observation.observed_evidence() else {
            return Ok(());
        };
        let matches = match (requested, evidence) {
            (ObservationTarget::Pr { .. }, ExternalEvidence::Pr { .. })
            | (ObservationTarget::Ci { .. }, ExternalEvidence::Ci { .. })
            | (ObservationTarget::Deploy { .. }, ExternalEvidence::Deploy { .. }) => true,
            (
                ObservationTarget::Main {
                    repository,
                    branch,
                    target_sha,
                },
                ExternalEvidence::Main(main),
            ) => {
                *repository == main.repository
                    && branch == &main.target_branch
                    && target_sha == &main.target_main_sha
            }
            _ => false,
        };
        if !matches {
            return Err(ContractError::ObservationTargetMismatch);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedTest {
    pub id: String,
    pub check_id: String,
    pub required: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutedTest {
    pub id: String,
    pub check_id: String,
    pub outcome: TestOutcome,
    pub log_refs: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum TestOutcome {
    Passed,
    Failed,
    Skipped { reason: String },
    Unknown { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestEvidence {
    pub execution_id: String,
    pub binding: AttemptBinding,
    pub environment: ExecutionEnvironment,
    pub planned: Vec<PlannedTest>,
    pub actual: Vec<ExecutedTest>,
    pub log_refs: Vec<String>,
}

// The candidate a stored execution verifies, resolved by the server; callers cannot construct it freely.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerResolvedSubject(Candidate);

impl ServerResolvedSubject {
    pub fn bound_candidate(binding: &AttemptBinding) -> Self {
        Self(Candidate {
            repository: binding.repository,
            base_sha: binding.base_sha.clone(),
            head_sha: binding.candidate_sha.clone(),
        })
    }

    pub(super) fn sealed(candidate: Candidate) -> Self {
        Self(candidate)
    }
}

impl TestEvidence {
    // Completeness/provenance only: a complete failed execution remains valid evidence, not a PASS.
    pub fn validate_execution(
        &self,
        plan: &VerificationPlan,
        binding: &AttemptBinding,
    ) -> Result<(), ContractError> {
        self.validate_execution_subject(
            plan,
            binding,
            &ServerResolvedSubject::bound_candidate(binding),
        )
    }

    // Only the candidate link is widened to the server-resolved subject; binding and completeness stay.
    pub fn validate_execution_subject(
        &self,
        plan: &VerificationPlan,
        binding: &AttemptBinding,
        subject: &ServerResolvedSubject,
    ) -> Result<(), ContractError> {
        plan.validate()?;
        if &self.binding != binding || plan.stage != binding.stage {
            return Err(ContractError::BindingMismatch);
        }
        if plan.candidate != subject.0 {
            return Err(ContractError::CandidateMismatch);
        }
        if self.environment.checkout.observed_sha != plan.candidate.head_sha {
            return Err(ContractError::CandidateMismatch);
        }
        if self.execution_id.trim().is_empty() || self.log_refs.is_empty() {
            return Err(ContractError::MissingExecutionIdentity);
        }
        if self.actual.is_empty() {
            return Err(ContractError::ZeroTests);
        }
        let mut actual = BTreeSet::new();
        for test in &self.actual {
            if !actual.insert((&test.check_id, &test.id)) {
                return Err(ContractError::DuplicateId(test.id.clone()));
            }
        }
        let mut planned = BTreeSet::new();
        for test in &self.planned {
            if !planned.insert((&test.check_id, &test.id)) {
                return Err(ContractError::DuplicateId(test.id.clone()));
            }
            if !plan
                .checks
                .iter()
                .any(|c| c.id == test.check_id && c.kind == CheckKind::Test)
            {
                return Err(ContractError::UnknownCheck(test.check_id.clone()));
            }
            if !test.required {
                continue;
            }
            match self
                .actual
                .iter()
                .find(|r| r.id == test.id && r.check_id == test.check_id)
            {
                None => return Err(ContractError::RequiredTestMissing(test.id.clone())),
                Some(ExecutedTest {
                    outcome: TestOutcome::Skipped { .. } | TestOutcome::Unknown { .. },
                    ..
                }) => {
                    return Err(ContractError::RequiredTestUnexecuted(test.id.clone()));
                }
                Some(_) => {}
            }
        }
        for check in plan
            .checks
            .iter()
            .filter(|c| c.required && c.kind == CheckKind::Test)
        {
            if !self
                .planned
                .iter()
                .any(|t| t.check_id == check.id && t.required)
            {
                return Err(ContractError::RequiredTestMissing(check.id.clone()));
            }
        }
        if self.planned.is_empty() {
            return Err(ContractError::ZeroTests);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewExtent {
    Full,
    Delta,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRange {
    pub base_sha: String,
    pub head_sha: String,
    pub extent: ReviewExtent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeArtifact {
    pub node_id: String,
    pub candidate: Candidate,
    pub hashes: DefinitionHashes,
    pub review: ReviewRange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationMethod {
    Merge,
    Squash,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationCheck {
    pub matches: bool,
    pub reference: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeMainMapping {
    pub artifact: NodeArtifact,
    pub main_commits: Vec<String>,
    pub ancestry: Observation<IntegrationCheck>,
    pub content: Observation<IntegrationCheck>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MainIntegrationEvidence {
    pub repository: Repository,
    pub target_branch: String,
    pub target_main_sha: String,
    pub pr_number: u64,
    pub merge_sha: String,
    pub method: IntegrationMethod,
    pub nodes: Vec<NodeMainMapping>,
}

impl MainIntegrationEvidence {
    // Preserve every requested node in grouped/squash results; integration acceptance is checked separately.
    pub fn validate_mappings(&self, requested: &[NodeArtifact]) -> Result<(), ContractError> {
        if requested.is_empty() || self.nodes.len() != requested.len() {
            return Err(ContractError::NodeMappingMismatch);
        }
        let mut ids = BTreeSet::new();
        for mapping in &self.nodes {
            let artifact = &mapping.artifact;
            if !ids.insert(&artifact.node_id)
                || artifact.candidate.repository != self.repository
                || !requested.contains(artifact)
                || mapping.main_commits.is_empty()
            {
                return Err(ContractError::NodeMappingMismatch);
            }
        }
        Ok(())
    }
}

pub trait ProfileAdapter {
    fn workspace(&self, candidate: &Candidate) -> Result<Workspace, ContractError>;
    fn verification_plan(
        &self,
        stage: Stage,
        candidate: &Candidate,
    ) -> Result<VerificationPlan, ContractError>;
    fn resources(&self, stage: Stage) -> Vec<ResourceRequirement>;
    fn observe(&self, target: &ObservationTarget) -> Result<TargetObservation, ContractError>;
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ContractError {
    #[error("candidate differs from the binding or observed checkout")]
    CandidateMismatch,
    #[error("attempt binding or stage differs")]
    BindingMismatch,
    #[error("CookingHeart requires a shared verification checkout distinct from editing")]
    SharedCheckoutRequired,
    #[error("AgentDesk requires an isolated verification checkout")]
    IsolatedCheckoutRequired,
    #[error("verification plan has no required checks")]
    NoRequiredChecks,
    #[error("duplicate or empty ID: {0}")]
    DuplicateId(String),
    #[error("unknown acceptance ID: {0}")]
    UnknownAcceptance(String),
    #[error("unknown test check ID: {0}")]
    UnknownCheck(String),
    #[error("execution ID or log references missing")]
    MissingExecutionIdentity,
    #[error("execution contains no tests")]
    ZeroTests,
    #[error("required test missing: {0}")]
    RequiredTestMissing(String),
    #[error("required test skipped or unknown: {0}")]
    RequiredTestUnexecuted(String),
    #[error("observation belongs to a different target")]
    ObservationTargetMismatch,
    #[error("main integration lost or changed a node artifact mapping")]
    NodeMappingMismatch,
}

#[cfg(test)]
#[path = "profile_tests.rs"]
mod tests;
