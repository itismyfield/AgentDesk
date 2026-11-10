use chrono::{DateTime, Utc};

use super::adk_workspace::WorkspaceObservation;
use super::candidate_seal::CandidateSeal;
use super::evidence::{self, EvidenceEnvelope, EvidenceStatus};
use super::model::*;
use super::profile::ExecutionEnvironment;
use super::stage_fsm::{self, Transition};
use super::verification_run::{
    NotSentReason, PhaseInitiation, ReconciledInitiation, VerificationError, VerificationRun,
};

fn reject<T>(code: RejectionCode) -> Result<T, Rejection> {
    Err(Rejection::new(code))
}

fn refused(error: VerificationError) -> Rejection {
    let code = match error {
        VerificationError::StaleAuthority => RejectionCode::StaleFence,
        VerificationError::AlreadyBound | VerificationError::Conflict => {
            RejectionCode::AlreadyExists
        }
        VerificationError::UnknownPhase(_) | VerificationError::ActionMismatch(_) => {
            RejectionCode::StaleBinding
        }
        _ => RejectionCode::InvalidState,
    };
    Rejection {
        target: Some(error.to_string()),
        ..Rejection::new(code)
    }
}

fn current_run(state: &CampaignState) -> Result<&Run, Rejection> {
    state
        .run
        .as_ref()
        .ok_or_else(|| Rejection::new(RejectionCode::StaleRun))
}

fn changed(execution_id: &str) -> Transition {
    Ok(Some(EventKind::VerificationChanged {
        execution_id: execution_id.into(),
    }))
}

pub(super) fn decide(
    next: &mut CampaignState,
    command: &Command,
    at: DateTime<Utc>,
    records: &mut Vec<EvidenceWrite>,
) -> Transition {
    match command {
        Command::SealCandidate {
            claim,
            repository_identity,
            workspace,
        } => seal_candidate(next, claim, repository_identity, workspace, at, records),
        Command::BindVerificationSubject {
            execution_id,
            consumer,
        } => bind(next, execution_id, consumer, |v, run| {
            let seal = run
                .seals
                .get(&v.origin.owner_attempt)
                .ok_or_else(|| VerificationError::Unbound("seal".into()))?;
            v.bind_subject(seal.bound_subject())
        }),
        Command::BindVerificationInput {
            execution_id,
            phase_id,
            consumer,
            input,
        } => bind(next, execution_id, consumer, |v, _| {
            v.bind_phase_input(phase_id, (**input).clone())
        }),
        Command::BindVerificationPlan {
            execution_id,
            consumer,
        } => bind(next, execution_id, consumer, |v, run| {
            let spec = run
                .attempts
                .get(&v.origin.owner_attempt)
                .and_then(|a| a.execution.as_deref())
                .map(|e| &e.spec)
                .ok_or_else(|| VerificationError::Unbound("spec".into()))?;
            v.bind_plan(spec)
        }),
        Command::RecordVerificationPhase {
            execution_id,
            phase_id,
            fact,
        } => record_phase(next, execution_id, phase_id, fact, at),
        Command::SettleVerification {
            execution_id,
            evidence,
        } => settle(next, execution_id, evidence, at, records),
        _ => reject(RejectionCode::InvalidState),
    }
}

