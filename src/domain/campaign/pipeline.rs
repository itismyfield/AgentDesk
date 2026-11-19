use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::pipeline_fields::PipelineField;
use super::verification_run::{Digest, digest_of};
use super::workflow_options::ApprovalGate;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineDefinition {
    pub schema_version: u32,
    pub vocabulary_version: u32,
    pub evaluator_version: u32,
    pub registry_version: u32,
    pub start: String,
    pub states: Vec<PipelineState>,
    pub transitions: Vec<PipelineTransition>,
    #[serde(default)]
    pub events: Vec<PipelineEvent>,
    #[serde(default)]
    pub policy: PipelinePolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_version: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineKind {
    AgentTask,
    HumanDecision,
    AutoCheck,
    AutoAction,
    Wait,
    EndSuccess,
    EndFailure,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineState {
    pub id: String,
    pub kind: PipelineKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor: Option<PipelineExecutor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<PipelineTimeout>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineExecutor {
    pub id: String,
    pub version: u32,
    #[serde(default)]
    pub parameters: PipelineParameters,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineParameters {
    #[serde(default)]
    pub effect_gates: BTreeSet<ApprovalGate>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineTimeout {
    pub after_seconds: u64,
    pub transition_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineTransition {
    pub id: String,
    pub from: String,
    pub to: String,
    pub priority: u32,
    pub condition: PipelineCondition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<PipelineRetry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<ApprovalGate>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineRetry {
    pub budget_id: String,
    pub max: u32,
    pub then_to: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PipelineCondition {
    All(PipelineAll),
    Compare(PipelineComparison),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineAll {
    pub all: Vec<PipelineCondition>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineComparison {
    pub field: PipelineField,
    pub op: ConditionOperator,
    pub value: ConditionValue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionOperator {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ConditionValue {
    Boolean(bool),
    Integer(i64),
    String(String),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineEvent {
    pub id: String,
    pub state_id: String,
    #[serde(rename = "type")]
    pub event_type: PipelineEventType,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PipelineEventType {
    #[serde(rename = "evidence.accepted")]
    EvidenceAccepted,
    #[serde(rename = "decision.recorded")]
    DecisionRecorded,
    #[serde(rename = "action.reconciled")]
    ActionReconciled,
    #[serde(rename = "timer.expired")]
    TimerExpired,
    #[serde(rename = "dependency.satisfied")]
    DependencySatisfied,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PipelinePolicy {
    pub design_enabled: bool,
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Serialize)]
struct ExecutionState<'a> {
    id: &'a str,
    kind: PipelineKind,
    executor: Option<&'a PipelineExecutor>,
    owner: Option<&'a str>,
    instructions: Option<&'a str>,
    timeout: Option<&'a PipelineTimeout>,
}

impl PipelineDefinition {
    /// Hashes the graph's execution inputs independently of definition and binding digests.
    pub fn execution_hash(&self) -> Result<Digest, serde_json::Error> {
        let mut states: Vec<_> = self
            .states
            .iter()
            .map(|state| ExecutionState {
                id: &state.id,
                kind: state.kind,
                executor: state.executor.as_ref(),
                owner: state.owner.as_deref(),
                instructions: state.instructions.as_deref(),
                timeout: state.timeout.as_ref(),
            })
            .collect();
        states.sort_unstable();
        let mut transitions = self.transitions.clone();
        for transition in &mut transitions {
            transition.condition.normalize();
        }
        transitions.sort_unstable();
        let mut events: Vec<_> = self.events.iter().collect();
        events.sort_unstable();
        digest_of(&(
            "campaign-pipeline-execution",
            self.schema_version,
            self.vocabulary_version,
            self.evaluator_version,
            self.registry_version,
            &self.start,
            &states,
            &transitions,
            &events,
            &self.policy,
        ))
    }
}

impl PipelineCondition {
    fn normalize(&mut self) {
        if let Self::All(group) = self {
            for condition in &mut group.all {
                condition.normalize();
            }
            group.all.sort_unstable();
        }
    }
}

#[cfg(test)]
#[path = "pipeline_tests.rs"]
mod tests;
