use std::os::unix::fs::PermissionsExt;
use std::process::Command as Process;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::campaign_execution::{
    adk_resources::AdkResources,
    adk_workspace,
    control::tests as domain,
    evidence::{EvidenceEnvelope, TuiPayload, TuiSubmission},
    execution_pin::IssueObservation,
    host_identity,
    pipeline_rules::MinimalPipelineScope,
    profile::Observation,
    runtime_profile::tests::{rule_files, witness},
    stage_fsm::tests as fsm,
    verification_run::PhaseOutput,
    verification_spec::ADK_NON_PG,
};

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Process::new("git")
        .current_dir(repo)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

async fn command(pool: &PgPool, key: &str, command: Command) {
    let report = commands::execute(
        pool,
        "campaign",
        &CommandEnvelope {
            idempotency_key: key.into(),
            observed_at: Utc::now(),
            command,
        },
        render,
    )
    .await
    .unwrap();
    assert!(
        matches!(report.outcome, CommandOutcome::Decided(Ok(_))),
        "{:?}",
        report.outcome
    );
}

#[tokio::test]
async fn m4_adk_adapter_observes_roles_before_start_and_replays_archive_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init"]);
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/itismyfield/AgentDesk.git",
        ],
    );
    git(&repo, &["commit", "--allow-empty", "-m", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["commit", "--allow-empty", "-m", "sealed"]);
    let output = git(&repo, &["rev-parse", "HEAD"]);

    let pipeline = root.join("pipeline");
    rule_files(&pipeline);
    std::fs::write(
        pipeline.join("scripts/wait_headroom.sh"),
        "printf 'HEADROOM_SUSTAINED ok=1 at 00:00:00\\n'",
    )
    .unwrap();
    let shim = pipeline.join("scripts/cargo-shim/cargo");
    std::fs::write(&shim,
        "#!/bin/sh\nset -eu\ntest \"$1:$2:$3:$4\" = nextest:archive:--lib:--archive-file\nprintf 'fixture archive' > \"$5\"\nprintf x >> \"$5.calls\"\n"
    ).unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o700)).unwrap();
    let resources = AdkResources::new(&repo, &pipeline).unwrap();
    let hostname = Process::new("/bin/hostname").output().unwrap();
    let hostname = String::from_utf8(hostname.stdout).unwrap();
    let host = host_identity::observe(hostname.trim(), "campaign").unwrap();
    let scope =
        MinimalPipelineScope::minimal(7438, host.host.clone(), ["scope".into(), "approved".into()]);
    let allow_review = AtomicBool::new(true);
    let observe = |role| {
        if role == Role::Review && !allow_review.load(Ordering::SeqCst) {
            return Observation::Unknown {
                reason: "review unavailable".into(),
                last_observed_at: None,
            };
        }
        Observation::Observed {
            evidence: witness(role, &host),
            observed_at: Utc::now(),
        }
    };
    let workspace = root.join("isolated");
    let runtime = || RuntimeProfile {
        rule_root: &pipeline,
        scope: &scope,
        tool_policy_digest: "tool-policy",
        observer: &observe,
    };
    let open = || {
        VerificationDriver::open(
            root,
            "campaign",
            "a",
            "campaign",
            AdkAdapter {
                repository: &repo,
                workspace_root: &workspace,
                resources: &resources,
            },
            runtime(),
        )
    };
    let mut driver = open().unwrap();
    let created = commands::create(
        &pool,
        "campaign",
        &domain::input(),
        "create",
        Utc::now(),
        render,
    )
    .await
    .unwrap();
    assert!(matches!(created.outcome, CommandOutcome::Decided(Ok(_))));
    command(
        &pool,
        "begin",
        Command::BeginRun {
            expected_revision: 1,
            run_id: "run".into(),
        },
    )
    .await;
    let polled = driver.worker().poll(&pool, Utc::now()).await.unwrap();
    let WorkerTurn::Next(crate::domain::campaign::lifecycle::NextAction::AssignWork { key }) =
        polled.turn
    else {
        panic!("unexpected {:?}", polled.turn);
    };
    let mut work = fsm::work("a", &key.attempt_id(Stage::Implementation));
    work.session = witness(Role::Implementation, &host).session;
    work.base_sha = base;
    work.candidate_sha.clone_from(&output);
    let requirements = RequirementsSnapshot::observe(IssueObservation {
        repository: Repository::Adk,
        repository_identity: adk_workspace::repository_identity(&repo).unwrap(),
        issue_number: 7438,
        issue_url: "https://github.com/itismyfield/AgentDesk/issues/7438".into(),
        observed_at: Utc::now(),
        issue_updated_at: Some(Utc::now()),
        body: "Preserve the verify boundary.".into(),
        acceptance: Vec::new(),
    })
    .unwrap();
    let spec = adk_verification_spec();
    let profile = runtime().observe(&host, &spec).unwrap();
    let pin = ExecutionPin::pin(
        work.hashes.clone(),
        &requirements,
        profile.profile().hash().unwrap(),
        spec.hash().unwrap(),
    )
    .unwrap();
    let Command::AssignWork {
        binding,
        assignment,
        mut launch,
    } = fsm::assign(
        &work,
        WorkAssignment {
            pin: pin.clone(),
            spec,
            acceptance: Vec::new(),
            review_start_checks: vec![ADK_NON_PG.into()],
            review_finish_checks: Vec::new(),
            gate: host.clone(),
        },
    )
    else {
        unreachable!();
    };
    let launch_id = key.launch_id(Stage::Implementation);
    launch.id.clone_from(&launch_id);
    driver
        .worker()
        .advance(
            &pool,
            Command::AssignWork {
                binding,
                assignment,
                launch,
            },
            Utc::now(),
        )
        .await
        .unwrap();
    for (key, state) in [
        ("claim", ActionState::Claimed),
        ("pending", ActionState::StartPending),
    ] {
        command(
            &pool,
            key,
            Command::AdvanceAction {
                action_id: launch_id.clone(),
                binding: Box::new(work.clone()),
                state,
            },
        )
        .await;
    }
    command(
        &pool,
        "launch-observed",
        Command::ObserveAction {
            action_id: launch_id,
            binding: Box::new(work.clone()),
            observation: ActionObservation::Started {
                receipt: "started".into(),
            },
        },
    )
    .await;
    command(
        &pool,
        "session-alive",
        Command::ObserveSession {
            identity: work.session.clone(),
            observation: SessionObservation::Alive {
                observed_at: Utc::now(),
            },
        },
    )
    .await;
    let state = store::get(&pool, "campaign").await.unwrap().unwrap();
    let execution = fsm::execution(&state, &work.attempt_id);
    let execution_id = &execution.start_executions[0];
    let claim = EvidenceEnvelope::from_tui(
        TuiSubmission {
            evidence_id: "output".into(),
            payload: TuiPayload::OutputClaim { output_sha: output },
        },
        &work,
        &execution.pin_hash,
        Utc::now(),
    )
    .unwrap();
    let checkout =
        adk_workspace::observe_checkout(Repository::Adk, &host.host, &repo, Utc::now()).unwrap();
    command(
        &pool,
        "seal",
        Command::SealCandidate {
            claim: Box::new(claim),
            repository_identity: checkout.repository_identity.clone(),
            workspace: Box::new(checkout),
        },
    )
    .await;
    let request = || VerificationRequest {
        execution: execution_id,
        pin: &pin,
        requirements: &requirements,
        headroom_wait: Duration::from_secs(5),
    };
    // Subject, immutable input, claim and StartPending are all recorded before the phase starts.
    for _ in 0..4 {
        assert!(matches!(
            driver.step(&pool, request()).await.unwrap(),
            VerificationStep::Progress
        ));
    }
    allow_review.store(false, Ordering::SeqCst);
    assert!(matches!(
        driver.step(&pool, request()).await,
        Err(DriverError::Runtime(RuntimeError::Unobserved(Role::Review)))
    ));
    let pending = store::get(&pool, "campaign").await.unwrap().unwrap();
    let run = pending.run.as_ref().unwrap();
    let archive = run.verifications[execution_id].phase("archive").unwrap();
    let input = &archive.input.bound().unwrap().input;
    let artifact = Path::new(&input.outputs[0].path);
    assert_eq!(
        run.actions[&archive.action_id].state,
        ActionState::StartPending
    );
    assert!(!artifact.exists());
    assert_eq!(input.command.program, shim.to_str().unwrap());
    assert_eq!(
        git(
            Path::new(&input.checkout_path),
            &["rev-parse", "--abbrev-ref", "HEAD"]
        ),
        "HEAD"
    );

    allow_review.store(true, Ordering::SeqCst);
    let VerificationStep::Running(active) = driver.step(&pool, request()).await.unwrap() else {
        panic!("archive did not start");
    };
    assert!(matches!(
        driver
            .finish(*active, Duration::from_secs(5))
            .await
            .unwrap(),
        VerificationStep::Progress
    ));
    drop(driver);
    let mut restarted = open().unwrap();
    assert!(matches!(
        restarted.step(&pool, request()).await.unwrap(),
        VerificationStep::Progress
    ));
    let state = store::get(&pool, "campaign").await.unwrap().unwrap();
    let run = state.run.as_ref().unwrap();
    let result = run.verifications[execution_id]
        .phase("archive")
        .unwrap()
        .result
        .as_ref()
        .unwrap();
    assert_eq!(result.record.exit_code, Some(0));
    assert!(matches!(&result.output, PhaseOutput::Archive { digest } if !digest.is_empty()));
    let calls = format!("{}.calls", artifact.display());
    assert_eq!(std::fs::read_to_string(calls).unwrap(), "x");
    drop(restarted);
    pool.close().await;
    db.drop().await;
}