// The claim says which output; the seal exists once the session's host saw that checkout clean.
fn seal_candidate(
    next: &mut CampaignState,
    claim: &EvidenceEnvelope,
    repository_identity: &str,
    workspace: &WorkspaceObservation,
    at: DateTime<Utc>,
    records: &mut Vec<EvidenceWrite>,
) -> Transition {
    let run = current_run(next)?;
    let binding = &claim.binding;
    let attempt = run
        .attempts
        .get(&binding.attempt_id)
        .filter(|a| a.binding == *binding)
        .ok_or_else(|| Rejection::new(RejectionCode::StaleBinding))?;
    let Some(execution) = attempt.execution.as_deref() else {
        return reject(RejectionCode::StaleBinding);
    };
    if workspace.host != binding.session.gate.host {
        return reject(RejectionCode::StaleBinding);
    }
    let pin = &execution.pin_hash;
    let seal = CandidateSeal::seal(binding, claim, repository_identity, workspace, pin, at)
        .map_err(|error| Rejection {
            target: Some(error.to_string()),
            ..Rejection::new(RejectionCode::GateNotPassed)
        })?;
    if let Some(stored) = run.seals.get(&binding.attempt_id) {
        return match seal.confirm_retry(stored) {
            Ok(()) => Ok(None),
            Err(_) => reject(RejectionCode::AlreadyExists),
        };
    }
    let interrupted = run
        .fences
        .get(&binding.node_id)
        .is_some_and(|f| f.interruption.is_some());
    if interrupted {
        return reject(RejectionCode::StaleFence);
    }
    if !attempt.started {
        return reject(RejectionCode::InvalidState);
    }
    let decision = evidence::admit(claim, next, pin, &[]);
    if decision.status != EvidenceStatus::Accepted {
        return Err(Rejection {
            target: Some(format!("{:?}", decision.reasons)),
            ..Rejection::new(RejectionCode::GateNotPassed)
        });
    }
    let event = EventKind::CandidateSealed {
        node_id: binding.node_id.clone(),
        attempt_id: binding.attempt_id.clone(),
        seal_id: seal.seal_id.clone(),
    };
    records.push(EvidenceWrite::Admitted {
        envelope: Box::new(claim.clone()),
        decision,
    });
    records.push(EvidenceWrite::Seal(Box::new(seal.clone())));
    let run = next.run.as_mut().expect("checked above");
    run.seals.insert(seal.owner_attempt.clone(), seal);
    Ok(Some(event))
}

// A slot is filled only by a current holder of the V; the same value again changes nothing.
fn bind(
    next: &mut CampaignState,
    execution_id: &str,
    consumer: &AttemptBinding,
    fill: impl FnOnce(&mut VerificationRun, &Run) -> Result<(), VerificationError>,
) -> Transition {
    let run = current_run(next)?;
    let v = run
        .verifications
        .get(execution_id)
        .ok_or_else(|| Rejection::new(RejectionCode::StaleBinding))?;
    stage_fsm::validate_verification_start(next, v, consumer)?;
    let mut bound = v.clone();
    fill(&mut bound, run).map_err(refused)?;
    if bound == *v {
        return Ok(None);
    }
    let run = next.run.as_mut().expect("checked above");
    run.verifications.insert(execution_id.into(), bound);
    changed(execution_id)
}

// Whether the node's current attempt may still start this V; an unobserved session is not a revocation.
fn holder_valid(state: &CampaignState, v: &VerificationRun) -> bool {
    let binding = state
        .nodes
        .get(&v.origin.node_id)
        .and_then(|n| n.current_attempt.as_ref())
        .and_then(|id| state.run.as_ref()?.attempts.get(id))
        .map(|a| &a.binding);
    binding.is_some_and(
        |binding| match stage_fsm::validate_verification_start(state, v, binding) {
            Ok(_) => true,
            Err(rejection) => rejection.code == RejectionCode::SessionsNotGone,
        },
    )
}

