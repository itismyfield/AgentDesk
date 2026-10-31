use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::candidate_seal::CandidateSeal;
use super::evidence_gate::{GateReason, StageAssessment, Verdict};
use super::model::{ActionState, AttemptBinding, HostGate, Role, Run, SessionIdentity, Stage};
use super::profile::{ReviewExtent, ReviewRange};
use super::verification_run::{
    AuthoritySource, BoundSubject, Digest, VerificationCollection, VerificationRun,
    VerificationStartAuthority, VerificationSubjectRef, digest_of,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    Clean,
    Issues,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub id: String,
    pub open: bool,
    pub resolution: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InheritedEvidence {
    pub evidence_id: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebGptResult {
    pub result_ref: String,
    pub target_sha: String,
}

// Reviewer identity is not in the body; the gate takes it from the server's session records.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewEvidence {
    pub range: ReviewRange,
    pub verdict: ReviewVerdict,
    pub findings: Vec<Finding>,
    pub unverified: Vec<String>,
    pub inherited: Vec<InheritedEvidence>,
    pub webgpt: Vec<WebGptResult>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorIdentity {
    pub attempt_id: String,
    pub session: SessionIdentity,
    pub native_conversation: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRecord {
    pub evidence_id: String,
    pub reviewer: ActorIdentity,
    pub review: ReviewEvidence,
}

impl ActorIdentity {
    // A renamed session or a fresh nonce on the same native conversation is still the same actor.
    fn independent_of(&self, other: &ActorIdentity) -> bool {
        self.attempt_id != other.attempt_id
            && self.native_conversation != other.native_conversation
            && (&self.session.session, &self.session.incarnation)
                != (&other.session.session, &other.session.incarnation)
    }
}

// A full review of the seal base plus a contiguous delta chain must reach the sealed output.
pub fn evaluate_review(
    seal: &CandidateSeal,
    implementer: &ActorIdentity,
    records: &[ReviewRecord],
    accepted: &BTreeSet<String>,
    required_scopes: &[String],
) -> StageAssessment {
    let mut reasons = Vec::new();
    let independent: Vec<_> = records
        .iter()
        .filter(|r| {
            let ok = r.reviewer.attempt_id != seal.owner_attempt
                && r.reviewer.independent_of(implementer);
            if !ok {
                reasons.push(GateReason::NotIndependent(r.evidence_id.clone()));
            }
            ok
        })
        .collect();
    let Some(start) = independent.iter().rposition(|r| {
        r.review.range.extent == ReviewExtent::Full && r.review.range.base_sha == seal.base_sha
    }) else {
        reasons.push(GateReason::MissingFullReview);
        return StageAssessment::new(Verdict::Reject, reasons, Vec::new());
    };
    let mut chain = vec![independent[start]];
    for record in &independent[start + 1..] {
        let range = &record.review.range;
        if range.extent == ReviewExtent::Delta
            && range.base_sha == chain[chain.len() - 1].review.range.head_sha
        {
            chain.push(record);
        }
    }
    let last = chain[chain.len() - 1];
    if last.review.range.head_sha != seal.output_sha {
        reasons.push(GateReason::RangeMismatch(last.evidence_id.clone()));
        return StageAssessment::new(Verdict::Reject, reasons, Vec::new());
    }
    // Findings keep their latest state across every independent review, so a fresh full review cannot drop one.
    // Any independent review keeps a finding open; only a record in the adopted chain may resolve it.
    let adopted: BTreeSet<_> = chain.iter().map(|r| &r.evidence_id).collect();
    let mut findings = BTreeMap::new();
    for record in &independent {
        let resolves = adopted.contains(&record.evidence_id);
        for finding in &record.review.findings {
            if finding.open || resolves {
                findings.insert(&finding.id, finding);
            }
        }
    }
    for record in &chain {
        let review = &record.review;
        reasons.extend(
            review
                .unverified
                .iter()
                .filter(|scope| required_scopes.contains(scope))
                .map(|scope| GateReason::UnverifiedScope(scope.clone())),
        );
        reasons.extend(
            review
                .inherited
                .iter()
                .filter(|i| !accepted.contains(&i.evidence_id) || i.reason.trim().is_empty())
                .map(|i| GateReason::InvalidInheritance(i.evidence_id.clone())),
        );
        reasons.extend(
            review
                .webgpt
                .iter()
                .filter(|w| w.target_sha != review.range.head_sha || w.result_ref.trim().is_empty())
                .map(|w| GateReason::WebGptMismatch(w.result_ref.clone())),
        );
    }
    for finding in findings.values() {
        if finding.open {
            reasons.push(GateReason::OpenFinding(finding.id.clone()));
        } else if finding
            .resolution
            .as_deref()
            .is_none_or(|r| r.trim().is_empty())
        {
            reasons.push(GateReason::UnresolvedFinding(finding.id.clone()));
        }
    }
    if last.review.verdict == ReviewVerdict::Issues {
        reasons.push(GateReason::ReviewIssues(last.evidence_id.clone()));
    }
    let verdict = reasons
        .iter()
        .map(|reason| match reason {
            GateReason::OpenFinding(_) | GateReason::ReviewIssues(_) => Verdict::QualityFail,
            GateReason::NotIndependent(_) => Verdict::Pass,
            _ => Verdict::Reject,
        })
        .max()
        .unwrap_or(Verdict::Pass);
    let adopted = match verdict {
        Verdict::Pass => chain.iter().map(|r| r.evidence_id.clone()).collect(),
        _ => Vec::new(),
    };
    StageAssessment::new(verdict, reasons, adopted)
}

// Exact reference to one registered V; same SHA, newer IDs or another host/spec never match.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionRef {
    pub execution_id: String,
    pub owner_attempt: String,
    pub binding_digest: Digest,
    pub series: u32,
    pub repair_cycle: u32,
    pub registered_node_epoch: u64,
    pub pin_hash: Digest,
    pub spec_hash: Digest,
    pub check_id: String,
    pub gate: HostGate,
    pub phase_ids: Vec<String>,
    pub subject: Option<BoundSubject>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "code", content = "field", rename_all = "snake_case")]
pub enum PrerequisiteError {
    #[error("verification differs from the exact reference: {0}")]
    OriginMismatch(String),
    #[error("consumer is not this node's Review attempt")]
    ConsumerMismatch,
    #[error("verification was cancelled")]
    Cancelled,
    #[error("encoding failed")]
    Encoding,
}

impl ExecutionRef {
    pub fn of(
        run: &VerificationRun,
        seal: Option<&CandidateSeal>,
    ) -> Result<Self, PrerequisiteError> {
        let o = &run.origin;
        if let Some(seal) = seal
            && (seal.owner_attempt != o.owner_attempt || seal.binding_digest != o.binding_digest)
        {
            return Err(PrerequisiteError::OriginMismatch("seal".into()));
        }
        if seal.is_some_and(|s| s.pin_hash != o.pin_hash) {
            return Err(PrerequisiteError::OriginMismatch("pin".into()));
        }
        let reference = Self {
            execution_id: o.execution_id.clone(),
            owner_attempt: o.owner_attempt.clone(),
            binding_digest: o.binding_digest.clone(),
            series: o.series,
            repair_cycle: o.repair_cycle,
            registered_node_epoch: o.registered_node_epoch,
            pin_hash: o.pin_hash.clone(),
            spec_hash: o.spec_hash.clone(),
            check_id: o.check_id.clone(),
            gate: o.gate.clone(),
            phase_ids: o.phase_ids.clone(),
            subject: seal.map(CandidateSeal::bound_subject),
        };
        reference.matches(run)?;
        Ok(reference)
    }

    pub fn matches(&self, run: &VerificationRun) -> Result<(), PrerequisiteError> {
        let o = &run.origin;
        let fields = [
            ("execution_id", self.execution_id == o.execution_id),
            ("owner", self.owner_attempt == o.owner_attempt),
            ("binding", self.binding_digest == o.binding_digest),
            (
                "cycle",
                (self.series, self.repair_cycle) == (o.series, o.repair_cycle),
            ),
            (
                "epoch",
                self.registered_node_epoch == o.registered_node_epoch,
            ),
            ("pin", self.pin_hash == o.pin_hash),
            (
                "spec",
                self.spec_hash == o.spec_hash && self.check_id == o.check_id,
            ),
            ("gate", self.gate == o.gate),
            ("phases", self.phase_ids == o.phase_ids),
        ];
        if let Some((field, _)) = fields.iter().find(|(_, same)| !same) {
            return Err(PrerequisiteError::OriginMismatch((*field).into()));
        }
        // A pinned seal fixes the whole subject; without one only the original owner may fill it.
        let subject_ok = match (run.subject.bound(), &self.subject) {
            (None, _) => true,
            (Some(bound), Some(expected)) => bound == expected,
            (Some(bound), None) => match &bound.reference {
                VerificationSubjectRef::SealedOutput { owner_attempt, .. }
                | VerificationSubjectRef::BoundCandidate {
                    attempt_id: owner_attempt,
                } => owner_attempt == &o.owner_attempt,
            },
        };
        if !subject_ok {
            return Err(PrerequisiteError::OriginMismatch("subject".into()));
        }
        Ok(())
    }
}

// Fixed in R's start transaction: R consumes this exact V's result and may start its unstarted phases.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagePrerequisite {
    pub id: String,
    pub target: ExecutionRef,
    pub consumer_attempt: String,
    pub consumer_binding_digest: Digest,
    pub consumer_stage: Stage,
    pub consumer_node_epoch: u64,
    pub run_control_version: u64,
}

