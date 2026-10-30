use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use super::candidate_seal::CandidateSeal;
use super::definition::WORKFLOW_VERSION;
use super::evidence_gate::{
    GateReason, StageAssessment, Verdict, assess_verification, evaluate_stage,
};
use super::failure_budget::{FailureDisposition, record_quality_failure};
use super::model::*;
use super::profile::{Candidate, ProfileLimits, RepositoryProfile};
use super::review_gate::{StagePrerequisite, evaluate_review_stage};
use super::verification_run::{
    AuthoritySource, BoundSubject, CurrentFence, PhaseAdmission, RegistrationInput,
    VerificationError, VerificationRun, VerificationStartAuthority, digest_of,
};

// None leaves the state unchanged: a retransmitted command adds no V, epoch, count or action.
pub(super) type Transition = Result<Option<EventKind>, Rejection>;

fn reject<T>(code: RejectionCode) -> Result<T, Rejection> {
    Err(Rejection::new(code))
}

fn gate_not_passed<T>(assessment: &StageAssessment) -> Result<T, Rejection> {
    Err(Rejection {
        target: Some(format!("{:?}", assessment.verdict)),
        ..Rejection::new(RejectionCode::GateNotPassed)
    })
}

fn current_run(state: &CampaignState) -> Result<&Run, Rejection> {
    state
        .run
        .as_ref()
        .ok_or_else(|| Rejection::new(RejectionCode::StaleRun))
}

fn node<'a>(state: &'a CampaignState, id: &str) -> Result<&'a NodeRuntime, Rejection> {
    state
        .nodes
        .get(id)
        .ok_or_else(|| Rejection::new(RejectionCode::StaleBinding))
}

fn digest<T: serde::Serialize>(value: &T) -> Result<String, Rejection> {
    digest_of(value).map_err(|_| Rejection::new(RejectionCode::InvalidState))
}

fn limits(state: &CampaignState) -> Result<ProfileLimits, Rejection> {
    RepositoryProfile::for_repository(state.definition.input().repository)
        .map(|p| p.limits)
        .map_err(|_| Rejection::new(RejectionCode::InvalidState))
}

// The launch intent is a new Planned action of exactly this attempt; it starts only through the gate.
fn launch_intent(run: &Run, launch: &Action, binding: &AttemptBinding) -> Result<(), Rejection> {
    if launch.id.trim().is_empty()
        || launch.kind != ActionKind::Launch
        || launch.binding != *binding
        || launch.state != ActionState::Planned
        || launch.initiation_receipt.is_some()
        || launch.reconciliation.is_some()
        || [
            &launch.target.host,
            &launch.target.gate,
            &launch.target.incarnation,
        ]
        .iter()
        .any(|v| v.is_empty())
    {
        return reject(RejectionCode::InvalidState);
    }
    if run.actions.contains_key(&launch.id) {
        return reject(RejectionCode::AlreadyExists);
    }
    Ok(())
}

fn bound_to_run(state: &CampaignState, run: &Run, binding: &AttemptBinding) -> bool {
    binding.campaign_id == state.id
        && binding.run_id == run.id
        && binding.repository == state.definition.input().repository
        && binding.workflow_version == WORKFLOW_VERSION
        && state.definition.hashes().get(&binding.node_id) == Some(&binding.hashes)
}

