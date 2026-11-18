use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Transaction};

use crate::db::server_campaigns::{
    self as store, CommandOutcome, ReceiptResponse, StoreError, command_transaction::StoreRequest,
};
use crate::domain::campaign::model::ReviewedCandidate;

use super::commands;
use super::control::{command_fingerprint, observe_action, validate_new_start};
use super::evidence::{EvidenceEnvelope, EvidencePayload, EvidenceStatus, admit};
use super::evidence_gate::{GateReason, StageAssessment, Verdict};
use super::model::*;
use super::profile::{ReviewExtent, ReviewRange};
use super::review_dispatch::{ReviewError, WebGptReply};
use super::review_gate::*;
use super::stage_fsm::{Transition, accept_stage_result};

pub(super) async fn record(
    pool: &PgPool,
    campaign: &str,
    action_id: &str,
    fact: ReviewFact,
) -> Result<(), ReviewError> {
    let mut tx = pool.begin().await.map_err(StoreError::from)?;
    let state = store::get_for_worker(&mut tx, campaign)
        .await?
        .ok_or(ReviewError::Binding)?;
    send(
        tx,
        campaign,
        &state,
        Command::RecordReview {
            action_id: action_id.into(),
            fact: Box::new(fact),
        },
    )
    .await
}

pub(super) async fn send(
    tx: Transaction<'_, Postgres>,
    campaign: &str,
    state: &CampaignState,
    command: Command,
) -> Result<(), ReviewError> {
    let key = format!(
        "review:{}:{}",
        state.event_sequence,
        command_fingerprint(&command).map_err(StoreError::from)?
    );
    let (tx, reply) = commands::apply_in(
        tx,
        campaign,
        &key,
        Utc::now(),
        StoreRequest::Command(command),
        |outcome| {
            Ok(ReceiptResponse {
                status: 200,
                body: serde_json::to_value(outcome)?,
            })
        },
    )
    .await?;
    tx.commit().await.map_err(StoreError::from)?;
    match serde_json::from_value(reply.response.body).map_err(StoreError::from)? {
        CommandOutcome::Decided(Ok(_)) => Ok(()),
        CommandOutcome::Decided(Err(rejection)) => Err(ReviewError::Rejected(rejection)),
        CommandOutcome::NotFound => Err(ReviewError::Binding),
    }
}

fn history<'a>(run: &'a Run, action: &Action, pin: &str) -> Vec<&'a ReviewJob> {
    let mut ordered: Vec<_> = run
        .reviews
        .iter()
        .filter_map(|(id, job)| {
            let other = &run.actions.get(id)?.binding;
            (id != &action.id
                && other.node_id == action.binding.node_id
                && other.series == action.binding.series
                && other.base_sha == action.binding.base_sha
                && job.request.pin_hash == pin
                && matches!(job.status, ReviewStatus::Received { .. }))
            .then_some(job)
        })
        .collect();
    ordered.sort_by_key(|job| job.ordered_at);
    ordered
}

fn current_candidate<'a>(
    run: &'a Run,
    action: &Action,
) -> Option<(&'a ReviewedCandidate, &'a Attempt)> {
    let candidate = run
        .fences
        .get(&action.binding.node_id)?
        .candidate
        .as_ref()?;
    let owner = run.attempts.get(&candidate.work_attempt)?;
    (action.kind == ActionKind::Launch
        && action.binding.role == Role::Review
        && action.binding.stage == Stage::Review
        && action.target == action.binding.session.gate
        && action.binding.candidate_sha == candidate.head_sha
        && action.binding.base_sha == candidate.base_sha)
        .then_some((candidate, owner))
}

pub(super) fn request(
    action: &Action,
    pin_hash: String,
    profile_hash: String,
    implementer: ActorIdentity,
    reviewer: ActorIdentity,
    delta_base: Option<String>,
    required_scopes: Vec<String>,
) -> ReviewRequest {
    let range = ReviewRange {
        extent: if delta_base.is_some() {
            ReviewExtent::Delta
        } else {
            ReviewExtent::Full
        },
        base_sha: delta_base.unwrap_or_else(|| action.binding.base_sha.clone()),
        head_sha: action.binding.candidate_sha.clone(),
    };
    ReviewRequest {
        range,
        pin_hash,
        profile_hash,
        implementer,
        reviewer,
        required_scopes: required_scopes.into_iter().collect(),
    }
}

