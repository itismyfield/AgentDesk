use serde::{Deserialize, Serialize};

use super::pipeline::{ConditionOperator, PipelineExecutor, PipelineKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionType {
    Boolean,
    Integer,
    Enum,
    String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionSource {
    Runtime,
    Timer,
    Event,
    Policy,
    Requirements,
    AgentReport,
    WorkSeal,
    Review,
    Verification,
    CiObservation,
    TlcObservation,
    ActionReceipt,
    PrObservation,
    MainObservation,
    DeploymentObservation,
    Decision,
    Acceptance,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectBinding {
    /// The current run, node, series and visit.
    Visit,
    /// The same run, node, series and candidate, with an exact current or inherited producer.
    CandidateProducer,
    GraphPin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    CurrentVisit,
    CurrentCandidate,
    Pinned,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ConditionFieldSpec {
    pub field: PipelineField,
    #[serde(rename = "type")]
    pub value_type: ConditionType,
    pub operators: &'static [ConditionOperator],
    pub enum_values: &'static [&'static str],
    pub source: ConditionSource,
    pub subject_binding: SubjectBinding,
    pub freshness: Freshness,
    pub available: bool,
}

#[derive(Clone, Copy)]
enum Capability {
    Common,
    Requirements,
    Agent,
    Work,
    Review,
    Verification,
    Landing,
    Action,
    FinalCheck,
    Decision,
    Wait,
    Completion,
}

impl Capability {
    fn available(self, kind: PipelineKind, executor: Option<&PipelineExecutor>) -> bool {
        use PipelineKind::{AgentTask, AutoAction, AutoCheck, HumanDecision, Wait};

        let registered = executor
            .filter(|executor| executor.version == 1)
            .map(|executor| (kind, executor.id.as_str()));
        let landing = registered == Some((AutoAction, "r8.landing"));
        let main_check = registered == Some((AutoCheck, "r8.main_check"));
        let final_check = registered == Some((AutoAction, "r8.final_check"));
        match self {
            Self::Common => true,
            Self::Requirements => registered == Some((AutoCheck, "r8.requirements")),
            Self::Agent => kind == AgentTask,
            Self::Work => registered == Some((AgentTask, "r8.work")),
            Self::Review => {
                landing
                    || matches!(
                        registered,
                        Some((AgentTask, "r8.review" | "r8.design_review"))
                    )
            }
            Self::Verification => {
                landing
                    || main_check
                    || final_check
                    || matches!(registered, Some((AgentTask, "r8.work" | "r8.review")))
            }
            Self::Landing => landing,
            Self::Action => landing || final_check,
            Self::FinalCheck => final_check,
            Self::Decision => kind == HumanDecision,
            Self::Wait => kind == Wait,
            Self::Completion => landing || main_check || final_check,
        }
    }
}

const EQ_NE: &[ConditionOperator] = &[ConditionOperator::Eq, ConditionOperator::Ne];
const INTEGER_OPS: &[ConditionOperator] = &[
    ConditionOperator::Eq,
    ConditionOperator::Ne,
    ConditionOperator::Lt,
    ConditionOperator::Le,
    ConditionOperator::Gt,
    ConditionOperator::Ge,
];

macro_rules! fields {
    ($($field:ident => $name:literal, $kind:ident, $values:expr, $source:ident, $binding:ident, $freshness:ident, $capability:ident;)*) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
        pub enum PipelineField {
            $(#[serde(rename = $name)] $field,)*
        }

        impl PipelineField {
            pub const ALL: &'static [Self] = &[$(Self::$field,)*];

            /// Describes the wire field and where the vocabulary admits it.
            pub fn spec(self, kind: PipelineKind, executor: Option<&PipelineExecutor>) -> ConditionFieldSpec {
                match self {
                    $(Self::$field => {
                        let value_type = ConditionType::$kind;
                        ConditionFieldSpec {
                            field: self,
                            value_type,
                            operators: if value_type == ConditionType::Integer { INTEGER_OPS } else { EQ_NE },
                            enum_values: $values,
                            source: ConditionSource::$source,
                            subject_binding: SubjectBinding::$binding,
                            freshness: Freshness::$freshness,
                            available: Capability::$capability.available(kind, executor),
                        }
                    },)*
                }
            }
        }
    };
}

fields! {
    StateOutcome => "state.outcome", Enum, &["succeeded", "failed", "quality_failed"], Runtime, Visit, CurrentVisit, Common;
    TimerExpired => "timer.expired", Boolean, &[], Timer, Visit, CurrentVisit, Common;
    EventType => "event.type", Enum, &["evidence.accepted", "decision.recorded", "action.reconciled", "timer.expired", "dependency.satisfied"], Event, Visit, CurrentVisit, Common;
    PolicyDesignEnabled => "policy.design_enabled", Boolean, &[], Policy, GraphPin, Pinned, Common;
    RequirementsReady => "requirements.ready", Boolean, &[], Requirements, Visit, CurrentVisit, Requirements;
    AgentReportResult => "agent.report.result", Enum, &["pass", "fail"], AgentReport, Visit, CurrentVisit, Agent;
    AgentReportArtifactPresent => "agent.report.artifact_present", Boolean, &[], AgentReport, Visit, CurrentVisit, Agent;
    WorkSealValid => "work.seal_valid", Boolean, &[], WorkSeal, CandidateProducer, CurrentCandidate, Work;
    VerificationReviewStartPassed => "verification.review_start_passed", Boolean, &[], Verification, CandidateProducer, CurrentCandidate, Work;
    ReviewVerdict => "review.verdict", Enum, &["clean", "issues"], Review, CandidateProducer, CurrentCandidate, Review;
    ReviewP0Count => "review.p0_count", Integer, &[], Review, CandidateProducer, CurrentCandidate, Review;
    ReviewP1Count => "review.p1_count", Integer, &[], Review, CandidateProducer, CurrentCandidate, Review;
    ReviewScopeComplete => "review.scope_complete", Boolean, &[], Review, CandidateProducer, CurrentCandidate, Review;
    VerificationVerdict => "verification.verdict", Enum, &["pass", "wait", "reject", "quality_fail"], Verification, CandidateProducer, CurrentCandidate, Verification;
    CiRequiredResult => "ci.required_result", Enum, &["pass", "fail", "pending"], CiObservation, CandidateProducer, CurrentCandidate, Verification;
    CiHeadMatches => "ci.head_matches", Boolean, &[], CiObservation, CandidateProducer, CurrentCandidate, Verification;
    TlcMergeGatePassed => "tlc.merge_gate_passed", Boolean, &[], TlcObservation, CandidateProducer, CurrentCandidate, Verification;
    ActionResult => "action.result", Enum, &["succeeded", "failed", "cancelled"], ActionReceipt, Visit, CurrentVisit, Action;
    PrMerged => "pr.merged", Boolean, &[], PrObservation, CandidateProducer, CurrentCandidate, Landing;
    MainContainsCandidate => "main.contains_candidate", Boolean, &[], MainObservation, CandidateProducer, CurrentCandidate, Completion;
    DeploymentObserved => "deployment.observed", Boolean, &[], DeploymentObservation, CandidateProducer, CurrentCandidate, FinalCheck;
    DecisionResult => "decision.result", Enum, &["approved", "rejected"], Decision, Visit, CurrentVisit, Decision;
    DecisionChoice => "decision.choice", String, &[], Decision, Visit, CurrentVisit, Decision;
    EventReceived => "event.received", Boolean, &[], Event, Visit, CurrentVisit, Wait;
    AcceptanceComplete => "acceptance.complete", Boolean, &[], Acceptance, CandidateProducer, CurrentCandidate, Completion;
    EvidenceCurrent => "evidence.current", Boolean, &[], Acceptance, CandidateProducer, CurrentCandidate, Completion;
    ActionsSettled => "actions.settled", Boolean, &[], Runtime, Visit, CurrentVisit, Completion;
}
