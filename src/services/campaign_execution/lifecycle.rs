use std::collections::BTreeMap;

use crate::domain::campaign::{
    candidate_seal::CandidateSeal,
    decision_item::{ApprovalBinding, ApprovalTarget},
    definition::valid_id,
    deployment::DeployJob,
    model::ReviewedCandidate,
    workflow_options::ApprovalGate,
};

use super::control;
use super::model::*;
use super::stage_fsm::Transition;

fn reject<T>(code: RejectionCode) -> Result<T, Rejection> {
    Err(Rejection::new(code))
}

// The exact candidate and pinned policy a boundary's approval has to name.
pub(super) struct GateTarget<'a> {
    pub base_sha: &'a str,
    pub head_sha: &'a str,
    pub policy_hash: &'a str,
}

impl<'a> GateTarget<'a> {
    pub(super) fn of(candidate: &'a ReviewedCandidate) -> Self {
        Self {
            base_sha: &candidate.base_sha,
            head_sha: &candidate.head_sha,
            policy_hash: &candidate.pin_hash,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GateStatus {
    Clear,
    Unopened,
    Unapproved,
}

// A configured slot is clear only while its current gate is approved for this run and revision
// and, where the boundary knows them, for this exact candidate and pin.
pub(super) fn gate_status(
    state: &CampaignState,
    run: &Run,
    node_id: &str,
    gate: ApprovalGate,
    target: Option<&GateTarget<'_>>,
) -> GateStatus {
    let options = state.definition.input().effective_workflow_options();
    if !options.required(gate) {
        return GateStatus::Clear;
    }
    let book = &run.approvals;
    let item = book
        .current
        .values()
        .filter_map(|id| book.items.get(id))
        .find(|item| item.binding.node_id == node_id && item.binding.gate == gate);
    let Some(item) = item else {
        return GateStatus::Unopened;
    };
    let bound = &item.binding;
    let exact = target.is_none_or(|t| {
        bound.policy_hash == t.policy_hash
            && matches!(&bound.target, ApprovalTarget::Candidate { base_sha, head_sha }
                if base_sha == t.base_sha && head_sha == t.head_sha)
    });
    if bound.run_id != run.id
        || bound.definition_revision != state.legacy_execution_revision()
        || !exact
    {
        GateStatus::Unopened
    } else if item.approval.is_none() {
        GateStatus::Unapproved
    } else {
        GateStatus::Clear
    }
}

pub(super) fn require_gate(
    state: &CampaignState,
    run: &Run,
    node_id: &str,
    gate: ApprovalGate,
    target: &GateTarget<'_>,
) -> Result<(), Rejection> {
    if gate_status(state, run, node_id, gate, Some(target)) == GateStatus::Clear {
        return Ok(());
    }
    Err(Rejection {
        target: Some(format!("approval:{gate:?}")),
        ..Rejection::new(RejectionCode::GateNotPassed)
    })
}

// A node's first Work also passes the Start slot; every Work passes Implementation.
pub(super) fn work_gates(stage: Stage) -> &'static [ApprovalGate] {
    match stage {
        Stage::Requirements => &[ApprovalGate::Start, ApprovalGate::Implementation],
        _ => &[ApprovalGate::Implementation],
    }
}

pub(super) fn unmet_dependency<'a>(state: &'a CampaignState, node_id: &str) -> Option<&'a str> {
    let definition = state.definition.input();
    let node = definition.nodes.iter().find(|n| n.id == node_id)?;
    node.dependencies
        .iter()
        .find(|d| !state.nodes.get(*d).is_some_and(|n| n.main_integrated))
        .map(String::as_str)
}

