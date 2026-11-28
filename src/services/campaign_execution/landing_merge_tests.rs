use super::*;
use crate::services::campaign_execution::adk_observe::ci::{
    CheckResult, CiMode, RequiredSet, TestProvenance, VerifiedTests, WorkerAdoption,
    WorkflowIdentity,
};
use crate::services::campaign_execution::control::test_support as domain;

fn facts() -> (OpenedPr, MergeObservations, DateTime<Utc>) {
    let head = "a".repeat(40);
    let now = domain::now();
    let pr = OpenedPr {
        repository: "owner/project".into(),
        number: 42,
        head_sha: head.clone(),
        head_ref: "campaign/a".into(),
        base_ref: "main".into(),
    };
    let snapshot = MergeObservations {
        pr: PrObservation {
            repository: Repository::Adk,
            repository_identity: pr.repository.clone(),
            number: pr.number,
            state: "open".into(),
            head_sha: head.clone(),
            head_ref: pr.head_ref.clone(),
            base_ref: pr.base_ref.clone(),
            base_sha: "b".repeat(40),
            merged: false,
            merge_sha: None,
            merged_at: None,
            updated_at: now,
        },
        ci: Some(CiObservation {
            repository_identity: pr.repository.clone(),
            pr_number: pr.number,
            pr_head_sha: head.clone(),
            pr_base_ref: pr.base_ref.clone(),
            workflow: WorkflowIdentity {
                path: ".github/workflows/ci-pr.yml".into(),
                name: "CI PR".into(),
                event: "pull_request".into(),
            },
            run_id: 130,
            run_attempt: 1,
            run_conclusion: Some("success".into()),
            required: RequiredSet {
                ruleset: Some(Vec::new()),
                classic: Some(Vec::new()),
                profile: vec!["CI required".into()],
            },
            checks: vec![CheckResult {
                context: "CI required".into(),
                check_run_id: 1,
                status: "completed".into(),
                conclusion: Some("success".into()),
            }],
            provenance: TestProvenance::Verified(VerifiedTests {
                tested_sha: head.clone(),
                tested_base: None,
                mode: CiMode::Head,
                plan_digest: "plan".into(),
                workers: vec![WorkerAdoption {
                    worker: 0,
                    attempt: 1,
                    artifact_id: 1,
                    artifact_digest: None,
                    inherited: false,
                }],
            }),
        }),
        tlc: Some(TlcEvidence {
            head_sha: head,
            started_at: now - TimeDelta::minutes(1),
            finished_at: now,
            reference: "tlc receipt".into(),
            outcome: TlcOutcome::Passed {
                selected: 1,
                passed: 1,
            },
        }),
        runner: None,
    };
    (pr, snapshot, now)
}

#[test]
fn m7_a_moved_pr_head_cannot_use_a_pinned_tlc_result_to_merge() {
    let (pr, snapshot, now) = facts();
    assert!(check_observations(&pr, snapshot, now).is_some());
    let (_, mut snapshot, _) = facts();
    snapshot.pr.head_sha = "e".repeat(40);
    let ci = snapshot.ci.as_mut().unwrap();
    ci.pr_head_sha = snapshot.pr.head_sha.clone();
    let TestProvenance::Verified(tests) = &mut ci.provenance else {
        unreachable!()
    };
    tests.tested_sha = snapshot.pr.head_sha.clone();
    assert!(check_observations(&pr, snapshot, now).is_none());
}

#[test]
fn m7_tlc_must_cover_this_head_and_finish_before_240_minutes() {
    let (pr, mut snapshot, now) = facts();
    snapshot.tlc.as_mut().unwrap().started_at = now - TimeDelta::minutes(240);
    assert!(check_observations(&pr, snapshot, now).is_none());

    let (_, mut snapshot, _) = facts();
    snapshot.tlc.as_mut().unwrap().outcome = TlcOutcome::Passed {
        selected: 0,
        passed: 0,
    };
    assert!(check_observations(&pr, snapshot, now).is_none());

    let (_, mut snapshot, _) = facts();
    snapshot.tlc.as_mut().unwrap().head_sha = "c".repeat(40);
    assert!(check_observations(&pr, snapshot, now).is_none());
}