fn matches_request(run: &Run, action: &Action, request: &ReviewRequest) -> bool {
    let Some((candidate, work)) = current_candidate(run, action) else {
        return false;
    };
    let range = &request.range;
    let base = match range.extent {
        ReviewExtent::Full => candidate.base_sha.as_str(),
        ReviewExtent::Delta => {
            let records = history(run, action, &candidate.pin_hash);
            let Some(prior) = records.last() else {
                return false;
            };
            if !records
                .iter()
                .any(|job| job.request.range.extent == ReviewExtent::Full)
                || prior.request.range.head_sha == range.head_sha
            {
                return false;
            }
            prior.request.range.head_sha.as_str()
        }
    };
    range.base_sha == base
        && range.head_sha == candidate.head_sha
        && request.pin_hash == candidate.pin_hash
        && !request.profile_hash.trim().is_empty()
        && request.implementer.attempt_id == work.binding.attempt_id
        && request.implementer.session == work.binding.session
        && request.reviewer.attempt_id == action.binding.attempt_id
        && request.reviewer.session == action.binding.session
        && request.reviewer.independent_of(&request.implementer)
        && request
            .required_scopes
            .iter()
            .all(|scope| !scope.trim().is_empty())
}

fn failed(reason: &str, uncertain: bool) -> ReviewFact {
    ReviewFact::Failed {
        reason: reason.into(),
        uncertain,
    }
}

