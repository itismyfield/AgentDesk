use super::*;
use crate::services::campaign_execution::{
    control::{decide, tests as domain},
    evidence_gate::tests::{REPO, a1_finish, claim, workspace},
    execution_pin::ExecutionPin,
    profile::TestOutcome,
    verification_run::{VerificationSpec, tests as a1},
};

const START: &str = "lib-tests";
const FINISH: &str = "slow-tests";

fn spec() -> VerificationSpec {
    let mut spec = a1::spec_with_audit();
    let mut slow = spec.checks[0].clone();
    slow.id = FINISH.into();
    spec.checks.push(slow);
    spec
}

// The running fixture without an attempt: node `a` is at Implementation and free for a Work.
fn base() -> CampaignState {
    let mut state = domain::running();
    state.run.as_mut().unwrap().attempts.clear();
    for id in ["a", "c"] {
        let node = state.nodes.get_mut(id).unwrap();
        node.stage = Stage::Implementation;
        node.current_attempt = None;
        node.disposition = NodeDisposition::Pending;
    }
    state
}

pub(crate) fn work(node: &str, attempt: &str) -> AttemptBinding {
    let state = domain::running();
    let mut binding = domain::binding(&state);
    binding.node_id = node.into();
    binding.attempt_id = attempt.into();
    binding.hashes = state.definition.hashes()[node].clone();
    binding.session.session = format!("worker-{node}");
    binding
}

pub(crate) fn review_of(work: &AttemptBinding) -> AttemptBinding {
    let mut review = work.clone();
    review.attempt_id = "review-r".into();
    review.role = Role::Review;
    review.stage = Stage::Review;
    review.session.session = "reviewer".into();
    review.session.incarnation = "process-9".into();
    review.candidate_sha = "h1".into();
    review
}

fn assignment(binding: &AttemptBinding) -> WorkAssignment {
    let spec = spec();
    WorkAssignment {
        pin: ExecutionPin {
            hashes: binding.hashes.clone(),
            requirements_snapshot_hash: "requirements".into(),
            resolved_profile_hash: "profile".into(),
            workflow_version: WORKFLOW_VERSION.into(),
            verification_spec_hash: spec.hash().unwrap(),
        },
        spec,
        acceptance: Vec::new(),
        review_start_checks: vec![START.into()],
        review_finish_checks: vec![FINISH.into()],
        gate: binding.session.gate.clone(),
    }
}

fn launch(binding: &AttemptBinding) -> Action {
    Action {
        id: format!("launch-{}", binding.attempt_id),
        binding: binding.clone(),
        kind: ActionKind::Launch,
        target: binding.session.gate.clone(),
        state: ActionState::Planned,
        initiation_receipt: None,
        reconciliation: None,
    }
}

fn assign(binding: &AttemptBinding, assignment: WorkAssignment) -> Command {
    Command::AssignWork {
        binding: Box::new(binding.clone()),
        assignment: Box::new(assignment),
        launch: Box::new(launch(binding)),
    }
}

fn epoch(state: &CampaignState, node: &str) -> u64 {
    state
        .run
        .as_ref()
        .and_then(|r| r.fences.get(node))
        .map_or(0, |f| f.epoch)
}

fn decided(state: &CampaignState, command: Command) -> Result<Decision, Rejection> {
    decide(state, &domain::envelope(command))
}

fn apply(state: &CampaignState, command: Command) -> CampaignState {
    decided(state, command).unwrap().state
}

fn refused(state: &CampaignState, command: Command) -> RejectionCode {
    decided(state, command).unwrap_err().code
}

fn unchanged(state: &CampaignState, command: Command) {
    let decision = decided(state, command).unwrap();
    assert!(decision.events.is_empty());
    assert_eq!(&decision.state, state);
}

pub(crate) fn execution<'a>(state: &'a CampaignState, attempt: &str) -> &'a AttemptExecution {
    state.run.as_ref().unwrap().attempts[attempt]
        .execution
        .as_deref()
        .unwrap()
}

fn action(state: &CampaignState, id: &str) -> ActionState {
    state.run.as_ref().unwrap().actions[id].state
}

fn phase_action(state: &CampaignState, v: &str, phase: &str) -> String {
    state.run.as_ref().unwrap().verifications[v]
        .phase(phase)
        .unwrap()
        .action_id
        .clone()
}

