use super::tests::{Harness, applied, envelope, execute, send, stop};
use super::*;
use crate::services::campaign_execution::{
    control::tests as domain,
    evidence::{EvidenceEnvelope, EvidenceStatus, TuiPayload, TuiSubmission},
    evidence_gate::tests::{REPO, workspace},
    profile::{ExecutedTest, ExecutionEnvironment, TestOutcome},
    stage_fsm::tests as fsm,
    verification_run::{
        ArtifactRef, ListedTest, NotSentReason, PhaseInitiation, PhaseInput, PhaseKind,
        PhaseOutput, ProcessIdentity, TerminationObservation, TreeState, VerificationCollection,
        digest_of, plan_scope, tests as a1,
    },
};
use evidence_store::StoredEvidence;

const LAUNCH: &str = "launch-work-a";

fn process() -> ProcessIdentity {
    ProcessIdentity {
        host: "remote".into(),
        pid: 7,
        process_group: Some(7),
        started_at: a1::at(),
        nonce: "nonce".into(),
    }
}

fn environment(os: &str) -> ExecutionEnvironment {
    let mut environment = a1::environment();
    environment.os = os.into();
    environment.checkout.host = "remote".into();
    environment
}

fn fact(v: &str, phase: &str, fact: PhaseFact) -> Command {
    Command::RecordVerificationPhase {
        execution_id: v.into(),
        phase_id: phase.into(),
        fact: Box::new(fact),
    }
}

fn started(v: &str, phase: &str, os: &str) -> Command {
    let started = PhaseFact::Started {
        receipt: format!("{phase}:7:nonce"),
        process: process(),
        environment: Box::new(environment(os)),
    };
    fact(v, phase, started)
}

fn unsent(v: &str, phase: &str) -> Command {
    let evidence = "gate refused the launch before spawning".into();
    fact(v, phase, PhaseFact::NotStarted { evidence })
}

fn output(kind: PhaseKind, check: &str) -> PhaseOutput {
    let test = ListedTest {
        binary_id: "agentdesk".into(),
        name: "a".into(),
        ignored: false,
    };
    match kind {
        PhaseKind::Archive => PhaseOutput::Archive {
            digest: "archive-digest".into(),
        },
        PhaseKind::List => PhaseOutput::Universe { tests: vec![test] },
        PhaseKind::Plan => PhaseOutput::Plan {
            plan_digest: "plan-digest".into(),
            plan_artifact_digest: "plan-file-digest".into(),
            config_digest: "config-digest".into(),
            scope: plan_scope(fsm::spec().check(check).unwrap().selection).into(),
            selected: vec![test],
        },
        PhaseKind::Test => PhaseOutput::Tests {
            executed: vec![ExecutedTest {
                id: "agentdesk a".into(),
                check_id: check.into(),
                outcome: TestOutcome::Passed,
                log_refs: vec![],
            }],
        },
        _ => PhaseOutput::None,
    }
}

fn settle(v: &str) -> Command {
    Command::SettleVerification {
        execution_id: v.into(),
        evidence: "driver saw the check quiet".into(),
    }
}

fn observe(observation: ActionObservation) -> Command {
    Command::ObserveAction {
        action_id: LAUNCH.into(),
        binding: Box::new(fsm::work("a", "work-a")),
        observation,
    }
}

const REFUSE: &str = "CREATE OR REPLACE FUNCTION refuse_write() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected failure'; END $$";
const ON_EVIDENCE_INSERT: &str = "CREATE TRIGGER refuse_write BEFORE INSERT ON server_campaign_evidence FOR EACH ROW EXECUTE FUNCTION refuse_write()";
const ON_EVIDENCE_UPDATE: &str = "CREATE TRIGGER refuse_write BEFORE UPDATE ON server_campaign_evidence FOR EACH ROW EXECUTE FUNCTION refuse_write()";
const ON_RECEIPT_INSERT: &str = "CREATE TRIGGER refuse_write BEFORE INSERT ON server_campaign_receipts FOR EACH ROW EXECUTE FUNCTION refuse_write()";
const HEAL_EVIDENCE: &str = "DROP TRIGGER refuse_write ON server_campaign_evidence";
const HEAL_RECEIPT: &str = "DROP TRIGGER refuse_write ON server_campaign_receipts";

