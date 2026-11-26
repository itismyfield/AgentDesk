use std::io;
use std::path::Path;

use chrono::{DateTime, TimeDelta, Utc};
use sqlx::PgPool;

use crate::db::server_campaigns::StoreError;
use crate::domain::campaign::decision_item::ApprovalBinding;
#[cfg(test)]
use crate::domain::campaign::landing::TlcOutcome;
use crate::domain::campaign::landing::{
    MergeFact, MergeJob, MergeReceipt, MergeStatus, OpenedPr, TlcEvidence,
};

use super::adk_observe::ci::{CiObservation, CiVerdict};
use super::adk_observe::{MainObservation, PrObservation, is_sha};
use super::ci_exception::{RunnerException, runner_exception};
use super::effect_envelope::execution_nonce;
use super::host_gate::{EffectAuthority, EffectEnvelope, Initiated, Initiation, JournalState};
use super::host_worker::{HostWorker, WorkerError};
use super::lifecycle;
use super::main_integration::judge_single_landing;
use super::profile::{IntegrationMethod, NodeArtifact, Observation};
use super::stage_fsm::Transition;
use super::verification_run::digest_of;
use crate::domain::campaign::model::*;

const WAIT_LIMIT: TimeDelta = TimeDelta::minutes(240);

pub struct MergeObservations {
    pub pr: PrObservation,
    pub ci: Option<CiObservation>,
    pub tlc: Option<TlcEvidence>,
    pub runner: Option<RunnerException>,
}

/// Reads current PR and CI with bounded I/O. Squash requires the observed base and exact head.
pub trait MergeRemote: Send + Sync {
    fn observe(&self, pr: &OpenedPr) -> io::Result<MergeObservations>;
    fn squash_exact(
        &self,
        pr: &OpenedPr,
        base_sha: &str,
        runner_losses: &[crate::domain::campaign::landing::CrossRunJob],
        nonce: &str,
    ) -> io::Result<String>;
    fn observe_main(
        &self,
        pr: &OpenedPr,
        checkout: &Path,
    ) -> io::Result<Option<(PrObservation, MainObservation)>>;
}

#[derive(Debug, PartialEq, Eq)]
pub enum MergeProgress {
    Waiting,
    AwaitApproval(Box<ApprovalBinding>),
    WaitingForMain,
    Adopted,
    Failed,
}

fn rejected(code: RejectionCode) -> Rejection {
    Rejection::new(code)
}

fn opening<'a>(
    state: &'a CampaignState,
    node: &str,
) -> Result<
    (
        &'a Run,
        &'a crate::domain::campaign::landing::Landing,
        &'a OpenedPr,
        &'a Action,
    ),
    Rejection,
> {
    let run = state
        .run
        .as_ref()
        .ok_or_else(|| rejected(RejectionCode::StaleRun))?;
    let landing = run
        .landings
        .get(node)
        .ok_or_else(|| rejected(RejectionCode::StaleBinding))?;
    let crate::domain::campaign::landing::LandingStatus::CiHold { pr } = &landing.status else {
        return Err(rejected(RejectionCode::InvalidState));
    };
    let opened = run
        .actions
        .get(&landing.action_id)
        .filter(|action| {
            action.binding.node_id == node
                && action.binding.run_id == run.id
                && action.kind == ActionKind::CreatePr
                && action.state == ActionState::Reconciled
        })
        .ok_or_else(|| rejected(RejectionCode::StaleBinding))?;
    if !landing.verified() || !landing.matches_pr(pr) || opened.target != landing.host {
        return Err(rejected(RejectionCode::GateNotPassed));
    }
    Ok((run, landing, pr, opened))
}

fn fail(reason: impl Into<String>, uncertain: bool) -> MergeFact {
    MergeFact::Failed {
        reason: reason.into(),
        uncertain,
    }
}