// Work A on node `a`, started by its gate, with both check slots registered.
pub(crate) fn assigned() -> (CampaignState, AttemptBinding) {
    let binding = work("a", "work-a");
    let mut state = apply(&base(), assign(&binding, assignment(&binding)));
    let run = state.run.as_mut().unwrap();
    run.attempts.get_mut("work-a").unwrap().started = true;
    (state, binding)
}

pub(crate) fn seal_of(state: &CampaignState, binding: &AttemptBinding) -> CandidateSeal {
    let pin = &execution(state, &binding.attempt_id).pin_hash;
    let claim = claim(binding, "h1");
    CandidateSeal::seal(binding, &claim, REPO, &workspace("h1"), pin, domain::now()).unwrap()
}

// Binds the sealed output and runs the V's fixed phases to collection, as the runner would.
pub(crate) fn run_v(state: &mut CampaignState, v: &str, seal: &CandidateSeal, test_exit: i32) {
    let outcome = if test_exit == 0 {
        TestOutcome::Passed
    } else {
        TestOutcome::Failed
    };
    let run = state.run.as_mut().unwrap();
    let v = run.verifications.get_mut(v).unwrap();
    v.subject.bind(seal.bound_subject()).unwrap();
    let exit = |phase: &str| match phase {
        "test" => test_exit,
        "audit" => i32::from(test_exit != 0),
        _ => 0,
    };
    a1_finish(v, &mut run.actions, &spec(), &[("a", outcome)], exit, "h1");
}

pub(crate) fn begin(
    state: &CampaignState,
    seal: &CandidateSeal,
    review: &AttemptBinding,
) -> Command {
    Command::BeginReview {
        node_id: "a".into(),
        expected_epoch: epoch(state, "a"),
        seal: Box::new(seal.clone()),
        review: Box::new(review.clone()),
        launch: Box::new(launch(review)),
    }
}

// A handed over to R after its start check passed; the finish V is still unstarted.
pub(crate) fn handed_over() -> (CampaignState, AttemptBinding, AttemptBinding, CandidateSeal) {
    let (mut state, work) = assigned();
    let seal = seal_of(&state, &work);
    let start = execution(&state, "work-a").start_executions[0].clone();
    run_v(&mut state, &start, &seal, 0);
    let review = review_of(&work);
    let state = apply(&state, begin(&state, &seal, &review));
    (state, work, review, seal)
}

fn finish_v(state: &CampaignState) -> String {
    execution(state, "work-a").finish_executions[0].clone()
}

fn advance(v: &str, phase: &str, consumer: &AttemptBinding, to: ActionState) -> Command {
    Command::AdvanceVerificationPhase {
        execution_id: v.into(),
        phase_id: phase.into(),
        consumer: Box::new(consumer.clone()),
        state: to,
    }
}

// The server materialized the finish V: its subject and first phase input are bound.
fn materialize(state: &mut CampaignState, v: &str, seal: &CandidateSeal) {
    let run = state.run.as_mut().unwrap();
    let v = run.verifications.get_mut(v).unwrap();
    v.subject.bind(seal.bound_subject()).unwrap();
    v.bind_phase_input("archive", a1::input(&[])).unwrap();
}

fn accept(state: &CampaignState, attempt: &str, review: Option<StageAssessment>) -> Command {
    Command::AcceptStageResult {
        node_id: "a".into(),
        expected_epoch: epoch(state, "a"),
        attempt_id: attempt.into(),
        review: review.map(Box::new),
    }
}

fn interrupt(state: &CampaignState, node: &str, session: &SessionIdentity) -> Command {
    Command::InterruptNode {
        expected_run: "run".into(),
        node_id: node.into(),
        expected_epoch: epoch(state, node),
        session: session.clone(),
        reason: "operator stop of this node".into(),
    }
}

