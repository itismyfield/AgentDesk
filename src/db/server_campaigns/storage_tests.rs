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
                records: Vec::new(),
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
            records: Vec::new(),
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
    // Each later file leaves (or, with `!`, drops) at least one object of its own,
    // so a partial apply cannot pass.
    let expected: [(i64, &[&str]); 28] = [
        (
            163,
            &["table:channel_home_preferences", "constraint:chp_mode_home"],
        ),
        (
            164,
            &[
                "column:intake_outbox.routing_state",
                "constraint:intake_routing_custody_shape",
                "function:intake_routing_bundle_pinned",
            ],
        ),
        (
            165,
            &[
                "column:message_outbox.output_origin",
                "constraint:message_outbox_output_origin_object",
            ],
        ),
        (
            166,
            &[
                "column:intake_session_owners.routing_cleanup",
                "constraint:execution_cleanup_shape",
                "function:intake_routing_bundle_retention",
            ],
        ),
        (
            167,
            &[
                "column:intake_outbox.routing_plan",
                "column:intake_session_owners.routing_notice_outbox_id",
                "constraint:input_fallback_plan_object",
                "constraint:input_fallback_notice_pointer",
            ],
        ),
        (
            168,
            &[
                "!table:meetings",
                "!table:meeting_transcripts",
                "!table:agent_quality_daily",
                "!table:quality_regression_cooldowns",
                "!table:agent_prompt_commits",
            ],
        ),
        (
            169,
            &[
                "!table:voice_turn_link",
                "!table:voice_background_handoff_meta",
                "!table:voice_transcript_announcement_meta",
                "!function:voice_background_handoff_meta_touch_updated_at",
                "!function:voice_transcript_announcement_meta_touch_updated_at",
            ],
        ),
        (
            170,
            &[
                "index:idx_routine_runs_routine_newest",
                "!index:idx_routine_runs_routine_id",
            ],
        ),
        (171, &["column:message_outbox.producer_turn_nonce"]),
        (172, &["!column:agents.xp"]),
        (173, &["column:message_outbox.body_census"]),
        (174, &["index:idx_task_dispatches_card_created"]),
        (175, &["index:idx_auto_queue_entries_card"]),
        (
            176,
            &[
                "!index:delivery_journal_obligation_order",
                "index:delivery_journal_obligation_slot_unique",
            ],
        ),
        (
            177,
            &[
                "!index:kakao_calendar_operations_target",
                "index:kakao_calendar_operations_target_id_revision_key",
            ],
        ),
        (
            178,
            &[
                "!index:idx_arce_channel_seq_desc",
                "index:uq_arce_channel_seq",
            ],
        ),
        (
            179,
            &["!index:iso_watermark", "index:iso_unique_generation"],
        ),
        (
            180,
            &[
                "!index:idx_aq_entry_dispatch_history_entry",
                "index:auto_queue_entry_dispatch_history_entry_id_dispatch_id_key",
            ],
        ),
        (
            181,
            &[
                "!index:idx_aci_card_id",
                "index:automation_candidate_iterations_card_id_iteration_key",
            ],
        ),
        (
            182,
            &[
                "!index:idx_turn_lifecycle_events_turn_id",
                "index:idx_turn_lifecycle_events_turn_channel_created_at",
            ],
        ),
        (
            183,
            &[
                "!index:idx_aq_phase_gates_run_phase",
                "index:uq_aq_phase_gates_run_phase_dispatch_key",
            ],
        ),
        (
            184,
            &[
                "constraint:auto_queue_entries_status_check",
                "constraint:auto_queue_runs_status_check",
                "constraint:message_outbox_status_check",
                "constraint:dispatch_outbox_status_check",
                "constraint:routines_status_check",
                "constraint:routine_runs_status_check",
            ],
        ),
        (
            185,
            &["constraint:auto_queue_entries_deploy_gate_unavailable_check"],
        ),
        (
            186,
            &[
                "!table:db_table_metadata",
                "!table:dispatch_queue",
                "!table:test_results",
                "!table:deferred_hooks",
                "!table:channel_recovery_custody",
            ],
        ),
        (187, &["index:idx_kanban_audit_logs_created_at"]),
        (188, &["index:idx_review_tuning_outcomes_created_at"]),
        (189, &["index:idx_review_decisions_decided_at"]),
        (190, &["index:idx_arce_at"]),
    ];
    assert_eq!(
        expected
            .iter()
            .map(|(version, _)| *version)
            .collect::<Vec<_>>(),
        later,
        "every later file needs its own schema check"
    );
    for (version, objects) in expected {
        for object in objects {
            let (kind, name) = object.split_once(':').unwrap();
            let (kind, want) = match kind.strip_prefix('!') {
                Some(kind) => (kind, false),
                None => (kind, true),
            };
            let query = match kind {
                "table" => "SELECT to_regclass($1) IS NOT NULL",
                "column" => {
                    "SELECT EXISTS(SELECT 1 FROM information_schema.columns WHERE table_schema='public' AND table_name=split_part($1,'.',1) AND column_name=split_part($1,'.',2))"
                }
                "constraint" => "SELECT EXISTS(SELECT 1 FROM pg_constraint WHERE conname=$1)",
                "function" => "SELECT EXISTS(SELECT 1 FROM pg_proc WHERE proname=$1)",
                "index" => {
                    "SELECT EXISTS(SELECT 1 FROM pg_indexes WHERE schemaname='public' AND indexname=$1)"
                }
                other => panic!("unknown schema object kind {other}"),
            };
            let present: bool = sqlx::query_scalar(query)
                .bind(name)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(
                present, want,
                "{version:04} must leave the schema as {object}"
            );
        }
    }
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