pub(super) fn decide(
    state: &mut CampaignState,
    node: &str,
    fact: &MergeFact,
    now: DateTime<Utc>,
) -> Transition {
    let (run, landing, pr, opened) = opening(state, node)?;
    let id = format!("merge:{}", opened.id);
    let event = || {
        Some(EventKind::ActionChanged {
            action_id: id.clone(),
        })
    };
    let active = lifecycle::merge_position(state, run, node, landing);
    if let MergeFact::Prepare { opened_at } = fact {
        if run.merges.contains_key(node) {
            return Ok(None);
        }
        let deadline = opened_at
            .filter(|at| *at <= now)
            .and_then(|at| at.checked_add_signed(WAIT_LIMIT))
            .unwrap_or(now);
        if run.actions.contains_key(&id) {
            return Err(rejected(RejectionCode::IdempotencyConflict));
        }
        let status = if !active || deadline <= now {
            MergeStatus::Failed {
                reason: "merge admission expired or lacks an opening receipt".into(),
                uncertain: false,
            }
        } else {
            MergeStatus::Waiting
        };
        let action = Action {
            id: id.clone(),
            binding: opened.binding.clone(),
            kind: ActionKind::Merge,
            target: landing.host.clone(),
            state: if status == MergeStatus::Waiting {
                ActionState::StartPending
            } else {
                ActionState::Cancelled
            },
            initiation_receipt: None,
            reconciliation: None,
        };
        let run = state
            .run
            .as_mut()
            .ok_or_else(|| rejected(RejectionCode::StaleRun))?;
        run.actions.insert(id.clone(), action);
        run.merges
            .insert(node.into(), MergeJob { deadline, status });
        return Ok(event());
    }
    let job = run
        .merges
        .get(node)
        .ok_or_else(|| rejected(RejectionCode::StaleBinding))?;
    let mut saved_receipt = None;
    let (status, action_state) = match (fact, &job.status) {
        (MergeFact::Started(receipt), MergeStatus::Waiting)
            if receipt.matches(pr, &opened.binding) && is_sha(&receipt.merge_sha) =>
        {
            saved_receipt = Some(
                serde_json::to_string(receipt)
                    .map_err(|_| rejected(RejectionCode::InvalidState))?,
            );
            (
                MergeStatus::WaitingForMain(receipt.clone()),
                ActionState::Reconciled,
            )
        }
        (MergeFact::Adopted(evidence), MergeStatus::WaitingForMain(receipt))
            if evidence.pr_number == pr.number
                && evidence.merge_sha == receipt.merge_sha
                && evidence.method == IntegrationMethod::Squash
                && evidence.repository == opened.binding.repository
                && evidence.target_branch == pr.base_ref
                && is_sha(&evidence.target_main_sha)
                && evidence
                    .validate_mappings(std::slice::from_ref(&receipt.artifact))
                    .is_ok() =>
        {
            (
                MergeStatus::Adopted(evidence.clone()),
                ActionState::Reconciled,
            )
        }
        (
            MergeFact::Failed { reason, uncertain },
            MergeStatus::Waiting | MergeStatus::WaitingForMain(_),
        ) if !reason.is_empty() => {
            let action = match job.status {
                MergeStatus::WaitingForMain(_) => ActionState::Reconciled,
                _ if *uncertain => ActionState::Unknown,
                _ => ActionState::Cancelled,
            };
            (
                MergeStatus::Failed {
                    reason: reason.clone(),
                    uncertain: *uncertain,
                },
                action,
            )
        }
        _ => return Err(rejected(RejectionCode::InvalidState)),
    };
    let pr_head = pr.head_sha.clone();
    let work_id = opened.binding.attempt_id.clone();
    let run = state
        .run
        .as_mut()
        .ok_or_else(|| rejected(RejectionCode::StaleRun))?;
    let action = run
        .actions
        .get_mut(&id)
        .ok_or_else(|| rejected(RejectionCode::StaleBinding))?;
    action.state = action_state;
    if saved_receipt.is_some() {
        action.initiation_receipt = saved_receipt;
    }
    run.merges
        .get_mut(node)
        .ok_or_else(|| rejected(RejectionCode::StaleBinding))?
        .status = status;
    if matches!(fact, MergeFact::Adopted(_)) {
        let current = state.run.as_ref().is_some_and(|run| {
            lifecycle::landing_candidate(state, run, node).is_some_and(|candidate| {
                candidate.head_sha == pr_head && candidate.work_attempt == work_id
            })
        });
        if current {
            return lifecycle::main_observed(state, node, active);
        }
    }
    if matches!(fact, MergeFact::Failed { .. })
        && active
        && let Some(node) = state.nodes.get_mut(node)
    {
        node.disposition = NodeDisposition::Blocked;
        node.wait_reason = Some(ReasonCode::StateNeedsInspection);
    }
    Ok(event())
}

