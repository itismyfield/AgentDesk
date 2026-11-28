#![cfg(unix)]

use std::sync::Mutex;

use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::server_campaigns::{
    CommandOutcome, ReceiptResponse,
    command_transaction::{self, Preparation, StoreRequest},
};
use crate::services::campaign_execution::{
    adk_resources::AdkResources,
    adk_workspace::observe_checkout,
    candidate_seal::CandidateSeal,
    commands,
    control::test_support as domain,
    landing_prepare::tests::{fixture_checkout, tools},
    lifecycle,
};

struct FixtureRemote {
    ci: RemoteCi,
    race: bool,
    state: Mutex<(Option<OpenedPr>, Option<String>, usize, usize)>,
}

impl LandingRemote for FixtureRemote {
    fn observe(&self, _: &LandingTarget) -> io::Result<RemoteLanding> {
        let state = self.state.lock().unwrap();
        Ok(RemoteLanding {
            pull_request: state.0.clone(),
            branch_head: state.1.clone(),
            ci: self.ci,
        })
    }

    fn push_new_if_idle(&self, target: &LandingTarget, _: &Path, nonce: &str) -> io::Result<bool> {
        assert!(!nonce.is_empty());
        assert_eq!(self.ci, RemoteCi::Idle);
        if self.race {
            return Ok(false);
        }
        let mut state = self.state.lock().unwrap();
        assert!(state.1.is_none());
        state.1 = Some(target.head_sha.clone());
        state.2 += 1;
        Ok(true)
    }

    fn open_once(&self, target: &LandingTarget, nonce: &str) -> io::Result<OpenedPr> {
        assert!(!nonce.is_empty());
        let mut state = self.state.lock().unwrap();
        assert!(state.0.is_none());
        assert_eq!(state.1.as_ref(), Some(&target.head_sha));
        let pr = OpenedPr {
            repository: target.repository.clone(),
            number: 42,
            head_sha: target.head_sha.clone(),
            head_ref: target.head_ref.clone(),
            base_ref: target.base_ref.clone(),
        };
        state.0 = Some(pr.clone());
        state.3 += 1;
        Ok(pr)
    }
}