#[test]
fn work_assignment_registers_every_owed_check_once_and_a_retry_adds_nothing() {
    let binding = work("a", "work-a");
    let state = apply(&base(), assign(&binding, assignment(&binding)));
    let run = state.run.as_ref().unwrap();
    let execution = execution(&state, "work-a");
    assert_eq!(
        (
            execution.start_executions.len(),
            execution.finish_executions.len()
        ),
        (1, 1)
    );
    assert_eq!(run.verifications.len(), 2);
    for v in run.verifications.values() {
        assert_eq!(v.origin.owner_attempt, "work-a");
        assert_eq!(v.origin.registered_node_epoch, 1);
        for phase in &v.phases {
            assert_eq!(run.actions[&phase.action_id].state, ActionState::Planned);
        }
    }
    assert_eq!(epoch(&state, "a"), 1);
    assert_eq!(state.nodes["a"].current_attempt.as_deref(), Some("work-a"));
    // A lost response is retried: the same V set comes back with no new execution ID or epoch.
    unchanged(&state, assign(&binding, assignment(&binding)));
    let other = work("a", "work-b");
    assert_eq!(
        refused(&state, assign(&other, assignment(&other))),
        RejectionCode::InvalidState
    );
    // An empty start gate or a required check owed by neither gate would read as a silent PASS.
    let mut empty = assignment(&binding);
    empty.review_start_checks.clear();
    let mut dropped = assignment(&binding);
    dropped.review_finish_checks.clear();
    let mut unknown = assignment(&binding);
    unknown.review_finish_checks.push("unregistered".into());
    for broken in [empty, dropped, unknown] {
        assert_eq!(
            refused(&base(), assign(&binding, broken)),
            RejectionCode::InvalidState
        );
    }
}

#[test]
fn begin_review_needs_a_passing_start_check_and_hands_over_once() {
    let (state, work) = assigned();
    let seal = seal_of(&state, &work);
    let review = review_of(&work);
    let start = execution(&state, "work-a").start_executions[0].clone();
    // Unrun and failed start checks are not a handover.
    let failed = {
        let mut failed = state.clone();
        run_v(&mut failed, &start, &seal, 100);
        failed
    };
    for (pending, verdict) in [(&state, "Wait"), (&failed, "QualityFail")] {
        let error = decided(pending, begin(pending, &seal, &review)).unwrap_err();
        assert_eq!(error.code, RejectionCode::GateNotPassed);
        assert_eq!(error.target.as_deref(), Some(verdict));
    }
    let (handed, _, _, _) = handed_over();
    let node = &handed.nodes["a"];
    assert_eq!((node.stage, epoch(&handed, "a")), (Stage::Review, 2));
    assert_eq!(node.current_attempt.as_deref(), Some("review-r"));
    let run = handed.run.as_ref().unwrap();
    assert_eq!(run.attempts["work-a"].outcome, Some(AttemptOutcome::Passed));
    let finish = finish_v(&handed);
    let prerequisites: Vec<_> = run.prerequisites.values().collect();
    assert_eq!(prerequisites.len(), 1);
    assert_eq!(prerequisites[0].target.execution_id, finish);
    assert_eq!(prerequisites[0].consumer_node_epoch, 2);
    // The unstarted finish V keeps A as owner and its phases stay Planned.
    let v = &run.verifications[&finish];
    assert_eq!(v.origin.owner_attempt, "work-a");
    assert!(
        v.phases
            .iter()
            .all(|p| run.actions[&p.action_id].state == ActionState::Planned)
    );
    // A retry adds no epoch; a stale epoch or another pin's seal is refused.
    let mut before = handed.clone();
    before
        .run
        .as_mut()
        .unwrap()
        .fences
        .get_mut("a")
        .unwrap()
        .epoch = 1;
    unchanged(&handed, begin(&before, &seal, &review));
    let (mut passed, work) = assigned();
    run_v(&mut passed, &start, &seal, 0);
    let mut stale = begin(&passed, &seal, &review);
    if let Command::BeginReview { expected_epoch, .. } = &mut stale {
        *expected_epoch = 0;
    }
    assert_eq!(refused(&passed, stale), RejectionCode::StaleFence);
    let mut foreign = seal.clone();
    foreign.pin_hash = "other-pin".into();
    assert_eq!(
        refused(&passed, begin(&passed, &foreign, &review)),
        RejectionCode::StaleBinding
    );
    assert_eq!(work.attempt_id, "work-a");
}

