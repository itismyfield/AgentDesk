use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::campaign_execution::control::tests as domain;

pub(super) fn render(outcome: &CommandOutcome) -> StoreResult<ReceiptResponse> {
    Ok(ReceiptResponse {
        status: match outcome {
            CommandOutcome::Decided(Ok(_)) => 200,
            CommandOutcome::Decided(Err(_)) => 409,
            CommandOutcome::NotFound => 404,
        },
        body: serde_json::to_value(outcome)?,
    })
}

fn changed_render(_: &CommandOutcome) -> StoreResult<ReceiptResponse> {
    Ok(ReceiptResponse {
        status: 208,
        body: serde_json::json!({"changed_renderer": true}),
    })
}

fn failed_render(_: &CommandOutcome) -> StoreResult<ReceiptResponse> {
    Err(StoreError::InconsistentState)
}

pub(super) async fn create(
    pool: &PgPool,
    id: &str,
    input: &DefinitionInput,
    key: &str,
    observed_at: DateTime<Utc>,
) -> StoreResult<CommandReply> {
    super::create(pool, id, input, key, observed_at, render).await
}

pub(super) async fn execute(
    pool: &PgPool,
    id: &str,
    envelope: &CommandEnvelope,
) -> StoreResult<CommandReply> {
    super::execute(pool, id, envelope, render).await
}

pub(super) struct Harness {
    pub fixture: TestPostgresDb,
    pub pool: PgPool,
}

impl Harness {
    pub async fn new() -> Self {
        let fixture = TestPostgresDb::create().await;
        let pool = fixture.connect_and_migrate().await;
        Self { fixture, pool }
    }

