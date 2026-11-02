use std::cell::Cell;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};

use chrono::{DateTime, Utc};
use tempfile::{TempDir, tempdir};

use super::*;
use crate::services::campaign_execution::candidate_seal::CandidateSeal;
use crate::services::campaign_execution::control::{decide, tests as domain};
use crate::services::campaign_execution::effect_envelope::{
    action_envelope, host_fences, phase_envelope,
};
use crate::services::campaign_execution::model::{
    ActionObservation, ActionState, AttemptBinding, CampaignState, Command, RejectionCode,
};
use crate::services::campaign_execution::stage_fsm::tests::{
    assigned, begin, execution, review_of, run_v, seal_of,
};
use crate::services::campaign_execution::verification_run::{PhaseInput, tests as a1};

const PHASE: &str = "archive";

fn now() -> DateTime<Utc> {
    domain::now()
}

fn apply(state: &CampaignState, command: Command) -> CampaignState {
    decide(state, &domain::envelope(command)).unwrap().state
}

fn refused(state: &CampaignState, command: Command) -> RejectionCode {
    decide(state, &domain::envelope(command)).unwrap_err().code
}

fn epoch(state: &CampaignState) -> u64 {
    state
        .run
        .as_ref()
        .unwrap()
        .fences
        .get("a")
        .map_or(0, |f| f.epoch)
}

// Claim, then StartPending: what the server commits before it hands a phase to its gate.
fn pending(state: &CampaignState, v: &str, consumer: &AttemptBinding) -> CampaignState {
    let advance = |state: &CampaignState, to| {
        apply(
            state,
            Command::AdvanceVerificationPhase {
                execution_id: v.into(),
                phase_id: PHASE.into(),
                consumer: Box::new(consumer.clone()),
                state: to,
            },
        )
    };
    advance(
        &advance(state, ActionState::Claimed),
        ActionState::StartPending,
    )
}

fn pending_action(state: &CampaignState, id: &str, binding: &AttemptBinding) -> CampaignState {
    let advance = |state: &CampaignState, to| {
        apply(
            state,
            Command::AdvanceAction {
                action_id: id.into(),
                binding: Box::new(binding.clone()),
                state: to,
            },
        )
    };
    advance(
        &advance(state, ActionState::Claimed),
        ActionState::StartPending,
    )
}

fn materialize(state: &mut CampaignState, v: &str, seal: &CandidateSeal, input: PhaseInput) {
    let v = state
        .run
        .as_mut()
        .unwrap()
        .verifications
        .get_mut(v)
        .unwrap();
    v.subject.bind(seal.bound_subject()).unwrap();
    v.bind_phase_input(PHASE, input).unwrap();
}

fn start_v(state: &CampaignState) -> String {
    execution(state, "work-a").start_executions[0].clone()
}

fn finish_v(state: &CampaignState) -> String {
    execution(state, "work-a").finish_executions[0].clone()
}

// Work A on node `a`: its finish V archive is materialized and StartPending under A.
fn work_pending() -> (CampaignState, AttemptBinding, CandidateSeal) {
    let (mut state, work) = assigned();
    let seal = seal_of(&state, &work);
    let finish = finish_v(&state);
    materialize(&mut state, &finish, &seal, a1::input(&[]));
    (pending(&state, &finish, &work), work, seal)
}

// A's start check passed and Review R took over node `a` at the next epoch.
fn hand_over(
    state: &CampaignState,
    work: &AttemptBinding,
    seal: &CandidateSeal,
) -> (CampaignState, AttemptBinding) {
    let mut passed = state.clone();
    run_v(&mut passed, &start_v(state), seal, 0);
    let review = review_of(work);
    (apply(&passed, begin(&passed, seal, &review)), review)
}

// Moves every registered phase onto its own verification host, where no session ever runs.
fn on_verify_host(state: &mut CampaignState) -> HostGate {
    let run = state.run.as_mut().unwrap();
    let mut verify = None;
    for v in run.verifications.values_mut() {
        let mut gate = v.origin.gate.clone();
        gate.gate = "verify".into();
        v.origin.gate = gate.clone();
        for phase in &mut v.phases {
            phase.target = gate.clone();
            run.actions.get_mut(&phase.action_id).unwrap().target = gate.clone();
        }
        verify = Some(gate);
    }
    verify.unwrap()
}

