use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};

use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::server_campaigns::{
    CommandOutcome, ReceiptResponse,
    command_transaction::{self, Preparation, StoreRequest},
};
use crate::domain::campaign::{
    candidate_seal::CandidateSeal,
    decision_item::{ApprovalTarget, ForumApproval},
    deployment::{AcceptanceCheck, AcceptanceFact},
    lifecycle::NextAction,
};
use crate::services::campaign_execution::{
    adk_observe::tests::{commit, sh_git},
    adk_resources::AdkResources,
    adk_workspace::observe_checkout,
    commands,
    deploy_observe::HealthReply,
    deploy_policy::tests::landed_on,
    final_acceptance,
    host_worker::WorkerTurn,
};

struct Executor {
    main: Mutex<String>,
    running: Mutex<Option<String>>,
    starts: AtomicUsize,
}

impl DeployExecutor for Executor {
    fn plan(&self, _: &Path) -> io::Result<DeployPlan> {
        Ok(DeployPlan {
            main_sha: self.main.lock().unwrap().clone(),
            hosts: vec![DeployHost {
                host: "mac-mini".into(),
                previous_incarnation: "boot-before".into(),
                expected_binary_digest: None,
            }],
        })
    }

    // The deployment starts, but its reply is lost on the way back to the gate.
    fn start_exact(&self, watch: &DeploymentWatch, nonce: &str) -> io::Result<String> {
        assert!(!nonce.is_empty());
        assert_eq!(watch.targets[0].expected_sha, *self.main.lock().unwrap());
        self.starts.fetch_add(1, Ordering::SeqCst);
        Err(io::Error::other("lost start reply"))
    }
}

impl DeploymentHealth for Executor {
    fn detail<'a>(&'a self, host: &'a str) -> HealthReply<'a> {
        Box::pin(async move {
            let Some(source) = self.running.lock().unwrap().clone() else {
                return Err("the host is restarting".to_owned());
            };
            tokio::task::yield_now().await;
            let now = Utc::now();
            let health: Value = json!({
                "boot_incarnation": "boot-after",
                "release_source": {"node_hostname": host, "deployed_repo_head": source},
                "running_binary": {
                    "source_sha": source,
                    "target": "aarch64-apple-darwin",
                    "observed_at": now,
                    "boot_started_at_ms": now.timestamp_millis() - 1,
                }
            });
            Ok(health)
        })
    }
}

// Node `a` landed on main in a throwaway campaign store, with a real checkout whose main holds
// the merge, a later commit and an unrelated side commit.
struct Bench {
    db: TestPostgresDb,
    pool: PgPool,
    temp: tempfile::TempDir,
    hostname: String,
    executor: Executor,
    merge: String,
    later: String,
    unrelated: String,
}

impl Bench {
    fn open(&self) -> HostWorker {
        let root = self.temp.path();
        HostWorker::open(
            root,
            "campaign",
            "a",
            &self.hostname,
            "campaign",
            AdkResources::new(root, root).unwrap(),
            Utc::now(),
        )
        .unwrap()
    }

    async fn seed(deploy_gate: bool) -> Self {
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let temp = tempfile::tempdir().unwrap();
        let checkout = temp.path().join("clone");
        std::fs::create_dir(&checkout).unwrap();
        for args in [
            &["init", "-q", "--initial-branch=main"][..],
            &["config", "user.name", "Fixture"],
            &["config", "user.email", "fixture@example.com"],
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/owner/project.git",
            ],
        ] {
            sh_git(&checkout, args);
        }
        let base = commit(&checkout, "base.txt", "base");
        let merge = commit(&checkout, "feature.txt", "feature");
        let later = commit(&checkout, "later.txt", "later");
        sh_git(&checkout, &["checkout", "-q", "-b", "side", &base]);
        let unrelated = commit(&checkout, "side.txt", "side");
        let hostname = std::process::Command::new("/bin/hostname")
            .output()
            .unwrap();
        let hostname = String::from_utf8(hostname.stdout)
            .unwrap()
            .trim()
            .to_owned();
        let bench = Self {
            db,
            pool,
            temp,
            hostname,
            executor: Executor {
                main: Mutex::new(merge.clone()),
                running: Mutex::new(None),
                starts: AtomicUsize::new(0),
            },
            merge,
            later,
            unrelated,
        };
        let host = bench.open().identity().clone();