async fn sql(pool: &PgPool, statements: &[&'static str]) {
    for statement in statements {
        sqlx::query(*statement).execute(pool).await.unwrap();
    }
}

// A Running campaign with no attempt yet: everything after the seed goes through store commands.
struct Flow {
    h: Harness,
    work: AttemptBinding,
    keys: u32,
}

impl Flow {
    async fn new() -> Self {
        let h = Harness::new().await;
        h.seed(fsm::base()).await;
        Self {
            h,
            work: fsm::work("a", "work-a"),
            keys: 0,
        }
    }

    async fn send(&mut self, command: Command) -> CommandOutcome {
        self.keys += 1;
        send(&self.h.pool, &format!("key-{}", self.keys), command).await
    }

    async fn ok(&mut self, command: Command) -> CommandResult {
        applied(self.send(command).await)
    }

    async fn no(&mut self, command: Command, code: RejectionCode) {
        match self.send(command).await {
            CommandOutcome::Decided(Err(rejection)) => assert_eq!(rejection.code, code),
            other => panic!("expected {code:?}, got {other:?}"),
        }
    }

    // A storage failure leaves neither a receipt nor any part of the command behind.
    async fn fails(&mut self, command: Command) {
        let before = self.state().await;
        self.keys += 1;
        let key = format!("key-{}", self.keys);
        let reply = execute(&self.h.pool, "campaign", &envelope(&key, command)).await;
        assert!(matches!(reply, Err(StoreError::Sql(_))), "{reply:?}");
        assert_eq!(self.state().await, before);
    }

    async fn state(&self) -> CampaignState {
        get(&self.h.pool, "campaign").await.unwrap().unwrap()
    }

    async fn run(&self) -> Run {
        self.state().await.run.unwrap()
    }

    async fn action(&self, v: &str, phase: &str) -> Action {
        let run = self.run().await;
        let id = &run.verifications[v].phase(phase).unwrap().action_id;
        run.actions[id].clone()
    }

    async fn stored(&self, evidence_id: &str) -> Option<StoredEvidence> {
        let document: Option<sqlx::types::Json<StoredEvidence>> = sqlx::query_scalar(
            "SELECT document FROM server_campaign_evidence WHERE evidence_id = $1",
        )
        .bind(evidence_id)
        .fetch_optional(&self.h.pool)
        .await
        .unwrap();
        document.map(|d| d.0)
    }

    // Registers the Work with both checks and hands its launch to the gate.
    async fn assign(&mut self) -> (String, String) {
        let work = self.work.clone();
        self.ok(fsm::assign(&work, fsm::assignment(&work))).await;
        for state in [ActionState::Claimed, ActionState::StartPending] {
            self.ok(Command::AdvanceAction {
                action_id: LAUNCH.into(),
                binding: Box::new(work.clone()),
                state,
            })
            .await;
        }
        let state = self.state().await;
        let execution = fsm::execution(&state, "work-a");
        (
            execution.start_executions[0].clone(),
            execution.finish_executions[0].clone(),
        )
    }

    async fn launched(&mut self) {
        self.ok(observe(ActionObservation::Started {
            receipt: "launch-receipt".into(),
        }))
        .await;
        self.ok(Command::ObserveSession {
            identity: self.work.session.clone(),
            observation: SessionObservation::Alive {
                observed_at: domain::now(),
            },
        })
        .await;
    }

    async fn seal_command(&self) -> Command {
        let state = self.state().await;
        let submission = TuiSubmission {
            evidence_id: "output".into(),
            payload: TuiPayload::OutputClaim {
                output_sha: "h1".into(),
            },
        };
        let pin = &fsm::execution(&state, "work-a").pin_hash;
        let claim = EvidenceEnvelope::from_tui(submission, &self.work, pin, domain::now()).unwrap();
        let mut workspace = workspace("h1");
        workspace.host = "remote".into();
        Command::SealCandidate {
            claim: Box::new(claim),
            repository_identity: REPO.into(),
            workspace: Box::new(workspace),
        }
    }

    // The session handed in its output: sealed, and bound as this V's subject.
    async fn sealed(&mut self, v: &str) {
        let seal = self.seal_command().await;
        self.ok(seal).await;
        self.ok(self.subject(v)).await;
    }

    fn subject(&self, v: &str) -> Command {
        Command::BindVerificationSubject {
            execution_id: v.into(),
            consumer: Box::new(self.work.clone()),
        }
    }

    async fn input(&mut self, v: &str, phase: &str) -> PhaseInput {
        let run = self.run().await;
        let stored = &run.verifications[v];
        let registered = stored.phase(phase).unwrap();
        let mut input = a1::input(&[]);
        input.predecessors = registered
            .requires
            .iter()
            .map(|r| {
                let output = &stored.phase(r).unwrap().result.as_ref().unwrap().output;
                (r.clone(), digest_of(output).unwrap())
            })
            .collect();
        if registered.kind == PhaseKind::Test {
            self.ok(Command::BindVerificationPlan {
                execution_id: v.into(),
                consumer: Box::new(self.work.clone()),
            })
            .await;
            for name in ["plan-file", "archive", "config"] {
                input.preflight.push(ArtifactRef {
                    name: name.into(),
                    path: format!("/scratch/{name}"),
                    digest: format!("{name}-digest"),
                });
            }
        }
        input
    }

    fn bind_input(&self, v: &str, phase: &str, input: PhaseInput) -> Command {
        Command::BindVerificationInput {
            execution_id: v.into(),
            phase_id: phase.into(),
            consumer: Box::new(self.work.clone()),
            input: Box::new(input),
        }
    }

    async fn advance(&mut self, v: &str, phase: &str) {
        for state in [ActionState::Claimed, ActionState::StartPending] {
            self.ok(fsm::advance(v, phase, &self.work, state)).await;
        }
    }

    // Binds the phase input and hands the phase start to the gate.
    async fn pend(&mut self, v: &str, phase: &str) {
        let input = self.input(v, phase).await;
        self.ok(self.bind_input(v, phase, input)).await;
        self.advance(v, phase).await;
    }

    async fn finished(&self, v: &str, phase: &str) -> Command {
        let run = self.run().await;
        let stored = &run.verifications[v];
        let output = output(stored.phase(phase).unwrap().kind, &stored.origin.check_id);
        let result = Box::new(a1::result(phase, 0, output));
        fact(v, phase, PhaseFact::Finished { result })
    }

    async fn run_phase(&mut self, v: &str, phase: &str, os: &str) {
        self.pend(v, phase).await;
        self.ok(started(v, phase, os)).await;
        let finished = self.finished(v, phase).await;
        self.ok(finished).await;
    }

    async fn run_check(&mut self, v: &str) {
        let run = self.run().await;
        let phases: Vec<String> = run.verifications[v]
            .phases
            .iter()
            .map(|p| p.id.clone())
            .collect();
        for (index, phase) in phases.iter().enumerate() {
            self.run_phase(v, phase, &format!("os-{index}")).await;
        }
    }

    async fn accept(&mut self) -> Command {
        fsm::accept(&self.state().await, "work-a", None)
    }

    // Closes what the stop still waits for outside the checks, then asks for Stopped.
    async fn stopped(&mut self, session_started: bool) {
        if session_started {
            let evidence = "session ended".to_string();
            self.ok(observe(ActionObservation::Reconciled {
                evidence: evidence.clone(),
            }))
            .await;
            self.ok(Command::ObserveSession {
                identity: self.work.session.clone(),
                observation: SessionObservation::Gone {
                    observed_at: domain::now(),
                    evidence,
                },
            })
            .await;
        }
        let run = self.run().await;
        self.ok(Command::AcknowledgeFence {
            expected_run: "run".into(),
            target: self.work.session.gate.clone(),
            control_version: run.control_version,
        })
        .await;
        self.ok(Command::ConfirmStopFence).await;
        self.ok(Command::ConfirmStopped).await;
        assert_eq!(self.state().await.control, CampaignControl::Stopped);
    }
}

#[tokio::test]
async fn work_result_reaches_review_through_store_commands_only_pg() {
    let mut f = Flow::new().await;
    let (start, finish) = f.assign().await;
    f.launched().await;
    // Nothing is bound before the server sealed the session's output.
    f.no(f.subject(&start), RejectionCode::InvalidState).await;
    f.sealed(&start).await;
    let sealed = f.state().await;
    let again = f.ok(f.subject(&start)).await;
    assert_eq!(again.event_sequence, sealed.event_sequence);
    let reseal = f.seal_command().await;
    assert_eq!(f.ok(reseal).await.event_sequence, sealed.event_sequence);

    f.run_phase(&start, "archive", "os-0").await;
    // A bound input is never replaced, and a result is kept only by the phase that started it.
    let mut other = f.input(&start, "archive").await;
    other.checkout_path = "/elsewhere".into();
    f.no(
        f.bind_input(&start, "archive", other),
        RejectionCode::AlreadyExists,
    )
    .await;
    let foreign = f.finished(&start, "archive").await;
    let Command::RecordVerificationPhase { fact: foreign, .. } = foreign else {
        unreachable!()
    };
    f.no(
        fact(&finish, "archive", *foreign),
        RejectionCode::InvalidState,
    )
    .await;
    let recorded = f.state().await;
    let resent = f.finished(&start, "archive").await;
    assert_eq!(f.ok(resent).await.event_sequence, recorded.event_sequence);
    assert_eq!(f.state().await, recorded);

    for phase in ["list", "classify", "plan", "test"] {
        f.run_phase(&start, phase, "os-later").await;
    }
    // An unfinished check is neither collected nor a pass.
    f.no(settle(&start), RejectionCode::InvalidState).await;
    let state = f.state().await;
    let seal = state.run.as_ref().unwrap().seals["work-a"].clone();
    let review = fsm::review_of(&f.work);
    f.no(
        fsm::begin(&state, &seal, &review),
        RejectionCode::GateNotPassed,
    )
    .await;
    f.run_phase(&start, "audit", "os-later").await;
    f.ok(settle(&start)).await;

    let state = f.state().await;
    f.ok(fsm::begin(&state, &seal, &review)).await;
    let state = f.state().await;
    assert_eq!(state.nodes["a"].stage, Stage::Review);
    let run = state.run.as_ref().unwrap();
    assert_eq!(run.attempts["work-a"].outcome, Some(AttemptOutcome::Passed));
    let Some(VerificationCollection::Executed { evidence, audit }) =
        &run.verifications[&start].collection
    else {
        panic!("start check was not collected as executed");
    };
    assert!(audit.complete && audit.all_passed);
    // Each log reference names the host that holds the file.
    assert!(
        evidence
            .log_refs
            .contains(&"remote:/log/test.stdout".to_string())
    );
    for id in ["tui:work-a:output".to_string(), format!("run:{start}")] {
        let Some(StoredEvidence::Evidence { status, .. }) = f.stored(&id).await else {
            panic!("{id} was not recorded as evidence");
        };
        assert_eq!(status, EvidenceStatus::Accepted);
    }
    assert!(matches!(
        f.stored("seal:work-a").await,
        Some(StoredEvidence::Seal { .. })
    ));
    f.h.close().await;
}

#[tokio::test]
async fn environment_is_stored_once_and_survives_restart_pg() {
    let mut f = Flow::new().await;
    let (start, _) = f.assign().await;
    f.launched().await;
    f.sealed(&start).await;
    f.pend(&start, "archive").await;
    // An environment observed on another host is not this phase's.
    let mut foreign = a1::environment();
    foreign.checkout.host = "elsewhere".into();
    let receipt = "archive:7:nonce".to_string();
    let wrong = PhaseFact::Started {
        receipt,
        process: process(),
        environment: Box::new(foreign),
    };
    f.no(fact(&start, "archive", wrong), RejectionCode::StaleBinding)
        .await;
    f.ok(started(&start, "archive", "os-first")).await;
    let finished = f.finished(&start, "archive").await;
    f.ok(finished).await;
    f.run_phase(&start, "list", "os-second").await;
    f.h.reconnect().await;
    assert_eq!(f.run().await.environments[&start], environment("os-first"));
    for phase in ["classify", "plan", "test", "audit"] {
        f.run_phase(&start, phase, "os-second").await;
    }
    f.ok(settle(&start)).await;
    let run = f.run().await;
    let Some(VerificationCollection::Executed { evidence, .. }) =
        &run.verifications[&start].collection
    else {
        panic!("start check was not collected as executed");
    };
    assert_eq!(evidence.environment, environment("os-first"));
    f.h.close().await;
}

#[tokio::test]
async fn generic_observation_is_refused_for_check_phases_pg() {
    let mut f = Flow::new().await;
    let (start, _) = f.assign().await;
    f.launched().await;
    f.sealed(&start).await;
    f.pend(&start, "archive").await;
    let before = f.state().await;
    let action = f.action(&start, "archive").await;
    let text = || "forged".to_string();
    for observation in [
        ActionObservation::Started { receipt: text() },
        ActionObservation::Unknown { reason: text() },
        ActionObservation::Reconciled { evidence: text() },
        ActionObservation::NotStarted { evidence: text() },
    ] {
        let generic = Command::ObserveAction {
            action_id: action.id.clone(),
            binding: Box::new(action.binding.clone()),
            observation,
        };
        f.no(generic, RejectionCode::InvalidState).await;
    }
    assert_eq!(f.state().await, before);
    f.h.close().await;
}

#[tokio::test]
async fn refused_start_returns_to_planned_while_holder_valid_pg() {
    let mut f = Flow::new().await;
    let (start, _) = f.assign().await;
    f.launched().await;
    f.sealed(&start).await;
    f.pend(&start, "archive").await;
    f.ok(unsent(&start, "archive")).await;
    assert_eq!(
        f.action(&start, "archive").await.state,
        ActionState::Planned
    );
    let reason = |run: &Run| match run.verifications[&start].phases[0].initiations.last() {
        Some(PhaseInitiation::NotSent { reason, .. }) => reason.clone(),
        other => panic!("expected a recorded non-start, got {other:?}"),
    };
    assert_eq!(reason(&f.run().await), NotSentReason::StaleAuthority);
    // The holder starts the same phase again; once it is revoked the non-start cancels the phase.
    f.advance(&start, "archive").await;
    let state = f.state().await;
    f.ok(fsm::interrupt(&state, "a", &f.work.session)).await;
    f.ok(unsent(&start, "archive")).await;
    assert_eq!(
        f.action(&start, "archive").await.state,
        ActionState::Cancelled
    );
    assert_eq!(reason(&f.run().await), NotSentReason::AuthorityRevoked);
    let input = a1::input(&[]);
    f.no(
        f.bind_input(&start, "list", input),
        RejectionCode::StaleFence,
    )
    .await;
    f.h.close().await;
}

#[tokio::test]
async fn gate_started_phase_is_recorded_after_fence_ack_pg() {
    let mut f = Flow::new().await;
    let (start, _) = f.assign().await;
    f.launched().await;
    f.sealed(&start).await;
    let input = f.input(&start, "archive").await;
    f.ok(f.bind_input(&start, "archive", input)).await;
    f.ok(stop(StopMode::FinishStage)).await;
    f.ok(Command::AcknowledgeFence {
        expected_run: "run".into(),
        target: f.work.session.gate.clone(),
        control_version: 2,
    })
    .await;
    // The frozen phase reaches the gate, then the task is stopped while the gate is starting it.
    f.advance(&start, "archive").await;
    let state = f.state().await;
    f.ok(fsm::interrupt(&state, "a", &f.work.session)).await;
    f.ok(started(&start, "archive", "os-0")).await;
    let action = f.action(&start, "archive").await;
    assert_eq!(action.state, ActionState::Started);
    assert_eq!(
        action.initiation_receipt.as_deref(),
        Some("archive:7:nonce")
    );
    let finished = f.finished(&start, "archive").await;
    f.ok(finished).await;
    assert_eq!(
        f.action(&start, "archive").await.state,
        ActionState::Reconciled
    );
    f.h.close().await;
}

#[tokio::test]
async fn finish_stage_closes_attempt_whose_check_was_interrupted_pg() {
    let mut f = Flow::new().await;
    let (start, _) = f.assign().await;
    f.launched().await;
    f.sealed(&start).await;
    f.pend(&start, "archive").await;
    f.ok(started(&start, "archive", "os-0")).await;
    f.ok(stop(StopMode::FinishStage)).await;
    // The server restarted: the phase's process tree is gone and its exit status with it.
    let action = f.action(&start, "archive").await;
    let observation = TerminationObservation {
        execution_id: start.clone(),
        action_id: action.id,
        phase_id: "archive".into(),
        receipt: "archive:7:nonce".into(),
        process: process(),
        tree: TreeState::Gone,
        evidence: "no process carries the nonce".into(),
        observed_at: domain::now(),
    };
    f.ok(fact(
        &start,
        "archive",
        PhaseFact::Terminated { observation },
    ))
    .await;
    f.ok(settle(&start)).await;
    let run = f.run().await;
    assert!(matches!(
        run.verifications[&start].collection,
        Some(VerificationCollection::Interrupted { .. })
    ));
    let accept = f.accept().await;
    f.ok(accept).await;
    let state = f.state().await;
    let run = state.run.as_ref().unwrap();
    assert_eq!(
        run.attempts["work-a"].outcome,
        Some(AttemptOutcome::UserInterrupted)
    );
    assert_eq!(state.nodes["a"].disposition, NodeDisposition::Interrupted);
    f.stopped(true).await;
    f.h.close().await;
}

#[tokio::test]
async fn settle_closes_partly_cancelled_check_after_immediate_stop_pg() {
    let mut f = Flow::new().await;
    let (start, finish) = f.assign().await;
    f.launched().await;
    f.sealed(&start).await;
    f.run_phase(&start, "archive", "os-0").await;
    f.pend(&start, "list").await;
    f.ok(started(&start, "list", "os-0")).await;
    // While its holder may still run the rest, an unfinished check stays open.
    f.no(settle(&start), RejectionCode::InvalidState).await;
    f.ok(stop(StopMode::Immediate)).await;
    f.no(settle(&start), RejectionCode::InvalidState).await;
    let finished = f.finished(&start, "list").await;
    f.ok(finished).await;
    f.ok(settle(&start)).await;
    // The check that never got a sealed subject closes the same way.
    f.ok(settle(&finish)).await;
    let run = f.run().await;
    for v in [&start, &finish] {
        let stored = &run.verifications[v];
        assert!(stored.cancellation.is_some() && stored.collection.is_none());
    }
    let closed = f.state().await;
    assert_eq!(
        f.ok(settle(&start)).await.event_sequence,
        closed.event_sequence
    );
    f.stopped(true).await;
    f.h.close().await;
}

#[tokio::test]
async fn late_launch_receipt_keeps_finish_scope_pg() {
    let mut f = Flow::new().await;
    let (start, finish) = f.assign().await;
    f.ok(stop(StopMode::FinishStage)).await;
    let run = f.run().await;
    let planned = |run: &Run| {
        run.actions
            .values()
            .filter(|a| a.kind == ActionKind::VerificationPhase)
            .all(|a| a.state == ActionState::Planned)
    };
    assert!(planned(&run) && run.attempts["work-a"].outcome.is_none());
    f.launched().await;
    let run = f.run().await;
    let stop_intent = run.stop.as_ref().unwrap();
    let granted: Vec<_> = stop_intent
        .finish_grants
        .iter()
        .map(|g| g.execution_id.clone())
        .collect();
    assert_eq!(granted.len(), 2);
    assert!(granted.contains(&start) && granted.contains(&finish));
    f.sealed(&start).await;
    f.pend(&start, "archive").await;
    assert_eq!(
        f.action(&start, "archive").await.state,
        ActionState::StartPending
    );
    f.h.close().await;

    // The same stop, but the gate proves the launch never happened.
    let mut f = Flow::new().await;
    let (start, finish) = f.assign().await;
    f.ok(stop(StopMode::FinishStage)).await;
    f.ok(observe(ActionObservation::NotStarted {
        evidence: "gate refused the launch".into(),
    }))
    .await;
    let run = f.run().await;
    assert_eq!(
        run.attempts["work-a"].outcome,
        Some(AttemptOutcome::UserInterrupted)
    );
    assert!(
        run.actions
            .values()
            .all(|a| a.state == ActionState::Cancelled)
    );
    for v in [&start, &finish] {
        assert!(run.verifications[v].cancellation.is_some());
    }
    f.stopped(false).await;
    f.h.close().await;
}

#[tokio::test]
async fn seal_and_settlement_commit_evidence_and_state_together_pg() {
    let mut f = Flow::new().await;
    let (start, _) = f.assign().await;
    f.launched().await;
    let seal = f.seal_command().await;
    sql(&f.h.pool, &[REFUSE, ON_EVIDENCE_INSERT]).await;
    f.fails(seal.clone()).await;
    sql(&f.h.pool, &[HEAL_EVIDENCE]).await;
    // The admission decision is the last evidence write of a seal.
    sql(&f.h.pool, &[ON_EVIDENCE_UPDATE]).await;
    f.fails(seal.clone()).await;
    assert!(f.stored("tui:work-a:output").await.is_none());
    sql(&f.h.pool, &[HEAL_EVIDENCE]).await;
    f.ok(seal).await;
    f.ok(f.subject(&start)).await;
    f.run_check(&start).await;

    let runner = format!("run:{start}");
    sql(&f.h.pool, &[ON_EVIDENCE_UPDATE]).await;
    f.fails(settle(&start)).await;
    sql(&f.h.pool, &[HEAL_EVIDENCE]).await;
    sql(&f.h.pool, &[ON_RECEIPT_INSERT]).await;
    f.fails(settle(&start)).await;
    sql(&f.h.pool, &[HEAL_RECEIPT]).await;
    assert!(f.stored(&runner).await.is_none());
    assert!(f.run().await.verifications[&start].collection.is_none());
    f.ok(settle(&start)).await;
    assert!(f.stored(&runner).await.is_some());
    assert!(f.run().await.verifications[&start].collection.is_some());
    f.h.close().await;
}
