use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::db::server_campaigns::StoreError;
use crate::domain::campaign::landing::*;
use crate::domain::campaign::workflow_options::ApprovalGate;

use super::adk_observe::{is_sha, valid_slug};
use super::effect_envelope::execution_nonce;
use super::host_gate::{EffectAuthority, EffectEnvelope, Initiated, Initiation, JournalState};
use super::host_worker::{HostWorker, WorkerError};
use super::landing_prepare::{LandingChecks, require_checkout};
use super::lifecycle::{GateTarget, landing_candidate, require_gate};
use super::stage_fsm::Transition;
use crate::domain::campaign::model::*;
use crate::domain::campaign::verification_run::digest_of;

const CI_WAIT: &str = "landing waits for idle CI";

/// Repository-scoped operations with bounded I/O. A write that may have started cannot be retried.
pub trait LandingRemote: Send + Sync {
    fn observe(&self, target: &LandingTarget) -> io::Result<RemoteLanding>;
    /// Atomically require an absent ref and idle CI before creating the pinned ref.
    /// False means CI changed and the ref was never written.
    fn push_new_if_idle(
        &self,
        target: &LandingTarget,
        checkout: &Path,
        nonce: &str,
    ) -> io::Result<bool>;
    /// Open once for these refs and require the pinned head.
    fn open_once(&self, target: &LandingTarget, nonce: &str) -> io::Result<OpenedPr>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LandingProgress {
    WaitingCi,
    Opened(OpenedPr),
    Failed { reason: String, uncertain: bool },
}

fn reject(code: RejectionCode) -> Rejection {
    Rejection::new(code)
}

struct Admission<'a> {
    run: &'a Run,
    binding: &'a AttemptBinding,
    epoch: u64,
    target: LandingTarget,
    checkout: PathBuf,
}

fn admission<'a>(
    state: &'a CampaignState,
    node_id: &str,
    host: &HostGate,
) -> Result<Admission<'a>, Rejection> {
    let run = state
        .run
        .as_ref()
        .ok_or_else(|| reject(RejectionCode::StaleRun))?;
    let node = state
        .nodes
        .get(node_id)
        .ok_or_else(|| reject(RejectionCode::StaleBinding))?;
    if state.control != CampaignControl::Running
        || run.stop.is_some()
        || state.definition.input().repository != Repository::Adk
        || node.stage != Stage::Landing
        || node.disposition != NodeDisposition::Assigned
        || node.current_attempt.is_some()
    {
        return Err(reject(RejectionCode::InvalidState));
    }
    let fence = run
        .fences
        .get(node_id)
        .filter(|fence| fence.interruption.is_none())
        .ok_or_else(|| reject(RejectionCode::StaleFence))?;
    let candidate = landing_candidate(state, run, node_id)
        .ok_or_else(|| reject(RejectionCode::StaleBinding))?;
    require_gate(
        state,
        run,
        node_id,
        ApprovalGate::Landing,
        &GateTarget::of(candidate),
    )?;
    let binding = &run
        .attempts
        .get(&candidate.work_attempt)
        .filter(|work| {
            work.outcome == Some(AttemptOutcome::Passed)
                && work.binding.node_id == node_id
                && work.binding.run_id == run.id
        })
        .ok_or_else(|| reject(RejectionCode::StaleBinding))?
        .binding;
    let seal = run
        .seals
        .get(&candidate.work_attempt)
        .filter(|seal| {
            seal.owner_attempt == binding.attempt_id
                && seal.repository == Repository::Adk
                && seal.output_sha == candidate.head_sha
                && seal.base_sha == candidate.base_sha
                && seal.pin_hash == candidate.pin_hash
                && seal.workspace.host == host.host
                && valid_slug(&seal.workspace.repository_identity)
                && is_sha(&seal.output_sha)
                && Path::new(&seal.workspace.path).is_absolute()
                && digest_of(binding).is_ok_and(|digest| digest == seal.binding_digest)
        })
        .ok_or_else(|| reject(RejectionCode::StaleBinding))?;
    let key = format!("{}\n{node_id}", state.id);
    Ok(Admission {
        run,
        binding,
        epoch: fence.epoch,
        checkout: PathBuf::from(&seal.workspace.path),
        target: LandingTarget {
            repository: seal.workspace.repository_identity.clone(),
            head_sha: seal.output_sha.clone(),
            head_ref: format!("campaign/{}", blake3::hash(key.as_bytes()).to_hex()),
            base_ref: "main".into(),
            pin_hash: seal.pin_hash.clone(),
        },
    })
}