// Every Work waits for its predecessors' main integration and the approvals of its candidate and pin.
pub(super) fn admit_work(
    state: &CampaignState,
    run: &Run,
    binding: &AttemptBinding,
    stage: Stage,
    pin_hash: &str,
) -> Result<(), Rejection> {
    if let Some(dependency) = unmet_dependency(state, &binding.node_id) {
        return Err(Rejection {
            target: Some(format!("predecessor:{dependency}")),
            ..Rejection::new(RejectionCode::GateNotPassed)
        });
    }
    let target = GateTarget {
        base_sha: &binding.base_sha,
        head_sha: &binding.candidate_sha,
        policy_hash: pin_hash,
    };
    for gate in work_gates(stage) {
        require_gate(state, run, &binding.node_id, *gate, &target)?;
    }
    Ok(())
}

pub(super) fn reviewed(seal: &CandidateSeal) -> ReviewedCandidate {
    ReviewedCandidate {
        work_attempt: seal.owner_attempt.clone(),
        base_sha: seal.base_sha.clone(),
        head_sha: seal.output_sha.clone(),
        pin_hash: seal.pin_hash.clone(),
    }
}

// The candidate Landing may admit: the reviewed one, still bound to the current definition.
pub(super) fn landing_candidate<'a>(
    state: &CampaignState,
    run: &'a Run,
    node_id: &str,
) -> Option<&'a ReviewedCandidate> {
    let candidate = run.fences.get(node_id)?.candidate.as_ref()?;
    let work = run.attempts.get(&candidate.work_attempt)?;
    (state.definition.hashes().get(node_id) == Some(&work.binding.hashes)).then_some(candidate)
}

// Everything the node started is closed: no open attempt, unsettled action or live session.
pub(super) fn settled(run: &Run, node_id: &str) -> bool {
    let own = |attempt_id: &str| {
        run.attempts
            .get(attempt_id)
            .is_some_and(|a| a.binding.node_id == node_id)
    };
    run.attempts
        .values()
        .all(|a| a.binding.node_id != node_id || a.outcome.is_some())
        && run.actions.values().all(|a| {
            a.binding.node_id != node_id
                || matches!(a.state, ActionState::Reconciled | ActionState::Cancelled)
        })
        && run.sessions.iter().all(|s| {
            !own(&s.attempt_id) || matches!(s.observation, SessionObservation::Gone { .. })
        })
}

pub(super) fn confirmed(node: &NodeRuntime) -> bool {
    node.main_integrated && node.acceptance_complete && node.evidence_current
}

fn begin_run(next: &mut CampaignState, expected_revision: u64, run_id: &str) -> Transition {
    if next.definition_revision != expected_revision {
        return reject(RejectionCode::StaleDefinition);
    }
    // A retransmitted start never restarts a run that already exists, stopped or not.
    if let Some(run) = &next.run {
        return if run.id == run_id {
            Ok(None)
        } else {
            reject(RejectionCode::AlreadyExists)
        };
    }
    if next.control != CampaignControl::Prepared || !valid_id(run_id) {
        return reject(RejectionCode::InvalidState);
    }
    next.run = Some(Run {
        id: run_id.into(),
        control_version: 1,
        stop: None,
        attempts: BTreeMap::new(),
        actions: BTreeMap::new(),
        sessions: Vec::new(),
        verifications: BTreeMap::new(),
        prerequisites: BTreeMap::new(),
        reviews: BTreeMap::new(),
        landings: BTreeMap::new(),
        merges: BTreeMap::new(),
        deployments: BTreeMap::new(),
        acceptance_checks: BTreeMap::new(),
        fences: BTreeMap::new(),
        seals: BTreeMap::new(),
        environments: BTreeMap::new(),
        approvals: Default::default(),
    });
    next.control = CampaignControl::Running;
    Ok(Some(EventKind::RunStarted))
}

