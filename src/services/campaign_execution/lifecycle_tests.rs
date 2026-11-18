use serde_json::json;

use super::*;
use crate::services::campaign_execution::{
    control::{decide, tests as domain},
    definition,
    evidence_gate::StageAssessment,
    stage_fsm::tests as fsm,
};

pub(crate) fn step(state: &CampaignState, command: Command) -> CampaignState {
    domain::step(state, command)
}

fn refused(state: &CampaignState, command: Command) -> Rejection {
    decide(state, &domain::envelope(command)).unwrap_err()
}

fn unchanged(state: &CampaignState, command: Command) {
    let decision = decide(state, &domain::envelope(command)).unwrap();
    assert!(decision.events.is_empty());
    assert_eq!(&decision.state, state);
}

pub(crate) fn epoch(state: &CampaignState, node: &str) -> u64 {
    let run = state.run.as_ref().unwrap();
    run.fences.get(node).map_or(0, |f| f.epoch)
}

pub(crate) fn started() -> CampaignState {
    let run = Command::BeginRun {
        expected_revision: 1,
        run_id: "run".into(),
    };
    step(&domain::prepared(), run)
}

pub(crate) fn gated(mut state: CampaignState, gates: &[&str]) -> CampaignState {
    let mut input = serde_json::to_value(state.definition.input()).unwrap();
    for gate in gates {
        input["workflow_options"][*gate] = json!(true);
    }
    let input = serde_json::from_value(input).unwrap();
    state.definition = definition::validate_definition(input).unwrap();
    state
}

fn gate_command(command: &str, state: &CampaignState, gate: &str, head: &str) -> Command {
    let policy = fsm::assignment(&fsm::work("a", "work-a"))
        .pin
        .hash()
        .unwrap();
    let mut body = json!({
        "command": command,
        "binding": {
            "run_id": "run", "node_id": "a", "gate": gate,
            "definition_revision": state.definition_revision,
            "target": {"kind": "candidate", "base_sha": "base", "head_sha": head},
            "policy_hash": policy
        },
        "evidence": {
            "decision_ref": "i7438-m2-approval",
            "response_ref": "https://github.com/itismyfield/AgentDesk/discussions/7438#discussioncomment-2",
            "response_at": domain::now()
        }
    });
    if command == "open_approval_gate" {
        body.as_object_mut().unwrap().remove("evidence");
    }
    serde_json::from_value(body).unwrap()
}

pub(crate) fn opened(state: &CampaignState, gate: &str, head: &str) -> CampaignState {
    step(state, gate_command("open_approval_gate", state, gate, head))
}

// The server opens the gate for one candidate and the coordinator records its approval.
pub(crate) fn approved(state: &CampaignState, gate: &str, head: &str) -> CampaignState {
    let state = opened(state, gate, head);
    step(&state, gate_command("approve_gate", &state, gate, head))
}

fn launch_id(binding: &AttemptBinding) -> String {
    format!("launch-{}", binding.attempt_id)
}

// The attempt's launch goes through its gate and its session is observed alive.
pub(crate) fn launched(state: &CampaignState, binding: &AttemptBinding) -> CampaignState {
    let mut state = state.clone();
    for phase in [ActionState::Claimed, ActionState::StartPending] {
        let advance = Command::AdvanceAction {
            action_id: launch_id(binding),
            binding: Box::new(binding.clone()),
            state: phase,
        };
        state = step(&state, advance);
    }
    let receipt = "effect-1".into();
    let state = observed(&state, binding, ActionObservation::Started { receipt });
    let observation = SessionObservation::Alive {
        observed_at: domain::now(),
    };
    session(&state, binding, observation)
}

fn observed(
    state: &CampaignState,
    binding: &AttemptBinding,
    observation: ActionObservation,
) -> CampaignState {
    let observe = Command::ObserveAction {
        action_id: launch_id(binding),
        binding: Box::new(binding.clone()),
        observation,
    };
    step(state, observe)
}

fn session(
    state: &CampaignState,
    binding: &AttemptBinding,
    observation: SessionObservation,
) -> CampaignState {
    let identity = binding.session.clone();
    let observe = Command::ObserveSession {
        identity,
        observation,
    };
    step(state, observe)
}

// The launch effect is reconciled and the exact session is observed gone.
pub(crate) fn closed(state: &CampaignState, binding: &AttemptBinding) -> CampaignState {
    let evidence = "session ended".into();
    let state = observed(state, binding, ActionObservation::Reconciled { evidence });
    let observation = SessionObservation::Gone {
        observed_at: domain::now(),
        evidence: "reachable host: exact process absent".into(),
    };
    session(&state, binding, observation)
}