fn existing<'a>(state: &'a CampaignState, node_id: &str) -> Result<&'a Landing, Rejection> {
    state
        .run
        .as_ref()
        .and_then(|run| run.landings.get(node_id))
        .ok_or_else(|| reject(RejectionCode::StaleBinding))
}

fn current(
    state: &CampaignState,
    node_id: &str,
    job: &Landing,
    now: DateTime<Utc>,
) -> Result<PathBuf, Rejection> {
    let observed = admission(state, node_id, &job.host)?;
    if observed.epoch != job.epoch
        || observed.run.control_version != job.control_version
        || observed.target != job.target
        || now >= job.deadline
    {
        return Err(reject(RejectionCode::StaleFence));
    }
    Ok(observed.checkout)
}

pub(super) fn decide(
    state: &mut CampaignState,
    node_id: &str,
    fact: &LandingFact,
    now: DateTime<Utc>,
) -> Transition {
    if let LandingFact::Prepare { host, deadline } = fact {
        let admitted = admission(state, node_id, host)?;
        if let Some(job) = admitted.run.landings.get(node_id) {
            current(state, node_id, job, now)?;
            return if &job.host == host && &job.deadline == deadline {
                Ok(None)
            } else {
                Err(reject(RejectionCode::IdempotencyConflict))
            };
        }
        if now >= *deadline {
            return Err(reject(RejectionCode::InvalidState));
        }
        let id = format!("landing:{}:{node_id}:{}", admitted.run.id, admitted.epoch);
        let action = Action {
            id: id.clone(),
            binding: admitted.binding.clone(),
            kind: ActionKind::CreatePr,
            target: host.clone(),
            state: ActionState::Planned,
            initiation_receipt: None,
            reconciliation: None,
        };
        let job = Landing {
            target: admitted.target,
            epoch: admitted.epoch,
            control_version: admitted.run.control_version,
            host: host.clone(),
            deadline: *deadline,
            action_id: id.clone(),
            preflight: None,
            status: LandingStatus::Preparing,
        };
        let run = state
            .run
            .as_mut()
            .ok_or_else(|| reject(RejectionCode::StaleRun))?;
        if run.actions.contains_key(&id) {
            return Err(reject(RejectionCode::IdempotencyConflict));
        }
        run.actions.insert(id.clone(), action);
        run.landings.insert(node_id.into(), job);
        return Ok(Some(EventKind::ActionChanged { action_id: id }));
    }

    let job = existing(state, node_id)?;
    let (status, action_state, receipt) = match fact {
        LandingFact::Prepare { .. } => unreachable!(),
        LandingFact::Preflight(check) => {
            current(state, node_id, job, now)?;
            if job.preflight.as_ref() == Some(check) {
                return Ok(None);
            }
            if check.head_sha != job.target.head_sha
                || check.transcript_digest.trim().is_empty()
                || job.preflight.is_some()
            {
                return Err(reject(RejectionCode::GateNotPassed));
            }
            (LandingStatus::Preparing, ActionState::StartPending, None)
        }
        LandingFact::Opened(pr) => {
            let status = LandingStatus::CiHold { pr: pr.clone() };
            if job.status == status {
                return Ok(None);
            }
            if !job.verified() || !job.matches_pr(pr) {
                return Err(reject(RejectionCode::StaleBinding));
            }
            let receipt = pr_receipt(pr).map_err(|_| reject(RejectionCode::InvalidState))?;
            (status, ActionState::Reconciled, Some(receipt))
        }
        LandingFact::Failed { reason, uncertain } => {
            let status = LandingStatus::Failed {
                reason: reason.clone(),
                uncertain: *uncertain,
            };
            if job.status == status {
                return Ok(None);
            }
            let action = if *uncertain {
                ActionState::Unknown
            } else {
                ActionState::Cancelled
            };
            (status, action, None)
        }
    };
    if job.status != LandingStatus::Preparing {
        return Err(reject(RejectionCode::InvalidState));
    }
    let id = job.action_id.clone();
    let blocked = matches!(&status, LandingStatus::Failed { .. })
        && admission(state, node_id, &job.host).is_ok_and(|held| {
            held.run.control_version == job.control_version && held.epoch == job.epoch
        });
    let run = state
        .run
        .as_mut()
        .ok_or_else(|| reject(RejectionCode::StaleRun))?;
    let action = run
        .actions
        .get_mut(&id)
        .ok_or_else(|| reject(RejectionCode::StaleBinding))?;
    if action.kind != ActionKind::CreatePr {
        return Err(reject(RejectionCode::StaleBinding));
    }
    action.state = action_state;
    action.initiation_receipt = receipt;
    let job = run
        .landings
        .get_mut(node_id)
        .ok_or_else(|| reject(RejectionCode::StaleBinding))?;
    job.status = status;
    if let LandingFact::Preflight(check) = fact {
        job.preflight = Some(check.clone());
    }
    if blocked && let Some(node) = state.nodes.get_mut(node_id) {
        node.disposition = NodeDisposition::Blocked;
        node.wait_reason = Some(ReasonCode::StateNeedsInspection);
    }
    Ok(Some(EventKind::ActionChanged { action_id: id }))
}

