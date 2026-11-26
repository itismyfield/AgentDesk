#[cfg(unix)]
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
#[cfg(unix)]
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::campaign_execution::control::tests as domain;
#[cfg(unix)]
use crate::services::campaign_execution::stage_fsm::tests as fsm;

fn identity(boot: u128) -> HostGate {
    host_identity::from_observation(
        "local",
        "campaign",
        "local\n",
        &uuid::Uuid::from_u128(boot).to_string(),
    )
    .unwrap()
}

#[test]
fn a_node_has_one_fixed_host_worker_across_process_restarts_and_boots() {
    let tmp = tempfile::tempdir().unwrap();
    let resources = AdkResources::new(tmp.path(), tmp.path()).unwrap();
    let open = |host| {
        HostWorker::open_observed(
            tmp.path(),
            "campaign",
            "a",
            host,
            resources.clone(),
            domain::now(),
        )
    };
    let worker = open(identity(1)).unwrap();
    assert!(matches!(
        open(identity(1)),
        Err(WorkerError::Gate(GateError::Busy))
    ));
    assert!(matches!(
        open(identity(2)),
        Err(WorkerError::Gate(GateError::Busy))
    ));
    drop(worker);

    let mut foreign = identity(2);
    foreign.host = "other".into();
    assert!(matches!(
        open(foreign),
        Err(WorkerError::Gate(GateError::WrongHost(_)))
    ));
    let worker = open(identity(2)).unwrap();
    assert_eq!(worker.identity(), &identity(2));
    assert!(!host_identity::previous_boot(
        &identity(2),
        worker.identity()
    ));
    assert!(host_identity::previous_boot(
        &identity(1),
        worker.identity()
    ));
    let mut legacy = identity(1);
    legacy.incarnation = "process-1234".into();
    assert!(!host_identity::previous_boot(&legacy, worker.identity()));
    assert!(host_identity::from_observation("other", "campaign", "local", "boot-2").is_err());
}

#[cfg(unix)]
async fn send(pool: &PgPool, key: &str, command: Command) {
    let reply = commands::execute(
        pool,
        "campaign",
        &CommandEnvelope {
            idempotency_key: key.into(),
            observed_at: domain::now(),
            command,
        },
        render_receipt,
    )
    .await
    .unwrap();
    assert!(
        matches!(reply.outcome, CommandOutcome::Decided(Ok(_))),
        "{:?}",
        reply.outcome
    );
}

#[cfg(unix)]
async fn create_run(pool: &PgPool) {
    let reply = commands::create(
        pool,
        "campaign",
        &domain::input(),
        "create",
        domain::now(),
        render_receipt,
    )
    .await
    .unwrap();
    assert!(matches!(reply.outcome, CommandOutcome::Decided(Ok(_))));
    send(
        pool,
        "start",
        Command::BeginRun {
            expected_revision: 1,
            run_id: "run".into(),
        },
    )
    .await;
}

#[cfg(unix)]
async fn pend_launch(pool: &PgPool, worker: &mut HostWorker) -> (AttemptBinding, String) {
    let turn = worker.poll(pool, domain::now()).await.unwrap().turn;
    let WorkerTurn::Next(NextAction::AssignWork { key }) = turn else {
        panic!("unexpected {turn:?}")
    };
    let mut binding = fsm::work("a", &key.attempt_id(Stage::Implementation));
    binding.session.gate = worker.identity().clone();
    let mut assignment = fsm::assignment(&binding);
    assignment.gate = worker.identity().clone();
    let Command::AssignWork { mut launch, .. } = fsm::assign(&binding, assignment.clone()) else {
        unreachable!()
    };
    let action_id = key.launch_id(Stage::Implementation);
    launch.id = action_id.clone();
    worker
        .advance(
            pool,
            Command::AssignWork {
                binding: Box::new(binding.clone()),
                assignment: Box::new(assignment),
                launch,
            },
            domain::now(),
        )
        .await
        .unwrap();
    for (name, next) in [
        ("claim", ActionState::Claimed),
        ("pend", ActionState::StartPending),
    ] {
        send(
            pool,
            name,
            Command::AdvanceAction {
                action_id: action_id.clone(),
                binding: Box::new(binding.clone()),
                state: next,
            },
        )
        .await;
    }
    (binding, action_id)
}

#[cfg(unix)]
fn fake_resources(root: &Path) -> AdkResources {
    use std::os::unix::fs::PermissionsExt;

    let dir = root.join("scripts/cargo-shim");
    std::fs::create_dir_all(&dir).unwrap();
    let shim = dir.join("cargo");
    std::fs::write(&shim, "# not executed by these tests\n").unwrap();
    std::fs::set_permissions(shim, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        root.join("scripts/wait_headroom.sh"),
        "printf 'HEADROOM_SUSTAINED ok=1 at 00:00:00\\n'",
    )
    .unwrap();
    AdkResources::new(root, root).unwrap()
}

