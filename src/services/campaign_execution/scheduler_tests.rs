use super::*;
use crate::services::campaign_execution::{
    control::{decide, test_support as domain},
    lifecycle::tests as life,
    stage_fsm::tests as fsm,
};

fn key(action: &NextAction) -> StepKey {
    match action {
        NextAction::AssignWork { key }
        | NextAction::BeginReview { key, .. }
        | NextAction::BeginLanding { key }
        | NextAction::CompleteNode { key } => key.clone(),
        other => panic!("no step is proposed: {other:?}"),
    }
}

// The Work the driver builds for a proposed step: IDs from the key, fence from the saved run.
fn work_for(state: &CampaignState, key: &StepKey) -> (AttemptBinding, Command) {
    let mut work = fsm::work(&key.node_id, &key.attempt_id(Stage::Implementation));
    work.control_version = state.run.as_ref().unwrap().control_version;
    let Command::AssignWork {
        binding,
        assignment,
        mut launch,
    } = fsm::assign(&work, fsm::assignment(&work))
    else {
        unreachable!()
    };
    launch.id = key.launch_id(Stage::Implementation);
    let command = Command::AssignWork {
        binding,
        assignment,
        launch,
    };
    (work, command)
}

fn resumed_with_new_predecessor() -> CampaignState {
    let running = life::started();
    let work = fsm::work("a", "original-work");
    let assigned = life::step(&running, fsm::assign(&work, fsm::assignment(&work)));
    let stopped = life::stopped(&assigned, &[]);
    let mut definition = stopped.definition.input().clone();
    definition.nodes[0].dependencies.push("c".into());
    let edited = life::step(
        &stopped,
        Command::EditDefinition {
            expected_revision: stopped.definition_revision,
            definition,
            change: None,
        },
    );
    let resumed = life::step(&edited, life::resume(&edited));
    assert_eq!(resumed.nodes["a"].stage, Stage::Implementation);
    resumed
}

#[test]
fn resumed_work_admission_waits_for_a_new_predecessor() {
    let mut resumed = resumed_with_new_predecessor();
    let mut work = fsm::work("a", "resumed-work");
    work.control_version = resumed.run.as_ref().unwrap().control_version;
    let command = fsm::assign(&work, fsm::assignment(&work));
    let Err(rejection) = decide(&resumed, &domain::envelope(command.clone())) else {
        panic!("Work was admitted before its new predecessor reached main");
    };
    assert_eq!(rejection.code, RejectionCode::GateNotPassed);
    assert_eq!(rejection.target.as_deref(), Some("predecessor:c"));

    resumed.nodes.get_mut("c").unwrap().main_integrated = true;
    let assigned = life::step(&resumed, command);
    assert_eq!(assigned.nodes["a"].current_attempt, Some(work.attempt_id));
}

#[test]
fn resumed_work_scheduler_waits_for_a_new_predecessor() {
    let mut resumed = resumed_with_new_predecessor();
    assert_eq!(
        next_action(&resumed, "a"),
        hold(HoldReason::PredecessorMain)
    );
    resumed.nodes.get_mut("c").unwrap().main_integrated = true;
    assert!(matches!(
        next_action(&resumed, "a"),
        NextAction::AssignWork { .. }
    ));
}

