use super::*;
use crate::services::campaign_execution::{
    control::{decide as command, test_support as domain},
    review_dispatch::WebGptReply,
    stage_fsm::test_support as fsm,
};

fn fixture() -> (CampaignState, String, ReviewRequest) {
    let (mut state, work, review, seal) = fsm::handed_over();
    let v = fsm::execution(&state, &work.attempt_id).finish_executions[0].clone();
    fsm::run_v(&mut state, &v, &seal, 0);
    let run = state.run.as_mut().unwrap();
    run.seals.insert(work.attempt_id.clone(), seal.clone());
    let action = run
        .actions
        .values()
        .find(|a| a.binding == review)
        .unwrap()
        .clone();
    let actor = |binding: &AttemptBinding| ActorIdentity {
        attempt_id: binding.attempt_id.clone(),
        session: binding.session.clone(),
        native_conversation: format!("native-{}", binding.attempt_id),
    };
    let job = request(
        &action,
        seal.pin_hash,
        "profile".into(),
        actor(&work),
        actor(&review),
        None,
        vec!["pg".into()],
    );
    (state, action.id, job)
}

fn change(
    state: &mut CampaignState,
    action_id: &str,
    fact: ReviewFact,
) -> Result<Decision, Rejection> {
    let mut envelope = domain::envelope(Command::RecordReview {
        action_id: action_id.into(),
        fact: Box::new(fact),
    });
    envelope.idempotency_key = format!("review-test:{}", state.event_sequence);
    let result = command(state, &envelope)?;
    *state = result.state.clone();
    Ok(result)
}

fn start(state: &mut CampaignState, action_id: &str, request: ReviewRequest) {
    change(state, action_id, ReviewFact::Prepare(Box::new(request))).unwrap();
    let action = state
        .run
        .as_mut()
        .unwrap()
        .actions
        .get_mut(action_id)
        .unwrap();
    action.state = ActionState::StartPending;
    change(
        state,
        action_id,
        ReviewFact::Sent {
            receipt: "remote-receipt".into(),
            deadline: domain::now() + chrono::TimeDelta::minutes(1),
        },
    )
    .unwrap();
}

fn answer_for(request: &ReviewRequest, findings: Vec<Finding>) -> ReviewEvidence {
    ReviewEvidence {
        range: request.range.clone(),
        verdict: ReviewVerdict::Clean,
        findings,
        unverified: vec!["pg".into()],
        inherited: Vec::new(),
        webgpt: Vec::new(),
    }
}

fn test_fact(request: &ReviewRequest, selected: u32) -> ReviewFact {
    ReviewFact::Scope(ScopeTest {
        scope: "pg".into(),
        head_sha: request.range.head_sha.clone(),
        selected,
        executed: selected,
        failed: 0,
        result_ref: "runner/test".into(),
        outcome: ScopeOutcome::Passed,
    })
}

#[test]
fn m5_review_and_scope_are_separate_and_p2_does_not_spawn_repair() {
    let (mut state, action_id, request) = fixture();
    start(&mut state, &action_id, request.clone());
    let mut review = answer_for(
        &request,
        vec![Finding {
            id: "follow-up".into(),
            severity: Some(FindingSeverity::P2),
            open: true,
            resolution: None,
        }],
    );
    review.verdict = ReviewVerdict::Issues;
    let reply = ReviewFact::Reply {
        receipt: "remote-receipt".into(),
        review: Box::new(review),
    };
    let received = change(&mut state, &action_id, reply).unwrap();
    let ReviewStatus::Received { record } = &state.run.as_ref().unwrap().reviews[&action_id].status
    else {
        panic!("no review recorded");
    };
    assert_eq!(record.review.verdict, ReviewVerdict::Clean);
    assert!(matches!(received.records.as_slice(),
        [EvidenceWrite::Admitted { envelope, .. }] if matches!(envelope.payload, EvidencePayload::ReviewResult(_))));
    change(&mut state, &action_id, ReviewFact::Finalize).unwrap();
    assert_eq!(state.nodes["a"].stage, Stage::Review);
    assert!(change(&mut state, &action_id, test_fact(&request, 0)).is_err());
    let verified = change(&mut state, &action_id, test_fact(&request, 1)).unwrap();
    assert!(matches!(verified.records.as_slice(),
        [EvidenceWrite::Admitted { envelope, .. }] if matches!(envelope.payload, EvidencePayload::ReviewScope(_))));
    change(&mut state, &action_id, ReviewFact::Finalize).unwrap();
    assert_eq!(state.nodes["a"].stage, Stage::Landing);
    assert_eq!(state.nodes["a"].repair_cycles, 0);
}