fn envelope(
    run: &Run,
    landing: &crate::domain::campaign::landing::Landing,
    pr: &OpenedPr,
    opened: &Action,
) -> Result<EffectEnvelope, WorkerError> {
    let action_id = format!("merge:{}", opened.id);
    if !run.actions.get(&action_id).is_some_and(|action| {
        action.state == ActionState::StartPending
            && action.target == opened.target
            && action.binding == opened.binding
            && action.kind == ActionKind::Merge
    }) {
        return Err(WorkerError::Binding);
    }
    let binding = digest_of(&opened.binding).map_err(StoreError::from)?;
    let payload = digest_of(&(pr, &landing.target.pin_hash)).map_err(StoreError::from)?;
    Ok(EffectEnvelope {
        execution_nonce: execution_nonce(&action_id, &payload),
        payload_digest: payload,
        action_id: action_id.clone(),
        kind: ActionKind::Merge,
        run_id: run.id.clone(),
        node_id: opened.binding.node_id.clone(),
        origin_attempt_id: opened.binding.attempt_id.clone(),
        origin_binding_digest: binding.clone(),
        target_host_gate: landing.host.clone(),
        phase: None,
        authority: EffectAuthority::LandingMerge {
            action_id,
            binding_digest: binding,
            node_epoch: landing.epoch,
            run_control_version: landing.control_version,
        },
    })
}

struct Checked {
    base_sha: String,
    ci: CiObservation,
    tlc: TlcEvidence,
    losses: Vec<crate::domain::campaign::landing::CrossRunJob>,
}

fn check_observations(
    pr: &OpenedPr,
    proof: MergeObservations,
    now: DateTime<Utc>,
) -> Option<Checked> {
    let ci = proof.ci?;
    let tlc = proof.tlc?;
    if proof.pr.state != "open"
        || proof.pr.merged
        || !is_sha(&proof.pr.base_sha)
        || !super::main_integration::matches_opened(pr, &proof.pr)
        || !ci.matches_merge(&proof.pr)
        || !tlc.covers(&pr.head_sha, now, WAIT_LIMIT)
    {
        return None;
    }
    let exceptions =
        if ci.run_conclusion.as_deref() == Some("success") && ci.verdict() == CiVerdict::Passed {
            Vec::new()
        } else {
            runner_exception(&ci, proof.runner.as_ref()?)?
        };
    Some(Checked {
        base_sha: proof.pr.base_sha,
        ci,
        tlc,
        losses: exceptions,
    })
}

