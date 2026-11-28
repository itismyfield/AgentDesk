use super::super::{
    control::{decide, test_support as domain},
    evidence_gate::test_support::{REPO, a1_finish, claim, workspace},
    verification_run::test_support as a1,
};
use crate::domain::campaign::{
    candidate_seal::CandidateSeal,
    definition::WORKFLOW_VERSION,
    evidence_gate::StageAssessment,
    execution_pin::ExecutionPin,
    model::{
        Action, ActionKind, ActionState, AttemptBinding, AttemptExecution, CampaignState, Command,
        Decision, NodeDisposition, Rejection, Role, SessionIdentity, Stage, WorkAssignment,
    },
    profile::TestOutcome,
    verification_run::VerificationSpec,
};

const START: &str = "lib-tests";
const FINISH: &str = "slow-tests";

pub fn spec() -> VerificationSpec {
    let mut spec = a1::spec_with_audit();
    let mut slow = spec.checks[0].clone();
    slow.id = FINISH.into();
    spec.checks.push(slow);
    spec
}

// The running fixture without an attempt: node `a` is at Implementation and free for a Work.
pub fn base() -> CampaignState {
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

pub fn work(node: &str, attempt: &str) -> AttemptBinding {
    let state = domain::running();
    let mut binding = domain::binding(&state);
    binding.node_id = node.into();
    binding.attempt_id = attempt.into();
    binding.hashes = state.definition.hashes()[node].clone();
    binding.session.session = format!("worker-{node}");
    binding
}

pub fn review_of(work: &AttemptBinding) -> AttemptBinding {
    let mut review = work.clone();
    review.attempt_id = "review-r".into();
    review.role = Role::Review;
    review.stage = Stage::Review;
    review.session.session = "reviewer".into();
    review.session.incarnation = "process-9".into();
    review.candidate_sha = "h1".into();
    review
}

pub fn assignment(binding: &AttemptBinding) -> WorkAssignment {
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

pub(super) fn launch(binding: &AttemptBinding) -> Action {
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

pub fn assign(binding: &AttemptBinding, assignment: WorkAssignment) -> Command {
    Command::AssignWork {
        binding: Box::new(binding.clone()),
        assignment: Box::new(assignment),
        launch: Box::new(launch(binding)),
    }
}

pub(super) fn epoch(state: &CampaignState, node: &str) -> u64 {
    state
        .run
        .as_ref()
        .and_then(|r| r.fences.get(node))
        .map_or(0, |f| f.epoch)
}

pub(super) fn decided(state: &CampaignState, command: Command) -> Result<Decision, Rejection> {
    decide(state, &domain::envelope(command))
}

pub(super) fn apply(state: &CampaignState, command: Command) -> CampaignState {
    decided(state, command).unwrap().state
}

pub fn execution<'a>(state: &'a CampaignState, attempt: &str) -> &'a AttemptExecution {
    state.run.as_ref().unwrap().attempts[attempt]
        .execution
        .as_deref()
        .unwrap()
}

// Work A on node `a`, started by its gate, with both check slots registered.
pub fn assigned() -> (CampaignState, AttemptBinding) {
    let binding = work("a", "work-a");
    let mut state = apply(&base(), assign(&binding, assignment(&binding)));
    let run = state.run.as_mut().unwrap();
    run.attempts.get_mut("work-a").unwrap().started = true;
    (state, binding)
}

pub fn seal_of(state: &CampaignState, binding: &AttemptBinding) -> CandidateSeal {
    let pin = &execution(state, &binding.attempt_id).pin_hash;
    let claim = claim(binding, "h1");
    CandidateSeal::seal(binding, &claim, REPO, &workspace("h1"), pin, domain::now()).unwrap()
}

// Binds the sealed output and runs the V's fixed phases to collection, as the runner would.
pub fn run_v(state: &mut CampaignState, v: &str, seal: &CandidateSeal, test_exit: i32) {
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

pub fn begin(state: &CampaignState, seal: &CandidateSeal, review: &AttemptBinding) -> Command {
    Command::BeginReview {
        node_id: "a".into(),
        expected_epoch: epoch(state, "a"),
        seal: Box::new(seal.clone()),
        review: Box::new(review.clone()),
        launch: Box::new(launch(review)),
    }
}

// A handed over to R after its start check passed; the finish V is still unstarted.
pub fn handed_over() -> (CampaignState, AttemptBinding, AttemptBinding, CandidateSeal) {
    let (mut state, work) = assigned();
    let seal = seal_of(&state, &work);
    let start = execution(&state, "work-a").start_executions[0].clone();
    run_v(&mut state, &start, &seal, 0);
    let review = review_of(&work);
    let state = apply(&state, begin(&state, &seal, &review));
    (state, work, review, seal)
}

pub fn advance(v: &str, phase: &str, consumer: &AttemptBinding, to: ActionState) -> Command {
    Command::AdvanceVerificationPhase {
        execution_id: v.into(),
        phase_id: phase.into(),
        consumer: Box::new(consumer.clone()),
        state: to,
    }
}

pub fn accept(state: &CampaignState, attempt: &str, review: Option<StageAssessment>) -> Command {
    Command::AcceptStageResult {
        node_id: "a".into(),
        expected_epoch: epoch(state, "a"),
        attempt_id: attempt.into(),
        review: review.map(Box::new),
    }
}

pub fn interrupt(state: &CampaignState, node: &str, session: &SessionIdentity) -> Command {
    Command::InterruptNode {
        expected_run: "run".into(),
        node_id: node.into(),
        expected_epoch: epoch(state, node),
        session: session.clone(),
        reason: "operator stop of this node".into(),
    }
}