#[cfg(unix)]
#[tokio::test]
async fn the_worker_replays_a_durable_launch_after_a_lost_response_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    create_run(&pool).await;
    let tmp = tempfile::tempdir().unwrap();
    let resources = fake_resources(tmp.path());
    let open = |boot| {
        HostWorker::open_observed(
            tmp.path(),
            "campaign",
            "a",
            identity(boot),
            resources.clone(),
            domain::now(),
        )
        .unwrap()
    };
    let mut worker = open(1);
    let (binding, action_id) = pend_launch(&pool, &mut worker).await;
    let launches = AtomicU32::new(0);
    let launch = |_: &str| {
        launches.fetch_add(1, Ordering::SeqCst);
        Ok(Initiated::Started {
            receipt: "durable-receipt".into(),
            process: None,
            handle: (),
        })
    };
    let first = worker
        .start_action(
            &pool,
            &action_id,
            "payload".into(),
            Duration::from_secs(3),
            domain::now(),
            launch,
        )
        .await
        .unwrap();
    assert_eq!(first.handle, Some(()));
    drop(worker);

    let mut worker = open(1);
    let poll = worker.poll(&pool, domain::now()).await.unwrap();
    assert_eq!(
        poll.turn,
        WorkerTurn::Reconcile {
            action_id: action_id.clone()
        }
    );
    let second = worker
        .start_action(
            &pool,
            &action_id,
            "payload".into(),
            Duration::from_secs(3),
            domain::now(),
            launch,
        )
        .await
        .unwrap();
    assert_eq!(second.initiation, first.initiation);
    assert!(second.handle.is_none());
    assert_eq!(launches.load(Ordering::SeqCst), 1);
    send(
        &pool,
        "observed-start",
        Command::ObserveAction {
            action_id,
            binding: Box::new(binding.clone()),
            observation: ActionObservation::Started {
                receipt: "durable-receipt".into(),
            },
        },
    )
    .await;
    assert_eq!(
        worker.poll(&pool, domain::now()).await.unwrap().turn,
        WorkerTurn::Next(NextAction::AwaitAttempt {
            attempt_id: binding.attempt_id
        })
    );
    pool.close().await;
    db.drop().await;
}

#[cfg(unix)]
#[tokio::test]
async fn a_stop_committed_while_the_worker_waits_for_the_campaign_row_blocks_initiation_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    create_run(&pool).await;
    let tmp = tempfile::tempdir().unwrap();
    let mut worker = HostWorker::open_observed(
        tmp.path(),
        "campaign",
        "a",
        identity(1),
        fake_resources(tmp.path()),
        domain::now(),
    )
    .unwrap();
    let (_, action_id) = pend_launch(&pool, &mut worker).await;
    let mut stop_tx = pool.begin().await.unwrap();
    assert!(
        store::get_for_worker(&mut stop_tx, "campaign")
            .await
            .unwrap()
            .is_some()
    );
    let launches = AtomicU32::new(0);
    let start = worker.start_action(
        &pool,
        &action_id,
        "payload".into(),
        Duration::from_secs(4),
        domain::now(),
        |_| {
            launches.fetch_add(1, Ordering::SeqCst);
            Ok(Initiated::Started {
                receipt: "forbidden".into(),
                process: None,
                handle: (),
            })
        },
    );
    tokio::pin!(start);
    let arrived = async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname = current_database() \
                 AND wait_event_type = 'Lock' AND query LIKE '%server_campaign_keys%FOR UPDATE%')",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if waiting {
                return;
            }
            tokio::task::yield_now().await;
        }
    };
    tokio::select! {
        _ = &mut start => panic!("the worker returned before it obtained the campaign lock"),
        ready = tokio::time::timeout(Duration::from_secs(8), arrived) => ready.unwrap(),
    }
    let (stop_tx, reply) = commands::apply_in(
        stop_tx,
        "campaign",
        "stop",
        domain::now(),
        StoreRequest::Command(Command::RequestStop {
            expected_revision: 1,
            expected_run: "run".into(),
            mode: StopMode::Immediate,
        }),
        render_receipt,
    )
    .await
    .unwrap();
    assert!(matches!(reply.outcome, CommandOutcome::Decided(Ok(_))));
    stop_tx.commit().await.unwrap();
    assert!(matches!(start.await, Err(WorkerError::Rejected(_))));
    assert_eq!(launches.load(Ordering::SeqCst), 0);
    pool.close().await;
    db.drop().await;
}