async fn sequences(pool: &PgPool, campaign: &str, run: Option<&str>) -> Vec<u64> {
    let mut seen = Vec::new();
    let mut after = 0;
    loop {
        let page = events(pool, campaign, run, after, 500).await.unwrap();
        let Some(last) = page.last() else {
            return seen;
        };
        after = i64::try_from(last.sequence).unwrap();
        seen.extend(page.iter().map(|event| event.sequence));
    }
}

/// Retention keeps each campaign's newest 5,000 event and history rows plus its current run's
/// rows; the cursor readers answer from what is left, and protection follows the current run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_keeps_the_newest_campaign_rows_and_the_whole_current_run_pg() {
    let h = Harness::new().await;
    h.seed(domain::with_action(ActionState::Reconciled, true))
        .await;
    // `old-run` holds 100..=5104 (plus `action-b` at 104 and 105), the current `run` 10..=12 and
    // a run-less event 20; `campaign-2` reuses the ids with three `old-run` rows per table.
    sqlx::raw_sql(
        "INSERT INTO server_campaign_runs (campaign_id, run_id, document, nodes, definition_pin)
             SELECT campaign_id, 'old-run', jsonb_set(document, '{id}', '\"old-run\"'), nodes,
                    definition_pin FROM server_campaign_runs WHERE run_id = 'run';
         INSERT INTO server_campaign_actions (campaign_id, run_id, action_id, document)
             SELECT campaign_id, 'old-run', action_id, document FROM server_campaign_actions
             WHERE run_id = 'run'
             UNION ALL SELECT campaign_id, 'old-run', 'action-b', document
             FROM server_campaign_actions WHERE run_id = 'run';
         CREATE TEMP TABLE seed_docs AS
             SELECT (SELECT document FROM server_campaign_events LIMIT 1) AS event_doc,
                    (SELECT document FROM server_campaign_run_history LIMIT 1) AS run_doc,
                    (SELECT nodes FROM server_campaign_run_history LIMIT 1) AS nodes,
                    (SELECT definition_pin FROM server_campaign_run_history LIMIT 1) AS pin,
                    (SELECT document FROM server_campaign_action_history LIMIT 1) AS action_doc;
         DELETE FROM server_campaign_events;
         DELETE FROM server_campaign_run_history;
         DELETE FROM server_campaign_action_history;
         INSERT INTO server_campaign_events (campaign_id, sequence, run_id, document)
             SELECT 'campaign', s.seq, s.run_id, jsonb_set(jsonb_set(d.event_doc, '{sequence}',
                    to_jsonb(s.seq)), '{run_id}', COALESCE(to_jsonb(s.run_id), 'null'::jsonb))
             FROM seed_docs d, (SELECT g AS seq, 'old-run' AS run_id FROM generate_series(100, 5104) g
                 UNION ALL SELECT g, 'run' FROM generate_series(10, 12) g
                 UNION ALL SELECT 20, NULL) s;
         INSERT INTO server_campaign_run_history
                 (campaign_id, run_id, sequence, document, nodes, definition_pin)
             SELECT 'campaign', s.run_id, s.seq, d.run_doc, d.nodes, d.pin
             FROM seed_docs d, (SELECT g AS seq, 'old-run' AS run_id FROM generate_series(100, 5104) g
                 UNION ALL SELECT g, 'run' FROM generate_series(10, 12) g) s;
         INSERT INTO server_campaign_action_history
                 (campaign_id, run_id, action_id, sequence, document)
             SELECT 'campaign', s.run_id, s.action_id, s.seq, d.action_doc
             FROM seed_docs d, (SELECT g AS seq, 'old-run' AS run_id, 'action' AS action_id
                     FROM generate_series(100, 5104) g
                 UNION ALL SELECT g, 'old-run', 'action-b' FROM generate_series(104, 105) g
                 UNION ALL SELECT g, 'run', 'action' FROM generate_series(10, 12) g) s;
         INSERT INTO server_campaign_keys (campaign_id) VALUES ('campaign-2');
         INSERT INTO server_campaign_runs (campaign_id, run_id, document, nodes, definition_pin)
             SELECT 'campaign-2', run_id, document, nodes, definition_pin
             FROM server_campaign_runs WHERE campaign_id = 'campaign';
         INSERT INTO server_campaign_definitions (campaign_id, revision, definition, control,
                 run_id, nodes, event_sequence, last_event_at)
             SELECT 'campaign-2', revision, definition, control, run_id, nodes, event_sequence,
                    last_event_at FROM server_campaign_definitions WHERE campaign_id = 'campaign';
         INSERT INTO server_campaign_actions (campaign_id, run_id, action_id, document)
             SELECT 'campaign-2', run_id, action_id, document FROM server_campaign_actions
             WHERE campaign_id = 'campaign' AND run_id = 'old-run' AND action_id = 'action';
         INSERT INTO server_campaign_events (campaign_id, sequence, run_id, document)
             SELECT 'campaign-2', g, 'old-run', jsonb_set(jsonb_set(d.event_doc, '{sequence}',
                    to_jsonb(g)), '{run_id}', '\"old-run\"')
             FROM seed_docs d, generate_series(10, 12) g;
         INSERT INTO server_campaign_run_history
                 (campaign_id, run_id, sequence, document, nodes, definition_pin)
             SELECT 'campaign-2', 'old-run', g, d.run_doc, d.nodes, d.pin
             FROM seed_docs d, generate_series(10, 12) g;
         INSERT INTO server_campaign_action_history
                 (campaign_id, run_id, action_id, sequence, document)
             SELECT 'campaign-2', 'old-run', 'action', g, d.action_doc
             FROM seed_docs d, generate_series(10, 12) g;",
    )
    .execute(&h.pool)
    .await
    .unwrap();

    let readers = |pool: PgPool| async move {
        (
            sequences(&pool, "campaign", Some("run")).await,
            run_history(&pool, "campaign", "run", 0, 500).await.unwrap(),
            action_history(&pool, "campaign", "run", "action", 0, 500)
                .await
                .unwrap(),
        )
    };
    let second = |pool: PgPool| async move {
        let keys: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT 'run_history', run_id, sequence FROM server_campaign_run_history
             WHERE campaign_id = 'campaign-2'
             UNION ALL SELECT 'action_history:' || action_id, run_id, sequence
             FROM server_campaign_action_history WHERE campaign_id = 'campaign-2'
             ORDER BY 1, 2, 3",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        (sequences(&pool, "campaign-2", None).await, keys)
    };
    let second_before = second(h.pool.clone()).await;
    assert_eq!(second_before.0, vec![10, 11, 12]);
    let before = readers(h.pool.clone()).await;
    let all_before = sequences(&h.pool, "campaign", None).await;
    assert_eq!(before.0, vec![10, 11, 12]);

    let dry = crate::services::maintenance::jobs::db_retention::db_retention_job(&h.pool, true)
        .await
        .unwrap();
    let counts = |report: &crate::services::maintenance::jobs::db_retention::RetentionReport,
                  action: &str| {
        [
            "server_campaign_events",
            "server_campaign_run_history",
            "server_campaign_action_history",
        ]
        .map(|table| report.get(table, action).map(|t| t.rows_affected))
    };
    assert_eq!(counts(&dry, "delete_would"), [Some(6), Some(5), Some(7)]);
    assert_eq!(
        sequences(&h.pool, "campaign", None).await,
        all_before,
        "dry-run deletes nothing"
    );

    let live = crate::services::maintenance::jobs::db_retention::db_retention_job(&h.pool, false)
        .await
        .unwrap();
    assert_eq!(
        readers(h.pool.clone()).await,
        before,
        "the current run reads the same"
    );
    let kept: Vec<u64> = all_before
        .iter()
        .copied()
        .filter(|seq| !(100..=104).contains(seq) && *seq != 20)
        .collect();
    assert_eq!(
        sequences(&h.pool, "campaign", None).await,
        kept,
        "the cursor walks what is left"
    );
    // An `after` older than the oldest kept old-run row starts at that row (the gap is silent).
    let resumed = events(&h.pool, "campaign", None, 13, 1).await.unwrap();
    assert_eq!(resumed[0].sequence, 105);
    let old_actions: Vec<(String, i64)> = sqlx::query_as(
        "SELECT action_id, sequence FROM server_campaign_action_history
         WHERE campaign_id = 'campaign' AND run_id = 'old-run' AND sequence <= 106
         ORDER BY sequence, action_id",
    )
    .fetch_all(&h.pool)
    .await
    .unwrap();
    assert_eq!(
        old_actions,
        vec![("action-b".to_string(), 105), ("action".to_string(), 106)],
        "ties at one sequence keep the larger action id"
    );
    assert_eq!(counts(&live, "delete"), [Some(6), Some(5), Some(7)]);
    assert_eq!(
        second(h.pool.clone()).await,
        second_before,
        "each campaign is ranked on its own, so the small campaign keeps every row"
    );

    // Once the campaign points at `old-run`, `run`'s rows are no longer protected.
    sqlx::query(
        "UPDATE server_campaign_definitions SET run_id = 'old-run' WHERE campaign_id = 'campaign'",
    )
    .execute(&h.pool)
    .await
    .unwrap();
    let next = crate::services::maintenance::jobs::db_retention::db_retention_job(&h.pool, false)
        .await
        .unwrap();
    assert_eq!(counts(&next, "delete"), [Some(3), Some(3), Some(3)]);
    assert!(sequences(&h.pool, "campaign", Some("run")).await.is_empty());
    assert_eq!(second(h.pool.clone()).await, second_before);
    h.close().await;
}