pub(super) fn answer(
    request: &ReviewRequest,
    reply: Option<WebGptReply>,
    expected_receipt: &str,
    deadline: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Option<ReviewFact> {
    if now >= deadline {
        return Some(failed("review reply timed out", true));
    }
    match reply? {
        WebGptReply::Pending => None,
        WebGptReply::Unavailable => Some(failed("WebGPT disconnected while collecting", true)),
        WebGptReply::Refused => Some(failed("review was refused", false)),
        WebGptReply::Output { receipt, body } if receipt == expected_receipt => {
            let Some((json, verdict)) = body.trim().rsplit_once('\n') else {
                return Some(failed("empty or unstructured review output", false));
            };
            let review: ReviewEvidence = match serde_json::from_str(json) {
                Ok(value) => value,
                Err(_) => return Some(failed("invalid review output", false)),
            };
            let marker = match review.verdict {
                ReviewVerdict::Clean => "VERDICT: CLEAN",
                ReviewVerdict::Issues => "VERDICT: ISSUES",
            };
            if !matches_reply(request, &review) {
                return Some(failed(
                    "review did not match the dispatched range or finding IDs",
                    false,
                ));
            }
            if verdict.trim() != marker {
                return Some(failed("review verdict did not match the response", false));
            }
            Some(ReviewFact::Reply {
                receipt,
                review: Box::new(review),
            })
        }
        WebGptReply::Output { .. } => {
            Some(failed("reply receipt did not match the sent review", true))
        }
    }
}

fn matches_reply(request: &ReviewRequest, review: &ReviewEvidence) -> bool {
    let ids: std::collections::BTreeSet<_> =
        review.findings.iter().map(|finding| &finding.id).collect();
    review.range == request.range
        && ids.len() == review.findings.len()
        && ids.iter().all(|id| !id.trim().is_empty())
        && (review.verdict == ReviewVerdict::Clean
            || review.findings.iter().any(|finding| finding.open))
}

fn assessment(run: &Run, action: &Action, job: &ReviewJob) -> Option<StageAssessment> {
    let (candidate, _) = current_candidate(run, action)?;
    let seal = run.seals.get(&candidate.work_attempt)?;
    let ReviewStatus::Received { record } = &job.status else {
        return None;
    };
    let mut records: Vec<_> = history(run, action, &candidate.pin_hash)
        .iter()
        .filter_map(|prior| match &prior.status {
            ReviewStatus::Received { record } => Some((**record).clone()),
            _ => None,
        })
        .collect();
    let prior_evidence = records
        .iter()
        .map(|record| record.evidence_id.clone())
        .collect();
    records.push((**record).clone());
    let review = evaluate_review(
        seal,
        &job.request.implementer,
        &records,
        &prior_evidence,
        &[],
    );
    let scope_result = |name: &String| {
        let Some(test) = job.scope_tests.get(name) else {
            return StageAssessment::of(Verdict::Wait, GateReason::UnverifiedScope(name.clone()));
        };
        match test.outcome {
            ScopeOutcome::Passed => StageAssessment::pass(vec![test.result_ref.clone()]),
            ScopeOutcome::FeatureFailed => {
                StageAssessment::of(Verdict::QualityFail, GateReason::TestFailed(name.clone()))
            }
            ScopeOutcome::Unavailable => {
                StageAssessment::of(Verdict::Wait, GateReason::UnverifiedScope(name.clone()))
            }
        }
    };
    Some(evaluate_review_stage(
        review,
        job.request.required_scopes.iter().map(scope_result),
    ))
}

fn scope_valid(test: &ScopeTest, request: &ReviewRequest) -> bool {
    test.head_sha == request.range.head_sha
        && request.required_scopes.contains(&test.scope)
        && !test.result_ref.trim().is_empty()
        && test.failed <= test.executed
        && test.executed <= test.selected
        && match test.outcome {
            ScopeOutcome::Passed => {
                test.selected > 0 && test.executed == test.selected && test.failed == 0
            }
            ScopeOutcome::FeatureFailed => test.failed > 0,
            ScopeOutcome::Unavailable => true,
        }
}

fn rejection() -> Rejection {
    Rejection::new(RejectionCode::StaleBinding)
}

pub(super) fn decide(
    state: &mut CampaignState,
    action_id: &str,
    fact: &ReviewFact,
    now: DateTime<Utc>,
    evidence: &mut Vec<EvidenceWrite>,
) -> Transition {
    let run = state.run.as_ref().ok_or_else(rejection)?;
    let action = run.actions.get(action_id).ok_or_else(rejection)?.clone();
    let existing = run.reviews.get(action_id);
    let changed = || {
        Some(EventKind::ActionChanged {
            action_id: action_id.into(),
        })
    };
    if let ReviewFact::Prepare(request) = fact {
        if let Some(job) = existing {
            return if &job.request == request.as_ref() {
                Ok(None)
            } else {
                Err(rejection())
            };
        }
        validate_new_start(state, &action.binding)?;
        if action.state != ActionState::Planned || !matches_request(run, &action, request) {
            return Err(rejection());
        }
        let ordered_at = state.event_sequence + 1;
        state.run.as_mut().ok_or_else(rejection)?.reviews.insert(
            action_id.into(),
            ReviewJob {
                request: (**request).clone(),
                ordered_at,
                status: ReviewStatus::Planned,
                scope_tests: Default::default(),
            },
        );
        return Ok(changed());
    }
    let mut job = existing.ok_or_else(rejection)?.clone();
    let binding = &action.binding;
    let active = state
        .nodes
        .get(&binding.node_id)
        .is_some_and(|node| node.current_attempt.as_deref() == Some(binding.attempt_id.as_str()));
    let mut payload = None;
    match fact {
        ReviewFact::WaitingForSlot
            if matches!(
                job.status,
                ReviewStatus::Planned | ReviewStatus::WaitingForSlot
            ) =>
        {
            validate_new_start(state, binding)?;
            if job.status == ReviewStatus::WaitingForSlot {
                return Ok(None);
            }
            job.status = ReviewStatus::WaitingForSlot;
        }
        ReviewFact::Sent { receipt, deadline }
            if matches!(
                job.status,
                ReviewStatus::Planned | ReviewStatus::WaitingForSlot
            ) =>
        {
            observe_action(
                state,
                action_id,
                binding,
                &ActionObservation::Started {
                    receipt: receipt.clone(),
                },
                now,
            )?;
            job.status = ReviewStatus::Sent {
                receipt: receipt.clone(),
                deadline: *deadline,
            };
        }
        ReviewFact::Reply { receipt, review } => {
            let ReviewStatus::Sent {
                receipt: started,
                deadline,
            } = &job.status
            else {
                return Err(rejection());
            };
            if receipt != started || now >= *deadline {
                return Err(rejection());
            }
            if !matches_reply(&job.request, review) {
                job.status = ReviewStatus::Failed {
                    reason: "review did not match the dispatched range or finding IDs".into(),
                };
            } else {
                let mut received = (**review).clone();
                received.verdict = received.verdict_by_severity();
                received.webgpt = vec![WebGptResult {
                    result_ref: receipt.clone(),
                    target_sha: received.range.head_sha.clone(),
                }];
                job.status = ReviewStatus::Received {
                    record: Box::new(ReviewRecord {
                        evidence_id: format!("review-result:{action_id}"),
                        reviewer: job.request.reviewer.clone(),
                        review: received,
                    }),
                };
            }
            observe_action(
                state,
                action_id,
                binding,
                &ActionObservation::Reconciled {
                    evidence: receipt.clone(),
                },
                now,
            )?;
            payload = Some(EvidencePayload::ReviewResult(Box::new(job.status.clone())));
        }
        ReviewFact::Failed { reason, uncertain }
            if !matches!(
                job.status,
                ReviewStatus::Received { .. } | ReviewStatus::Failed { .. }
            ) =>
        {
            if reason.trim().is_empty() {
                return Err(rejection());
            }
            if *uncertain {
                observe_action(
                    state,
                    action_id,
                    binding,
                    &ActionObservation::Unknown {
                        reason: reason.clone(),
                    },
                    now,
                )?;
            } else if action.state == ActionState::Started {
                observe_action(
                    state,
                    action_id,
                    binding,
                    &ActionObservation::Reconciled {
                        evidence: reason.clone(),
                    },
                    now,
                )?;
            } else {
                let live = state
                    .run
                    .as_mut()
                    .and_then(|run| run.actions.get_mut(action_id))
                    .ok_or_else(rejection)?;
                live.state = ActionState::Cancelled;
                live.reconciliation = Some(reason.clone());
            }
            job.status = ReviewStatus::Failed {
                reason: reason.clone(),
            };
            payload = Some(EvidencePayload::ReviewResult(Box::new(job.status.clone())));
        }
        ReviewFact::Scope(test) if active && scope_valid(test, &job.request) => {
            if let Some(old) = job.scope_tests.get(&test.scope) {
                if old == test {
                    return Ok(None);
                }
                if old.outcome != ScopeOutcome::Unavailable {
                    return Err(rejection());
                }
            }
            job.scope_tests.insert(test.scope.clone(), test.clone());
            payload = Some(EvidencePayload::ReviewScope(Box::new(test.clone())));
        }
        ReviewFact::Finalize => {
            if !active {
                return Ok(None);
            }
            let Some(assessment) = assessment(run, &action, &job) else {
                return Ok(None);
            };
            if !matches!(assessment.verdict, Verdict::Pass | Verdict::QualityFail) {
                return Ok(None);
            }
            let epoch = run
                .fences
                .get(&binding.node_id)
                .ok_or_else(rejection)?
                .epoch;
            return accept_stage_result(
                state,
                &binding.node_id,
                epoch,
                &binding.attempt_id,
                Some(&assessment),
                now,
            );
        }
        _ => return Err(rejection()),
    }
    if matches!(job.status, ReviewStatus::Failed { .. })
        && active
        && state.control == CampaignControl::Running
    {
        state
            .nodes
            .get_mut(&binding.node_id)
            .ok_or_else(rejection)?
            .disposition = NodeDisposition::Blocked;
    }
    let envelope = payload
        .map(|payload| {
            EvidenceEnvelope::from_review(binding, &job.request.pin_hash, action_id, payload, now)
        })
        .transpose()
        .map_err(|_| rejection())?;
    state
        .run
        .as_mut()
        .ok_or_else(rejection)?
        .reviews
        .insert(action_id.into(), job);
    if let Some(envelope) = envelope {
        let decision = admit(&envelope, state, &envelope.pin_hash, &[]);
        if decision.status != EvidenceStatus::Accepted {
            return Err(rejection());
        }
        evidence.push(EvidenceWrite::Admitted {
            envelope: Box::new(envelope),
            decision,
        });
    }
    Ok(changed())
}

#[cfg(test)]
#[path = "review_wait_tests.rs"]
mod tests;