#[test]
fn verification_phases_start_only_through_the_current_authority() {
    let (state, work) = assigned();
    let start = execution(&state, "work-a").start_executions[0].clone();
    let archive = phase_action(&state, &start, "archive");
    // The general action path never claims a registered verification phase.
    assert_eq!(
        refused(
            &state,
            Command::AdvanceAction {
                action_id: archive.clone(),
                binding: Box::new(work.clone()),
                state: ActionState::Claimed,
            }
        ),
        RejectionCode::InvalidState
    );
    // Even the owning Work cannot claim before the subject and input are bound.
    assert_eq!(
        refused(
            &state,
            advance(&start, "archive", &work, ActionState::Claimed)
        ),
        RejectionCode::InvalidState
    );
    let (mut handed, work, review, seal) = handed_over();
    let finish = finish_v(&handed);
    materialize(&mut handed, &finish, &seal);
    let archive = phase_action(&handed, &finish, "archive");
    // A's old request after the handover is stale; it does not cancel the phase.
    assert_eq!(
        refused(
            &handed,
            advance(&finish, "archive", &work, ActionState::Claimed)
        ),
        RejectionCode::StaleFence
    );
    assert_eq!(action(&handed, &archive), ActionState::Planned);
    let claimed = apply(
        &handed,
        advance(&finish, "archive", &review, ActionState::Claimed),
    );
    assert_eq!(action(&claimed, &archive), ActionState::Claimed);
    let pending = apply(
        &claimed,
        advance(&finish, "archive", &review, ActionState::StartPending),
    );
    assert_eq!(action(&pending, &archive), ActionState::StartPending);
    // The same action ID continues: a repeated request changes nothing.
    unchanged(
        &pending,
        advance(&finish, "archive", &review, ActionState::StartPending),
    );
    let owner = &pending.run.as_ref().unwrap().actions[&archive].binding;
    assert_eq!(owner.attempt_id, "work-a");
}

#[test]
fn interrupt_node_revokes_this_node_once_and_leaves_other_nodes_alone() {
    let (mut handed, _, review, seal) = handed_over();
    let finish = finish_v(&handed);
    materialize(&mut handed, &finish, &seal);
    let other = work("c", "work-c");
    let both = apply(&handed, assign(&other, assignment(&other)));
    let c_epoch = epoch(&both, "c");
    let c_actions: Vec<_> = both
        .run
        .as_ref()
        .unwrap()
        .actions
        .values()
        .filter(|a| a.binding.node_id == "c")
        .cloned()
        .collect();
    assert_eq!(
        refused(&both, interrupt(&both, "a", &other.session)),
        RejectionCode::StaleBinding
    );
    let interrupted = apply(&both, interrupt(&both, "a", &review.session));
    let node = &interrupted.nodes["a"];
    assert_eq!(epoch(&interrupted, "a"), 3);
    let fence = &interrupted.run.as_ref().unwrap().fences["a"];
    assert_eq!(
        fence.interruption.as_ref().unwrap().attempt_id.as_deref(),
        Some("review-r")
    );
    // Commit records intent only: R stays current and unfinished until the stop is observed.
    assert_eq!(node.current_attempt.as_deref(), Some("review-r"));
    let archive = phase_action(&interrupted, &finish, "archive");
    assert_eq!(action(&interrupted, &archive), ActionState::Cancelled);
    assert_eq!(
        action(&interrupted, "launch-review-r"),
        ActionState::Cancelled
    );
    assert_eq!(epoch(&interrupted, "c"), c_epoch);
    for kept in &c_actions {
        assert_eq!(&interrupted.run.as_ref().unwrap().actions[&kept.id], kept);
    }
    // The same request again, with the epoch it was sent for, does not advance the epoch twice.
    unchanged(&interrupted, interrupt(&both, "a", &review.session));
    assert_eq!(
        refused(
            &interrupted,
            advance(&finish, "archive", &review, ActionState::Claimed)
        ),
        RejectionCode::StaleFence
    );
    let pass = StageAssessment::pass(vec!["review".into()]);
    assert_eq!(
        refused(&interrupted, accept(&interrupted, "review-r", Some(pass))),
        RejectionCode::StaleFence
    );
}

#[test]
fn review_result_waits_for_its_prerequisite_and_only_a_confirmed_failure_counts() {
    let (handed, _, _, seal) = handed_over();
    let finish = finish_v(&handed);
    let clean = || Some(StageAssessment::pass(vec!["review".into()]));
    // R's CLEAN before its prerequisite finished keeps R current and Verifying.
    let waiting = apply(&handed, accept(&handed, "review-r", clean()));
    let node = &waiting.nodes["a"];
    assert_eq!(node.disposition, NodeDisposition::Verifying);
    assert_eq!((epoch(&waiting, "a"), node.quality_failures), (2, 0));
    assert_eq!(node.current_attempt.as_deref(), Some("review-r"));
    let mut failed = handed.clone();
    run_v(&mut failed, &finish, &seal, 100);
    let repaired = apply(&failed, accept(&failed, "review-r", clean()));
    let node = &repaired.nodes["a"];
    assert_eq!((node.quality_failures, node.repair_cycles), (1, 1));
    assert_eq!(
        (node.stage, epoch(&repaired, "a")),
        (Stage::Implementation, 3)
    );
    assert_eq!(node.current_attempt, None);
    assert_eq!(
        repaired.run.as_ref().unwrap().attempts["review-r"].outcome,
        Some(AttemptOutcome::QualityFailed)
    );
    let mut passed = handed.clone();
    run_v(&mut passed, &finish, &seal, 0);
    let landed = apply(&passed, accept(&passed, "review-r", clean()));
    assert_eq!(landed.nodes["a"].stage, Stage::Landing);
    assert_eq!(landed.nodes["a"].quality_failures, 0);
    // A review with findings and a failed self-check in the same cycle is one failure, not two.
    let issues = Some(StageAssessment::of(
        Verdict::QualityFail,
        GateReason::OpenFinding("finding".into()),
    ));
    let both = apply(&failed, accept(&failed, "review-r", issues));
    assert_eq!(both.nodes["a"].quality_failures, 1);
}