// Reopens after a drop. A child forked by a parallel test holds the released lock until its exec,
// so only that short Busy window is retried.
fn reopen(root: &std::path::Path, identity: &HostGate) -> Result<EffectGate, GateError> {
    for _ in 0..300 {
        match EffectGate::recover_journal(root, identity.clone(), now()) {
            Err(GateError::Busy) => std::thread::sleep(std::time::Duration::from_millis(10)),
            other => return other,
        }
    }
    Err(GateError::Busy)
}

fn open(dir: &TempDir, identity: &HostGate) -> EffectGate {
    reopen(dir.path(), identity).unwrap()
}

// The host applies node `a`'s fence and the run fence exactly as the server holds them now.
fn sync(gate: &mut EffectGate, state: &CampaignState) -> FenceReport {
    let (run, node) = host_fences(state, "a").unwrap();
    gate.apply_node_fence(node).unwrap();
    gate.apply_run_fence(run).unwrap()
}

fn process(nonce: &str) -> ProcessIdentity {
    ProcessIdentity {
        host: "remote".into(),
        pid: 4242,
        process_group: Some(4242),
        started_at: now(),
        nonce: nonce.into(),
    }
}

// Stands in for the real start and counts how many effects actually began.
fn admit(gate: &mut EffectGate, envelope: &EffectEnvelope, calls: &Cell<u32>) -> Initiation {
    gate.admit_and_initiate(envelope, now(), |nonce| {
        calls.set(calls.get() + 1);
        Ok(Initiated::Started {
            receipt: format!("receipt-{nonce}"),
            process: Some(process(nonce)),
            handle: (),
        })
    })
    .unwrap()
    .initiation
}

fn receipt_of(initiation: &Initiation) -> String {
    match initiation {
        Initiation::Started { receipt, .. } => receipt.clone(),
        other => panic!("expected a start, got {other:?}"),
    }
}

fn refusal(initiation: &Initiation) -> Option<GateRefusal> {
    match initiation {
        Initiation::NotSent { reason, .. } => Some(*reason),
        _ => None,
    }
}

fn observe(
    state: &CampaignState,
    id: &str,
    owner: &AttemptBinding,
    observation: ActionObservation,
) -> CampaignState {
    apply(
        state,
        Command::ObserveAction {
            action_id: id.into(),
            binding: Box::new(owner.clone()),
            observation,
        },
    )
}