#[test]
fn m5_capacity_wait_and_failed_reply_never_make_a_verdict() {
    let (mut state, action_id, request) = fixture();
    change(
        &mut state,
        &action_id,
        ReviewFact::Prepare(Box::new(request.clone())),
    )
    .unwrap();
    change(&mut state, &action_id, ReviewFact::WaitingForSlot).unwrap();
    let run = state.run.as_ref().unwrap();
    assert_eq!(run.reviews[&action_id].status, ReviewStatus::WaitingForSlot);
    assert_eq!(run.actions[&action_id].state, ActionState::Planned);
    for reply in [
        WebGptReply::Refused,
        WebGptReply::Unavailable,
        WebGptReply::Output {
            receipt: "remote-receipt".into(),
            body: " \n ".into(),
        },
    ] {
        let (mut state, action_id, request) = fixture();
        start(&mut state, &action_id, request.clone());
        let at = domain::now();
        let fact = answer(
            &request,
            Some(reply),
            "remote-receipt",
            at + chrono::TimeDelta::seconds(10),
            at,
        )
        .unwrap();
        assert!(matches!(fact, ReviewFact::Failed { .. }));
        change(&mut state, &action_id, fact).unwrap();
        assert!(matches!(
            state.run.as_ref().unwrap().reviews[&action_id].status,
            ReviewStatus::Failed { .. }
        ));
        change(&mut state, &action_id, ReviewFact::Finalize).unwrap();
        assert_eq!(state.nodes["a"].stage, Stage::Review);
        assert_eq!(state.nodes["a"].repair_cycles, 0);
    }
}

#[test]
fn m5_p1_blocks_and_timeout_at_the_deadline_closes_the_review() {
    let (mut state, action_id, request) = fixture();
    start(&mut state, &action_id, request.clone());
    let at = domain::now();
    assert!(matches!(
        answer(
            &request,
            Some(WebGptReply::Pending),
            "remote-receipt",
            at,
            at
        ),
        Some(ReviewFact::Failed { .. })
    ));
    let review = answer_for(
        &request,
        vec![Finding {
            id: "blocker".into(),
            severity: Some(FindingSeverity::P1),
            open: true,
            resolution: None,
        }],
    );
    change(
        &mut state,
        &action_id,
        ReviewFact::Reply {
            receipt: "remote-receipt".into(),
            review: Box::new(review),
        },
    )
    .unwrap();
    change(&mut state, &action_id, test_fact(&request, 1)).unwrap();
    change(&mut state, &action_id, ReviewFact::Finalize).unwrap();
    assert_eq!(state.nodes["a"].stage, Stage::Implementation);
    assert_eq!(state.nodes["a"].repair_cycles, 1);
}

#[test]
fn m5_scope_requires_current_head_and_real_execution() {
    let (_, _, request) = fixture();
    let ReviewFact::Scope(mut result) = test_fact(&request, 0) else {
        unreachable!()
    };
    assert!(!scope_valid(&result, &request));
    result.outcome = ScopeOutcome::Unavailable;
    assert!(scope_valid(&result, &request));
    result.outcome = ScopeOutcome::FeatureFailed;
    assert!(!scope_valid(&result, &request));
    result.selected = 1;
    result.executed = 1;
    result.failed = 1;
    assert!(scope_valid(&result, &request));
    result.head_sha = "stale".into();
    assert!(!scope_valid(&result, &request));
}