// Registers every check slot with the Work attempt, its pin and launch intent in one decision.
pub(super) fn assign_work(
    next: &mut CampaignState,
    binding: &AttemptBinding,
    assignment: &WorkAssignment,
    launch: &Action,
    at: DateTime<Utc>,
) -> Transition {
    if next.control != CampaignControl::Running {
        return reject(RejectionCode::InvalidState);
    }
    let run = current_run(next)?;
    let pin_hash = assignment
        .pin
        .hash()
        .map_err(|_| Rejection::new(RejectionCode::InvalidState))?;
    if let Some(existing) = run.attempts.get(&binding.attempt_id) {
        let same = existing.binding == *binding
            && existing.execution.as_ref().is_some_and(|e| {
                (&e.pin_hash, &e.spec, &e.acceptance)
                    == (&pin_hash, &assignment.spec, &assignment.acceptance)
            });
        return if same {
            Ok(None)
        } else {
            reject(RejectionCode::AlreadyExists)
        };
    }
    let current = node(next, &binding.node_id)?;
    if binding.control_version != run.control_version {
        return reject(RejectionCode::StaleFence);
    }
    if !bound_to_run(next, run, binding)
        || binding.role != Role::Implementation
        || binding.stage != Stage::Implementation
        || binding.series != current.series
        || assignment.pin.hashes != binding.hashes
        || assignment.pin.workflow_version != WORKFLOW_VERSION
        || assignment.spec.repository != binding.repository
        || assignment.spec.hash().ok().as_ref() != Some(&assignment.pin.verification_spec_hash)
    {
        return reject(RejectionCode::StaleBinding);
    }
    let busy = current
        .current_attempt
        .as_ref()
        .and_then(|id| run.attempts.get(id))
        .is_some_and(|a| a.outcome.is_none());
    if busy
        || current.stage != Stage::Implementation
        || current.interruption.is_some()
        || matches!(
            current.disposition,
            NodeDisposition::Failed | NodeDisposition::Complete
        )
    {
        return reject(RejectionCode::InvalidState);
    }
    launch_intent(run, launch, binding)?;
    // Every required check must be owed by one of the two gates; an empty start set never passes.
    let start = &assignment.review_start_checks;
    let finish = &assignment.review_finish_checks;
    let mut owed: Vec<_> = start.iter().chain(finish).collect();
    owed.sort();
    owed.dedup();
    if start.is_empty()
        || owed.len() != start.len() + finish.len()
        || assignment
            .spec
            .checks
            .iter()
            .any(|c| c.required && !owed.contains(&&c.id))
        || owed.iter().any(|id| assignment.spec.check(id).is_none())
    {
        return reject(RejectionCode::InvalidState);
    }
    let epoch = current.epoch + 1;
    let repair_cycle = current.repair_cycles;
    let mut registered = Vec::new();
    let mut actions = Vec::new();
    for check_id in start.iter().chain(finish) {
        let (v, phases) = VerificationRun::register(RegistrationInput {
            owner_binding: binding.clone(),
            repair_cycle,
            registered_node_epoch: epoch,
            pin_hash: pin_hash.clone(),
            spec: assignment.spec.clone(),
            check_id: check_id.clone(),
            gate: assignment.gate.clone(),
            registered_at: at,
        })
        .map_err(|_| Rejection::new(RejectionCode::InvalidState))?;
        if run.verifications.contains_key(&v.origin.execution_id)
            || phases.iter().any(|a| run.actions.contains_key(&a.id))
        {
            return reject(RejectionCode::AlreadyExists);
        }
        registered.push(v);
        actions.extend(phases);
    }
    let ids: Vec<String> = registered
        .iter()
        .map(|v| v.origin.execution_id.clone())
        .collect();
    let execution = AttemptExecution {
        pin_hash,
        repair_cycle,
        node_epoch: epoch,
        spec: assignment.spec.clone(),
        acceptance: assignment.acceptance.clone(),
        start_executions: ids[..start.len()].to_vec(),
        finish_executions: ids[start.len()..].to_vec(),
    };
    let run = next.run.as_mut().expect("checked above");
    run.attempts.insert(
        binding.attempt_id.clone(),
        Attempt {
            binding: binding.clone(),
            started: false,
            outcome: None,
            execution: Some(Box::new(execution)),
        },
    );
    for v in registered {
        run.verifications.insert(v.origin.execution_id.clone(), v);
    }
    for action in actions.into_iter().chain([launch.clone()]) {
        run.actions.insert(action.id.clone(), action);
    }
    let node = next.nodes.get_mut(&binding.node_id).expect("checked above");
    node.epoch = epoch;
    node.current_attempt = Some(binding.attempt_id.clone());
    node.disposition = NodeDisposition::Assigned;
    node.wait_reason = None;
    Ok(Some(EventKind::VerificationRegistered {
        node_id: binding.node_id.clone(),
        attempt_id: binding.attempt_id.clone(),
        executions: ids,
    }))
}