        let render = |outcome: &CommandOutcome| {
            Ok(ReceiptResponse {
                status: 200,
                body: serde_json::to_value(outcome)?,
            })
        };
        let mut state = landed_on(&host, &base, &bench.merge, &bench.merge, deploy_gate);
        let input = state.definition.input().clone();
        commands::create(
            &bench.pool,
            "campaign",
            &input,
            "create",
            Utc::now(),
            render,
        )
        .await
        .unwrap();
        let tx = bench.pool.begin().await.unwrap();
        let Preparation::Fresh(prepared) = command_transaction::prepare(
            tx,
            "campaign",
            "seed-landed",
            Utc::now(),
            StoreRequest::Command(Command::ConfirmStopped),
        )
        .await
        .unwrap() else {
            panic!("fixture command was already present");
        };
        state.event_sequence = prepared.previous().unwrap().event_sequence + 1;
        let run = state.run.as_mut().unwrap();
        let work = run.attempts["attempt"].binding.clone();
        run.seals.insert(
            work.attempt_id.clone(),
            CandidateSeal {
                seal_id: "seal:attempt".into(),
                owner_attempt: work.attempt_id.clone(),
                binding_digest: digest_of(&work).unwrap(),
                repository: Repository::Adk,
                base_sha: base,
                output_sha: bench.merge.clone(),
                claim_id: "claim".into(),
                pin_hash: "pin".into(),
                sealed_at: Utc::now(),
                workspace: observe_checkout(
                    Repository::Adk,
                    &bench.hostname,
                    &checkout,
                    Utc::now(),
                )
                .unwrap(),
            },
        );
        let decision = Decision {
            events: vec![Event {
                sequence: state.event_sequence,
                run_id: Some(run.id.clone()),
                command_id: "seed-landed".into(),
                observed_at: Utc::now(),
                kind: EventKind::NodeAdvanced {
                    node_id: "a".into(),
                    stage: Stage::MainVerification,
                },
            }],
            result: CommandResult {
                definition_revision: state.definition_revision,
                run_id: Some(run.id.clone()),
                control_version: Some(run.control_version),
                event_sequence: state.event_sequence,
                control: state.control,
            },
            state,
            records: Vec::new(),
        };
        let staged = prepared.stage(Some(Ok(decision))).await.unwrap();
        let response = render(staged.outcome()).unwrap();
        let (tx, _) = staged.record_receipt(response).await.unwrap();
        tx.commit().await.unwrap();
        bench
    }

    async fn step(&self, worker: &mut HostWorker) -> DeployProgress {
        let timeout = Duration::from_secs(1);
        step_deploy(worker, &self.pool, &self.executor, &self.executor, timeout)
            .await
            .unwrap()
    }

    async fn commit(&self, worker: &HostWorker, command: Command) {
        let (tx, state) = worker.locked(&self.pool).await.unwrap();
        worker
            .commit(tx, state.event_sequence, command, Utc::now())
            .await
            .unwrap();
    }

    async fn approve(&self, worker: &HostWorker, binding: Box<ApprovalBinding>) {
        let evidence = Box::new(ForumApproval {
            decision_ref: "test forum".into(),
            response_ref: "https://example.com/answer".into(),
            response_at: Utc::now(),
        });
        self.commit(worker, Command::ApproveGate { binding, evidence })
            .await;
    }

    fn starts(&self) -> usize {
        self.executor.starts.load(Ordering::SeqCst)
    }

    async fn close(self) {
        self.pool.close().await;
        self.db.drop().await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn m8_fixed_worker_starts_the_approved_head_once_and_completes_on_real_evidence_pg() {
    let bench = Bench::seed(true).await;
    let (merge, later) = (bench.merge.clone(), bench.later.clone());
    let mut worker = bench.open();

    // A main that does not contain the node's merge is never requested.
    *bench.executor.main.lock().unwrap() = bench.unrelated.clone();
    assert_eq!(bench.step(&mut worker).await, DeployProgress::Waiting);
    let (tx, state) = worker.locked(&bench.pool).await.unwrap();
    assert!(state.run.unwrap().deployments.is_empty());
    drop(tx);
    *bench.executor.main.lock().unwrap() = merge.clone();
    assert_eq!(bench.step(&mut worker).await, DeployProgress::Waiting);
    let DeployProgress::AwaitApproval(first) = bench.step(&mut worker).await else {
        panic!("missing Deploy gate");
    };
    assert_eq!(first.target, ApprovalTarget::Main { sha: merge });
    assert_eq!(
        bench.step(&mut worker).await,
        DeployProgress::AwaitApproval(first.clone())
    );
    bench.approve(&worker, first).await;

    // Main moves after the approval: the old approval starts nothing for the new head.
    *bench.executor.main.lock().unwrap() = later.clone();
    assert_eq!(bench.step(&mut worker).await, DeployProgress::Waiting);
    let DeployProgress::AwaitApproval(second) = bench.step(&mut worker).await else {
        panic!("the approval of another head was reused");
    };
    assert_eq!(second.target, ApprovalTarget::Main { sha: later.clone() });
    assert_eq!(bench.starts(), 0);
    bench.approve(&worker, second).await;

    assert_eq!(bench.step(&mut worker).await, DeployProgress::Observing);
    assert_eq!(bench.starts(), 1);
    let (tx, state) = worker.locked(&bench.pool).await.unwrap();
    let run = state.run.as_ref().unwrap();
    let action_id = run.deployments["a"].action_id.clone();
    assert_eq!(run.actions[&action_id].state, ActionState::Unknown);
    assert!(final_acceptance::unmet(&state, "a").unwrap().is_some());
    drop(tx);

    // The restarted worker reconciles the lost start and never starts it again.
    drop(worker);
    let mut worker = bench.open();
    for _ in 0..2 {
        assert_eq!(bench.step(&mut worker).await, DeployProgress::Observing);
    }
    assert!(!final_acceptance::judge(&worker, &bench.pool).await.unwrap());
    // Another source on the host confirms nothing.
    *bench.executor.running.lock().unwrap() = Some(bench.merge.clone());
    assert_eq!(bench.step(&mut worker).await, DeployProgress::Observing);
    *bench.executor.running.lock().unwrap() = Some(later.clone());
    assert_eq!(bench.step(&mut worker).await, DeployProgress::Confirmed);
    drop(worker);
    let mut worker = bench.open();
    assert_eq!(bench.step(&mut worker).await, DeployProgress::Confirmed);
    assert_eq!(bench.starts(), 1);
    assert_eq!(worker.effect_entry(&action_id).unwrap().attempts, 1);

    assert!(!final_acceptance::judge(&worker, &bench.pool).await.unwrap());
    let check = AcceptanceFact::Checked(AcceptanceCheck {
        condition_id: "ac-live".into(),
        reference: "https://example.com/live-check".into(),
        deployed_sha: later,
        observed_at: Utc::now(),
    });
    let command = Command::RecordAcceptance {
        node_id: "a".into(),
        fact: Box::new(check),
    };
    bench.commit(&worker, command).await;
    assert!(final_acceptance::judge(&worker, &bench.pool).await.unwrap());
    let WorkerTurn::Next(NextAction::CompleteNode { key }) =
        worker.poll(&bench.pool, Utc::now()).await.unwrap().turn
    else {
        panic!("the judged node is not ready to complete");
    };
    let complete = Command::CompleteNode {
        expected_run: key.run_id,
        node_id: key.node_id,
        expected_epoch: key.epoch,
    };
    worker
        .advance(&bench.pool, complete, Utc::now())
        .await
        .unwrap();
    let (tx, state) = worker.locked(&bench.pool).await.unwrap();
    assert_eq!(state.nodes["a"].stage, Stage::Complete);
    assert_eq!(
        state.run.unwrap().actions[&action_id].state,
        ActionState::Reconciled
    );
    drop(tx);
    drop(worker);
    bench.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn m8_a_start_journaled_before_a_crash_is_reconciled_and_not_repeated_pg() {
    let bench = Bench::seed(false).await;
    let mut worker = bench.open();
    assert_eq!(bench.step(&mut worker).await, DeployProgress::Waiting);

    // The worker dies after the final gate started the executor and before the campaign record.
    let (tx, state) = worker.locked(&bench.pool).await.unwrap();
    let landed = deploy_policy::landed(&state, "a").unwrap();
    let job = &landed.run.deployments["a"];
    let watch = watch_of(job);
    let effect = envelope(landed.run, job, landed.opened, &watch).unwrap();
    let admitted = worker
        .admit_deploy(&state, &effect, Utc::now(), |nonce| {
            let receipt = bench.executor.start_exact(&watch, nonce)?;
            Ok(Initiated::Started {
                receipt,
                process: None,
                handle: (),
            })
        })
        .unwrap();
    assert!(matches!(
        admitted.initiation,
        Initiation::Indeterminate { .. }
    ));
    drop(tx);
    drop(worker);

    // Main has moved meanwhile; the journaled start still decides what is reconciled.
    *bench.executor.main.lock().unwrap() = bench.later.clone();
    let mut worker = bench.open();
    assert_eq!(bench.step(&mut worker).await, DeployProgress::Observing);
    let (tx, state) = worker.locked(&bench.pool).await.unwrap();
    let job = &state.run.as_ref().unwrap().deployments["a"];
    assert_eq!(job.intent.target_sha, bench.merge);
    assert!(matches!(job.status, DeployStatus::Indeterminate { .. }));
    drop(tx);
    assert_eq!(bench.step(&mut worker).await, DeployProgress::Observing);
    *bench.executor.running.lock().unwrap() = Some(bench.merge.clone());
    assert_eq!(bench.step(&mut worker).await, DeployProgress::Confirmed);
    assert_eq!(bench.starts(), 1);
    drop(worker);
    bench.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn m8_a_stop_before_the_start_cancels_the_deployment_without_calling_the_executor_pg() {
    for mode in [StopMode::Immediate, StopMode::FinishStage] {
        let bench = Bench::seed(false).await;
        let mut worker = bench.open();
        assert_eq!(bench.step(&mut worker).await, DeployProgress::Waiting);
        let stop = Command::RequestStop {
            expected_revision: 1,
            expected_run: "run".into(),
            mode,
        };
        bench.commit(&worker, stop).await;
        for _ in 0..2 {
            assert_eq!(bench.step(&mut worker).await, DeployProgress::Failed);
        }
        assert_eq!(bench.starts(), 0);
        let (tx, state) = worker.locked(&bench.pool).await.unwrap();
        let run = state.run.unwrap();
        let job = &run.deployments["a"];
        assert!(worker.effect_entry(&job.action_id).is_none());
        assert_eq!(run.actions[&job.action_id].state, ActionState::Cancelled);
        drop(tx);
        drop(worker);
        bench.close().await;
    }
}