// Records what the phase's gate already did. A proven start, end or result is kept whatever the
// stop and fence state; a proven non-start returns the phase to its holder or cancels it.
fn record_phase(
    next: &mut CampaignState,
    execution_id: &str,
    phase_id: &str,
    fact: &PhaseFact,
    at: DateTime<Utc>,
) -> Transition {
    let run = current_run(next)?;
    let stored = run
        .verifications
        .get(execution_id)
        .ok_or_else(|| Rejection::new(RejectionCode::StaleBinding))?;
    let action_id = &stored.phase(phase_id).map_err(refused)?.action_id;
    let recorded = run
        .actions
        .get(action_id)
        .ok_or_else(|| Rejection::new(RejectionCode::StaleBinding))?;
    let (mut v, mut action) = (stored.clone(), recorded.clone());
    let mut environment: Option<&ExecutionEnvironment> = None;
    let unsent = |held: bool, evidence: &str, action: &mut Action| {
        if held {
            action.state = ActionState::Planned;
        } else {
            action.state = ActionState::Cancelled;
            action.reconciliation = Some(evidence.into());
        }
    };
    match fact {
        PhaseFact::Started {
            receipt,
            process,
            environment: observed,
        } => {
            if receipt.trim().is_empty()
                || !matches!(
                    action.state,
                    ActionState::StartPending | ActionState::Started
                )
                || action
                    .initiation_receipt
                    .as_ref()
                    .is_some_and(|old| old != receipt)
            {
                return reject(RejectionCode::InvalidState);
            }
            let initiation = PhaseInitiation::Started {
                receipt: receipt.clone(),
                process: process.clone(),
            };
            v.record_initiation(phase_id, initiation).map_err(refused)?;
            action.state = ActionState::Started;
            action.initiation_receipt = Some(receipt.clone());
            environment = Some(observed);
        }
        PhaseFact::NotStarted { evidence } => {
            if evidence.trim().is_empty()
                || action.state != ActionState::StartPending
                || action.initiation_receipt.is_some()
            {
                return reject(RejectionCode::InvalidState);
            }
            let held = holder_valid(next, stored);
            let reason = if held {
                NotSentReason::StaleAuthority
            } else {
                NotSentReason::AuthorityRevoked
            };
            let initiation = PhaseInitiation::NotSent {
                reason,
                evidence: evidence.clone(),
                observed_at: at,
            };
            v.record_initiation(phase_id, initiation).map_err(refused)?;
            unsent(held, evidence, &mut action);
        }
        PhaseFact::Indeterminate {
            initiation_id,
            evidence,
        } => {
            if evidence.trim().is_empty() || action.state != ActionState::StartPending {
                return reject(RejectionCode::InvalidState);
            }
            let initiation = PhaseInitiation::Indeterminate {
                initiation_id: initiation_id.clone(),
                evidence: evidence.clone(),
                observed_at: at,
            };
            v.record_initiation(phase_id, initiation).map_err(refused)?;
            action.state = ActionState::Unknown;
        }
        PhaseFact::Resolved {
            observation,
            environment: observed,
        } => {
            if observation.execution_id != execution_id || observation.phase_id != phase_id {
                return reject(RejectionCode::StaleBinding);
            }
            let resolved = v
                .reconcile_initiation(observation.clone())
                .map_err(refused)?;
            // A replay of an earlier resolution leaves the action where later commands put it.
            if action.state == ActionState::Unknown {
                match (&observation.outcome, observed) {
                    (ReconciledInitiation::Started { receipt, .. }, Some(observed)) => {
                        action.state = resolved;
                        action.initiation_receipt = Some(receipt.clone());
                        environment = Some(observed);
                    }
                    (ReconciledInitiation::Started { .. }, None) => {
                        return reject(RejectionCode::InvalidState);
                    }
                    (ReconciledInitiation::Unstarted, _) => unsent(
                        holder_valid(next, stored),
                        &observation.evidence,
                        &mut action,
                    ),
                }
            }
        }
        PhaseFact::Terminated { observation } => {
            if observation.execution_id != execution_id || observation.phase_id != phase_id {
                return reject(RejectionCode::StaleBinding);
            }
            v.reconcile_termination(&action, observation.clone())
                .map_err(refused)?;
            action.state = ActionState::Reconciled;
            action.reconciliation = Some(observation.evidence.clone());
        }
        PhaseFact::Finished { result } => {
            v.record_result(phase_id, &action, (**result).clone())
                .map_err(refused)?;
            action.state = ActionState::Reconciled;
            action.reconciliation = Some(result.receipt.clone());
        }
    }
    if environment.is_some_and(|e| e.checkout.host != action.target.host) {
        return reject(RejectionCode::StaleBinding);
    }
    let first_environment = environment.filter(|_| !run.environments.contains_key(execution_id));
    if v == *stored && action == *recorded && first_environment.is_none() {
        return Ok(None);
    }
    let first_environment = first_environment.cloned();
    let run = next.run.as_mut().expect("checked above");
    if let Some(environment) = first_environment {
        run.environments.insert(execution_id.into(), environment);
    }
    run.actions.insert(action.id.clone(), action);
    run.verifications.insert(execution_id.into(), v);
    changed(execution_id)
}