pub(crate) fn assigned(state: &CampaignState, work: &AttemptBinding) -> CampaignState {
    let state = step(state, fsm::assign(work, fsm::assignment(work)));
    launched(&state, work)
}

// The Work's output is sealed and its start check passed, as the seal command and V runner record.
pub(crate) fn sealed(
    state: &CampaignState,
    work: &AttemptBinding,
) -> (CampaignState, CandidateSeal) {
    let mut state = state.clone();
    let seal = fsm::seal_of(&state, work);
    let seals = &mut state.run.as_mut().unwrap().seals;
    seals.insert(work.attempt_id.clone(), seal.clone());
    let start = fsm::execution(&state, &work.attempt_id).start_executions[0].clone();
    fsm::run_v(&mut state, &start, &seal, 0);
    (state, seal)
}

pub(crate) fn in_review(
    state: &CampaignState,
    work: &AttemptBinding,
    seal: &CandidateSeal,
) -> (CampaignState, AttemptBinding) {
    let review = fsm::review_of(work);
    let state = step(state, fsm::begin(state, seal, &review));
    (launched(&state, &review), review)
}

// The finish check and the review pass, and both sessions end.
pub(crate) fn reviewed(
    state: &CampaignState,
    work: &AttemptBinding,
    review: &AttemptBinding,
    seal: &CandidateSeal,
) -> CampaignState {
    let mut state = state.clone();
    let finish = fsm::execution(&state, &work.attempt_id).finish_executions[0].clone();
    fsm::run_v(&mut state, &finish, seal, 0);
    let clean = Some(StageAssessment::pass(vec!["review".into()]));
    let state = step(&state, fsm::accept(&state, &review.attempt_id, clean));
    closed(&closed(&state, work), review)
}

// Node `a` reviewed and waiting at the Landing door, from a fresh run.
pub(crate) fn at_landing(state: &CampaignState) -> CampaignState {
    let work = fsm::work("a", "work-a");
    let (state, seal) = sealed(&assigned(state, &work), &work);
    let (state, review) = in_review(&state, &work, &seal);
    reviewed(&state, &work, &review, &seal)
}

pub(crate) fn landing(
    state: &CampaignState,
    command: fn(String, String, u64) -> Command,
) -> Command {
    command("run".into(), "a".into(), epoch(state, "a"))
}

pub(crate) fn begin_landing(expected_run: String, node_id: String, expected_epoch: u64) -> Command {
    Command::BeginLanding {
        expected_run,
        node_id,
        expected_epoch,
    }
}

pub(crate) fn complete(expected_run: String, node_id: String, expected_epoch: u64) -> Command {
    Command::CompleteNode {
        expected_run,
        node_id,
        expected_epoch,
    }
}

// The main, acceptance and current-evidence facts the later landing stages record.
pub(crate) fn confirm(state: &mut CampaignState, node: &str) {
    let node = state.nodes.get_mut(node).unwrap();
    node.main_integrated = true;
    node.acceptance_complete = true;
    node.evidence_current = true;
}

// The run stopped immediately and came to rest: effects reconciled, fences confirmed.
pub(crate) fn stopped(state: &CampaignState, live: &[&AttemptBinding]) -> CampaignState {
    let mut state = domain::stop(state, StopMode::Immediate);
    for binding in live {
        state = closed(&state, binding);
    }
    let run = state.run.as_ref().unwrap();
    let (run_id, version) = (run.id.clone(), run.control_version);
    for target in run.stop.as_ref().unwrap().targets.clone() {
        let acknowledge = Command::AcknowledgeFence {
            expected_run: run_id.clone(),
            target,
            control_version: version,
        };
        state = step(&state, acknowledge);
    }
    step(
        &step(&state, Command::ConfirmStopFence),
        Command::ConfirmStopped,
    )
}

pub(crate) fn resume(state: &CampaignState) -> Command {
    Command::ResumeRun {
        expected_revision: state.definition_revision,
        expected_run: "run".into(),
    }
}