// The stage verdict over exact V IDs, recomputed from stored runs against the sealed candidate.
fn verification_verdict(
    run: &Run,
    execution: &AttemptExecution,
    ids: &[String],
    candidate: &Candidate,
    stage: Stage,
) -> StageAssessment {
    let plan = execution
        .spec
        .plan(stage, candidate, execution.acceptance.clone());
    let assessed: BTreeMap<_, _> = ids
        .iter()
        .filter_map(|id| {
            let v = run.verifications.get(id)?;
            Some((id.clone(), assess_verification(v, &run.actions, &plan)))
        })
        .collect();
    evaluate_stage(ids, &assessed)
}

fn subject_candidate(subject: &BoundSubject) -> Candidate {
    Candidate {
        repository: subject.repository,
        base_sha: subject.base_sha.clone(),
        head_sha: subject.output_sha.clone(),
    }
}

fn sealed_candidate(seal: &CandidateSeal) -> Candidate {
    Candidate {
        repository: seal.repository,
        base_sha: seal.base_sha.clone(),
        head_sha: seal.output_sha.clone(),
    }
}

// Work -> Review: A's result, R, one epoch advance, R's exact prerequisites and launch intent together.
pub(super) fn begin_review(
    next: &mut CampaignState,
    node_id: &str,
    expected_epoch: u64,
    seal: &CandidateSeal,
    review: &AttemptBinding,
    launch: &Action,
) -> Transition {
    if next.control != CampaignControl::Running {
        return reject(RejectionCode::InvalidState);
    }
    let run = current_run(next)?;
    let current = node(next, node_id)?;
    if let Some(existing) = run.attempts.get(&review.attempt_id) {
        let same = existing.binding == *review
            && current.current_attempt.as_ref() == Some(&review.attempt_id)
            && run
                .attempts
                .get(&seal.owner_attempt)
                .is_some_and(|a| a.outcome == Some(AttemptOutcome::Passed));
        return if same {
            Ok(None)
        } else {
            reject(RejectionCode::AlreadyExists)
        };
    }
    if current.epoch != expected_epoch || current.interruption.is_some() {
        return reject(RejectionCode::StaleFence);
    }
    let work = current
        .current_attempt
        .as_ref()
        .and_then(|id| run.attempts.get(id))
        .ok_or_else(|| Rejection::new(RejectionCode::StaleBinding))?;
    let Some(execution) = work.execution.as_deref() else {
        return reject(RejectionCode::StaleBinding);
    };
    if work.outcome.is_some()
        || !work.started
        || work.binding.stage != Stage::Implementation
        || current.stage != Stage::Implementation
    {
        return reject(RejectionCode::InvalidState);
    }
    let a = &work.binding;
    if seal.owner_attempt != a.attempt_id
        || seal.seal_id != format!("seal:{}", a.attempt_id)
        || seal.binding_digest != digest(a)?
        || seal.pin_hash != execution.pin_hash
        || seal.repository != a.repository
        || seal.base_sha != a.base_sha
    {
        return reject(RejectionCode::StaleBinding);
    }
    if !bound_to_run(next, run, review)
        || review.role != Role::Review
        || review.stage != Stage::Review
        || (&review.node_id, review.series, &review.hashes) != (&a.node_id, a.series, &a.hashes)
        || (&review.base_sha, &review.candidate_sha) != (&seal.base_sha, &seal.output_sha)
        || review.control_version != run.control_version
        || review.session == a.session
    {
        return reject(RejectionCode::StaleBinding);
    }
    launch_intent(run, launch, review)?;
    let start = verification_verdict(
        run,
        execution,
        &execution.start_executions,
        &sealed_candidate(seal),
        a.stage,
    );
    if start.verdict != Verdict::Pass {
        return gate_not_passed(&start);
    }
    let epoch = current.epoch + 1;
    let mut prerequisites = Vec::new();
    for id in &execution.finish_executions {
        let v = run
            .verifications
            .get(id)
            .ok_or_else(|| Rejection::new(RejectionCode::StaleBinding))?;
        let prerequisite = StagePrerequisite::bind(v, seal, review, epoch, run.control_version)
            .map_err(|_| Rejection::new(RejectionCode::StaleBinding))?;
        prerequisites.push(prerequisite);
    }
    let work_id = a.attempt_id.clone();
    let run = next.run.as_mut().expect("checked above");
    if let Some(work) = run.attempts.get_mut(&work_id) {
        work.outcome = Some(AttemptOutcome::Passed);
    }
    run.attempts.insert(
        review.attempt_id.clone(),
        Attempt {
            binding: review.clone(),
            started: false,
            outcome: None,
            execution: None,
        },
    );
    let ids = prerequisites.iter().map(|p| p.id.clone()).collect();
    for prerequisite in prerequisites {
        run.prerequisites
            .insert(prerequisite.id.clone(), prerequisite);
    }
    run.actions.insert(launch.id.clone(), launch.clone());
    let node = next.nodes.get_mut(node_id).expect("checked above");
    node.epoch = epoch;
    node.stage = Stage::Review;
    node.current_attempt = Some(review.attempt_id.clone());
    node.disposition = NodeDisposition::Assigned;
    Ok(Some(EventKind::ReviewPrerequisitesBound {
        node_id: node_id.into(),
        review_attempt: review.attempt_id.clone(),
        prerequisites: ids,
    }))
}