    // This fixture can seed only its own isolated database, never a caller's pool.
    pub async fn seed(&self, state: CampaignState) {
        let mut tx = self.pool.begin().await.unwrap();
        sqlx::query("INSERT INTO server_campaign_keys (campaign_id) VALUES ($1)")
            .bind(&state.id)
            .execute(&mut *tx)
            .await
            .unwrap();
        let event = Event {
            sequence: state.event_sequence,
            run_id: state.run.as_ref().map(|r| r.id.clone()),
            command_id: "fixture".into(),
            observed_at: domain::now(),
            kind: EventKind::DefinitionCreated,
        };
        let result = CommandResult {
            definition_revision: state.definition_revision,
            run_id: event.run_id.clone(),
            control_version: state.run.as_ref().map(|r| r.control_version),
            event_sequence: state.event_sequence,
            control: state.control,
        };
        write::persist(
            &mut tx,
            None,
            &Decision {
                state,
                events: vec![event],
                result,
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }

    pub async fn reconnect(&mut self) {
        self.pool.close().await;
        self.pool = self.fixture.connect_and_migrate().await;
    }

    pub async fn close(self) {
        self.pool.close().await;
        self.fixture.drop().await;
    }
}

pub(super) fn envelope(key: &str, command: Command) -> CommandEnvelope {
    CommandEnvelope {
        idempotency_key: key.into(),
        observed_at: domain::now(),
        command,
    }
}

pub(super) async fn send(pool: &PgPool, key: &str, command: Command) -> CommandOutcome {
    execute(pool, "campaign", &envelope(key, command))
        .await
        .unwrap()
        .outcome
}

pub(super) fn applied(outcome: CommandOutcome) -> CommandResult {
    match outcome {
        CommandOutcome::Decided(Ok(result)) => result,
        other => panic!("expected decision, got {other:?}"),
    }
}

pub(super) fn rejected(outcome: CommandOutcome, code: RejectionCode) {
    assert_eq!(outcome, CommandOutcome::Decided(Err(Rejection::new(code))));
}

pub(super) fn edit(revision: u64, title: &str) -> Command {
    let mut definition = domain::input();
    definition.title = title.into();
    Command::EditDefinition {
        expected_revision: revision,
        definition,
        change: None,
    }
}

pub(super) fn stop(mode: StopMode) -> Command {
    Command::RequestStop {
        expected_revision: 1,
        expected_run: "run".into(),
        mode,
    }
}

#[tokio::test]
async fn concurrent_create_same_key_and_lost_response_replay_before_stale_pg() {
    let mut h = Harness::new().await;
    let input = domain::input();
    let (a, b) = tokio::join!(
        create(&h.pool, "campaign", &input, "create", domain::now()),
        create(&h.pool, "campaign", &input, "create", domain::now()),
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(a.outcome, b.outcome);
    assert_eq!(a.response, b.response);
    assert_ne!(a.replayed, b.replayed);
    assert_eq!(applied(a.outcome.clone()).event_sequence, 1);
    applied(send(&h.pool, "edit", edit(1, "Changed after response loss")).await);
    h.reconnect().await;
    let retry = create(&h.pool, "campaign", &input, "create", domain::now())
        .await
        .unwrap();
    assert!(retry.replayed);
    assert_eq!(retry.outcome, a.outcome);
    assert_eq!(retry.response, a.response);
    let retry_with_changed_renderer = super::create(
        &h.pool,
        "campaign",
        &input,
        "create",
        domain::now(),
        changed_render,
    )
    .await
    .unwrap();
    assert_eq!(retry_with_changed_renderer.response, a.response);
    assert_eq!(
        events(&h.pool, "campaign", None, 0, 500)
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(list(&h.pool, 100, 0).await.unwrap().len(), 1);
    assert_eq!(
        get(&h.pool, "campaign")
            .await
            .unwrap()
            .unwrap()
            .definition_revision,
        2
    );
    h.close().await;
}

#[tokio::test]
async fn concurrent_create_different_body_same_key_conflicts_pg() {
    let h = Harness::new().await;
    let input = domain::input();
    let mut other = input.clone();
    other.title = "Different body".into();
    let (a, b) = tokio::join!(
        create(&h.pool, "campaign", &input, "same", domain::now()),
        create(&h.pool, "campaign", &other, "same", domain::now()),
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert!(matches!(
        a.as_ref().err().or(b.as_ref().err()),
        Some(StoreError::IdempotencyConflict)
    ));
    let saved: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM server_campaign_receipts")
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(saved, 1);
    assert_eq!(
        events(&h.pool, "campaign", None, 0, 100)
            .await
            .unwrap()
            .len(),
        1
    );
    let occupied = create(&h.pool, "campaign", &input, "another-key", domain::now())
        .await
        .unwrap();
    rejected(occupied.outcome, RejectionCode::AlreadyExists);
    h.close().await;
}

#[tokio::test]
async fn concurrent_commands_share_receipt_and_cas_loser_is_durable_pg() {
    let mut h = Harness::new().await;
    h.seed(domain::prepared()).await;
    let same = envelope("same", edit(1, "Revision two"));
    let (a, b) = tokio::join!(
        execute(&h.pool, "campaign", &same),
        execute(&h.pool, "campaign", &same)
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(a.outcome, b.outcome);
    assert_ne!(a.replayed, b.replayed);
    let different = envelope("same", edit(2, "Different body"));
    assert!(matches!(
        execute(&h.pool, "campaign", &different).await,
        Err(StoreError::IdempotencyConflict)
    ));
    let left = envelope("left", edit(2, "Left"));
    let right = envelope("right", edit(2, "Right"));
    let (left_result, right_result) = tokio::join!(
        execute(&h.pool, "campaign", &left),
        execute(&h.pool, "campaign", &right)
    );
    let (left_result, right_result) = (left_result.unwrap(), right_result.unwrap());
    let loser = if matches!(left_result.outcome, CommandOutcome::Decided(Err(_))) {
        &left
    } else {
        &right
    };
    assert_eq!(
        usize::from(matches!(
            left_result.outcome,
            CommandOutcome::Decided(Ok(_))
        )) + usize::from(matches!(
            right_result.outcome,
            CommandOutcome::Decided(Ok(_))
        )),
        1
    );
    h.reconnect().await;
    let retry = execute(&h.pool, "campaign", loser).await.unwrap();
    assert!(retry.replayed);
    rejected(retry.outcome, RejectionCode::StaleDefinition);
    assert_eq!(
        events(&h.pool, "campaign", None, 0, 100)
            .await
            .unwrap()
            .len(),
        3
    );
    let first_body = envelope("body-race", edit(3, "First body"));
    let other_body = envelope("body-race", edit(3, "Other body"));
    let (first, other) = tokio::join!(
        execute(&h.pool, "campaign", &first_body),
        execute(&h.pool, "campaign", &other_body),
    );
    assert_eq!(usize::from(first.is_ok()) + usize::from(other.is_ok()), 1);
    assert!(matches!(
        first.as_ref().err().or(other.as_ref().err()),
        Some(StoreError::IdempotencyConflict)
    ));
    assert_eq!(
        get(&h.pool, "campaign")
            .await
            .unwrap()
            .unwrap()
            .definition_revision,
        4
    );
    h.close().await;
}

#[tokio::test]
async fn failed_create_and_missing_target_receipts_survive_new_pool_pg() {
    let mut h = Harness::new().await;
    let mut invalid = domain::input();
    invalid.nodes[0].acceptance.clear();
    let failed = create(&h.pool, "campaign", &invalid, "bad-create", domain::now())
        .await
        .unwrap();
    assert_eq!(
        failed.outcome,
        CommandOutcome::Decided(Err(Rejection {
            code: RejectionCode::InvalidDefinition,
            target: Some("a".into()),
            definition_error: Some(definition::DefinitionError {
                code: definition::DefinitionErrorCode::EmptyAcceptance,
                target: Some("a".into()),
                path: Vec::new(),
            }),
        }))
    );
    let missing = envelope(
        "missing",
        Command::Start {
            expected_revision: 1,
        },
    );
    assert_eq!(
        execute(&h.pool, "campaign", &missing)
            .await
            .unwrap()
            .outcome,
        CommandOutcome::NotFound
    );
    create(
        &h.pool,
        "campaign",
        &domain::input(),
        "create",
        domain::now(),
    )
    .await
    .unwrap();
    h.reconnect().await;
    let retry = create(&h.pool, "campaign", &invalid, "bad-create", domain::now())
        .await
        .unwrap();
    assert!(retry.replayed);
    assert_eq!(retry.outcome, failed.outcome);
    let retry_with_changed_renderer = super::create(
        &h.pool,
        "campaign",
        &invalid,
        "bad-create",
        domain::now(),
        changed_render,
    )
    .await
    .unwrap();
    assert_eq!(retry_with_changed_renderer.response, failed.response);
    assert_eq!(retry_with_changed_renderer.response.status, 409);
    assert_eq!(
        execute(&h.pool, "campaign", &missing)
            .await
            .unwrap()
            .outcome,
        CommandOutcome::NotFound
    );
    assert!(matches!(
        create(
            &h.pool,
            "campaign",
            &domain::input(),
            "bad-create",
            domain::now()
        )
        .await,
        Err(StoreError::IdempotencyConflict)
    ));
    h.close().await;
}

#[tokio::test]
async fn rollback_and_receipt_insert_failure_leave_no_partial_action_or_event_pg() {
    let h = Harness::new().await;
    let state = domain::with_action(ActionState::Claimed, false);
    let binding = domain::binding(&state);
    h.seed(state.clone()).await;
    let command = Command::AdvanceAction {
        action_id: "action".into(),
        binding: Box::new(binding),
        state: ActionState::StartPending,
    };
    let mut tx = h.pool.begin().await.unwrap();
    applied(
        apply(
            &mut tx,
            "campaign",
            "rolled-back",
            domain::now(),
            Request::Command(&command),
            render,
        )
        .await
        .unwrap()
        .outcome,
    );
    assert_eq!(get(&h.pool, "campaign").await.unwrap(), Some(state.clone()));
    tx.rollback().await.unwrap();
    assert!(matches!(
        super::execute(
            &h.pool,
            "campaign",
            &envelope("render-failure", command.clone()),
            failed_render
        )
        .await,
        Err(StoreError::InconsistentState)
    ));
    assert_eq!(get(&h.pool, "campaign").await.unwrap(), Some(state.clone()));
    // A failure after all state writes must roll the entire command back.
    sqlx::query("CREATE FUNCTION reject_receipt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected receipt failure'; END $$")
        .execute(&h.pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_receipt BEFORE INSERT ON server_campaign_receipts FOR EACH ROW EXECUTE FUNCTION reject_receipt()")
        .execute(&h.pool).await.unwrap();
    assert!(matches!(
        execute(&h.pool, "campaign", &envelope("injected", command.clone())).await,
        Err(StoreError::Sql(_))
    ));
    assert_eq!(get(&h.pool, "campaign").await.unwrap(), Some(state));
    assert_eq!(
        events(&h.pool, "campaign", None, 0, 100)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        action_history(&h.pool, "campaign", "run", "action", 0, 100)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        run_history(&h.pool, "campaign", "run", 0, 100)
            .await
            .unwrap()
            .len(),
        1
    );
    let receipts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM server_campaign_receipts")
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(receipts, 0);
    sqlx::query("DROP TRIGGER reject_receipt ON server_campaign_receipts")
        .execute(&h.pool)
        .await
        .unwrap();
    applied(send(&h.pool, "injected", command).await);
    h.close().await;
}

#[tokio::test]
async fn run_lock_holds_command_until_commit_and_replays_response_loss_pg() {
    let h = Harness::new().await;
    h.seed(domain::with_action(ActionState::Claimed, false))
        .await;
    let command = stop(StopMode::Immediate);
    let mut tx = h.pool.begin().await.unwrap();
    let first = apply(
        &mut tx,
        "campaign",
        "stop",
        domain::now(),
        Request::Command(&command),
        render,
    )
    .await
    .unwrap();
    let pool = h.pool.clone();
    let mut pending =
        tokio::spawn(async move { execute(&pool, "campaign", &envelope("stop", command)).await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut pending)
            .await
            .is_err()
    );
    tx.commit().await.unwrap();
    let second = tokio::time::timeout(std::time::Duration::from_secs(10), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(second.replayed);
    assert_eq!(first.outcome, second.outcome);
    assert_eq!(
        events(&h.pool, "campaign", Some("run"), 0, 100)
            .await
            .unwrap()
            .len(),
        2
    );
    h.close().await;
}

#[tokio::test]
async fn prune_preserves_execution_failure_evidence_pins_actions_and_receipts_pg() {
    let mut h = Harness::new().await;
    let mut state = domain::with_action(ActionState::Reconciled, true);
    let binding = domain::binding(&state);
    let run = state.run.as_mut().unwrap();
    run.actions.get_mut("action").unwrap().initiation_receipt = Some("external-effect".into());
    run.actions.get_mut("action").unwrap().reconciliation = Some("effect observed".into());
    run.attempts.get_mut("attempt").unwrap().outcome = Some(AttemptOutcome::QualityFailed);
    run.sessions.push(SessionRecord {
        identity: binding.session.clone(),
        attempt_id: "attempt".into(),
        observation: SessionObservation::Gone {
            observed_at: domain::now(),
            evidence: "exact process absent".into(),
        },
    });
    state.nodes.get_mut("a").unwrap().quality_failures = 3;
    state.nodes.get_mut("a").unwrap().main_integrated = true;
    let original_pin = state.definition.clone();
    h.seed(state).await;
    let source = EvidenceRecord {
        id: "evidence".into(),
        binding: binding.clone(),
        kind: "test".into(),
        source: "fixture-log".into(),
        document: serde_json::json!({"tests": 16, "failed": 1, "server_acceptance": "unimplemented"}),
        recorded_at: domain::now(),
    };
    let historical_acceptance = EvidenceRecord {
        id: "acceptance-evidence".into(),
        kind: "historical-acceptance-fixture".into(),
        document: serde_json::json!({"ac_id": "ac-a", "fixture_accepted": true, "production_acceptance": "requires F3"}),
        ..source.clone()
    };
    for record in [&source, &historical_acceptance] {
        sqlx::query("INSERT INTO server_campaign_evidence (campaign_id, run_id, attempt_id, evidence_id, document) VALUES ($1, $2, $3, $4, $5)")
            .bind("campaign").bind("run").bind("attempt").bind(&record.id).bind(Json(record)).execute(&h.pool).await.unwrap();
    }
    let failure = envelope(
        "failure",
        Command::Start {
            expected_revision: 1,
        },
    );
    let failure_reply = execute(&h.pool, "campaign", &failure).await.unwrap();
    rejected(
        failure_reply.outcome.clone(),
        RejectionCode::ExecutionUnavailable,
    );
    applied(send(&h.pool, "stop", stop(StopMode::Immediate)).await);
    applied(
        send(
            &h.pool,
            "ack",
            Command::AcknowledgeFence {
                expected_run: binding.run_id.clone(),
                target: binding.session.gate.clone(),
                control_version: 2,
            },
        )
        .await,
    );
    applied(send(&h.pool, "fenced", Command::ConfirmStopFence).await);
    applied(send(&h.pool, "stopped", Command::ConfirmStopped).await);
    for revision in 1..=12 {
        let mut next = domain::input();
        next.goal = format!("New requirements {revision}");
        next.nodes.retain(|n| n.id != "a");
        next.nodes.iter_mut().for_each(|n| n.dependencies.clear());
        let command = Command::EditDefinition {
            expected_revision: revision,
            definition: next,
            change: Some(ChangeReason {
                reference: "https://github.com/itismyfield/AgentDesk/issues/7438".into(),
                explanation: "Replace failed node".into(),
            }),
        };
        applied(send(&h.pool, &format!("edit-{revision}"), command).await);
    }
    h.reconnect().await;
    let kept = revisions(&h.pool, "campaign").await.unwrap();
    assert_eq!(
        kept.iter().map(|r| r.revision).collect::<Vec<_>>(),
        (4..=13).rev().collect::<Vec<_>>()
    );
    let recorded = attempts(&h.pool, "campaign", "run").await.unwrap();
    assert_eq!(recorded[0].definition_pin, original_pin);
    assert_eq!(recorded[0].attempt.binding, binding);
    assert_eq!(
        recorded[0].attempt.outcome,
        Some(AttemptOutcome::QualityFailed)
    );
    assert_eq!(
        evidence(&h.pool, "campaign", "run").await.unwrap(),
        vec![historical_acceptance, source]
    );
    let old = run_history(&h.pool, "campaign", "run", 0, 500)
        .await
        .unwrap();
    assert_eq!(old.len(), 17);
    assert_eq!(
        events(&h.pool, "campaign", None, 0, 500)
            .await
            .unwrap()
            .len(),
        17
    );
    assert_eq!(old[0].nodes["a"].quality_failures, 3);
    assert!(old[0].nodes["a"].main_integrated);
    assert_eq!(old[0].definition_pin, original_pin);
    assert!(
        !get(&h.pool, "campaign")
            .await
            .unwrap()
            .unwrap()
            .nodes
            .contains_key("a")
    );
    assert!(
        action_history(&h.pool, "campaign", "run", "action", 0, 500)
            .await
            .unwrap()
            .iter()
            .all(|a| a.initiation_receipt.as_deref() == Some("external-effect"))
    );
    let retry = execute(&h.pool, "campaign", &failure).await.unwrap();
    assert!(retry.replayed);
    assert_eq!(retry.outcome, failure_reply.outcome);
    h.close().await;
}

#[tokio::test]
async fn immutable_attempt_binding_and_action_target_mismatch_roll_back_pg() {
    let h = Harness::new().await;
    let original = domain::with_action(ActionState::Claimed, false);
    h.seed(original.clone()).await;
    for rewrite_attempt in [true, false] {
        let mut wrong = original.clone();
        wrong.event_sequence += 1;
        if rewrite_attempt {
            wrong
                .run
                .as_mut()
                .unwrap()
                .attempts
                .get_mut("attempt")
                .unwrap()
                .binding
                .candidate_sha = "rewritten".into();
        } else {
            wrong
                .run
                .as_mut()
                .unwrap()
                .actions
                .get_mut("action")
                .unwrap()
                .target
                .incarnation = "another-boot".into();
        }
        let event = Event {
            sequence: 2,
            run_id: Some("run".into()),
            command_id: "fixture-rebind".into(),
            observed_at: domain::now(),
            kind: EventKind::ActionChanged {
                action_id: "action".into(),
            },
        };
        let result = CommandResult {
            definition_revision: 1,
            run_id: Some("run".into()),
            control_version: Some(1),
            event_sequence: 2,
            control: CampaignControl::Running,
        };
        let mut tx = h.pool.begin().await.unwrap();
        let decision = Decision {
            state: wrong,
            events: vec![event],
            result,
        };
        assert!(matches!(
            write::persist(&mut tx, Some(&original), &decision).await,
            Err(StoreError::InconsistentState)
        ));
        tx.rollback().await.unwrap();
        assert_eq!(
            get(&h.pool, "campaign").await.unwrap(),
            Some(original.clone())
        );
        assert_eq!(
            run_history(&h.pool, "campaign", "run", 0, 100)
                .await
                .unwrap()
                .len(),
            1
        );
    }
    h.close().await;
}

#[tokio::test]
async fn resume_and_retry_preserve_failure_budget_and_create_no_legacy_work_pg() {
    let h = Harness::new().await;
    let mut state = domain::running();
    state.control = CampaignControl::Stopped;
    state.nodes.get_mut("a").unwrap().quality_failures = 3;
    state.nodes.get_mut("a").unwrap().disposition = NodeDisposition::Failed;
    state
        .run
        .as_mut()
        .unwrap()
        .attempts
        .get_mut("attempt")
        .unwrap()
        .outcome = Some(AttemptOutcome::QualityFailed);
    h.seed(state.clone()).await;
    for (key, command) in [
        (
            "resume",
            Command::Resume {
                expected_revision: 1,
                expected_run: "run".into(),
            },
        ),
        (
            "retry",
            Command::PrepareRetry {
                expected_revision: 1,
                expected_run: "run".into(),
                node_id: "a".into(),
            },
        ),
    ] {
        rejected(
            send(&h.pool, key, command).await,
            RejectionCode::ExecutionUnavailable,
        );
    }
    assert_eq!(get(&h.pool, "campaign").await.unwrap(), Some(state));
    for sql in [
        "SELECT COUNT(*) FROM auto_queue_entries",
        "SELECT COUNT(*) FROM task_dispatches",
        "SELECT COUNT(*) FROM dispatch_outbox",
    ] {
        let count: i64 = sqlx::query_scalar(sql).fetch_one(&h.pool).await.unwrap();
        assert_eq!(count, 0);
    }
    h.close().await;
}

#[tokio::test]
async fn execution_stays_unavailable_and_legacy_crud_cas_issue_sync_unchanged_pg() {
    use crate::db::campaigns::{self as legacy, issue_sync};
    let h = Harness::new().await;
    let mut input: legacy::CampaignInput = serde_json::from_value(serde_json::json!({
        "title": "Legacy", "status": "active", "round": 1, "auto_queue": true,
        "nodes": [{"id": "a", "title": "A", "status": "pending", "stage": "plan", "round": 1,
        "issue_url": "https://github.com/itismyfield/AgentDesk/issues/7438"}]
    }))
    .unwrap();
    let created = legacy::create(&h.pool, "campaign".into(), input.clone())
        .await
        .unwrap();
    input.auto_queue = None;
    let saved = legacy::replace(&h.pool, "campaign", created.revision, input.clone())
        .await
        .unwrap();
    assert!(saved.auto_queue);
    assert!(matches!(
        legacy::replace(&h.pool, "campaign", created.revision, input).await,
        Err(legacy::CampaignError::Conflict)
    ));
    h.seed(domain::prepared()).await;
    let before = get(&h.pool, "campaign").await.unwrap().unwrap();
    assert_eq!(before.definition.input().nodes.len(), 3);
    assert_eq!(
        before.definition.input().nodes[0].issue_url,
        before.definition.input().nodes[1].issue_url
    );
    rejected(
        send(
            &h.pool,
            "start",
            Command::Start {
                expected_revision: 1,
            },
        )
        .await,
        RejectionCode::ExecutionUnavailable,
    );
    let facts = [(
        7438,
        issue_sync::IssueFact {
            open: false,
            state_reason: Some("NOT_PLANNED".into()),
            closed_at: Some(Utc::now()),
            closing: Some(issue_sync::ClosingEvent {
                at: Utc::now(),
                closer: issue_sync::Closer::Nobody {
                    actor: Some("fixture".into()),
                },
            }),
            fetched_at: Some(Utc::now()),
        },
    )]
    .into_iter()
    .collect();
    let sync = issue_sync::apply_issue_facts(
        &h.pool,
        "itismyfield/AgentDesk",
        &facts,
        &["campaign".into()],
    )
    .await;
    assert_eq!(sync.errors, 0);
    assert_eq!(sync.changed_nodes, 1);
    assert_eq!(get(&h.pool, "campaign").await.unwrap().unwrap(), before);
    let changed = legacy::get(&h.pool, "campaign").await.unwrap();
    assert_eq!(changed.nodes[0].input.status, legacy::NodeStatus::Skipped);
    assert_eq!(legacy::history(&h.pool, "campaign").await.unwrap().len(), 3);
    for sql in [
        "SELECT COUNT(*) FROM auto_queue_entries",
        "SELECT COUNT(*) FROM task_dispatches",
        "SELECT COUNT(*) FROM dispatch_outbox",
    ] {
        let count: i64 = sqlx::query_scalar(sql).fetch_one(&h.pool).await.unwrap();
        assert_eq!(count, 0);
    }
    h.close().await;
}

/// A production database that already ran main through 0161 takes only this branch's later files.
#[tokio::test]
async fn later_branch_migrations_upgrade_a_main_database_already_at_0161_pg() {
    use crate::db::postgres;
    use sqlx::migrate::Migrator;

    type MigrationRow = (i64, Vec<u8>, bool, DateTime<Utc>, i64);
    const RECORDS: &str = "SELECT version, checksum, success, installed_on, execution_time FROM _sqlx_migrations ORDER BY version";
    let fixture = TestPostgresDb::create().await;
    let pool = postgres::connect_test_pool(&fixture.database_url, "main at 0161 upgrade")
        .await
        .unwrap();
    let embedded = sqlx::migrate!("./migrations/postgres");
    let campaigns = 161;
    let main = Migrator::with_migrations(
        embedded
            .iter()
            .filter(|m| m.version <= campaigns)
            .cloned()
            .collect(),
    );
    main.run(&pool).await.unwrap();
    let before = sqlx::query_as::<_, MigrationRow>(RECORDS)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(before.last().unwrap().0, campaigns);
    let mut later: Vec<i64> = embedded
        .iter()
        .map(|m| m.version)
        .filter(|v| *v > campaigns)
        .collect();
    later.dedup();
    assert!(!later.is_empty());
    assert_eq!(
        postgres::migration_status(&pool)
            .await
            .unwrap()
            .pending_versions,
        later
    );

    postgres::migrate(&pool).await.unwrap();
    let after = sqlx::query_as::<_, MigrationRow>(RECORDS)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(
        after[..before.len()],
        before[..],
        "main's journal stays as it was"
    );
    let appended: Vec<i64> = after[before.len()..].iter().map(|row| row.0).collect();
    assert_eq!(appended, later, "later files apply after 0161, in order");
    for row in &after[before.len()..] {
        let migration = embedded.iter().find(|m| m.version == row.0).unwrap();
        assert!(row.2);
        assert_eq!(row.1.as_slice(), migration.checksum.as_ref());
    }
    let schema: bool = sqlx::query_scalar(
        "SELECT to_regclass('channel_home_preferences') IS NOT NULL
            AND EXISTS(SELECT 1 FROM information_schema.columns WHERE table_name='intake_outbox' AND column_name='routing_plan')
            AND EXISTS(SELECT 1 FROM information_schema.columns WHERE table_name='intake_session_owners' AND column_name='routing_notice_outbox_id')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        schema,
        "the later files' schema must exist after the upgrade"
    );
    let status = postgres::migration_status(&pool).await.unwrap();
    assert!(status.pending_versions.is_empty());
    assert!(status.missing_from_resolved.is_empty());
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test]
async fn migration_upgrades_main_schema_and_preserves_legacy_records_pg() {
    use crate::db::{campaigns as legacy, postgres};
    use sqlx::migrate::Migrator;

    let fixture = TestPostgresDb::create().await;
    let pool = postgres::connect_test_pool(&fixture.database_url, "server campaign upgrade")
        .await
        .unwrap();
    let embedded = sqlx::migrate!("./migrations/postgres");
    let version = 161;
    let main = Migrator::with_migrations(
        embedded
            .iter()
            .filter(|m| m.version < version)
            .cloned()
            .collect(),
    );
    let main_max = main.iter().map(|m| m.version).max().unwrap();
    assert_eq!(main_max, 156);
    assert!(version > main_max);
    main.run(&pool).await.unwrap();
    let input = serde_json::from_value(serde_json::json!({
        "title": "Legacy before upgrade", "status": "active", "round": 1,
        "nodes": [{"id": "a", "title": "A", "status": "pending", "stage": "plan", "round": 1}]
    }))
    .unwrap();
    let saved = legacy::create(&pool, "legacy-before-upgrade".into(), input)
        .await
        .unwrap();
    let saved = serde_json::to_value(saved).unwrap();
    type MigrationRow = (i64, Vec<u8>, bool, DateTime<Utc>, i64);
    const RECORDS: &str = "SELECT version, checksum, success, installed_on, execution_time FROM _sqlx_migrations ORDER BY version";
    const TABLES: &str = "SELECT COUNT(*) FROM pg_tables WHERE schemaname = 'public' AND tablename LIKE 'server_campaign_%'";
    let before = sqlx::query_as::<_, MigrationRow>(RECORDS)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(before.last().unwrap().0, main_max);
    assert!(before.iter().all(|row| row.2));
    assert_eq!(
        sqlx::query_scalar::<_, i64>(TABLES)
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    let mut pending: Vec<i64> = embedded
        .iter()
        .map(|m| m.version)
        .filter(|v| *v >= version)
        .collect();
    pending.dedup();
    assert_eq!(
        postgres::migration_status(&pool)
            .await
            .unwrap()
            .pending_versions,
        pending
    );

    // Run the production migrator over real main records, without editing its journal.
    postgres::migrate(&pool).await.unwrap();
    let after = sqlx::query_as::<_, MigrationRow>(RECORDS)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(
        after
            .iter()
            .filter(|row| row.0 < version)
            .cloned()
            .collect::<Vec<_>>(),
        before
    );
    let added = after.iter().find(|row| row.0 == version).unwrap();
    let migration = embedded.iter().find(|m| m.version == version).unwrap();
    assert!(added.2);
    assert_eq!(added.1.as_slice(), migration.checksum.as_ref());
    assert_eq!(after.len(), before.len() + pending.len());
    assert_eq!(
        sqlx::query_scalar::<_, i64>(TABLES)
            .fetch_one(&pool)
            .await
            .unwrap(),
        11
    );
    assert_eq!(
        serde_json::to_value(legacy::get(&pool, "legacy-before-upgrade").await.unwrap()).unwrap(),
        saved
    );
    assert_eq!(
        serde_json::to_value(
            legacy::history(&pool, "legacy-before-upgrade")
                .await
                .unwrap()
        )
        .unwrap(),
        serde_json::json!([saved])
    );
    let status = postgres::migration_status(&pool).await.unwrap();
    assert!(status.pending_versions.is_empty());
    assert!(status.missing_from_resolved.is_empty());
    postgres::migrate(&pool).await.unwrap();
    assert_eq!(
        sqlx::query_as::<_, MigrationRow>(RECORDS)
            .fetch_all(&pool)
            .await
            .unwrap(),
        after
    );
    assert!(
        postgres::migration_status(&pool)
            .await
            .unwrap()
            .pending_versions
            .is_empty()
    );
    pool.close().await;
    fixture.drop().await;
}