#[test]
fn self_check_failure_and_environment_waits_are_counted_apart() {
    let (state, work) = assigned();
    let seal = seal_of(&state, &work);
    let start = execution(&state, "work-a").start_executions[0].clone();
    // A preparation failure without a confirmed cause is insufficient evidence, never a failure.
    let mut preparation = state.clone();
    {
        let run = preparation.run.as_mut().unwrap();
        let v = run.verifications.get_mut(&start).unwrap();
        v.subject.bind(seal.bound_subject()).unwrap();
        let exit = |phase: &str| i32::from(phase == "plan");
        a1_finish(
            v,
            &mut run.actions,
            &spec(),
            &[("a", TestOutcome::Passed)],
            exit,
            "h1",
        );
    }
    let waited = apply(&preparation, accept(&preparation, "work-a", None));
    assert_eq!(waited.nodes["a"].quality_failures, 0);
    assert_eq!(waited.nodes["a"].current_attempt.as_deref(), Some("work-a"));
    let mut failed = state.clone();
    run_v(&mut failed, &start, &seal, 100);
    let repaired = apply(&failed, accept(&failed, "work-a", None));
    let node = &repaired.nodes["a"];
    assert_eq!((node.quality_failures, node.repair_cycles), (1, 1));
    // The finish V of the failed attempt can no longer start.
    let finish = &execution(&failed, "work-a").finish_executions[0];
    let archive = phase_action(&repaired, finish, "archive");
    assert_eq!(action(&repaired, &archive), ActionState::Cancelled);
    // A passing Work stage moves on only through BeginReview.
    let mut passed = state.clone();
    run_v(&mut passed, &start, &seal, 0);
    assert_eq!(
        refused(&passed, accept(&passed, "work-a", None)),
        RejectionCode::InvalidState
    );
}

#[test]
fn finish_stage_freezes_the_current_review_v_and_immediate_revokes_it() {
    let (mut handed, work, review, seal) = handed_over();
    let finish = finish_v(&handed);
    materialize(&mut handed, &finish, &seal);
    handed
        .run
        .as_mut()
        .unwrap()
        .attempts
        .get_mut("review-r")
        .unwrap()
        .started = true;
    let finishing = domain::stop(&handed, StopMode::FinishStage);
    let stop = finishing.run.as_ref().unwrap().stop.as_ref().unwrap();
    assert_eq!(stop.finish_grants.len(), 1);
    assert_eq!(stop.finish_grants[0].consumer_attempt, "review-r");
    let archive = phase_action(&finishing, &finish, "archive");
    assert_eq!(action(&finishing, &archive), ActionState::Planned);
    assert_eq!(
        action(&finishing, "launch-review-r"),
        ActionState::Cancelled
    );
    let continued = apply(
        &finishing,
        advance(&finish, "archive", &review, ActionState::Claimed),
    );
    assert_eq!(action(&continued, &archive), ActionState::Claimed);
    assert_eq!(
        refused(
            &finishing,
            advance(&finish, "archive", &work, ActionState::Claimed)
        ),
        RejectionCode::StaleFence
    );
    let immediate = domain::stop(&continued, StopMode::Immediate);
    let stop = immediate.run.as_ref().unwrap().stop.as_ref().unwrap();
    assert!(stop.finish_grants.is_empty());
    assert_eq!(action(&immediate, &archive), ActionState::Cancelled);
    assert_eq!(
        refused(
            &immediate,
            advance(&finish, "list", &review, ActionState::Claimed)
        ),
        RejectionCode::StaleFence
    );
}