#[cfg(unix)]
#[tokio::test]
async fn m4_regression_a_late_launch_keeps_the_finish_fence_at_the_same_version_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    create_run(&pool).await;
    let tmp = tempfile::tempdir().unwrap();
    let mut worker = HostWorker::open_observed(
        tmp.path(),
        "campaign",
        "a",
        identity(1),
        fake_resources(tmp.path()),
        domain::now(),
    )
    .unwrap();
    let (binding, action_id) = pend_launch(&pool, &mut worker).await;
    let launch = worker
        .start_action(
            &pool,
            &action_id,
            "payload".into(),
            Duration::from_secs(4),
            domain::now(),
            |_| {
                Ok(Initiated::Started {
                    receipt: "late-receipt".into(),
                    process: None,
                    handle: (),
                })
            },
        )
        .await
        .unwrap();
    assert_eq!(launch.handle, Some(()));
    send(
        &pool,
        "finish-stop",
        Command::RequestStop {
            expected_revision: 1,
            expected_run: "run".into(),
            mode: StopMode::FinishStage,
        },
    )
    .await;
    let first = worker.poll(&pool, domain::now()).await.unwrap();
    assert!(
        matches!(&first.turn, WorkerTurn::Reconcile { action_id: pending } if pending == &action_id)
    );
    let version = first.fence.unwrap().acknowledgement.control_version;
    let stopped = store::get(&pool, "campaign").await.unwrap().unwrap();
    let run = stopped.run.as_ref().unwrap();
    assert!(run.stop.as_ref().unwrap().finish_grants.is_empty());
    let execution = fsm::execution(&stopped, &binding.attempt_id);
    let verification_id = &execution.start_executions[0];
    let v = &run.verifications[verification_id];
    assert!(super::super::stage_fsm::validate_verification_start(&stopped, v, &binding).is_err());

    send(
        &pool,
        "late-start",
        Command::ObserveAction {
            action_id: action_id.clone(),
            binding: Box::new(binding.clone()),
            observation: ActionObservation::Started {
                receipt: "late-receipt".into(),
            },
        },
    )
    .await;
    let after = worker.poll(&pool, domain::now()).await.unwrap();
    assert_eq!(
        after.fence.unwrap().acknowledgement.control_version,
        version
    );
    let state = store::get(&pool, "campaign").await.unwrap().unwrap();
    let run = state.run.as_ref().unwrap();
    let grants = &run.stop.as_ref().unwrap().finish_grants;
    assert_eq!(
        run.actions[&action_id].initiation_receipt.as_deref(),
        Some("late-receipt")
    );
    assert_eq!(
        run.actions
            .values()
            .filter(|a| a.kind == ActionKind::Launch)
            .count(),
        1
    );
    assert!(
        grants.iter().any(|g| g.execution_id == *verification_id
            && g.phase_actions.contains(&v.phases[0].action_id))
    );
    send(
        &pool,
        "late-session-alive",
        Command::ObserveSession {
            identity: binding.session.clone(),
            observation: SessionObservation::Alive {
                observed_at: domain::now(),
            },
        },
    )
    .await;
    let observed = store::get(&pool, "campaign").await.unwrap().unwrap();
    let observed_v = &observed.run.as_ref().unwrap().verifications[verification_id];
    assert!(matches!(
        super::super::stage_fsm::validate_verification_start(&observed, observed_v, &binding)
            .unwrap()
            .source,
        crate::domain::campaign::verification_run::AuthoritySource::FinishPlan { .. }
    ));
    pool.close().await;
    db.drop().await;
}