#[test]
fn the_next_action_follows_the_saved_state_and_is_never_proposed_after_it_took_effect() {
    let hold = |reason| NextAction::Hold { reason };
    assert_eq!(
        next_action(&domain::prepared(), "a"),
        hold(HoldReason::NotRunning)
    );
    let started = life::started();
    assert_eq!(
        next_action(&started, "b"),
        hold(HoldReason::PredecessorMain)
    );
    let first = key(&next_action(&started, "a"));
    let (work, assign) = work_for(&started, &first);
    let assigned = life::step(&started, assign.clone());
    let attempt_id = work.attempt_id.clone();
    let waiting = NextAction::AwaitAttempt { attempt_id };
    assert_eq!(next_action(&assigned, "a"), waiting);
    // The same step sent again after a lost reply adds no second attempt, action or event.
    let replay = decide(&assigned, &domain::envelope(assign)).unwrap();
    assert!(replay.events.is_empty());
    assert_eq!(replay.state, assigned);

    // An interrupted Work is followed by a new step, not by the spent one.
    let launch_id = first.launch_id(Stage::Implementation);
    let mut cancelled = domain::stop(&assigned, StopMode::Immediate);
    let run = cancelled.run.as_ref().unwrap();
    assert_eq!(run.actions[&launch_id].state, ActionState::Cancelled);
    assert_eq!(next_action(&cancelled, "a"), hold(HoldReason::NotRunning));
    cancelled = life::stopped(&assigned, &[]);
    let resumed = life::step(&cancelled, life::resume(&cancelled));
    let second = key(&next_action(&resumed, "a"));
    assert!(second.epoch > first.epoch);
    let (again, assign) = work_for(&resumed, &second);
    assert_ne!(again.attempt_id, work.attempt_id);
    let reassigned = life::step(&resumed, assign);
    assert_eq!(
        reassigned.nodes["a"].current_attempt,
        Some(again.attempt_id)
    );

    // The later positions of the fixed Work → Review → Landing path.
    let work = fsm::work("a", "work-a");
    let running = life::assigned(&started, &work);
    let attempt_id = work.attempt_id.clone();
    assert_eq!(
        next_action(&running, "a"),
        NextAction::AwaitAttempt { attempt_id }
    );
    let (sealed, seal) = life::sealed(&running, &work);
    let handover = next_action(&sealed, "a");
    let work_attempt = work.attempt_id.clone();
    let key = key(&handover);
    assert_eq!(handover, NextAction::BeginReview { key, work_attempt });
    let (reviewing, review) = life::in_review(&sealed, &work, &seal);
    let attempt_id = review.attempt_id.clone();
    assert_eq!(
        next_action(&reviewing, "a"),
        NextAction::AwaitAttempt { attempt_id }
    );
    let door = life::reviewed(&reviewing, &work, &review, &seal);
    let admit = next_action(&door, "a");
    assert!(
        matches!(admit, NextAction::BeginLanding { .. }),
        "{admit:?}"
    );
    let mut admitted = life::step(&door, life::landing(&door, life::begin_landing));
    let stage = Stage::Landing;
    assert_eq!(
        next_action(&admitted, "a"),
        NextAction::AwaitEvidence { stage }
    );
    life::confirm(&mut admitted, "a");
    let finish = next_action(&admitted, "a");
    assert!(
        matches!(finish, NextAction::CompleteNode { .. }),
        "{finish:?}"
    );
    let done = life::step(&admitted, life::landing(&admitted, life::complete));
    assert_eq!(next_action(&done, "a"), hold(HoldReason::Complete));
}

#[test]
fn a_gate_an_unknown_observation_or_a_spent_budget_holds_the_next_start() {
    let hold = |reason| NextAction::Hold { reason };
    let gated = life::gated(life::started(), &["start", "implementation", "review"]);
    let gate = ApprovalGate::Start;
    assert_eq!(next_action(&gated, "a"), NextAction::OpenGate { gate });
    let open = life::opened(&gated, "start", "candidate");
    assert_eq!(next_action(&open, "a"), NextAction::AwaitApproval { gate });
    let approved = life::approved(&open, "start", "candidate");
    let gate = ApprovalGate::Implementation;
    assert_eq!(next_action(&approved, "a"), NextAction::OpenGate { gate });
    let approved = life::approved(&approved, "implementation", "candidate");
    let assign = next_action(&approved, "a");
    assert!(
        matches!(assign, NextAction::AssignWork { .. }),
        "{assign:?}"
    );

    // A sealed Work waits for the approval of its own output.
    let work = fsm::work("a", "work-a");
    let (sealed, _) = life::sealed(&life::assigned(&approved, &work), &work);
    let gate = ApprovalGate::Review;
    assert_eq!(next_action(&sealed, "a"), NextAction::OpenGate { gate });
    let stale = life::approved(&sealed, "review", "candidate");
    assert_eq!(next_action(&stale, "a"), NextAction::OpenGate { gate });
    let current = life::approved(&sealed, "review", "h1");
    let handover = next_action(&current, "a");
    assert!(
        matches!(handover, NextAction::BeginReview { .. }),
        "{handover:?}"
    );

    // No new start is proposed while a session of the run is unobserved.
    let mut unknown = current.clone();
    let sessions = &mut unknown.run.as_mut().unwrap().sessions;
    sessions[0].observation = SessionObservation::Unknown {
        reason: "host unreachable".into(),
        last_observed_at: None,
    };
    assert_eq!(
        next_action(&unknown, "a"),
        hold(HoldReason::ObservationUnknown)
    );
    // A node that spent its failure budget is never offered another Work.
    let mut failed = life::started();
    failed.nodes.get_mut("a").unwrap().disposition = NodeDisposition::Failed;
    assert_eq!(next_action(&failed, "a"), hold(HoldReason::BudgetExhausted));
}