// Only three holders may start a registered phase: the owning Work, the current Review through
// its fixed prerequisite, or the frozen FinishPlan. A stale holder is refused without cancelling.
pub(super) fn validate_verification_start(
    state: &CampaignState,
    v: &VerificationRun,
    consumer: &AttemptBinding,
) -> Result<VerificationStartAuthority, Rejection> {
    let run = current_run(state)?;
    let current = node(state, &v.origin.node_id)?;
    let attempt = run
        .attempts
        .get(&consumer.attempt_id)
        .filter(|a| a.binding == *consumer && consumer.node_id == v.origin.node_id)
        .ok_or_else(|| Rejection::new(RejectionCode::StaleBinding))?;
    if current.interruption.is_some()
        || current.current_attempt.as_ref() != Some(&consumer.attempt_id)
        || attempt.outcome.is_some()
        || (current.stage, current.series) != (consumer.stage, consumer.series)
    {
        return reject(RejectionCode::StaleFence);
    }
    let local_unknown = run.sessions.iter().any(|s| {
        matches!(s.observation, SessionObservation::Unknown { .. })
            && run
                .attempts
                .get(&s.attempt_id)
                .is_some_and(|a| a.binding.node_id == v.origin.node_id)
    });
    if local_unknown {
        return reject(RejectionCode::SessionsNotGone);
    }
    let consumer_digest = digest(consumer)?;
    let authority = |source| VerificationStartAuthority {
        execution_id: v.origin.execution_id.clone(),
        original_binding_digest: v.origin.binding_digest.clone(),
        consumer_attempt: consumer.attempt_id.clone(),
        consumer_binding_digest: consumer_digest.clone(),
        consumer_stage: consumer.stage,
        consumer_node_epoch: current.epoch,
        run_control_version: run.control_version,
        source,
    };
    let id = &v.origin.execution_id;
    match (state.control, &run.stop) {
        (CampaignControl::Running, None) if consumer.attempt_id == v.origin.owner_attempt => {
            let owns = attempt.execution.as_deref().is_some_and(|e| {
                e.node_epoch == current.epoch
                    && e.start_executions
                        .iter()
                        .chain(&e.finish_executions)
                        .any(|x| x == id)
            });
            if !owns
                || consumer.control_version != run.control_version
                || v.origin.registered_node_epoch != current.epoch
                || v.origin.repair_cycle != current.repair_cycles
            {
                return reject(RejectionCode::StaleFence);
            }
            Ok(authority(AuthoritySource::CurrentWork))
        }
        (CampaignControl::Running, None) => {
            let prerequisite = run
                .prerequisites
                .values()
                .find(|p| p.consumer_attempt == consumer.attempt_id && &p.target.execution_id == id)
                .ok_or_else(|| Rejection::new(RejectionCode::StaleFence))?;
            if prerequisite.consumer_binding_digest != consumer_digest
                || prerequisite.consumer_node_epoch != current.epoch
                || prerequisite.run_control_version != run.control_version
                || prerequisite.target.matches(v).is_err()
            {
                return reject(RejectionCode::StaleFence);
            }
            Ok(prerequisite.authority())
        }
        (CampaignControl::Stopping, Some(stop)) if stop.mode == StopMode::FinishStage => {
            let grant = stop
                .finish_grants
                .iter()
                .find(|g| &g.execution_id == id && g.consumer_attempt == consumer.attempt_id)
                .ok_or_else(|| Rejection::new(RejectionCode::StaleFence))?;
            if grant.consumer_binding_digest != consumer_digest
                || grant.consumer_node_epoch != current.epoch
                || grant.original_binding_digest != v.origin.binding_digest
            {
                return reject(RejectionCode::StaleFence);
            }
            Ok(authority(AuthoritySource::FinishPlan {
                stop_control_version: run.control_version,
                grant_ref: format!("{}:{}", grant.consumer_attempt, grant.execution_id),
            }))
        }
        _ => reject(RejectionCode::StaleFence),
    }
}