#[test]
fn a_node_runs_from_start_to_success_and_each_step_takes_effect_once() {
    let started = started();
    let run = started.run.as_ref().unwrap();
    assert_eq!(
        (started.control, run.control_version),
        (CampaignControl::Running, 1)
    );
    assert_eq!(started.nodes["a"].stage, Stage::Requirements);
    let again = |run_id: &str| Command::BeginRun {
        expected_revision: 1,
        run_id: run_id.into(),
    };
    unchanged(&started, again("run"));
    assert_eq!(
        refused(&started, again("other")).code,
        RejectionCode::AlreadyExists
    );
    // A dependent node cannot take its first Work before its predecessor is on main.
    let dependent = fsm::work("b", "work-b");
    let early = refused(
        &started,
        fsm::assign(&dependent, fsm::assignment(&dependent)),
    );
    assert_eq!(early.target.as_deref(), Some("predecessor:a"));

    let waiting = at_landing(&started);
    let node = &waiting.nodes["a"];
    assert_eq!(
        (node.stage, node.disposition),
        (Stage::Landing, NodeDisposition::Pending)
    );
    let early = refused(&waiting, landing(&waiting, complete));
    assert_eq!(early.code, RejectionCode::InvalidState);
    // A candidate reviewed under requirements that were edited since is not admitted.
    let mut input = serde_json::to_value(waiting.definition.input()).unwrap();
    input["nodes"][0]["acceptance"][0]["description"] = json!("Prove A again");
    let mut edited = waiting.clone();
    edited.definition =
        definition::validate_definition(serde_json::from_value(input).unwrap()).unwrap();
    let stale = refused(&edited, landing(&edited, begin_landing));
    assert_eq!(stale.code, RejectionCode::StaleBinding);
    let admitted = step(&waiting, landing(&waiting, begin_landing));
    assert_eq!(admitted.nodes["a"].disposition, NodeDisposition::Assigned);
    assert_eq!(epoch(&admitted, "a"), epoch(&waiting, "a") + 1);
    unchanged(&admitted, landing(&waiting, begin_landing));

    // Completion needs every recorded fact and nothing of the node left open.
    for missing in 0..3 {
        let mut partial = admitted.clone();
        confirm(&mut partial, "a");
        let node = partial.nodes.get_mut("a").unwrap();
        match missing {
            0 => node.main_integrated = false,
            1 => node.acceptance_complete = false,
            _ => node.evidence_current = false,
        }
        let rejection = refused(&partial, landing(&partial, complete));
        assert_eq!(rejection.code, RejectionCode::GateNotPassed, "{missing}");
    }
    let mut confirmed = admitted.clone();
    confirm(&mut confirmed, "a");
    let mut live = confirmed.clone();
    let sessions = &mut live.run.as_mut().unwrap().sessions;
    sessions[0].observation = SessionObservation::Alive {
        observed_at: domain::now(),
    };
    let rejection = refused(&live, landing(&live, complete));
    assert_eq!(rejection.code, RejectionCode::UnreconciledActions);

    let done = step(&confirmed, landing(&confirmed, complete));
    let node = &done.nodes["a"];
    assert_eq!(
        (node.stage, node.disposition),
        (Stage::Complete, NodeDisposition::Complete)
    );
    assert_eq!(done.control, CampaignControl::Running);
    unchanged(&done, landing(&confirmed, complete));
    // The run succeeds with its last node only.
    let mut last = confirmed.clone();
    for other in ["b", "c"] {
        last.nodes.get_mut(other).unwrap().stage = Stage::Complete;
    }
    let succeeded = step(&last, landing(&last, complete));
    assert_eq!(succeeded.control, CampaignControl::Succeeded);
}

#[test]
fn every_configured_gate_blocks_its_boundary_until_the_exact_candidate_is_approved() {
    let state = gated(started(), &["start", "implementation", "review", "landing"]);
    let work = fsm::work("a", "work-a");
    let assign = || fsm::assign(&work, fsm::assignment(&work));
    let blocked = |state: &CampaignState, command: Command, slot: &str| {
        let rejection = refused(state, command);
        assert_eq!(rejection.code, RejectionCode::GateNotPassed);
        assert_eq!(rejection.target.as_deref(), Some(slot));
    };
    blocked(&state, assign(), "approval:Start");
    // An opened gate is not an approval, and one slot never stands in for the other.
    let state = opened(&state, "start", "candidate");
    blocked(&state, assign(), "approval:Start");
    let state = approved(&state, "start", "candidate");
    blocked(&state, assign(), "approval:Implementation");
    // An approval of another starting candidate does not cover this Work.
    let other = approved(&state, "implementation", "other-head");
    blocked(&other, assign(), "approval:Implementation");
    let state = approved(&state, "implementation", "candidate");
    // An approval recorded for an earlier definition revision is no longer current.
    let mut edited = state.clone();
    edited.definition_revision += 1;
    blocked(&edited, assign(), "approval:Start");

    let (state, seal) = sealed(&assigned(&state, &work), &work);
    let review = fsm::review_of(&work);
    blocked(
        &state,
        fsm::begin(&state, &seal, &review),
        "approval:Review",
    );
    // The Review slot is bound to the sealed output, not to the Work's starting candidate.
    let stale = approved(&state, "review", "candidate");
    blocked(
        &stale,
        fsm::begin(&stale, &seal, &review),
        "approval:Review",
    );
    let state = approved(&state, "review", "h1");
    let (state, review) = in_review(&state, &work, &seal);
    let state = reviewed(&state, &work, &review, &seal);

    blocked(&state, landing(&state, begin_landing), "approval:Landing");
    let state = approved(&state, "landing", "h1");
    let admitted = step(&state, landing(&state, begin_landing));
    assert_eq!(admitted.nodes["a"].disposition, NodeDisposition::Assigned);
}