fn checked_review(
    run: &Run,
    opened: &Action,
    pr: &OpenedPr,
    pin: &str,
) -> Option<(NodeArtifact, Vec<String>)> {
    use super::profile::Candidate;
    let work = &opened.binding;
    let (action, review) = run
        .reviews
        .iter()
        .filter_map(|(id, job)| {
            let review = run.actions.get(id)?;
            let binding = &review.binding;
            (binding.node_id == work.node_id
                && binding.series == work.series
                && binding.base_sha == work.base_sha
                && binding.candidate_sha == pr.head_sha
                && job.request.range.head_sha == pr.head_sha
                && job.request.pin_hash == pin)
                .then_some((review, job))
        })
        .max_by_key(|(_, job)| job.ordered_at)?;
    let attempt = run.attempts.get(&action.binding.attempt_id)?;
    let assessment = super::review_wait::assessment(run, action, review)?;
    if attempt.outcome != Some(AttemptOutcome::Passed)
        || assessment.verdict != crate::domain::campaign::evidence_gate::Verdict::Pass
        || assessment.adopted.is_empty()
    {
        return None;
    }
    Some((
        NodeArtifact {
            node_id: work.node_id.clone(),
            hashes: work.hashes.clone(),
            review: review.request.range.clone(),
            candidate: Candidate {
                repository: work.repository,
                base_sha: work.base_sha.clone(),
                head_sha: pr.head_sha.clone(),
            },
        },
        assessment.adopted,
    ))
}

fn opening_time(worker: &HostWorker, opened: &Action, pr: &OpenedPr) -> Option<DateTime<Utc>> {
    let entry = worker.effect_entry(&opened.id)?;
    let JournalState::Started {
        receipt,
        process: None,
    } = &entry.state
    else {
        return None;
    };
    (entry.identity.target == opened.target
        && entry.identity.kind == ActionKind::CreatePr
        && opened.initiation_receipt.as_ref() == Some(receipt)
        && serde_json::from_str::<OpenedPr>(receipt).ok().as_ref() == Some(pr))
    .then_some(entry.updated_at)
}