#[test]
fn a_start_before_the_stop_fence_is_reconciled_and_one_after_it_never_happens() {
    let (mut state, work) = assigned();
    let verify = on_verify_host(&mut state);
    let seal = seal_of(&state, &work);
    let (finish, start) = (finish_v(&state), start_v(&state));
    for v in [&finish, &start] {
        materialize(&mut state, v, &seal, a1::input(&[]));
    }
    let state = pending(&pending(&state, &finish, &work), &start, &work);
    let state = pending_action(&state, "launch-work-a", &work);
    let early = phase_envelope(&state, &finish, PHASE, &work).unwrap();
    let late = phase_envelope(&state, &start, PHASE, &work).unwrap();
    let launch = action_envelope(&state, "launch-work-a", &work, "launch".into()).unwrap();
    let (verify_dir, launch_dir) = (tempdir().unwrap(), tempdir().unwrap());
    let calls = Cell::new(0);
    let mut gate = open(&verify_dir, &verify);
    sync(&mut gate, &state);
    let receipt = receipt_of(&admit(&mut gate, &early, &calls));

    let stopped = domain::stop(&state, StopMode::Immediate);
    let version = stopped.run.as_ref().unwrap().control_version;
    let report = sync(&mut gate, &stopped);
    assert_eq!(report.acknowledgement.target, verify);
    assert_eq!(report.acknowledgement.control_version, version);
    assert!(matches!(&report.entries[..],
        [e] if matches!(&e.state, JournalState::Started { receipt: r, .. } if *r == receipt)));
    // The fence survives a host restart: the late request is cancelled, the early one replayed.
    drop(gate);
    let mut gate = open(&verify_dir, &verify);
    // A delayed pre-stop fence does not move the host back.
    assert_eq!(
        sync(&mut gate, &state).acknowledgement.control_version,
        version
    );
    assert_eq!(
        refusal(&admit(&mut gate, &late, &calls)),
        Some(GateRefusal::Revoked)
    );
    assert_eq!(receipt_of(&admit(&mut gate, &early, &calls)), receipt);
    let mut launch_gate = open(&launch_dir, &launch.target_host_gate);
    let launch_report = sync(&mut launch_gate, &stopped);
    assert_eq!(
        refusal(&admit(&mut launch_gate, &launch, &calls)),
        Some(GateRefusal::Revoked)
    );
    assert_eq!(calls.get(), 1);

    let not_sent = || ActionObservation::NotStarted {
        evidence: "host fence applied; journal holds no start".into(),
    };
    let mut reconciled = observe(
        &stopped,
        &early.action_id,
        &work,
        ActionObservation::Started { receipt },
    );
    reconciled = observe(&reconciled, &late.action_id, &work, not_sent());
    reconciled = observe(&reconciled, &launch.action_id, &work, not_sent());
    // The verification-only host is a stop target of its own and must acknowledge.
    assert_eq!(
        refused(&reconciled, Command::ConfirmStopFence),
        RejectionCode::FenceIncomplete
    );
    for ack in [report.acknowledgement, launch_report.acknowledgement] {
        reconciled = apply(
            &reconciled,
            Command::AcknowledgeFence {
                expected_run: "run".into(),
                target: ack.target,
                control_version: ack.control_version,
            },
        );
    }
    let confirmed = apply(&reconciled, Command::ConfirmStopFence);
    assert!(
        confirmed
            .run
            .as_ref()
            .unwrap()
            .stop
            .as_ref()
            .unwrap()
            .confirmed_event
            .is_some()
    );
}

#[test]
fn node_interruption_applied_on_the_host_revokes_every_holder_of_the_node() {
    let (state, work, seal) = work_pending();
    let (handed, review) = hand_over(&state, &work, &seal);
    let finish = finish_v(&handed);
    let request = phase_envelope(&handed, &finish, PHASE, &review).unwrap();
    let interrupted = apply(
        &handed,
        Command::InterruptNode {
            expected_run: "run".into(),
            node_id: "a".into(),
            expected_epoch: epoch(&handed),
            session: review.session.clone(),
            reason: "operator stop of this node".into(),
        },
    );
    let dir = tempdir().unwrap();
    let mut gate = open(&dir, &request.target_host_gate);
    sync(&mut gate, &interrupted);
    sync(&mut gate, &handed);
    let calls = Cell::new(0);
    for _ in 0..2 {
        assert_eq!(
            refusal(&admit(&mut gate, &request, &calls)),
            Some(GateRefusal::Revoked)
        );
    }
    assert_eq!(calls.get(), 0);
    assert!(matches!(
        gate.entry(&request.action_id).unwrap().state,
        JournalState::Cancelled { .. }
    ));
    assert_eq!(
        phase_envelope(&interrupted, &finish, PHASE, &review)
            .unwrap_err()
            .code,
        RejectionCode::StaleFence
    );
}

