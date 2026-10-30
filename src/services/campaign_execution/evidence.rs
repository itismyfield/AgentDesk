use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::evidence_gate::{GATE_RULE_VERSION, GateReason};
use super::model::{AttemptBinding, CampaignState, Role};
use super::profile::{ObservationTarget, TargetObservation};
use super::review_gate::{ReviewEvidence, StagePrerequisite};
use super::verification_run::{Digest, VerificationCollection, VerificationRun, digest_of};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    BoundTui,
    ServerRunner,
    ServerObserver,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStatus {
    Recorded,
    Accepted,
    Rejected,
    Superseded,
}

impl EvidenceStatus {
    // Decisions are made once from Recorded; only accepted evidence can later be superseded.
    pub fn can_become(self, next: EvidenceStatus) -> bool {
        matches!(
            (self, next),
            (Self::Recorded, _) | (Self::Accepted, Self::Superseded)
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportKind {
    Investigation,
    Design,
    Question,
    Handoff,
}

// The public TUI body: no source, owner, status, consumer or authority can be expressed in it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TuiPayload {
    Report {
        topic: ReportKind,
        reference: String,
        digest: Digest,
    },
    OutputClaim {
        output_sha: String,
    },
    Review(ReviewEvidence),
    // Execution IDs and log links only ask the server to read the run; they are never evidence.
    ObservationRequest {
        execution_id: Option<String>,
        links: Vec<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TuiSubmission {
    pub evidence_id: String,
    pub payload: TuiPayload,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "class", content = "body", rename_all = "snake_case")]
pub enum EvidencePayload {
    Submitted(TuiPayload),
    Verification {
        execution_id: String,
        collection: Box<VerificationCollection>,
    },
    External {
        requested: ObservationTarget,
        observation: TargetObservation,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceEnvelope {
    pub evidence_id: String,
    pub payload_digest: Digest,
    pub binding: AttemptBinding,
    pub pin_hash: Digest,
    pub source: EvidenceSource,
    pub payload: EvidencePayload,
    pub recorded_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceDecision {
    pub status: EvidenceStatus,
    pub reasons: Vec<GateReason>,
    pub prerequisites: Vec<String>,
    pub rule_version: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum EvidenceError {
    #[error("evidence ID is empty")]
    EmptyId,
    #[error("verification has no collected result yet")]
    NotCollected,
    #[error("encoding failed")]
    Encoding,
}

impl EvidenceEnvelope {
    fn new(
        evidence_id: String,
        binding: &AttemptBinding,
        pin_hash: &str,
        source: EvidenceSource,
        payload: EvidencePayload,
        recorded_at: DateTime<Utc>,
    ) -> Result<Self, EvidenceError> {
        Ok(Self {
            evidence_id,
            payload_digest: digest_of(&payload).map_err(|_| EvidenceError::Encoding)?,
            binding: binding.clone(),
            pin_hash: pin_hash.into(),
            source,
            payload,
            recorded_at,
        })
    }

    // The binding is the server's session resolution; IDs are namespaced so a TUI cannot squat runner IDs.
    pub fn from_tui(
        submission: TuiSubmission,
        binding: &AttemptBinding,
        pin_hash: &str,
        at: DateTime<Utc>,
    ) -> Result<Self, EvidenceError> {
        if submission.evidence_id.trim().is_empty() {
            return Err(EvidenceError::EmptyId);
        }
        let id = format!("tui:{}:{}", binding.attempt_id, submission.evidence_id);
        let payload = EvidencePayload::Submitted(submission.payload);
        Self::new(id, binding, pin_hash, EvidenceSource::BoundTui, payload, at)
    }

    // Runner evidence keeps the V's original owner even after a Review attempt consumes it.
    pub fn from_runner(run: &VerificationRun, at: DateTime<Utc>) -> Result<Self, EvidenceError> {
        let collection = Box::new(run.collection.clone().ok_or(EvidenceError::NotCollected)?);
        let origin = &run.origin;
        let payload = EvidencePayload::Verification {
            execution_id: origin.execution_id.clone(),
            collection,
        };
        let id = format!("run:{}", origin.execution_id);
        let source = EvidenceSource::ServerRunner;
        Self::new(
            id,
            &origin.owner_binding,
            &origin.pin_hash,
            source,
            payload,
            at,
        )
    }

    pub fn from_observer(
        binding: &AttemptBinding,
        pin_hash: &str,
        requested: ObservationTarget,
        observation: TargetObservation,
        at: DateTime<Utc>,
    ) -> Result<Self, EvidenceError> {
        let payload = EvidencePayload::External {
            requested,
            observation,
        };
        let digest = digest_of(&payload).map_err(|_| EvidenceError::Encoding)?;
        let id = format!("obs:{}:{digest}", binding.attempt_id);
        Self::new(
            id,
            binding,
            pin_hash,
            EvidenceSource::ServerObserver,
            payload,
            at,
        )
    }
}

// Accepted means the source, binding and content contract hold; it is not a stage PASS.
pub fn admit(
    envelope: &EvidenceEnvelope,
    state: &CampaignState,
    current_pin: &str,
    prerequisites: &[StagePrerequisite],
) -> EvidenceDecision {
    let decide = |status, reasons, prerequisites| EvidenceDecision {
        status,
        reasons,
        prerequisites,
        rule_version: GATE_RULE_VERSION.into(),
    };
    let reject = |reason| decide(EvidenceStatus::Rejected, vec![reason], Vec::new());
    if digest_of(&envelope.payload).ok().as_ref() != Some(&envelope.payload_digest) {
        return reject(GateReason::DigestMismatch);
    }
    let Some(run) = state.run.as_ref() else {
        return reject(GateReason::BindingMismatch);
    };
    let binding = &envelope.binding;
    let Some(attempt) = run
        .attempts
        .get(&binding.attempt_id)
        .filter(|a| &a.binding == binding)
    else {
        return reject(GateReason::BindingMismatch);
    };
    if envelope.pin_hash != current_pin {
        return reject(GateReason::StalePin);
    }
    match (envelope.source, &envelope.payload) {
        (EvidenceSource::BoundTui, EvidencePayload::Submitted(payload)) => {
            let current = state.nodes.get(&binding.node_id).is_some_and(|n| {
                n.current_attempt.as_ref() == Some(&binding.attempt_id) && attempt.outcome.is_none()
            });
            if !current {
                return reject(GateReason::StaleAttempt);
            }
            match (payload, binding.role) {
                (TuiPayload::ObservationRequest { .. }, _) => decide(
                    EvidenceStatus::Recorded,
                    vec![GateReason::NotEvidence],
                    Vec::new(),
                ),
                (TuiPayload::Review(_), Role::Implementation)
                | (TuiPayload::OutputClaim { .. }, Role::Review) => {
                    reject(GateReason::RoleMismatch)
                }
                _ => decide(EvidenceStatus::Accepted, Vec::new(), Vec::new()),
            }
        }
        (
            EvidenceSource::ServerRunner,
            EvidencePayload::Verification {
                execution_id,
                collection,
            },
        ) => {
            let Some(v) = run.verifications.get(execution_id) else {
                return reject(GateReason::OriginMismatch(execution_id.clone()));
            };
            if envelope.evidence_id != format!("run:{execution_id}")
                || &v.origin.owner_binding != binding
                || v.origin.pin_hash != envelope.pin_hash
            {
                return reject(GateReason::OriginMismatch(execution_id.clone()));
            }
            if v.collection.as_ref() != Some(collection.as_ref()) {
                return reject(GateReason::ResultMismatch(execution_id.clone()));
            }
            let consumed = prerequisites
                .iter()
                .filter(|p| p.target.matches(v).is_ok())
                .map(|p| p.id.clone())
                .collect();
            decide(EvidenceStatus::Accepted, Vec::new(), consumed)
        }
        (
            EvidenceSource::ServerObserver,
            EvidencePayload::External {
                requested,
                observation,
            },
        ) => match observation.validate_target(requested) {
            Ok(()) => decide(EvidenceStatus::Accepted, Vec::new(), Vec::new()),
            Err(_) => reject(GateReason::TargetMismatch),
        },
        _ => reject(GateReason::SourceMismatch),
    }
}

#[cfg(test)]
#[path = "evidence_tests.rs"]
mod tests;