impl StagePrerequisite {
    pub fn bind(
        run: &VerificationRun,
        seal: &CandidateSeal,
        consumer: &AttemptBinding,
        consumer_node_epoch: u64,
        run_control_version: u64,
    ) -> Result<Self, PrerequisiteError> {
        let o = &run.origin;
        if consumer.role != Role::Review
            || consumer.stage != Stage::Review
            || consumer.attempt_id == o.owner_attempt
            || (&consumer.run_id, &consumer.node_id, consumer.series)
                != (&o.run_id, &o.node_id, o.series)
        {
            return Err(PrerequisiteError::ConsumerMismatch);
        }
        if run.cancellation.is_some() {
            return Err(PrerequisiteError::Cancelled);
        }
        Ok(Self {
            id: format!("{}:{}", consumer.attempt_id, o.execution_id),
            target: ExecutionRef::of(run, Some(seal))?,
            consumer_attempt: consumer.attempt_id.clone(),
            consumer_binding_digest: digest_of(consumer)
                .map_err(|_| PrerequisiteError::Encoding)?,
            consumer_stage: consumer.stage,
            consumer_node_epoch,
            run_control_version,
        })
    }

    pub fn authority(&self) -> VerificationStartAuthority {
        VerificationStartAuthority {
            execution_id: self.target.execution_id.clone(),
            original_binding_digest: self.target.binding_digest.clone(),
            consumer_attempt: self.consumer_attempt.clone(),
            consumer_binding_digest: self.consumer_binding_digest.clone(),
            consumer_stage: self.consumer_stage,
            consumer_node_epoch: self.consumer_node_epoch,
            run_control_version: self.run_control_version,
            source: AuthoritySource::ReviewPrerequisite {
                prerequisite_id: self.id.clone(),
            },
        }
    }