#[test]
fn handover_keeps_one_effect_and_a_stale_holder_does_not_cancel_it() {
    let (state, work, seal) = work_pending();
    let finish = finish_v(&state);
    let from_a = phase_envelope(&state, &finish, PHASE, &work).unwrap();
    let (handed, review) = hand_over(&state, &work, &seal);
    let from_r = phase_envelope(&handed, &finish, PHASE, &review).unwrap();
    // Same action, payload and nonce; only the holder differs.
    assert_eq!(from_a.identity(), from_r.identity());
    assert_eq!(from_a.execution_nonce, from_r.execution_nonce);
    assert_ne!(from_a.authority, from_r.authority);
    let mut foreign = from_r.clone();
    foreign.authority = EffectAuthority::CurrentAttempt {
        attempt_id: review.attempt_id.clone(),
        binding_digest: "review-binding".into(),
        node_epoch: epoch(&handed),
        run_control_version: 1,
    };

    // A's start passed the gate before the handover: R gets that receipt, nothing starts again.
    let (dir, calls) = (tempdir().unwrap(), Cell::new(0));
    let mut gate = open(&dir, &from_a.target_host_gate);
    sync(&mut gate, &state);
    let receipt = receipt_of(&admit(&mut gate, &from_a, &calls));
    sync(&mut gate, &handed);
    assert_eq!(receipt_of(&admit(&mut gate, &from_r, &calls)), receipt);
    assert_eq!(calls.get(), 1);

    // A's request arrives only after the handover: it is stale, and R still starts the phase once.
    let (dir, calls) = (tempdir().unwrap(), Cell::new(0));
    let mut gate = open(&dir, &from_a.target_host_gate);
    sync(&mut gate, &state);
    let behind = gate.admit_and_initiate(&from_r, now(), |_| -> io::Result<Initiated<()>> {
        unreachable!("R starts nothing before its epoch is applied")
    });
    assert_eq!(behind.err(), Some(GateError::FenceBehind));
    sync(&mut gate, &handed);
    assert_eq!(
        refusal(&admit(&mut gate, &from_a, &calls)),
        Some(GateRefusal::StaleAuthority)
    );
    assert!(gate.entry(&from_a.action_id).is_none());
    // An authority resolved for another holder never starts this effect, even at the current epoch.
    let refused = gate.admit_and_initiate(&foreign, now(), |_| -> io::Result<Initiated<()>> {
        unreachable!("an authority for another holder starts nothing")
    });
    assert!(matches!(refused, Err(GateError::Conflict(_))));
    let receipt = receipt_of(&admit(&mut gate, &from_r, &calls));
    assert_eq!(receipt_of(&admit(&mut gate, &from_a, &calls)), receipt);
    assert_eq!(calls.get(), 1);
}

#[test]
fn finish_plan_continues_only_its_frozen_phases_and_immediate_revokes_them() {
    let (state, work, seal) = work_pending();
    let (mut handed, review) = hand_over(&state, &work, &seal);
    let run = handed.run.as_mut().unwrap();
    run.attempts.get_mut("review-r").unwrap().started = true;
    let handed = pending_action(&handed, "launch-review-r", &review);
    let finish = finish_v(&handed);
    let before_stop = phase_envelope(&handed, &finish, PHASE, &review).unwrap();
    let launch = action_envelope(&handed, "launch-review-r", &review, "launch".into()).unwrap();
    let finishing = domain::stop(&handed, StopMode::FinishStage);
    let continuation = phase_envelope(&finishing, &finish, PHASE, &review).unwrap();
    assert!(matches!(&continuation.authority,
        EffectAuthority::Verification(a) if matches!(a.source, AuthoritySource::FinishPlan { .. })));
    assert_eq!(continuation.identity(), before_stop.identity());

    let (dir, calls) = (tempdir().unwrap(), Cell::new(0));
    let mut gate = open(&dir, &continuation.target_host_gate);
    sync(&mut gate, &finishing);
    assert_eq!(
        refusal(&admit(&mut gate, &before_stop, &calls)),
        Some(GateRefusal::StaleAuthority)
    );
    assert_eq!(
        refusal(&admit(&mut gate, &launch, &calls)),
        Some(GateRefusal::Revoked)
    );
    receipt_of(&admit(&mut gate, &continuation, &calls));
    assert_eq!(calls.get(), 1);

    // Escalation to Immediate ends the FinishPlan too: its unsent continuation never starts.
    let escalated = domain::stop(&finishing, StopMode::Immediate);
    let (dir, calls) = (tempdir().unwrap(), Cell::new(0));
    let mut gate = open(&dir, &continuation.target_host_gate);
    sync(&mut gate, &escalated);
    assert_eq!(
        refusal(&admit(&mut gate, &continuation, &calls)),
        Some(GateRefusal::Revoked)
    );
    assert_eq!(calls.get(), 0);
}