#[test]
fn resume_returns_interrupted_work_to_a_fresh_work_without_spending_the_budget() {
    let work = fsm::work("a", "work-a");
    let mut running = assigned(&started(), &work);
    let failed = running.nodes.get_mut("c").unwrap();
    (failed.stage, failed.disposition) = (Stage::Implementation, NodeDisposition::Failed);
    let admitted = running.nodes.get_mut("b").unwrap();
    (admitted.stage, admitted.disposition) = (Stage::Landing, NodeDisposition::Assigned);
    // A run that is still stopping is not resumed.
    let stopping = domain::stop(&running, StopMode::Immediate);
    let early = refused(&stopping, resume(&stopping));
    assert_eq!(early.code, RejectionCode::InvalidState);
    let stopped = stopped(&running, &[&work]);
    assert_eq!(stopped.control, CampaignControl::Stopped);
    // A late observation that the session is not gone keeps the run stopped.
    let unknown = SessionObservation::Unknown {
        reason: "host unreachable".into(),
        last_observed_at: None,
    };
    let unsure = session(&stopped, &work, unknown);
    assert_eq!(
        refused(&unsure, resume(&unsure)).code,
        RejectionCode::SessionsNotGone
    );

    let decision = decide(&stopped, &domain::envelope(resume(&stopped))).unwrap();
    let reset = EventKind::RunResumed {
        reset_nodes: vec!["a".into(), "b".into()],
    };
    assert_eq!(decision.events[0].kind, reset);
    let resumed = decision.state;
    // An admission that started nothing goes back to the Landing door.
    let door = &resumed.nodes["b"];
    assert_eq!(
        (door.stage, door.disposition),
        (Stage::Landing, NodeDisposition::Pending)
    );
    let run = resumed.run.as_ref().unwrap();
    let version = stopped.run.as_ref().unwrap().control_version + 1;
    assert_eq!(
        (resumed.control, run.control_version),
        (CampaignControl::Running, version)
    );
    assert!(run.stop.is_none());
    let node = &resumed.nodes["a"];
    assert_eq!(
        (node.stage, node.disposition),
        (Stage::Implementation, NodeDisposition::Pending)
    );
    assert_eq!(
        (
            node.current_attempt.as_ref(),
            node.quality_failures,
            node.repair_cycles
        ),
        (None, 0, 0)
    );
    assert_eq!(epoch(&resumed, "a"), epoch(&stopped, "a") + 1);
    unchanged(&resumed, resume(&resumed));
    // The interrupted Work is over; only a Work bound to the resumed fence starts.
    let mut stale = fsm::work("a", "work-a2");
    let old = refused(&resumed, fsm::assign(&stale, fsm::assignment(&stale)));
    assert_eq!(old.code, RejectionCode::StaleFence);
    stale.control_version = version;
    let fresh = step(&resumed, fsm::assign(&stale, fsm::assignment(&stale)));
    assert_eq!(fresh.nodes["a"].current_attempt.as_deref(), Some("work-a2"));
    // A node that spent its failure budget stays failed across the resume.
    let exhausted = &resumed.nodes["c"];
    assert_eq!(exhausted.disposition, NodeDisposition::Failed);
    let mut retry = fsm::work("c", "work-c");
    retry.control_version = version;
    let rejection = refused(&resumed, fsm::assign(&retry, fsm::assignment(&retry)));
    assert_eq!(rejection.code, RejectionCode::InvalidState);
}