#[cfg(unix)]
#[tokio::test]
async fn a_retained_phase_result_is_committed_once_and_then_acknowledged_pg() {
    use crate::domain::campaign::evidence::{EvidenceEnvelope, TuiPayload, TuiSubmission};
    use crate::domain::campaign::verification_run::{PhaseOutput, ProcessIdentity};
    use crate::services::campaign_execution::{
        evidence_gate::tests::{REPO, workspace},
        verification_run::tests as a1,
    };

    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    create_run(&pool).await;
    let tmp = tempfile::tempdir().unwrap();
    let resources = fake_resources(tmp.path());
    let open = |boot| {
        HostWorker::open_observed(
            tmp.path(),
            "campaign",
            "a",
            identity(boot),
            resources.clone(),
            domain::now(),
        )
        .unwrap()
    };
    let mut worker = open(1);
    let (work, action_id) = pend_launch(&pool, &mut worker).await;
    send(
        &pool,
        "launch-observed",
        Command::ObserveAction {
            action_id,
            binding: Box::new(work.clone()),
            observation: ActionObservation::Started {
                receipt: "launch-receipt".into(),
            },
        },
    )
    .await;
    send(
        &pool,
        "session-alive",
        Command::ObserveSession {
            identity: work.session.clone(),
            observation: SessionObservation::Alive {
                observed_at: domain::now(),
            },
        },
    )
    .await;

    let state = store::get(&pool, "campaign").await.unwrap().unwrap();
    let execution = fsm::execution(&state, &work.attempt_id);
    let execution_id = execution.start_executions[0].clone();
    let claim = EvidenceEnvelope::from_tui(
        TuiSubmission {
            evidence_id: "output".into(),
            payload: TuiPayload::OutputClaim {
                output_sha: "h1".into(),
            },
        },
        &work,
        &execution.pin_hash,
        domain::now(),
    )
    .unwrap();
    let mut checkout = workspace("h1");
    checkout.host = worker.identity().host.clone();
    send(
        &pool,
        "seal",
        Command::SealCandidate {
            claim: Box::new(claim),
            repository_identity: REPO.into(),
            workspace: Box::new(checkout),
        },
    )
    .await;
    send(
        &pool,
        "subject",
        Command::BindVerificationSubject {
            execution_id: execution_id.clone(),
            consumer: Box::new(work.clone()),
        },
    )
    .await;
    let phase = "archive";
    send(
        &pool,
        "input",
        Command::BindVerificationInput {
            execution_id: execution_id.clone(),
            phase_id: phase.into(),
            consumer: Box::new(work.clone()),
            input: Box::new(a1::input(&[])),
        },
    )
    .await;
    for (key, action) in [
        ("phase-claim", ActionState::Claimed),
        ("phase-pending", ActionState::StartPending),
    ] {
        send(
            &pool,
            key,
            fsm::advance(&execution_id, phase, &work, action),
        )
        .await;
    }

    let state = store::get(&pool, "campaign").await.unwrap().unwrap();
    let launch = phase_launch(&state, &execution_id, phase, &work).unwrap();
    let mut result = a1::result(
        phase,
        0,
        PhaseOutput::Archive {
            digest: "archive-digest".into(),
        },
    );
    result.receipt = format!("{phase}:7:{}", launch.envelope().execution_nonce);
    let process = ProcessIdentity {
        host: worker.identity().host.clone(),
        pid: 7,
        process_group: Some(7),
        started_at: a1::at(),
        nonce: launch.envelope().execution_nonce.clone(),
    };
    worker.apply_fences(&state).unwrap();
    worker
        .gate
        .admit_and_initiate(launch.envelope(), domain::now(), |_| {
            Ok(Initiated::Started {
                receipt: result.receipt.clone(),
                process: Some(process.clone()),
                handle: (),
            })
        })
        .unwrap();
    let mut environment = a1::environment();
    environment.checkout.host = worker.identity().host.clone();
    send(
        &pool,
        "phase-observed",
        Command::RecordVerificationPhase {
            execution_id: execution_id.clone(),
            phase_id: phase.into(),
            fact: Box::new(PhaseFact::Started {
                receipt: result.receipt.clone(),
                process,
                environment: Box::new(environment),
            }),
        },
    )
    .await;
    let phase_action = &launch.envelope().action_id;
    drop(worker);
    let mut worker = open(2);
    let observed = worker.poll(&pool, domain::now()).await.unwrap();
    assert!(
        matches!(&observed.recovered[..], [Recovered::Running { action_id, tree: TreeState::Gone, .. }] if action_id == phase_action)
    );
    let still_open = store::get(&pool, "campaign").await.unwrap().unwrap();
    assert!(
        still_open.run.as_ref().unwrap().verifications[&execution_id]
            .phase(phase)
            .unwrap()
            .result
            .is_none()
    );

    worker
        .finish_phase(phase_action, &result, domain::now())
        .unwrap();
    drop(worker);

    let scope = blake3::hash(b"campaign\na").to_hex().to_string();
    let name = format!("{}.json", blake3::hash(phase_action.as_bytes()).to_hex());
    let path = tmp.path().join(scope).join("entries").join(name);
    let mut old: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(old.as_object_mut().unwrap().remove("delivered").is_some());
    std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();

    let mut worker = open(2);
    let poll = worker.poll(&pool, domain::now()).await.unwrap();
    assert!(poll.recovered.is_empty());
    assert_eq!(
        poll.turn,
        WorkerTurn::Replayed {
            action_id: phase_action.clone()
        }
    );
    assert!(worker.gate.entry(phase_action).unwrap().delivered);
    let saved = store::get(&pool, "campaign").await.unwrap().unwrap();
    assert_eq!(
        saved.run.as_ref().unwrap().verifications[&execution_id]
            .phase(phase)
            .unwrap()
            .result
            .as_ref(),
        Some(&result)
    );
    drop(worker);
    assert!(
        open(2)
            .poll(&pool, domain::now())
            .await
            .unwrap()
            .recovered
            .is_empty()
    );
    assert_eq!(store::get(&pool, "campaign").await.unwrap().unwrap(), saved);
    pool.close().await;
    db.drop().await;
}