#[test]
fn a_start_lost_in_a_crash_stays_indeterminate_until_directly_resolved() {
    let (state, work, _) = work_pending();
    let request = phase_envelope(&state, &finish_v(&state), PHASE, &work).unwrap();
    let dir = tempdir().unwrap();
    let mut gate = open(&dir, &request.target_host_gate);
    sync(&mut gate, &state);
    let crashed = catch_unwind(AssertUnwindSafe(|| {
        gate.admit_and_initiate(&request, now(), |_| -> io::Result<Initiated<()>> {
            panic!("host died between the spawn and its receipt")
        })
    }));
    assert!(crashed.is_err());
    drop(gate);

    let mut gate = open(&dir, &request.target_host_gate);
    let calls = Cell::new(0);
    let lost = admit(&mut gate, &request, &calls);
    let Initiation::Indeterminate { initiation_id, .. } = &lost else {
        panic!("expected an indeterminate start, got {lost:?}");
    };
    assert_eq!(calls.get(), 0);
    assert_eq!(
        gate.restart_observations(|_| TreeState::Gone, now()),
        vec![RestartObservation::Indeterminate {
            action_id: request.action_id.clone(),
            initiation_id: initiation_id.clone(),
            evidence: "host restarted before the initiation receipt was recorded".into(),
        }]
    );
    let unstarted = || Resolution::Unstarted {
        evidence: "no process carries the nonce".into(),
    };
    assert!(
        gate.resolve_indeterminate(&request.action_id, "older#0", unstarted(), now())
            .is_err()
    );
    gate.resolve_indeterminate(&request.action_id, initiation_id, unstarted(), now())
        .unwrap();
    let receipt = receipt_of(&admit(&mut gate, &request, &calls));
    assert_eq!(calls.get(), 1);

    // A recorded result answers later requests and is no longer a running process after restart.
    gate.record_finished(
        &request.action_id,
        &receipt,
        serde_json::json!({"exit": 0}),
        now(),
    )
    .unwrap();
    drop(gate);
    let mut gate = open(&dir, &request.target_host_gate);
    assert!(
        gate.restart_observations(|_| TreeState::Alive, now())
            .is_empty()
    );
    assert_eq!(receipt_of(&admit(&mut gate, &request, &calls)), receipt);
    assert_eq!(calls.get(), 1);
}