pub(super) fn publication_envelope<'a>(
    state: &'a CampaignState,
    node_id: &str,
    host: &HostGate,
    now: DateTime<Utc>,
) -> Result<(EffectEnvelope, &'a Landing, PathBuf), WorkerError> {
    let job = existing(state, node_id).map_err(WorkerError::Rejected)?;
    let checkout = current(state, node_id, job, now).map_err(WorkerError::Rejected)?;
    let run = state.run.as_ref().ok_or(WorkerError::Binding)?;
    let action = run
        .actions
        .get(&job.action_id)
        .filter(|action| {
            action.kind == ActionKind::CreatePr
                && action.state == ActionState::StartPending
                && &action.target == host
                && &job.host == host
                && action.binding.run_id == run.id
                && action.binding.node_id == node_id
        })
        .ok_or(WorkerError::Binding)?;
    if !job.verified() || job.status != LandingStatus::Preparing {
        return Err(WorkerError::StaleStep);
    }
    let payload = digest_of(&(&job.target, &job.preflight)).map_err(StoreError::from)?;
    let binding = digest_of(&action.binding).map_err(StoreError::from)?;
    let envelope = EffectEnvelope {
        action_id: action.id.clone(),
        run_id: run.id.clone(),
        node_id: node_id.into(),
        origin_attempt_id: action.binding.attempt_id.clone(),
        origin_binding_digest: binding.clone(),
        target_host_gate: host.clone(),
        execution_nonce: execution_nonce(&action.id, &payload),
        payload_digest: payload,
        kind: ActionKind::CreatePr,
        phase: None,
        authority: EffectAuthority::Landing {
            action_id: action.id.clone(),
            binding_digest: binding,
            node_epoch: job.epoch,
            run_control_version: job.control_version,
        },
    };
    Ok((envelope, job, checkout))
}

fn pr_receipt(pr: &OpenedPr) -> serde_json::Result<String> {
    serde_json::to_string(pr)
}