// Each log reference names the host that holds the file.
fn log_refs(v: &VerificationRun) -> Vec<String> {
    v.phases
        .iter()
        .filter_map(|p| Some((&p.target.host, &p.result.as_ref()?.record)))
        .flat_map(|(host, record)| {
            [&record.stdout_ref, &record.stderr_ref]
                .into_iter()
                .filter(|reference| !reference.is_empty())
                .map(move |reference| format!("{host}:{reference}"))
        })
        .collect()
}

// A quiet V is collected, or closed as cancelled once a phase was cancelled and nothing runs;
// unstarted phases left behind are cancelled either way.
fn settle(
    next: &mut CampaignState,
    execution_id: &str,
    evidence: &str,
    at: DateTime<Utc>,
    records: &mut Vec<EvidenceWrite>,
) -> Transition {
    if evidence.trim().is_empty() {
        return reject(RejectionCode::InvalidState);
    }
    let run = current_run(next)?;
    let stored = run
        .verifications
        .get(execution_id)
        .ok_or_else(|| Rejection::new(RejectionCode::StaleBinding))?;
    if stored.collection.is_some() || stored.cancellation.is_some() {
        return Ok(None);
    }
    let state_of = |action_id: &str| run.actions.get(action_id).map(|a| a.state);
    let collected = match run.environments.get(execution_id) {
        Some(environment) => stored.collect(
            |id| run.actions.get(id),
            environment.clone(),
            log_refs(stored),
        ),
        None => Err(VerificationError::NotFinished),
    };
    let mut v = stored.clone();
    match collected {
        Ok(collection) => v.collection = Some(collection),
        Err(VerificationError::NotFinished) => {
            let states: Vec<_> = v.phases.iter().map(|p| state_of(&p.action_id)).collect();
            if states.contains(&Some(ActionState::Claimed))
                || !states.contains(&Some(ActionState::Cancelled))
            {
                return reject(RejectionCode::InvalidState);
            }
            v.cancel(&run.actions, evidence.into(), at)
                .map_err(refused)?;
        }
        Err(error) => return Err(refused(error)),
    }
    let unstarted: Vec<String> = v
        .phases
        .iter()
        .filter(|p| {
            matches!(
                state_of(&p.action_id),
                Some(ActionState::Planned | ActionState::Claimed)
            )
        })
        .map(|p| p.action_id.clone())
        .collect();
    let envelope = match &v.collection {
        Some(_) => Some(
            EvidenceEnvelope::from_runner(&v, at)
                .map_err(|_| Rejection::new(RejectionCode::InvalidState))?,
        ),
        None => None,
    };
    let run = next.run.as_mut().expect("checked above");
    for action_id in unstarted {
        if let Some(action) = run.actions.get_mut(&action_id) {
            action.state = ActionState::Cancelled;
        }
    }
    run.verifications.insert(execution_id.into(), v);
    if let Some(envelope) = envelope {
        let run = current_run(next)?;
        let prerequisites: Vec<_> = run.prerequisites.values().cloned().collect();
        let decision = evidence::admit(&envelope, next, &envelope.pin_hash, &prerequisites);
        if decision.status != EvidenceStatus::Accepted {
            return reject(RejectionCode::InvalidState);
        }
        records.push(EvidenceWrite::Admitted {
            envelope: Box::new(envelope),
            decision,
        });
    }
    changed(execution_id)
}