// Resumes the same run once it is quiet. An interrupted Work or Review returns to a fresh Work of
// the same repair cycle; a stop is never a quality failure and a failed node stays failed.
fn resume_run(next: &mut CampaignState, expected_revision: u64, expected_run: &str) -> Transition {
    if next.definition_revision != expected_revision {
        return reject(RejectionCode::StaleDefinition);
    }
    let Some(run) = next.run.as_ref().filter(|r| r.id == expected_run) else {
        return reject(RejectionCode::StaleRun);
    };
    if next.control == CampaignControl::Running && run.stop.is_none() {
        return Ok(None);
    }
    if next.control != CampaignControl::Stopped {
        return reject(RejectionCode::InvalidState);
    }
    control::edit_guard(next)?;
    let run = next.run.as_mut().expect("checked above");
    run.stop = None;
    run.control_version += 1;
    let mut reset_nodes = Vec::new();
    for (id, node) in &mut next.nodes {
        let work = matches!(node.stage, Stage::Implementation | Stage::Review)
            && !matches!(
                node.disposition,
                NodeDisposition::Failed | NodeDisposition::Complete
            );
        // The stop also revokes a Landing admission that started nothing; the door checks again.
        let admitted =
            (node.stage, node.disposition) == (Stage::Landing, NodeDisposition::Assigned);
        if !work && !admitted {
            continue;
        }
        if work {
            if let Some(attempt) = node
                .current_attempt
                .take()
                .and_then(|id| run.attempts.get_mut(&id))
                && attempt.outcome.is_none()
            {
                attempt.outcome = Some(AttemptOutcome::UserInterrupted);
            }
            node.stage = Stage::Implementation;
        }
        node.disposition = NodeDisposition::Pending;
        let fence = run.fences.entry(id.clone()).or_default();
        fence.epoch += 1;
        fence.interruption = None;
        reset_nodes.push(id.clone());
    }
    next.control = CampaignControl::Running;
    Ok(Some(EventKind::RunResumed { reset_nodes }))
}

// The running position of one node; lifecycle steps never act under a stop or an interruption.
fn position<'a>(
    state: &'a CampaignState,
    expected_run: &str,
    node_id: &str,
) -> Result<(&'a Run, &'a NodeRuntime, NodeFence), Rejection> {
    let Some(run) = state.run.as_ref().filter(|r| r.id == expected_run) else {
        return reject(RejectionCode::StaleRun);
    };
    if state.control != CampaignControl::Running || run.stop.is_some() {
        return reject(RejectionCode::InvalidState);
    }
    let Some(node) = state.nodes.get(node_id) else {
        return reject(RejectionCode::StaleBinding);
    };
    let fence = run.fences.get(node_id).cloned().unwrap_or_default();
    Ok((run, node, fence))
}

fn advance(
    next: &mut CampaignState,
    node_id: &str,
    stage: Stage,
    to: NodeDisposition,
) -> Transition {
    if let Some(run) = next.run.as_mut() {
        run.fences.entry(node_id.into()).or_default().epoch += 1;
    }
    if let Some(node) = next.nodes.get_mut(node_id) {
        node.stage = stage;
        node.disposition = to;
        node.wait_reason = None;
    }
    Ok(Some(EventKind::NodeAdvanced {
        node_id: node_id.into(),
        stage,
    }))
}

// Admits the reviewed candidate to Landing; landing effects start only from the admitted node.
fn begin_landing(
    next: &mut CampaignState,
    expected_run: &str,
    node_id: &str,
    expected_epoch: u64,
) -> Transition {
    let (run, node, fence) = position(next, expected_run, node_id)?;
    if node.stage != Stage::Landing {
        return reject(RejectionCode::InvalidState);
    }
    if node.disposition != NodeDisposition::Pending {
        let retried = node.disposition == NodeDisposition::Assigned
            && expected_epoch.checked_add(1) == Some(fence.epoch);
        return if retried {
            Ok(None)
        } else {
            reject(RejectionCode::InvalidState)
        };
    }
    if fence.epoch != expected_epoch || fence.interruption.is_some() {
        return reject(RejectionCode::StaleFence);
    }
    let Some(candidate) = landing_candidate(next, run, node_id) else {
        return reject(RejectionCode::StaleBinding);
    };
    let target = GateTarget::of(candidate);
    require_gate(next, run, node_id, ApprovalGate::Landing, &target)?;
    advance(next, node_id, Stage::Landing, NodeDisposition::Assigned)
}