    // Result consumption is checked apart from start authority; a late result never widens the target.
    pub fn consumes(
        &self,
        consumer: &AttemptBinding,
        run: &VerificationRun,
    ) -> Result<(), PrerequisiteError> {
        if consumer.attempt_id != self.consumer_attempt
            || digest_of(consumer).ok().as_ref() != Some(&self.consumer_binding_digest)
        {
            return Err(PrerequisiteError::ConsumerMismatch);
        }
        self.target.matches(run)
    }
}

// R's own review result is never final while a fixed prerequisite is still owed or failed.
pub fn evaluate_review_stage(
    review: StageAssessment,
    prerequisites: impl IntoIterator<Item = StageAssessment>,
) -> StageAssessment {
    StageAssessment::combine(std::iter::once(review).chain(prerequisites))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumeDisposition {
    Reuse,
    Replace,
    Blocked,
}

// Input for a resumed Review: reuse a collected exact V, replace a cancelled one, wait on unreconciled phases.
pub fn resume_disposition(
    v: &VerificationRun,
    seal: &CandidateSeal,
    run: &Run,
) -> Result<ResumeDisposition, PrerequisiteError> {
    ExecutionRef::of(v, Some(seal))?;
    let consumable = matches!(v.collection, Some(VerificationCollection::Executed { .. }));
    if consumable && v.cancellation.is_none() {
        return Ok(ResumeDisposition::Reuse);
    }
    let unreconciled = v.phases.iter().any(|p| {
        run.actions.get(&p.action_id).is_none_or(|a| {
            matches!(
                a.state,
                ActionState::Claimed
                    | ActionState::StartPending
                    | ActionState::Started
                    | ActionState::Unknown
            )
        })
    });
    let ended = v.cancellation.is_some() || v.collection.is_some();
    Ok(match (unreconciled, ended) {
        (true, _) => ResumeDisposition::Blocked,
        (false, true) => ResumeDisposition::Replace,
        (false, false) => ResumeDisposition::Reuse,
    })
}

#[cfg(test)]
#[path = "review_gate_tests.rs"]
mod tests;
