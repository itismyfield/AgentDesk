use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::adk_resources::{BuildTokenOutcome, classify_build_token_exit};
use super::candidate_seal::resolved_subject;
use super::model::{Action, ActionState};
use super::profile::{TestOutcome, VerificationPlan};
use super::verification_run::{
    NotSentReason, PhaseInitiation, PhaseKind, VerificationCollection, VerificationRun,
};

pub const GATE_RULE_VERSION: &str = "e1-gate-v1";

// Ordered by precedence: a confirmed quality failure outranks missing evidence, which outranks waiting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Wait,
    Reject,
    QualityFail,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", content = "target", rename_all = "snake_case")]
pub enum GateReason {
    DigestMismatch,
    SourceMismatch,
    BindingMismatch,
    StaleAttempt,
    RoleMismatch,
    StalePin,
    NotEvidence,
    TargetMismatch,
    OriginMismatch(String),
    ResultMismatch(String),
    ResourceWait(String),
    Pending(String),
    ObservationUnknown(String),
    Cancelled(String),
    Interrupted(String),
    PreparationFailed(String),
    Incomplete(String),
    PlanMismatch(String),
    TestFailed(String),
    ExitMismatch(String),
    MissingCheck(String),
    NotIndependent(String),
    MissingFullReview,
    RangeMismatch(String),
    OpenFinding(String),
    UnresolvedFinding(String),
    UnverifiedScope(String),
    InvalidInheritance(String),
    WebGptMismatch(String),
    ReviewIssues(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageAssessment {
    pub verdict: Verdict,
    pub reasons: Vec<GateReason>,
    pub adopted: Vec<String>,
    pub rule_version: String,
}

impl StageAssessment {
    pub fn pass(adopted: Vec<String>) -> Self {
        Self::new(Verdict::Pass, Vec::new(), adopted)
    }

    pub fn of(verdict: Verdict, reason: GateReason) -> Self {
        Self::new(verdict, vec![reason], Vec::new())
    }

    pub(super) fn new(verdict: Verdict, reasons: Vec<GateReason>, adopted: Vec<String>) -> Self {
        Self {
            verdict,
            reasons,
            adopted,
            rule_version: GATE_RULE_VERSION.into(),
        }
    }

    // The strictest part decides; adoption is reported only when every part passed.
    pub fn combine(parts: impl IntoIterator<Item = StageAssessment>) -> Self {
        let mut combined = Self::pass(Vec::new());
        for part in parts {
            combined.verdict = combined.verdict.max(part.verdict);
            combined.reasons.extend(part.reasons);
            combined.adopted.extend(part.adopted);
        }
        if combined.verdict != Verdict::Pass {
            combined.adopted.clear();
        }
        combined
    }
}

// A required check without an assessment is still owed, never an implicit PASS.
pub fn evaluate_stage(
    required: &[String],
    assessed: &BTreeMap<String, StageAssessment>,
) -> StageAssessment {
    if required.is_empty() {
        return StageAssessment::of(Verdict::Reject, GateReason::MissingCheck("required".into()));
    }
    StageAssessment::combine(required.iter().map(|id| {
        assessed.get(id).cloned().unwrap_or_else(|| {
            StageAssessment::of(Verdict::Wait, GateReason::MissingCheck(id.clone()))
        })
    }))
}

// Judges one registered V from its own stored result; actions carry the single phase state.
pub fn assess_verification(
    run: &VerificationRun,
    actions: &BTreeMap<String, Action>,
    plan: &VerificationPlan,
) -> StageAssessment {
    let id = run.origin.execution_id.clone();
    if run.cancellation.is_some() {
        return StageAssessment::of(Verdict::Wait, GateReason::Cancelled(id));
    }
    match &run.collection {
        None => StageAssessment::of(Verdict::Wait, pending_reason(run, actions)),
        Some(VerificationCollection::Interrupted { phase_id, .. }) => {
            StageAssessment::of(Verdict::Wait, GateReason::Interrupted(phase_id.clone()))
        }
        Some(VerificationCollection::PreparationFailed { phase_id, record }) => {
            // Only a plain nonzero exit outside the build-token codes is a confirmed code failure.
            let code_failure = record.signal.is_none()
                && record.exit_code.is_some_and(|code| code != 0)
                && classify_build_token_exit(record.exit_code) == Some(BuildTokenOutcome::Held);
            let reason = GateReason::PreparationFailed(phase_id.clone());
            if code_failure {
                StageAssessment::of(Verdict::QualityFail, reason)
            } else {
                StageAssessment::new(
                    Verdict::Reject,
                    vec![reason, GateReason::Incomplete(phase_id.clone())],
                    Vec::new(),
                )
            }
        }
        Some(VerificationCollection::Executed { evidence, audit }) => {
            let (Some(subject), Some(actual_plan)) = (resolved_subject(run), run.plan.bound())
            else {
                return StageAssessment::of(Verdict::Reject, GateReason::OriginMismatch(id));
            };
            if evidence.execution_id != id {
                return StageAssessment::of(Verdict::Reject, GateReason::OriginMismatch(id));
            }
            if evidence.planned != actual_plan.manifest {
                return StageAssessment::of(Verdict::Reject, GateReason::PlanMismatch(id));
            }
            if let Err(error) =
                evidence.validate_execution_subject(plan, &run.origin.owner_binding, &subject)
            {
                return StageAssessment::of(
                    Verdict::Reject,
                    GateReason::Incomplete(error.to_string()),
                );
            }
            if !audit.complete {
                return StageAssessment::of(Verdict::Reject, GateReason::Incomplete(id));
            }
            let required: BTreeSet<_> = evidence
                .planned
                .iter()
                .filter(|t| t.required)
                .map(|t| (&t.check_id, &t.id))
                .collect();
            let failed: Vec<_> = evidence
                .actual
                .iter()
                .filter(|t| {
                    t.outcome == TestOutcome::Failed && required.contains(&(&t.check_id, &t.id))
                })
                .map(|t| GateReason::TestFailed(t.id.clone()))
                .collect();
            if !failed.is_empty() {
                return StageAssessment::new(Verdict::QualityFail, failed, Vec::new());
            }
            // A fixed audit phase must also have exited cleanly; its disagreement is never a PASS.
            let audited = run.phases.iter().any(|p| p.kind == PhaseKind::Audit);
            if audit.exit_code != Some(0) || (audited && audit.audit_exit_code != Some(0)) {
                return StageAssessment::of(Verdict::Reject, GateReason::ExitMismatch(id));
            }
            StageAssessment::pass(vec![id])
        }
    }
}

fn pending_reason(run: &VerificationRun, actions: &BTreeMap<String, Action>) -> GateReason {
    let id = run.origin.execution_id.clone();
    for phase in &run.phases {
        let unknown = actions
            .get(&phase.action_id)
            .is_none_or(|a| a.state == ActionState::Unknown);
        match phase.initiations.last() {
            _ if unknown => return GateReason::ObservationUnknown(phase.id.clone()),
            Some(PhaseInitiation::Indeterminate { .. }) => {
                return GateReason::ObservationUnknown(phase.id.clone());
            }
            Some(PhaseInitiation::NotSent {
                reason: NotSentReason::ResourceWait { resource },
                ..
            }) => return GateReason::ResourceWait(resource.identity.clone()),
            _ => {}
        }
    }
    GateReason::Pending(id)
}

#[cfg(test)]
#[path = "evidence_gate_tests.rs"]
pub(crate) mod tests;