// Completes a landed node on its recorded main, acceptance and current-evidence facts; the run
// succeeds with its last node.
fn complete_node(
    next: &mut CampaignState,
    expected_run: &str,
    node_id: &str,
    expected_epoch: u64,
) -> Transition {
    if next
        .nodes
        .get(node_id)
        .is_some_and(|n| n.stage == Stage::Complete)
        && next.run.as_ref().is_some_and(|r| r.id == expected_run)
    {
        return Ok(None);
    }
    let (run, node, fence) = position(next, expected_run, node_id)?;
    let admitted = match node.stage {
        Stage::Landing => node.disposition != NodeDisposition::Pending,
        Stage::MainVerification | Stage::FinalVerification => true,
        _ => false,
    };
    if !admitted || node.disposition == NodeDisposition::Failed {
        return reject(RejectionCode::InvalidState);
    }
    if fence.epoch != expected_epoch || fence.interruption.is_some() {
        return reject(RejectionCode::StaleFence);
    }
    if !confirmed(node) {
        return reject(RejectionCode::GateNotPassed);
    }
    if !settled(run, node_id) {
        return reject(RejectionCode::UnreconciledActions);
    }
    let event = advance(next, node_id, Stage::Complete, NodeDisposition::Complete);
    if next.nodes.values().all(|n| n.stage == Stage::Complete) {
        next.control = CampaignControl::Succeeded;
    }
    event
}

pub(super) fn decide(next: &mut CampaignState, command: &Command) -> Transition {
    match command {
        Command::BeginRun {
            expected_revision,
            run_id,
        } => begin_run(next, *expected_revision, run_id),
        Command::ResumeRun {
            expected_revision,
            expected_run,
        } => resume_run(next, *expected_revision, expected_run),
        Command::BeginLanding {
            expected_run,
            node_id,
            expected_epoch,
        } => begin_landing(next, expected_run, node_id, *expected_epoch),
        Command::CompleteNode {
            expected_run,
            node_id,
            expected_epoch,
        } => complete_node(next, expected_run, node_id, *expected_epoch),
        _ => reject(RejectionCode::InvalidState),
    }
}

pub(super) fn merge_position(
    state: &CampaignState,
    run: &Run,
    node_id: &str,
    landing: &super::landing_effects::Landing,
) -> bool {
    state.control == CampaignControl::Running
        && run.stop.is_none()
        && state.nodes.get(node_id).is_some_and(|node| {
            node.stage == Stage::Landing
                && node.disposition == NodeDisposition::Assigned
                && node.current_attempt.is_none()
        })
        && run.control_version == landing.control_version
        && run
            .fences
            .get(node_id)
            .is_some_and(|fence| fence.epoch == landing.epoch && fence.interruption.is_none())
        && landing_candidate(state, run, node_id).is_some_and(|candidate| {
            candidate.head_sha == landing.target.head_sha
                && candidate.pin_hash == landing.target.pin_hash
                && require_gate(
                    state,
                    run,
                    node_id,
                    ApprovalGate::Landing,
                    &GateTarget::of(candidate),
                )
                .is_ok()
        })
}

pub(super) fn merge_approval(
    state: &CampaignState,
    run: &Run,
    node_id: &str,
    landing: &super::landing_effects::Landing,
    pr: &super::landing_effects::OpenedPr,
) -> Option<(ApprovalBinding, bool)> {
    let target = ApprovalTarget::PullRequest {
        number: pr.number,
        head_sha: pr.head_sha.clone(),
    };
    let policy = &landing.target.pin_hash;
    slot_approval(state, run, node_id, ApprovalGate::Merge, policy, target)
}

// The Deploy slot names the exact main commit of the intent; another commit is another gate.
pub(super) fn deploy_approval(
    state: &CampaignState,
    run: &Run,
    node_id: &str,
    job: &DeployJob,
) -> Option<(ApprovalBinding, bool)> {
    let target = ApprovalTarget::Main {
        sha: job.intent.target_sha.clone(),
    };
    slot_approval(
        state,
        run,
        node_id,
        ApprovalGate::Deploy,
        &job.pin_hash,
        target,
    )
}

