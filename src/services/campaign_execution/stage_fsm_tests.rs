use super::test_support::{
    accept, advance, apply, assign, assigned, assignment, base, begin, decided, epoch, execution,
    handed_over, interrupt, launch, review_of, run_v, seal_of, spec, work,
};
use super::*;
use crate::services::campaign_execution::{
    control::test_support as domain,
    evidence_gate::test_support::a1_finish,
    profile::TestOutcome,
    verification_run::{ProcessIdentity, test_support as a1},
};

fn refused(state: &CampaignState, command: Command) -> RejectionCode {
    decided(state, command).unwrap_err().code
}

fn unchanged(state: &CampaignState, command: Command) {
    let decision = decided(state, command).unwrap();
    assert!(decision.events.is_empty());
    assert_eq!(&decision.state, state);
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

fn finish_v(state: &CampaignState) -> String {
    execution(state, "work-a").finish_executions[0].clone()
}

// The server materialized the finish V: its subject and first phase input are bound.
fn materialize(state: &mut CampaignState, v: &str, seal: &CandidateSeal) {
    let run = state.run.as_mut().unwrap();
    let v = run.verifications.get_mut(v).unwrap();
    v.subject.bind(seal.bound_subject()).unwrap();
    v.bind_phase_input("archive", a1::input(&[])).unwrap();
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

#[test]
fn finish_stage_keeps_the_unsealed_work_v_slots_for_the_same_work() {
    let (state, work) = assigned();
    let finishing = domain::stop(&state, StopMode::FinishStage);
    let stop = finishing.run.as_ref().unwrap().stop.as_ref().unwrap();
    assert_eq!(stop.finish_grants.len(), 2);
    assert!(
        stop.finish_grants
            .iter()
            .all(|g| g.consumer_attempt == "work-a")
    );
    let start = execution(&state, "work-a").start_executions[0].clone();
    let archive = phase_action(&finishing, &start, "archive");
    assert_eq!(action(&finishing, &archive), ActionState::Planned);
    assert_eq!(action(&finishing, "launch-work-a"), ActionState::Cancelled);
    // The output sealed after the stop fills the slot registered before it; no new V appears.
    let mut materialized = finishing.clone();
    materialize(&mut materialized, &start, &seal_of(&state, &work));
    let claimed = apply(
        &materialized,
        advance(&start, "archive", &work, ActionState::Claimed),
    );
    assert_eq!(action(&claimed, &archive), ActionState::Claimed);
    assert_eq!(claimed.run.as_ref().unwrap().verifications.len(), 2);
    // A passing self-check still settles the finishing Work; the Review stays only the next stage.
    let mut passed = finishing.clone();
    let seal = seal_of(&state, &work);
    run_v(&mut passed, &start, &seal, 0);
    let before = passed.run.as_ref().unwrap();
    let (attempts, actions) = (before.attempts.len(), before.actions.len());
    let settled = apply(&passed, accept(&passed, "work-a", None));
    let run = settled.run.as_ref().unwrap();
    assert_eq!(run.attempts["work-a"].outcome, Some(AttemptOutcome::Passed));
    assert_eq!((run.attempts.len(), run.actions.len()), (attempts, actions));
    let node = &settled.nodes["a"];
    assert_eq!(
        (node.stage, node.current_attempt.as_deref()),
        (Stage::Review, None)
    );
    assert_eq!(epoch(&settled, "a"), epoch(&passed, "a") + 1);
    let finish = execution(&state, "work-a").finish_executions[0].clone();
    let finish_archive = phase_action(&settled, &finish, "archive");
    assert_eq!(action(&settled, &finish_archive), ActionState::Cancelled);
    assert_eq!(
        refused(
            &settled,
            advance(&finish, "archive", &work, ActionState::Claimed)
        ),
        RejectionCode::StaleFence
    );
}

#[test]
fn interrupted_node_gets_no_new_general_action_but_late_facts_still_land() {
    let (mut state, work_a) = assigned();
    let other = work("c", "work-c");
    state = apply(&state, assign(&other, assignment(&other)));
    let run = state.run.as_mut().unwrap();
    run.attempts.get_mut("work-c").unwrap().started = true;
    // The launch was already in flight when the node was interrupted.
    run.actions.get_mut("launch-work-a").unwrap().state = ActionState::StartPending;
    let interrupted = apply(&state, interrupt(&state, "a", &work_a.session));
    let general = |binding: &AttemptBinding, id: &str, kind: ActionKind| {
        let mut action = launch(binding);
        action.id = id.into();
        action.kind = kind;
        Command::PlanAction {
            action: Box::new(action),
        }
    };
    for kind in [ActionKind::Launch, ActionKind::DeliverStage] {
        assert_eq!(
            refused(&interrupted, general(&work_a, "again-a", kind)),
            RejectionCode::StaleFence
        );
    }
    let mut pending = interrupted.clone();
    let mut planned = launch(&work_a);
    planned.id = "planned-a".into();
    planned.kind = ActionKind::DeliverStage;
    let run = pending.run.as_mut().unwrap();
    run.actions.insert(planned.id.clone(), planned);
    assert_eq!(
        refused(
            &pending,
            Command::AdvanceAction {
                action_id: "planned-a".into(),
                binding: Box::new(work_a.clone()),
                state: ActionState::Claimed,
            }
        ),
        RejectionCode::StaleFence
    );
    let deliver = general(&other, "deliver-c", ActionKind::DeliverStage);
    let planned_c = apply(&interrupted, deliver);
    assert_eq!(action(&planned_c, "deliver-c"), ActionState::Planned);
    let observed = apply(
        &interrupted,
        Command::ObserveAction {
            action_id: "launch-work-a".into(),
            binding: Box::new(work_a),
            observation: ActionObservation::Started {
                receipt: "launch-receipt".into(),
            },
        },
    );
    assert_eq!(action(&observed, "launch-work-a"), ActionState::Started);
}

#[test]
fn finish_plan_phase_may_start_after_its_gate_applied_the_stop_fence() {
    let (mut handed, work, review, seal) = handed_over();
    let finish = finish_v(&handed);
    materialize(&mut handed, &finish, &seal);
    let run = handed.run.as_mut().unwrap();
    run.attempts.get_mut("review-r").unwrap().started = true;
    let finishing = domain::stop(&handed, StopMode::FinishStage);
    let archive = phase_action(&finishing, &finish, "archive");
    let run = finishing.run.as_ref().unwrap();
    let fenced = apply(
        &finishing,
        Command::AcknowledgeFence {
            expected_run: run.id.clone(),
            target: run.actions[&archive].target.clone(),
            control_version: run.control_version,
        },
    );
    let claimed = apply(
        &fenced,
        advance(&finish, "archive", &review, ActionState::Claimed),
    );
    let pending = apply(
        &claimed,
        advance(&finish, "archive", &review, ActionState::StartPending),
    );
    let mut environment = a1::environment();
    environment.checkout.host = work.session.gate.host.clone();
    let fact = PhaseFact::Started {
        receipt: "archive-receipt".into(),
        process: ProcessIdentity {
            host: environment.checkout.host.clone(),
            pid: 7,
            process_group: Some(7),
            started_at: domain::now(),
            nonce: "nonce".into(),
        },
        environment: Box::new(environment),
    };
    let started = apply(
        &pending,
        Command::RecordVerificationPhase {
            execution_id: finish.clone(),
            phase_id: "archive".into(),
            fact: Box::new(fact),
        },
    );
    assert_eq!(action(&started, &archive), ActionState::Started);
}