async fn seed(pool: &PgPool, checkout: &Path, target: &LandingTarget, host: &HostGate) {
    let render = |outcome: &CommandOutcome| {
        Ok(ReceiptResponse {
            status: 200,
            body: serde_json::to_value(outcome)?,
        })
    };
    let mut state = domain::running();
    commands::create(
        pool,
        &state.id,
        &domain::input(),
        "create",
        Utc::now(),
        render,
    )
    .await
    .unwrap();
    let tx = pool.begin().await.unwrap();
    let Preparation::Fresh(prepared) = command_transaction::prepare(
        tx,
        &state.id,
        "fixture-review",
        Utc::now(),
        StoreRequest::Command(Command::ConfirmStopped),
    )
    .await
    .unwrap() else {
        panic!("missing pending fixture")
    };
    state.event_sequence = prepared.previous().unwrap().event_sequence + 1;
    let run = state.run.as_mut().unwrap();
    let attempt = run.attempts.get_mut("attempt").unwrap();
    attempt.outcome = Some(AttemptOutcome::Passed);
    let seal = CandidateSeal {
        seal_id: "seal:attempt".into(),
        owner_attempt: "attempt".into(),
        binding_digest: digest_of(&attempt.binding).unwrap(),
        repository: Repository::Adk,
        base_sha: attempt.binding.base_sha.clone(),
        output_sha: target.head_sha.clone(),
        claim_id: "fixture-claim".into(),
        pin_hash: target.pin_hash.clone(),
        sealed_at: Utc::now(),
        workspace: observe_checkout(Repository::Adk, &host.host, checkout, Utc::now()).unwrap(),
    };
    run.fences.entry("a".into()).or_default().candidate = Some(lifecycle::reviewed(&seal));
    run.seals.insert("attempt".into(), seal);
    let node = state.nodes.get_mut("a").unwrap();
    node.stage = Stage::Landing;
    node.disposition = NodeDisposition::Pending;
    node.current_attempt = None;
    let decision = Decision {
        events: vec![Event {
            sequence: state.event_sequence,
            run_id: Some(run.id.clone()),
            command_id: "fixture-review".into(),
            observed_at: Utc::now(),
            kind: EventKind::NodeAdvanced {
                node_id: "a".into(),
                stage: Stage::Landing,
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
}

#[tokio::test]
async fn m6_fixed_worker_waits_for_ci_and_replays_one_exact_pr_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let checkout = root.join("repo");
    std::fs::create_dir(&checkout).unwrap();
    let target = fixture_checkout(&checkout);
    let scripts = root.join("scripts");
    let checks = tools(&scripts, "SOURCE_SCAN no module reads a changed file");
    std::fs::write(
        scripts.join("wait_headroom.sh"),
        "printf 'HEADROOM_SUSTAINED ok=1 at 00:00:00\\n'\n",
    )
    .unwrap();
    let name = std::process::Command::new("/bin/hostname")
        .output()
        .unwrap();
    let host = String::from_utf8(name.stdout).unwrap();
    let open = || {
        HostWorker::open(
            root,
            "campaign",
            "a",
            host.trim(),
            "campaign",
            AdkResources::new(root, root).unwrap(),
            Utc::now(),
        )
        .unwrap()
    };
    let mut worker = open();
    seed(&pool, &checkout, &target, worker.identity()).await;
    let deadline = Utc::now() + chrono::TimeDelta::minutes(2);
    assert!(prepare_landing(&worker, &pool, deadline).await.is_err());
    worker
        .advance(
            &pool,
            Command::BeginLanding {
                expected_run: "run".into(),
                node_id: "a".into(),
                expected_epoch: 0,
            },
            Utc::now(),
        )
        .await
        .unwrap();
    prepare_landing(&worker, &pool, deadline).await.unwrap();
    prepare_landing(&worker, &pool, deadline).await.unwrap();
    let mut remote = FixtureRemote {
        ci: RemoteCi::Active,
        race: false,
        state: Mutex::default(),
    };
    assert_eq!(
        step_landing(
            &mut worker,
            &pool,
            &checks,
            PreflightMode::PreTlc,
            &remote,
            Duration::from_secs(2)
        )
        .await
        .unwrap(),
        LandingProgress::WaitingCi
    );
    assert_eq!(*remote.state.lock().unwrap(), (None, None, 0, 0));
    let (tx, state) = worker.locked(&pool).await.unwrap();
    let job = existing(&state, "a").unwrap();
    let action_id = job.action_id.clone();
    assert!(job.verified());
    assert_eq!(job.target.head_sha, target.head_sha);
    drop(tx);

    remote.ci = RemoteCi::Idle;
    remote.race = true;
    assert_eq!(
        step_landing(
            &mut worker,
            &pool,
            &checks,
            PreflightMode::PreTlc,
            &remote,
            Duration::ZERO
        )
        .await
        .unwrap(),
        LandingProgress::WaitingCi
    );
    assert_eq!(*remote.state.lock().unwrap(), (None, None, 0, 0));
    remote.race = false;
    // Lose the server reply after publication, leaving only the gate's durable receipt.
    let first = worker
        .start_landing(&pool, &action_id, |job, checkout, nonce| {
            initiate(&remote, job, checkout, nonce)
        })
        .await
        .unwrap();
    assert!(matches!(first.initiation, Initiation::Started { .. }));
    let tx = pool.begin().await.unwrap();
    let (tx, reply) = commands::apply_in(
        tx,
        "campaign",
        "stop",
        Utc::now(),
        StoreRequest::Command(Command::RequestStop {
            expected_revision: 1,
            expected_run: "run".into(),
            mode: StopMode::Immediate,
        }),
        |outcome| {
            Ok(ReceiptResponse {
                status: 200,
                body: serde_json::to_value(outcome)?,
            })
        },
    )
    .await
    .unwrap();
    assert!(matches!(reply.outcome, CommandOutcome::Decided(Ok(_))));
    tx.commit().await.unwrap();
    drop(worker);

    let mut worker = open();
    assert!(
        worker
            .start_landing(&pool, &action_id, |_, _, _| panic!(
                "Stop must fence new writes"
            ))
            .await
            .is_err()
    );
    for _ in 0..2 {
        let LandingProgress::Opened(pr) = step_landing(
            &mut worker,
            &pool,
            &checks,
            PreflightMode::PreTlc,
            &remote,
            Duration::ZERO,
        )
        .await
        .unwrap() else {
            panic!("missing PR receipt")
        };
        assert_eq!((pr.number, pr.head_sha), (42, target.head_sha.clone()));
    }
    let (tx, state) = worker.locked(&pool).await.unwrap();
    assert_eq!(state.control, CampaignControl::Stopping);
    let run = state.run.unwrap();
    let job = &run.landings["a"];
    assert!(matches!(job.status, LandingStatus::CiHold { .. }));
    assert_eq!(run.actions[&job.action_id].state, ActionState::Reconciled);
    assert_eq!(state.nodes["a"].stage, Stage::Landing);
    let counts = remote.state.into_inner().unwrap();
    assert_eq!((counts.2, counts.3), (1, 1));
    drop(tx);
    pool.close().await;
    db.drop().await;
}