// A configured slot's exact binding and whether its current gate carries that approval.
fn slot_approval(
    state: &CampaignState,
    run: &Run,
    node_id: &str,
    gate: ApprovalGate,
    policy_hash: &str,
    target: ApprovalTarget,
) -> Option<(ApprovalBinding, bool)> {
    if !state
        .definition
        .input()
        .effective_workflow_options()
        .required(gate)
    {
        return None;
    }
    let binding = ApprovalBinding {
        run_id: run.id.clone(),
        node_id: node_id.into(),
        gate,
        definition_revision: state.legacy_execution_revision(),
        policy_hash: policy_hash.into(),
        target,
    };
    let approved = binding
        .slot_id()
        .ok()
        .and_then(|slot| run.approvals.current.get(&slot))
        .and_then(|id| run.approvals.items.get(id))
        .is_some_and(|item| item.binding == binding && item.approval.is_some());
    Some((binding, approved))
}

// The landed node whose deployment may still be requested or started: running, unstopped,
// waiting on main verification with its reviewed candidate still current.
pub(super) fn deploy_opening<'a>(
    state: &CampaignState,
    run: &'a Run,
    node_id: &str,
) -> Option<(u64, &'a ReviewedCandidate)> {
    let node = state.nodes.get(node_id)?;
    let fence = run.fences.get(node_id)?;
    (state.control == CampaignControl::Running
        && run.stop.is_none()
        && node.stage == Stage::MainVerification
        && node.disposition == NodeDisposition::Verifying
        && node.main_integrated
        && node.current_attempt.is_none()
        && fence.interruption.is_none())
    .then(|| landing_candidate(state, run, node_id))
    .flatten()
    .map(|candidate| (fence.epoch, candidate))
}

pub(super) fn deploy_position(
    state: &CampaignState,
    run: &Run,
    node_id: &str,
    job: &DeployJob,
) -> bool {
    deploy_opening(state, run, node_id).is_some_and(|(epoch, candidate)| {
        epoch == job.epoch
            && run.control_version == job.control_version
            && candidate.pin_hash == job.pin_hash
    })
}

// A witnessed deployment moves a running node on to its final checks; a stopped one stays put.
pub(super) fn deployment_confirmed(state: &mut CampaignState, node_id: &str) -> Transition {
    let running = state.control == CampaignControl::Running
        && state.run.as_ref().is_some_and(|run| run.stop.is_none());
    let stage = state.nodes.get(node_id).map(|node| node.stage);
    if running && stage == Some(Stage::MainVerification) {
        return advance(
            state,
            node_id,
            Stage::FinalVerification,
            NodeDisposition::Verifying,
        );
    }
    Ok(stage.map(|stage| EventKind::NodeAdvanced {
        node_id: node_id.into(),
        stage,
    }))
}

// Recorded only by the final acceptance judgement, for the candidate the definition still names.
pub(super) fn acceptance_judged(state: &mut CampaignState, node_id: &str) -> Transition {
    let node = state
        .nodes
        .get_mut(node_id)
        .ok_or_else(|| Rejection::new(RejectionCode::StaleBinding))?;
    node.acceptance_complete = true;
    node.evidence_current = true;
    Ok(Some(EventKind::NodeAdvanced {
        node_id: node_id.into(),
        stage: node.stage,
    }))
}

pub(super) fn main_observed(state: &mut CampaignState, node_id: &str, active: bool) -> Transition {
    let node = state
        .nodes
        .get_mut(node_id)
        .ok_or_else(|| Rejection::new(RejectionCode::StaleBinding))?;
    node.main_integrated = true;
    if active {
        advance(
            state,
            node_id,
            Stage::MainVerification,
            NodeDisposition::Verifying,
        )
    } else {
        Ok(Some(EventKind::NodeAdvanced {
            node_id: node_id.into(),
            stage: node.stage,
        }))
    }
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
pub(crate) mod tests;