// Claim or StartPending of a pre-registered phase; started, finished or unknown phases never relaunch.
pub(super) fn advance_verification_phase(
    next: &mut CampaignState,
    execution_id: &str,
    phase_id: &str,
    consumer: &AttemptBinding,
    target: ActionState,
) -> Transition {
    let run = current_run(next)?;
    let v = run
        .verifications
        .get(execution_id)
        .ok_or_else(|| Rejection::new(RejectionCode::StaleBinding))?;
    let authority = validate_verification_start(next, v, consumer)?;
    let action_id = v
        .phase(phase_id)
        .map_err(|_| Rejection::new(RejectionCode::StaleBinding))?
        .action_id
        .clone();
    let action = run
        .actions
        .get(&action_id)
        .ok_or_else(|| Rejection::new(RejectionCode::StaleBinding))?;
    let fence = CurrentFence {
        run_control_version: run.control_version,
        node_epoch: node(next, &v.origin.node_id)?.epoch,
    };
    match v.admit_phase_start(phase_id, action, &authority, fence) {
        Ok(PhaseAdmission::Start { .. }) => {}
        Ok(_) => return Ok(None),
        Err(VerificationError::StaleAuthority) => return reject(RejectionCode::StaleFence),
        Err(_) => return reject(RejectionCode::InvalidState),
    }
    match (action.state, target) {
        (ActionState::Planned, ActionState::Claimed)
        | (ActionState::Claimed, ActionState::StartPending) => {}
        (current, wanted) if current == wanted => return Ok(None),
        _ => return reject(RejectionCode::InvalidState),
    }
    let run = next.run.as_mut().expect("checked above");
    if let Some(action) = run.actions.get_mut(&action_id) {
        action.state = target;
    }
    Ok(Some(EventKind::ActionChanged { action_id }))
}

// Cancels the unstarted phases of these V; started phases keep their identity for reconciliation.
pub(super) fn revoke_unstarted(
    run: &mut Run,
    executions: &[String],
    evidence: &str,
    at: DateTime<Utc>,
) -> Vec<String> {
    let mut revoked = Vec::new();
    for id in executions {
        let Some(v) = run.verifications.get_mut(id) else {
            continue;
        };
        if v.collection.is_some() {
            continue;
        }
        let unstarted: Vec<String> = match v.cancel(&run.actions, evidence.into(), at) {
            Ok(ids) => ids,
            Err(_) => v.phases.iter().map(|p| p.action_id.clone()).collect(),
        };
        for action_id in unstarted {
            if let Some(action) = run.actions.get_mut(&action_id)
                && matches!(action.state, ActionState::Planned | ActionState::Claimed)
            {
                action.state = ActionState::Cancelled;
                revoked.push(action_id);
            }
        }
    }
    revoked
}

fn node_executions(run: &Run, node_id: &str) -> Vec<String> {
    run.verifications
        .values()
        .filter(|v| v.origin.node_id == node_id)
        .map(|v| v.origin.execution_id.clone())
        .collect()
}