fn initiate(
    remote: &dyn LandingRemote,
    job: &Landing,
    checkout: &Path,
    nonce: &str,
) -> io::Result<Initiated<()>> {
    let not_sent = |reason: &str| {
        Ok(Initiated::NotSent {
            evidence: reason.into(),
        })
    };
    if require_checkout(checkout, &job.host.host, &job.target).is_err() {
        return not_sent("landing checkout changed after preflight");
    }
    let snapshot = match remote.observe(&job.target) {
        Ok(snapshot) => snapshot,
        Err(_) => return not_sent("landing remote observation failed"),
    };
    if Utc::now() >= job.deadline {
        return not_sent("landing preparation timed out");
    }
    match job.publication(&snapshot) {
        Publication::Wait => return not_sent(CI_WAIT),
        Publication::Refused => {
            return not_sent("landing remote ref or PR conflicts with the pinned candidate");
        }
        Publication::Open => {}
        Publication::PushThenOpen => {
            if !remote.push_new_if_idle(&job.target, checkout, nonce)? {
                return not_sent(CI_WAIT);
            }
            let after_push = remote.observe(&job.target)?;
            if after_push.branch_head.as_ref() != Some(&job.target.head_sha)
                || after_push.pull_request.is_some()
            {
                return Err(io::Error::other(
                    "landing remote changed after its ref was created",
                ));
            }
        }
    }
    if Utc::now() >= job.deadline {
        return Err(io::Error::other(
            "landing deadline passed before PR opening",
        ));
    }
    let pr = remote.open_once(&job.target, nonce)?;
    if !job.matches_pr(&pr) {
        return Err(io::Error::other(
            "opened PR did not match the pinned landing target",
        ));
    }
    Ok(Initiated::Started {
        receipt: pr_receipt(&pr)?,
        process: None,
        handle: (),
    })
}

fn failed(reason: impl Into<String>, uncertain: bool) -> LandingFact {
    LandingFact::Failed {
        reason: reason.into(),
        uncertain,
    }
}

fn recovery(job: &Landing, worker: &HostWorker) -> Result<Option<LandingFact>, WorkerError> {
    let Some(entry) = worker.effect_entry(&job.action_id) else {
        return Ok(None);
    };
    let payload = digest_of(&(&job.target, &job.preflight)).map_err(StoreError::from)?;
    if entry.identity.kind != ActionKind::CreatePr
        || entry.identity.target != job.host
        || entry.identity.payload_digest != payload
        || entry.nonce != execution_nonce(&job.action_id, &payload)
    {
        return Err(WorkerError::Binding);
    }
    let fact = match &entry.state {
        JournalState::Started {
            receipt,
            process: None,
        } => LandingFact::Opened(serde_json::from_str(receipt).map_err(StoreError::from)?),
        JournalState::Initiating | JournalState::Indeterminate { .. } => {
            failed("landing publication needs remote reconciliation", true)
        }
        JournalState::Cancelled { evidence } => failed(evidence.clone(), false),
        JournalState::NotSent { evidence } if evidence != CI_WAIT => {
            failed(evidence.clone(), false)
        }
        JournalState::NotSent { .. } => return Ok(None),
        _ => return Err(WorkerError::Binding),
    };
    Ok(Some(fact))
}

pub async fn prepare_landing(
    worker: &HostWorker,
    pool: &PgPool,
    deadline: DateTime<Utc>,
) -> Result<(), WorkerError> {
    let (tx, state) = worker.locked(pool).await?;
    worker
        .record_landing(
            tx,
            &state,
            LandingFact::Prepare {
                host: worker.identity().clone(),
                deadline,
            },
        )
        .await
}