#[test]
fn m7_ci_of_a_synthetic_ref_needs_the_current_pr_base() {
    let (pr, mut snapshot, now) = facts();
    let TestProvenance::Verified(tests) = &mut snapshot.ci.as_mut().unwrap().provenance else {
        unreachable!()
    };
    tests.mode = CiMode::MergeRef;
    tests.tested_base = Some("c".repeat(40));
    assert!(check_observations(&pr, snapshot, now).is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn m7_fixed_worker_waits_for_approval_and_observes_main_after_stop_pg() {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::db::server_campaigns::{
        CommandOutcome, ReceiptResponse,
        command_transaction::{self, Preparation, StoreRequest},
    };
    use crate::domain::campaign::{
        candidate_seal::CandidateSeal,
        decision_item::ForumApproval,
        landing::{LandingFact, LandingPreflight, PreflightMode},
        profile::{ReviewExtent, ReviewRange},
        review_gate::{
            ActorIdentity, ReviewEvidence, ReviewJob, ReviewRecord, ReviewRequest, ReviewStatus,
            ReviewVerdict,
        },
    };
    use crate::services::campaign_execution::{
        adk_observe::tests::{commit, sh_git},
        adk_resources::AdkResources,
        adk_workspace::observe_checkout,
        commands,
        landing_effects::prepare_landing,
    };

    struct Fixture {
        checkout: std::path::PathBuf,
        base_sha: String,
        landed: Mutex<Option<(String, DateTime<Utc>)>>,
        main_visible: AtomicBool,
    }
    impl Fixture {
        fn snapshot(&self, pr: &OpenedPr) -> MergeObservations {
            let (_, mut facts, _) = facts();
            let now = Utc::now();
            facts.pr.repository_identity = pr.repository.clone();
            facts.pr.number = pr.number;
            facts.pr.head_ref = pr.head_ref.clone();
            facts.pr.head_sha = pr.head_sha.clone();
            facts.pr.base_sha = self.base_sha.clone();
            facts.pr.updated_at = now;
            let ci = facts.ci.as_mut().unwrap();
            ci.pr_head_sha = pr.head_sha.clone();
            let TestProvenance::Verified(tests) = &mut ci.provenance else {
                unreachable!()
            };
            tests.tested_sha = pr.head_sha.clone();
            let tlc = facts.tlc.as_mut().unwrap();
            tlc.head_sha = pr.head_sha.clone();
            tlc.started_at = now;
            tlc.finished_at = now;
            facts
        }
    }
    impl MergeRemote for Fixture {
        fn observe(&self, pr: &OpenedPr) -> io::Result<MergeObservations> {
            Ok(self.snapshot(pr))
        }

        fn squash_exact(
            &self,
            pr: &OpenedPr,
            base_sha: &str,
            losses: &[crate::domain::campaign::landing::CrossRunJob],
            nonce: &str,
        ) -> io::Result<String> {
            assert!(!nonce.is_empty());
            assert!(losses.is_empty());
            assert_eq!(base_sha, self.base_sha);
            let mut landed = self.landed.lock().unwrap();
            assert!(landed.is_none(), "duplicate merge");
            let git = |args: &[&str]| sh_git(&self.checkout, args);
            git(&["checkout", "-q", "main"]);
            git(&["merge", "--squash", &pr.head_sha]);
            git(&["commit", "-q", "-m", "fixture squash"]);
            let commit = git(&["rev-parse", "HEAD"]);
            *landed = Some((commit.clone(), Utc::now()));
            Ok(commit)
        }

        fn observe_main(
            &self,
            pr: &OpenedPr,
            checkout: &Path,
        ) -> io::Result<Option<(PrObservation, MainObservation)>> {
            assert_eq!(checkout, self.checkout);
            let (merge, merged_at) = self.landed.lock().unwrap().clone().unwrap();
            let mut observed = self.snapshot(pr).pr;
            observed.state = "closed".into();
            observed.merged = true;
            observed.merge_sha = Some(merge);
            observed.merged_at = Some(merged_at);
            Ok(Some((
                observed,
                MainObservation {
                    repository_identity: pr.repository.clone(),
                    remote: "origin".into(),
                    branch: "main".into(),
                    observed_ref: "refs/adk-observe/main".into(),
                    main_sha: if self.main_visible.load(Ordering::SeqCst) {
                        sh_git(checkout, &["rev-parse", "main"])
                    } else {
                        self.base_sha.clone()
                    },
                    fetched_at: Utc::now(),
                },
            )))
        }
    }

    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let checkout = root.join("clone");
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
    sh_git(&checkout, &["checkout", "-q", "-b", "feature"]);
    let head = commit(&checkout, "feature.txt", "feature");

    let host = std::process::Command::new("/bin/hostname")
        .output()
        .unwrap();
    let host = String::from_utf8(host.stdout).unwrap();
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

    let render = |outcome: &CommandOutcome| {
        Ok(ReceiptResponse {
            status: 200,
            body: serde_json::to_value(outcome)?,
        })
    };
    let mut input = domain::input();
    input.workflow_options.as_mut().unwrap().merge = true;
    commands::create(&pool, "campaign", &input, "create", Utc::now(), render)
        .await
        .unwrap();
    let tx = pool.begin().await.unwrap();
    let Preparation::Fresh(prepared) = command_transaction::prepare(
        tx,
        "campaign",
        "seed-landing",
        Utc::now(),
        StoreRequest::Command(Command::ConfirmStopped),
    )
    .await
    .unwrap() else {
        panic!("fixture command was already present");
    };
    let previous = prepared.previous().unwrap();
    let mut state = domain::running();
    state.definition = previous.definition.clone();
    state.event_sequence = previous.event_sequence + 1;
    let run = state.run.as_mut().unwrap();
    let attempt = run.attempts.get_mut("attempt").unwrap();
    attempt.binding.hashes = state.definition.hashes()["a"].clone();
    attempt.binding.base_sha = base.clone();
    attempt.outcome = Some(AttemptOutcome::Passed);
    let work = attempt.binding.clone();
    let mut review = work.clone();
    review.attempt_id = "review".into();
    review.role = Role::Review;
    review.stage = Stage::Review;
    review.candidate_sha = head.clone();
    review.session.session = "reviewer".into();
    run.attempts.insert(
        "review".into(),
        Attempt {
            binding: review.clone(),
            started: true,
            outcome: Some(AttemptOutcome::Passed),
            execution: None,
        },
    );
    run.actions.insert(
        "review-action".into(),
        Action {
            id: "review-action".into(),
            kind: ActionKind::Launch,
            state: ActionState::Reconciled,
            binding: review.clone(),
            target: review.session.gate.clone(),
            initiation_receipt: None,
            reconciliation: None,
        },
    );
    let actor = |binding: &AttemptBinding| ActorIdentity {
        attempt_id: binding.attempt_id.clone(),
        session: binding.session.clone(),
        native_conversation: binding.attempt_id.clone(),
    };
    let range = ReviewRange {
        base_sha: base.clone(),
        head_sha: head.clone(),
        extent: ReviewExtent::Full,
    };
    run.reviews.insert(
        "review-action".into(),
        ReviewJob {
            request: ReviewRequest {
                range: range.clone(),
                pin_hash: "pin".into(),
                profile_hash: "profile".into(),
                implementer: actor(&work),
                reviewer: actor(&review),
                required_scopes: Default::default(),
            },
            ordered_at: 1,
            scope_tests: Default::default(),
            status: ReviewStatus::Received {
                record: Box::new(ReviewRecord {
                    evidence_id: "review-evidence".into(),
                    reviewer: actor(&review),
                    review: ReviewEvidence {
                        range,
                        verdict: ReviewVerdict::Clean,
                        findings: Vec::new(),
                        unverified: Vec::new(),
                        inherited: Vec::new(),
                        webgpt: Vec::new(),
                    },
                }),
            },
        },
    );
    let seal = CandidateSeal {
        seal_id: "seal:attempt".into(),
        owner_attempt: work.attempt_id.clone(),
        binding_digest: digest_of(&work).unwrap(),
        repository: Repository::Adk,
        base_sha: base.clone(),
        output_sha: head.clone(),
        claim_id: "claim".into(),
        pin_hash: "pin".into(),
        sealed_at: Utc::now(),
        workspace: observe_checkout(Repository::Adk, host.trim(), &checkout, Utc::now()).unwrap(),
    };
    run.fences.entry("a".into()).or_default().candidate = Some(lifecycle::reviewed(&seal));
    run.seals.insert(work.attempt_id.clone(), seal);
    let node = state.nodes.get_mut("a").unwrap();
    node.stage = Stage::Landing;
    node.disposition = NodeDisposition::Assigned;
    node.current_attempt = None;
    let decision = Decision {
        events: vec![Event {
            sequence: state.event_sequence,
            run_id: Some(run.id.clone()),
            command_id: "seed-landing".into(),
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

    prepare_landing(&worker, &pool, Utc::now() + TimeDelta::minutes(2))
        .await
        .unwrap();
    let (tx, state) = worker.locked(&pool).await.unwrap();
    let action_id = state.run.as_ref().unwrap().landings["a"].action_id.clone();
    worker
        .record_landing(
            tx,
            &state,
            LandingFact::Preflight(LandingPreflight {
                head_sha: head.clone(),
                mode: PreflightMode::PreTlc,
                transcript_digest: "fixture".into(),
            }),
        )
        .await
        .unwrap();
    let opened = worker.start_landing(&pool, &action_id, |job, _, _| {
        let pr = OpenedPr {
            repository: job.target.repository.clone(),
            number: 42,
            head_sha: job.target.head_sha.clone(),
            head_ref: job.target.head_ref.clone(),
            base_ref: job.target.base_ref.clone(),
        };
        Ok(Initiated::Started {
            receipt: serde_json::to_string(&pr)?,
            process: None,
            handle: (),
        })
    });
    // Register the PR through the same journal boundary used by M6.
    let started = opened.await.unwrap();
    let Initiation::Started { receipt, .. } = started.initiation else {
        panic!("fixture PR did not open")
    };
    let pr = serde_json::from_str(&receipt).unwrap();
    let (tx, state) = worker.locked(&pool).await.unwrap();
    worker
        .record_landing(tx, &state, LandingFact::Opened(pr))
        .await
        .unwrap();

    let remote = Fixture {
        checkout,
        base_sha: base,
        landed: Mutex::new(None),
        main_visible: AtomicBool::new(false),
    };
    assert_eq!(
        step_merge(&mut worker, &pool, &remote).await.unwrap(),
        MergeProgress::Waiting
    );
    let MergeProgress::AwaitApproval(binding) =
        step_merge(&mut worker, &pool, &remote).await.unwrap()
    else {
        panic!("missing exact approval gate");
    };
    assert!(remote.landed.lock().unwrap().is_none());
    let (tx, state) = worker.locked(&pool).await.unwrap();
    worker
        .commit(
            tx,
            state.event_sequence,
            Command::ApproveGate {
                binding,
                evidence: Box::new(ForumApproval {
                    decision_ref: "test forum".into(),
                    response_ref: "https://example.com/answer".into(),
                    response_at: Utc::now(),
                }),
            },
            Utc::now(),
        )
        .await
        .unwrap();

    for _ in 0..2 {
        assert_eq!(
            step_merge(&mut worker, &pool, &remote).await.unwrap(),
            MergeProgress::WaitingForMain
        );
    }
    let (tx, state) = worker.locked(&pool).await.unwrap();
    assert!(!state.nodes["a"].main_integrated);
    worker
        .commit(
            tx,
            state.event_sequence,
            Command::RequestStop {
                expected_revision: 1,
                expected_run: "run".into(),
                mode: StopMode::FinishStage,
            },
            Utc::now(),
        )
        .await
        .unwrap();
    drop(worker);
    let mut worker = open();
    remote.main_visible.store(true, Ordering::SeqCst);
    for _ in 0..2 {
        assert_eq!(
            step_merge(&mut worker, &pool, &remote).await.unwrap(),
            MergeProgress::Adopted
        );
    }
    let (tx, state) = worker.locked(&pool).await.unwrap();
    assert_eq!(state.control, CampaignControl::Stopping);
    assert!(state.nodes["a"].main_integrated);
    assert_eq!(state.nodes["a"].stage, Stage::Landing);
    let run = state.run.unwrap();
    assert!(matches!(run.merges["a"].status, MergeStatus::Adopted(_)));
    assert_eq!(
        run.actions[&format!("merge:{action_id}")].state,
        ActionState::Reconciled
    );
    drop(tx);
    pool.close().await;
    db.drop().await;
}

#[test]
fn m7_both_stop_modes_close_the_merge_boundary_without_changing_the_head() {
    use crate::domain::campaign::landing::{Landing, LandingStatus};
    use crate::domain::campaign::model::ReviewedCandidate;

    let (pr, _, now) = facts();
    let mut state = domain::running();
    let work = domain::binding(&state);
    let landing = Landing {
        target: crate::domain::campaign::landing::LandingTarget {
            repository: pr.repository.clone(),
            head_sha: pr.head_sha.clone(),
            head_ref: pr.head_ref.clone(),
            base_ref: pr.base_ref.clone(),
            pin_hash: "pin".into(),
        },
        epoch: 1,
        control_version: 1,
        host: work.session.gate,
        action_id: "opening".into(),
        deadline: now,
        preflight: None,
        status: LandingStatus::CiHold { pr: pr.clone() },
    };
    let node = state.nodes.get_mut("a").unwrap();
    node.stage = Stage::Landing;
    node.disposition = NodeDisposition::Assigned;
    node.current_attempt = None;
    let run = state.run.as_mut().unwrap();
    let fence = run.fences.entry("a".into()).or_default();
    fence.epoch = landing.epoch;
    fence.candidate = Some(ReviewedCandidate {
        work_attempt: work.attempt_id,
        base_sha: work.base_sha,
        head_sha: pr.head_sha,
        pin_hash: landing.target.pin_hash.clone(),
    });
    assert!(lifecycle::merge_position(
        &state,
        state.run.as_ref().unwrap(),
        "a",
        &landing
    ));
    for mode in [StopMode::Immediate, StopMode::FinishStage] {
        state.run.as_mut().unwrap().stop = Some(StopIntent {
            mode,
            requested_at: now,
            targets: Default::default(),
            acknowledgements: Vec::new(),
            finish_attempts: Default::default(),
            confirmed_event: None,
            finish_grants: Vec::new(),
        });
        assert!(!lifecycle::merge_position(
            &state,
            state.run.as_ref().unwrap(),
            "a",
            &landing
        ));
    }
    state.run.as_mut().unwrap().stop = None;
    state.control = CampaignControl::Stopping;
    assert!(!lifecycle::merge_position(
        &state,
        state.run.as_ref().unwrap(),
        "a",
        &landing
    ));
}