// Records the interruption intent and revokes every start authority of this node only; the actual
// stop of started work is a later effect, so started phases and bindings are kept as they are.
pub(super) fn interrupt_node(
    next: &mut CampaignState,
    expected_run: &str,
    node_id: &str,
    expected_epoch: u64,
    session: &SessionIdentity,
    reason: &str,
    at: DateTime<Utc>,
) -> Transition {
    if !matches!(
        next.control,
        CampaignControl::Running | CampaignControl::Stopping
    ) || reason.trim().is_empty()
    {
        return reject(RejectionCode::InvalidState);
    }
    let run = current_run(next)?;
    if run.id != expected_run {
        return reject(RejectionCode::StaleRun);
    }
    let current = node(next, node_id)?;
    if let Some(existing) = &current.interruption {
        return if existing.session == *session {
            Ok(None)
        } else {
            reject(RejectionCode::StaleBinding)
        };
    }
    let attempt_id = current.current_attempt.clone();
    let bound = attempt_id
        .as_ref()
        .and_then(|id| run.attempts.get(id))
        .is_some_and(|a| a.binding.session == *session);
    if !bound {
        return reject(RejectionCode::StaleBinding);
    }
    if current.epoch != expected_epoch {
        return reject(RejectionCode::StaleFence);
    }
    let executions = node_executions(run, node_id);
    let run = next.run.as_mut().expect("checked above");
    let mut revoked = revoke_unstarted(run, &executions, "node interruption", at);
    for action in run.actions.values_mut() {
        if action.binding.node_id == node_id
            && action.kind != ActionKind::VerificationPhase
            && matches!(action.state, ActionState::Planned | ActionState::Claimed)
        {
            action.state = ActionState::Cancelled;
            revoked.push(action.id.clone());
        }
    }
    if let Some(stop) = run.stop.as_mut() {
        stop.finish_grants
            .retain(|g| !executions.contains(&g.execution_id));
    }
    let node = next.nodes.get_mut(node_id).expect("checked above");
    node.epoch += 1;
    node.interruption = Some(NodeInterruption {
        epoch: node.epoch,
        attempt_id,
        session: session.clone(),
        reason: reason.into(),
        requested_at: at,
    });
    Ok(Some(EventKind::NodeInterruptionRequested {
        node_id: node_id.into(),
        epoch: node.epoch,
        revoked,
    }))
}