/// Advances the admitted job. Recovery replays an exact journal receipt even after Stop.
pub async fn step_landing(
    worker: &mut HostWorker,
    pool: &PgPool,
    checks: &LandingChecks,
    mode: PreflightMode,
    remote: &dyn LandingRemote,
    headroom_wait: Duration,
) -> Result<LandingProgress, WorkerError> {
    let (tx, state) = worker.locked(pool).await?;
    let job = existing(&state, worker.node())
        .map_err(WorkerError::Rejected)?
        .clone();
    match job.status {
        LandingStatus::Failed { reason, uncertain } => {
            return Ok(LandingProgress::Failed { reason, uncertain });
        }
        LandingStatus::CiHold { pr } => return Ok(LandingProgress::Opened(pr)),
        LandingStatus::Preparing => {}
    }
    if let Some(fact) = recovery(&job, worker)? {
        let progress = progress(&fact)?;
        worker.record_landing(tx, &state, fact).await?;
        return Ok(progress);
    }
    let identity = worker.identity();
    if job.host.host != identity.host || job.host.gate != identity.gate {
        return Err(WorkerError::Binding);
    }
    let checkout = match current(&state, worker.node(), &job, Utc::now()) {
        Ok(checkout) if &job.host == identity => checkout,
        position => {
            let fact = if &job.host != identity {
                failed(
                    "landing host incarnation changed",
                    job.preflight.is_some() && worker.effect_entry(&job.action_id).is_none(),
                )
            } else if Utc::now() >= job.deadline {
                failed("landing preparation timed out", false)
            } else {
                debug_assert!(position.is_err());
                failed("landing admission was revoked", false)
            };
            let progress = progress(&fact)?;
            worker.record_landing(tx, &state, fact).await?;
            return Ok(progress);
        }
    };
    drop(tx);
    if job.preflight.is_none() {
        worker.wait_for_headroom(headroom_wait).await?;
        let host = job.host.host.clone();
        let target = job.target.clone();
        let check = checks.clone();
        let result = tokio::task::spawn_blocking(move || {
            check.run(&checkout, &host, &target, mode, job.deadline)
        })
        .await
        .map_err(io::Error::other)?;
        let fact = match result {
            Ok(receipt) if Utc::now() < job.deadline => LandingFact::Preflight(receipt),
            Ok(_) => failed("landing preparation timed out during preflight", false),
            Err(error) => failed(error.to_string(), false),
        };
        let (tx, state) = worker.locked(pool).await?;
        if existing(&state, worker.node())
            .map_err(WorkerError::Rejected)?
            .action_id
            != job.action_id
        {
            return Err(WorkerError::StaleStep);
        }
        let failed = matches!(fact, LandingFact::Failed { .. })
            .then(|| progress(&fact))
            .transpose()?;
        worker.record_landing(tx, &state, fact).await?;
        if let Some(progress) = failed {
            return Ok(progress);
        }
    }
    let admitted = worker
        .start_landing(pool, &job.action_id, |job, checkout, nonce| {
            initiate(remote, job, checkout, nonce)
        })
        .await?;
    let fact = match admitted.initiation {
        Initiation::Started {
            receipt,
            process: None,
        } => LandingFact::Opened(serde_json::from_str(&receipt).map_err(StoreError::from)?),
        Initiation::NotSent { evidence, .. } if evidence == CI_WAIT => {
            return Ok(LandingProgress::WaitingCi);
        }
        Initiation::NotSent { evidence, .. } => failed(evidence, false),
        Initiation::Indeterminate { .. } => {
            failed("landing publication needs remote reconciliation", true)
        }
        _ => return Err(WorkerError::Binding),
    };
    let progress = progress(&fact)?;
    let (tx, state) = worker.locked(pool).await?;
    if existing(&state, worker.node())
        .map_err(WorkerError::Rejected)?
        .action_id
        != job.action_id
    {
        return Err(WorkerError::StaleStep);
    }
    worker.record_landing(tx, &state, fact).await?;
    Ok(progress)
}

fn progress(fact: &LandingFact) -> Result<LandingProgress, WorkerError> {
    match fact {
        LandingFact::Opened(pr) => Ok(LandingProgress::Opened(pr.clone())),
        LandingFact::Failed { reason, uncertain } => Ok(LandingProgress::Failed {
            reason: reason.clone(),
            uncertain: *uncertain,
        }),
        _ => Err(WorkerError::Binding),
    }
}

#[cfg(test)]
#[path = "landing_effects_tests.rs"]
mod tests;