/// Advance one merge, replaying the host result before considering new effects.
pub async fn step_merge(
    worker: &mut HostWorker,
    pool: &PgPool,
    remote: &dyn MergeRemote,
) -> Result<MergeProgress, WorkerError> {
    let (tx, state) = worker.locked(pool).await?;
    let node = worker.node().to_owned();
    let (run, landing, pr, opened) = opening(&state, &node).map_err(WorkerError::Rejected)?;
    let now = Utc::now();
    let Some(job) = run.merges.get(&node) else {
        let fact = MergeFact::Prepare {
            opened_at: opening_time(worker, opened, pr),
        };
        worker
            .commit(
                tx,
                state.event_sequence,
                Command::RecordMerge {
                    node_id: node,
                    fact: Box::new(fact),
                },
                now,
            )
            .await?;
        return Ok(MergeProgress::Waiting);
    };
    let (fact, progress) = match &job.status {
        MergeStatus::Adopted(_) => return Ok(MergeProgress::Adopted),
        MergeStatus::Failed { .. } => return Ok(MergeProgress::Failed),
        MergeStatus::WaitingForMain(receipt) => {
            let adoption = run.seals.get(&opened.binding.attempt_id).and_then(|seal| {
                let checkout = Path::new(&seal.workspace.path);
                let (observed, main) = remote.observe_main(pr, checkout).ok().flatten()?;
                Some(judge_single_landing(
                    checkout, pr, receipt, &observed, &main,
                ))
            });
            match adoption {
                Some(Observation::Observed { evidence, .. })
                    if evidence.is_adopted()
                        && evidence
                            .evidence
                            .as_ref()
                            .is_some_and(|proof| proof.method == IntegrationMethod::Squash) =>
                {
                    let proof = evidence.evidence.ok_or(WorkerError::Binding)?;
                    (MergeFact::Adopted(Box::new(proof)), MergeProgress::Adopted)
                }
                Some(Observation::Observed { .. }) => (
                    fail("main content differed from the landing", false),
                    MergeProgress::Failed,
                ),
                _ if Utc::now() >= job.deadline => (
                    fail("main confirmation timed out", false),
                    MergeProgress::Failed,
                ),
                _ => return Ok(MergeProgress::WaitingForMain),
            }
        }
        MergeStatus::Waiting => {
            let envelope = envelope(run, landing, pr, opened)?;
            if let Some(entry) = worker.effect_entry(&envelope.action_id) {
                if entry.nonce != envelope.execution_nonce || entry.identity != envelope.identity()
                {
                    return Err(WorkerError::Binding);
                }
                match &entry.state {
                    JournalState::Started {
                        receipt,
                        process: None,
                    } => {
                        let receipt = serde_json::from_str(receipt).map_err(StoreError::from)?;
                        (MergeFact::Started(receipt), MergeProgress::WaitingForMain)
                    }
                    JournalState::Cancelled { evidence } | JournalState::NotSent { evidence } => {
                        (fail(evidence.clone(), false), MergeProgress::Failed)
                    }
                    _ => (
                        fail("merge outcome needs reconciliation", true),
                        MergeProgress::Failed,
                    ),
                }
            } else if &landing.host != worker.identity() {
                (
                    fail("merge host incarnation changed", true),
                    MergeProgress::Failed,
                )
            } else if !lifecycle::merge_position(&state, run, &node, landing) {
                (
                    fail("merge authority was revoked", false),
                    MergeProgress::Failed,
                )
            } else if now >= job.deadline {
                (
                    fail("merge checks timed out or lack an opening receipt", false),
                    MergeProgress::Failed,
                )
            } else {
                if let Some((binding, false)) =
                    lifecycle::merge_approval(&state, run, &node, landing, pr)
                {
                    let binding = Box::new(binding);
                    worker
                        .commit(
                            tx,
                            state.event_sequence,
                            Command::OpenApprovalGate {
                                binding: binding.clone(),
                            },
                            now,
                        )
                        .await?;
                    return Ok(MergeProgress::AwaitApproval(binding));
                }
                let Some((artifact, review_refs)) =
                    checked_review(run, opened, pr, &landing.target.pin_hash)
                else {
                    return Ok(MergeProgress::Waiting);
                };
                let Some(proof) = remote
                    .observe(pr)
                    .ok()
                    .and_then(|snapshot| check_observations(pr, snapshot, Utc::now()))
                else {
                    return Ok(MergeProgress::Waiting);
                };
                let at = Utc::now();
                if at >= job.deadline || proof.tlc.finished_at >= job.deadline {
                    (fail("merge checks timed out", false), MergeProgress::Failed)
                } else {
                    let result = worker.admit_merge(&state, &envelope, at, |nonce| {
                        let merge_sha =
                            remote.squash_exact(pr, &proof.base_sha, &proof.losses, nonce)?;
                        if !is_sha(&merge_sha) {
                            return Err(io::Error::other("merge response did not name a commit"));
                        }
                        let receipt = MergeReceipt {
                            artifact,
                            pr_number: pr.number,
                            merge_sha,
                            ci_run_id: proof.ci.run_id,
                            ci_run_attempt: proof.ci.run_attempt,
                            tlc: proof.tlc,
                            review_refs,
                            runner_losses: proof.losses,
                        };
                        Ok(Initiated::Started {
                            receipt: serde_json::to_string(&receipt)?,
                            process: None,
                            handle: (),
                        })
                    })?;
                    match result.initiation {
                        Initiation::Started {
                            receipt,
                            process: None,
                        } => {
                            let receipt =
                                serde_json::from_str(&receipt).map_err(StoreError::from)?;
                            (MergeFact::Started(receipt), MergeProgress::WaitingForMain)
                        }
                        Initiation::NotSent { evidence, .. } => {
                            (fail(evidence, false), MergeProgress::Failed)
                        }
                        _ => (
                            fail("merge outcome needs reconciliation", true),
                            MergeProgress::Failed,
                        ),
                    }
                }
            }
        }
    };
    worker
        .commit(
            tx,
            state.event_sequence,
            Command::RecordMerge {
                node_id: node,
                fact: Box::new(fact),
            },
            Utc::now(),
        )
        .await?;
    Ok(progress)
}

#[cfg(test)]
#[path = "landing_merge_tests.rs"]
mod tests;