#[test]
fn m5_delta_requires_the_last_review_head_and_full_uses_the_seal_base() {
    let (mut state, action_id, full) = fixture();
    let run = state.run.as_mut().unwrap();
    let action = run.actions[&action_id].clone();
    let mut previous_action = action.clone();
    previous_action.id = "previous".into();
    run.actions
        .insert(previous_action.id.clone(), previous_action);
    let mut previous = full.clone();
    previous.range.head_sha = "previous-head".into();
    let record = ReviewRecord {
        evidence_id: "prior".into(),
        reviewer: previous.reviewer.clone(),
        review: answer_for(&previous, Vec::new()),
    };
    run.reviews.insert(
        "previous".into(),
        ReviewJob {
            request: previous,
            ordered_at: 1,
            status: ReviewStatus::Received {
                record: Box::new(record),
            },
            scope_tests: Default::default(),
        },
    );
    assert!(matches_request(run, &action, &full));
    let mut delta = full;
    delta.range.extent = ReviewExtent::Delta;
    assert!(!matches_request(run, &action, &delta));
    delta.range.base_sha = "previous-head".into();
    assert!(matches_request(run, &action, &delta));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn m5_webgpt_pro_reuses_its_receipt_and_stores_scope_evidence_pg() {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::db::server_campaigns::command_transaction::{self, Preparation};
    use crate::services::campaign_execution::{
        adk_resources::AdkResources,
        host_identity,
        host_worker::HostWorker,
        pipeline_rules::MinimalPipelineScope,
        profile::Observation,
        review_dispatch::{ReviewDispatcher, ReviewProgress, WebGptAdmission, WebGptPro},
        runtime_profile::{
            RuntimeProfile,
            tests::{rule_files, witness},
        },
        verification_spec::adk_verification_spec,
    };

    struct Provider {
        full: AtomicBool,
        sends: AtomicUsize,
    }

    impl WebGptPro for Provider {
        type Slot = ();

        fn reserve(&self) -> WebGptAdmission<()> {
            if self.full.load(Ordering::SeqCst) {
                WebGptAdmission::Full
            } else {
                WebGptAdmission::Ready(())
            }
        }

        fn send_pro(&self, _: (), _: &ReviewRequest, nonce: &str) -> std::io::Result<String> {
            assert!(!nonce.is_empty());
            self.sends.fetch_add(1, Ordering::SeqCst);
            Ok("one-remote-receipt".into())
        }

        fn collect_pro(&self, request: &ReviewRequest, receipt: &str) -> WebGptReply {
            assert_eq!(receipt, "one-remote-receipt");
            let json = serde_json::to_string(&answer_for(request, Vec::new())).unwrap();
            WebGptReply::Output {
                receipt: receipt.into(),
                body: format!("{json}\nVERDICT: CLEAN"),
            }
        }
    }

    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    rule_files(root);
    let shim = root.join("scripts/cargo-shim/cargo");
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        root.join("scripts/wait_headroom.sh"),
        "printf 'HEADROOM_SUSTAINED ok=1 at 00:00:00\\n'",
    )
    .unwrap();
    let hostname = std::process::Command::new("/bin/hostname")
        .output()
        .unwrap();
    let hostname = String::from_utf8(hostname.stdout).unwrap();
    let host = host_identity::observe(hostname.trim(), "campaign").unwrap();
    let (mut state, action_id, mut job) = fixture();
    let run = state.run.as_mut().unwrap();
    for attempt in run.attempts.values_mut() {
        attempt.binding.session.gate = host.clone();
    }
    let review_action = run.actions.get_mut(&action_id).unwrap();
    review_action.target = host.clone();
    review_action.binding.session.gate = host.clone();
    let review_binding = review_action.binding.clone();
    for prerequisite in run.prerequisites.values_mut() {
        let target = &prerequisite.target;
        *prerequisite = StagePrerequisite::bind(
            &run.verifications[&target.execution_id],
            &run.seals[&target.owner_attempt],
            &review_binding,
            prerequisite.consumer_node_epoch,
            prerequisite.run_control_version,
        )
        .unwrap();
    }
    job.reviewer.session.gate = host.clone();
    job.implementer.session.gate = host.clone();
    let scope =
        MinimalPipelineScope::minimal(7438, host.host.clone(), ["scope".into(), "approved".into()]);
    let roles = |role| {
        let mut evidence = witness(role, &host);
        evidence.session = if role == Role::Review {
            job.reviewer.session.clone()
        } else {
            job.implementer.session.clone()
        };
        Observation::Observed {
            evidence,
            observed_at: Utc::now(),
        }
    };
    let runtime = RuntimeProfile {
        rule_root: root,
        scope: &scope,
        tool_policy_digest: "tool-policy",
        observer: &roles,
    };
    let profile_hash = runtime
        .observe(&host, &adk_verification_spec())
        .unwrap()
        .profile()
        .hash()
        .unwrap();

    let render = |outcome: &CommandOutcome| {
        Ok(ReceiptResponse {
            status: 200,
            body: serde_json::to_value(outcome)?,
        })
    };
    commands::create(
        &pool,
        &state.id,
        &domain::input(),
        "create",
        Utc::now(),
        render,
    )
    .await
    .unwrap();
    // The upstream Work handover is fixture state. M5 transitions below use the real command writer.
    let tx = pool.begin().await.unwrap();
    let Preparation::Fresh(prepared) = command_transaction::prepare(
        tx,
        &state.id,
        "seed-handover",
        Utc::now(),
        StoreRequest::Command(Command::ConfirmStopped),
    )
    .await
    .unwrap() else {
        panic!("missing fixture command");
    };
    state.event_sequence = prepared.previous().unwrap().event_sequence + 1;
    let command_result = CommandResult {
        definition_revision: state.definition_revision,
        run_id: Some("run".into()),
        control_version: Some(1),
        event_sequence: state.event_sequence,
        control: state.control,
    };
    let decision = Decision {
        events: vec![Event {
            sequence: state.event_sequence,
            run_id: Some("run".into()),
            command_id: "seed-handover".into(),
            observed_at: Utc::now(),
            kind: EventKind::RunStarted,
        }],
        result: command_result,
        state: state.clone(),
        records: Vec::new(),
    };
    let staged = prepared.stage(Some(Ok(decision))).await.unwrap();
    let response = render(staged.outcome()).unwrap();
    let (tx, _) = staged.record_receipt(response).await.unwrap();
    tx.commit().await.unwrap();

    let raw = fsm::accept(
        &state,
        &job.reviewer.attempt_id,
        Some(StageAssessment::pass(Vec::new())),
    );
    let rejected = commands::execute(&pool, &state.id, &domain::envelope(raw), render)
        .await
        .unwrap();
    assert!(matches!(rejected.outcome, CommandOutcome::Decided(Err(_))));
    let mut request = job.clone();
    request.profile_hash = profile_hash;
    record(
        &pool,
        &state.id,
        &action_id,
        ReviewFact::Prepare(Box::new(request.clone())),
    )
    .await
    .unwrap();
    let resources = AdkResources::new(root, root).unwrap();
    let open = || {
        HostWorker::open(
            root,
            &state.id,
            "a",
            &host.host,
            "campaign",
            resources.clone(),
            Utc::now(),
        )
        .unwrap()
    };
    let mut worker = open();
    let transport = Provider {
        full: AtomicBool::new(true),
        sends: AtomicUsize::new(0),
    };
    let driver = ReviewDispatcher {
        campaign: &state.id,
        transport: &transport,
        runtime,
    };
    let headroom = Duration::from_secs(5);
    assert_eq!(
        driver
            .step(&pool, &mut worker, &action_id, headroom)
            .await
            .unwrap(),
        ReviewProgress::WaitingForSlot
    );
    transport.full.store(false, Ordering::SeqCst);
    assert_eq!(
        driver
            .step(&pool, &mut worker, &action_id, headroom)
            .await
            .unwrap(),
        ReviewProgress::AwaitingReply
    );
    drop(worker);
    let mut worker = open();
    assert_eq!(
        driver
            .step(&pool, &mut worker, &action_id, headroom)
            .await
            .unwrap(),
        ReviewProgress::Recorded
    );
    assert_eq!(transport.sends.load(Ordering::SeqCst), 1);
    let ReviewFact::Scope(test) = test_fact(&request, 1) else {
        unreachable!();
    };
    driver
        .scope_test(
            &pool,
            &action_id,
            Observation::Observed {
                evidence: test,
                observed_at: Utc::now(),
            },
        )
        .await
        .unwrap();
    driver.finalize(&pool, &action_id).await.unwrap();
    let records: Vec<String> = sqlx::query_scalar(
        "SELECT document->'envelope'->'payload'->>'class' FROM server_campaign_evidence WHERE campaign_id = $1 ORDER BY evidence_id",
    ).bind(&state.id).fetch_all(&pool).await.unwrap();
    assert_eq!(records, ["review_result", "review_scope"]);
    let final_state = store::get(&pool, &state.id).await.unwrap().unwrap();
    assert_eq!(final_state.nodes["a"].stage, Stage::Landing);
    drop(worker);
    pool.close().await;
    db.drop().await;
}