#[cfg(unix)]
#[test]
fn restart_observes_a_phase_child_without_turning_it_into_a_result() {
    use super::super::verification_run::{CommandSpec, Termination, VerificationCollection};
    use super::super::verification_runner::{Settled, process_tree_state};

    let dir = tempdir().unwrap();
    let marker = dir.path().join("spawns");
    let command = CommandSpec {
        program: "sh".into(),
        argv: vec![
            "-c".into(),
            format!(
                "echo \"$ADK_CAMPAIGN_PHASE_NONCE\" >> '{}'",
                marker.display()
            ),
        ],
        cwd: dir.path().to_string_lossy().into_owned(),
        env_identity: Default::default(),
    };
    let (mut state, work) = assigned();
    let seal = seal_of(&state, &work);
    let finish = finish_v(&state);
    let input = PhaseInput {
        checkout_path: command.cwd.clone(),
        command,
        ..a1::input(&[])
    };
    materialize(&mut state, &finish, &seal, input.clone());
    let state = pending(&state, &finish, &work);
    let request = phase_envelope(&state, &finish, PHASE, &work).unwrap();
    let gate_dir = dir.path().join("gate");
    let open_gate = || reopen(&gate_dir, &request.target_host_gate).unwrap();
    let launch_ctx = || LaunchContext {
        check_id: "slow-tests",
        log_dir: dir.path(),
        host: "remote",
        now: now(),
        handoff: None,
        nonce: String::new(),
    };
    let bound = BoundInput::new(input).unwrap();
    let mut gate = open_gate();
    sync(&mut gate, &state);
    let started = start_phase(
        &mut gate,
        &request,
        PhaseKind::Archive,
        PHASE,
        &bound,
        &[],
        launch_ctx(),
    )
    .unwrap();
    let PhaseInitiation::Started { receipt, process } = started.initiation.clone() else {
        panic!("expected a start, got {:?}", started.initiation);
    };
    // The child exits, but the host dies before its result reaches the journal or the server.
    let Settled::Finished(_) = started
        .running
        .unwrap()
        .wait(std::time::Duration::from_secs(20), now())
    else {
        panic!("phase child did not settle");
    };
    drop(gate);

    let mut gate = open_gate();
    let replay = start_phase(
        &mut gate,
        &request,
        PhaseKind::Archive,
        PHASE,
        &bound,
        &[],
        launch_ctx(),
    )
    .unwrap();
    assert_eq!(replay.initiation, started.initiation);
    assert!(replay.running.is_none());
    let spawns = std::fs::read_to_string(&marker).unwrap();
    assert_eq!(spawns, format!("{}\n", request.execution_nonce));

    let started_state = observe(
        &state,
        &request.action_id,
        &work,
        ActionObservation::Started {
            receipt: receipt.clone(),
        },
    );
    let mut v = started_state.run.as_ref().unwrap().verifications[&finish].clone();
    let action = started_state.run.as_ref().unwrap().actions[&request.action_id].clone();
    v.record_initiation(PHASE, started.initiation).unwrap();
    // A probe that cannot tell keeps the phase open; only a tree seen Gone closes it, as interrupted.
    let unknown = gate.restart_observations(|_| TreeState::Unknown, now());
    let unclear = termination_observation(&v, &unknown[0]).unwrap();
    assert_eq!(unclear.tree, TreeState::Unknown);
    assert!(v.clone().reconcile_termination(&action, unclear).is_err());
    let seen = gate.restart_observations(process_tree_state, now());
    let gone = termination_observation(&v, &seen[0]).unwrap();
    assert_eq!(
        (gone.tree, &gone.process, &gone.receipt),
        (TreeState::Gone, &process, &receipt)
    );
    v.reconcile_termination(&action, gone).unwrap();
    let result = v.phase(PHASE).unwrap().result.clone().unwrap();
    assert!(matches!(
        result.termination,
        Termination::Interrupted { .. }
    ));
    assert_eq!(result.record.exit_code, None);
    let actions = &started_state.run.as_ref().unwrap().actions;
    let collected = v.collect(|id| actions.get(id), a1::environment(), vec![]);
    assert!(matches!(
        collected,
        Ok(VerificationCollection::Interrupted { .. })
    ));
}

#[test]
fn only_the_exact_incarnation_and_a_single_writer_start_effects() {
    let (mut state, work, seal) = work_pending();
    let start = start_v(&state);
    materialize(&mut state, &start, &seal, a1::input(&[]));
    let state = pending(&state, &start, &work);
    let first = phase_envelope(&state, &finish_v(&state), PHASE, &work).unwrap();
    let second = phase_envelope(&state, &start, PHASE, &work).unwrap();
    let home = first.target_host_gate.clone();
    let (dir, calls) = (tempdir().unwrap(), Cell::new(0));
    let mut gate = open(&dir, &home);
    sync(&mut gate, &state);
    let receipt = receipt_of(&admit(&mut gate, &first, &calls));
    assert_eq!(
        EffectGate::recover_journal(dir.path(), home.clone(), now()).err(),
        Some(GateError::Busy)
    );
    drop(gate);

    // After a reboot the gate answers what the old incarnation started but starts nothing for it.
    let rebooted = HostGate {
        incarnation: "boot-3".into(),
        ..home.clone()
    };
    let mut gate = open(&dir, &rebooted);
    assert_eq!(receipt_of(&admit(&mut gate, &first, &calls)), receipt);
    let wrong = gate.admit_and_initiate(&second, now(), |_| -> io::Result<Initiated<()>> {
        unreachable!("no start for another incarnation")
    });
    assert!(matches!(wrong, Err(GateError::WrongHost(_))));
    assert_eq!(calls.get(), 1);
    drop(gate);
    let elsewhere = HostGate {
        host: "macbook".into(),
        ..home
    };
    assert!(matches!(
        reopen(dir.path(), &elsewhere),
        Err(GateError::WrongHost(_))
    ));
}