// Finalizes the current attempt's stage result from stored V runs (and the review gate for R).
// Waits and insufficient evidence keep the attempt current; only a confirmed failure is counted.
pub(super) fn accept_stage_result(
    next: &mut CampaignState,
    node_id: &str,
    expected_epoch: u64,
    attempt_id: &str,
    review: Option<&StageAssessment>,
    at: DateTime<Utc>,
) -> Transition {
    let run = current_run(next)?;
    let finishing = run
        .stop
        .as_ref()
        .is_some_and(|s| s.mode == StopMode::FinishStage && s.finish_attempts.contains(attempt_id));
    if !(next.control == CampaignControl::Running
        || (next.control == CampaignControl::Stopping && finishing))
    {
        return reject(RejectionCode::InvalidState);
    }
    let current = node(next, node_id)?;
    if current.epoch != expected_epoch || current.interruption.is_some() {
        return reject(RejectionCode::StaleFence);
    }
    let attempt = run
        .attempts
        .get(attempt_id)
        .filter(|a| current.current_attempt.as_deref() == Some(attempt_id) && a.outcome.is_none())
        .ok_or_else(|| Rejection::new(RejectionCode::StaleBinding))?;
    let (assessment, cycle, executions) = match (attempt.binding.stage, review) {
        (Stage::Implementation, None) => {
            let execution = attempt
                .execution
                .as_deref()
                .ok_or_else(|| Rejection::new(RejectionCode::StaleBinding))?;
            // A self-check is judged against the output its own V verified; no bound subject is a wait.
            let candidate = execution
                .start_executions
                .iter()
                .filter_map(|id| run.verifications.get(id)?.subject.bound())
                .map(subject_candidate)
                .next();
            let assessment = match candidate {
                Some(subject) => verification_verdict(
                    run,
                    execution,
                    &execution.start_executions,
                    &subject,
                    attempt.binding.stage,
                ),
                None => StageAssessment::of(Verdict::Wait, GateReason::Pending(attempt_id.into())),
            };
            if assessment.verdict == Verdict::Pass {
                // A passing Work stage hands over only through BeginReview.
                return reject(RejectionCode::InvalidState);
            }
            let executions = execution
                .start_executions
                .iter()
                .chain(&execution.finish_executions)
                .cloned()
                .collect();
            (assessment, execution.repair_cycle, executions)
        }
        (Stage::Review, Some(review)) => {
            let mut parts = Vec::new();
            let mut executions = Vec::new();
            let mut cycle = None;
            for prerequisite in run
                .prerequisites
                .values()
                .filter(|p| p.consumer_attempt == attempt_id)
            {
                let target = &prerequisite.target;
                executions.push(target.execution_id.clone());
                cycle = Some(target.repair_cycle);
                let owner = run
                    .attempts
                    .get(&target.owner_attempt)
                    .and_then(|a| a.execution.as_deref());
                let part = match (
                    run.verifications.get(&target.execution_id),
                    owner,
                    &target.subject,
                ) {
                    (Some(v), Some(owner), Some(subject))
                        if prerequisite.consumes(&attempt.binding, v).is_ok() =>
                    {
                        let candidate = subject_candidate(subject);
                        let id = std::slice::from_ref(&target.execution_id);
                        verification_verdict(run, owner, id, &candidate, Stage::Implementation)
                    }
                    _ => StageAssessment::of(
                        Verdict::Reject,
                        GateReason::OriginMismatch(target.execution_id.clone()),
                    ),
                };
                parts.push(part);
            }
            let cycle = cycle.unwrap_or(current.repair_cycles);
            (
                evaluate_review_stage(review.clone(), parts),
                cycle,
                executions,
            )
        }
        _ => return reject(RejectionCode::InvalidState),
    };
    let verdict = assessment.verdict;
    let limits = limits(next)?;
    let run = next.run.as_mut().expect("checked above");
    let node = next.nodes.get_mut(node_id).expect("checked above");
    match verdict {
        Verdict::Wait | Verdict::Reject => {
            node.disposition = NodeDisposition::Verifying;
        }
        Verdict::Pass | Verdict::QualityFail => {
            let outcome = if verdict == Verdict::Pass {
                node.stage = Stage::Landing;
                node.disposition = NodeDisposition::Pending;
                AttemptOutcome::Passed
            } else {
                if record_quality_failure(node, cycle, &limits) == FailureDisposition::Repair {
                    node.stage = Stage::Implementation;
                }
                AttemptOutcome::QualityFailed
            };
            if let Some(attempt) = run.attempts.get_mut(attempt_id) {
                attempt.outcome = Some(outcome);
            }
            node.epoch += 1;
            node.current_attempt = None;
            revoke_unstarted(run, &executions, "stage result finalized", at);
        }
    }
    Ok(Some(EventKind::StageEvaluated {
        node_id: node_id.into(),
        attempt_id: attempt_id.into(),
        verdict,
    }))
}

// FinishStage freezes the exact V each finishing consumer may still continue.
pub(super) fn finish_grants(state: &CampaignState, run: &Run) -> Vec<FinishGrant> {
    let mut grants = Vec::new();
    for current in state.nodes.values().filter(|n| n.interruption.is_none()) {
        let Some(attempt) = current
            .current_attempt
            .as_ref()
            .and_then(|id| run.attempts.get(id))
            .filter(|a| a.started && a.outcome.is_none())
        else {
            continue;
        };
        let Ok(consumer_digest) = digest_of(&attempt.binding) else {
            continue;
        };
        let owned = attempt
            .execution
            .iter()
            .flat_map(|e| e.start_executions.iter().chain(&e.finish_executions))
            .cloned();
        let consumed = run
            .prerequisites
            .values()
            .filter(|p| p.consumer_attempt == attempt.binding.attempt_id)
            .map(|p| p.target.execution_id.clone());
        for id in owned.chain(consumed) {
            let Some(v) = run
                .verifications
                .get(&id)
                .filter(|v| v.collection.is_none() && v.cancellation.is_none())
            else {
                continue;
            };
            grants.push(FinishGrant {
                execution_id: id,
                original_binding_digest: v.origin.binding_digest.clone(),
                consumer_attempt: attempt.binding.attempt_id.clone(),
                consumer_binding_digest: consumer_digest.clone(),
                consumer_stage: attempt.binding.stage,
                consumer_node_epoch: current.epoch,
                phase_actions: v.phases.iter().map(|p| p.action_id.clone()).collect(),
            });
        }
    }
    grants
}

#[cfg(test)]
#[path = "stage_fsm_tests.rs"]
mod tests;
