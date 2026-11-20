use super::*;
use crate::services::campaign_execution::scheduler::next_action;

const PIPELINE: &str = "/server-campaigns/campaign/pipeline";
const VALIDATE: &str = "/server-campaigns/pipeline/validate";
const MIGRATE: &str = "/server-campaigns/campaign/pipeline/migrate";

fn graph() -> Value {
    json!({
        "schema_version":1,"vocabulary_version":1,"evaluator_version":1,"registry_version":1,
        "start":"requirements",
        "states":[{"id":"requirements","kind":"auto_check"},{"id":"complete","kind":"end_success"}],
        "transitions":[transition("finish","requirements","complete",20)]
    })
}

fn transition(id: &str, from: &str, to: &str, priority: u32) -> Value {
    json!({"id":id,"from":from,"to":to,"priority":priority,
        "condition":{"field":"state.outcome","op":"eq","value":"succeeded"}})
}

async fn diagnostic(pipeline: Value, code: &str, path: &str, state: Value, transition: Value) {
    let (app, _dir) = app(None);
    let (status, body) = request(&app, "POST", VALIDATE, Some(json!({"pipeline":pipeline}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let expected = json!({"code":code,"path":path,"state_id":state,"transition_id":transition});
    assert!(
        body["errors"].as_array().unwrap().contains(&expected),
        "{body}"
    );
}

#[tokio::test]
async fn pipeline_v1_has_one_existing_start() {
    let mut pipeline = graph();
    pipeline.as_object_mut().unwrap().remove("start");
    diagnostic(pipeline, "START_MISSING", "/start", json!(""), Value::Null).await;
    let mut pipeline = graph();
    pipeline["start"] = json!("missing");
    diagnostic(
        pipeline,
        "START_NOT_FOUND",
        "/start",
        json!("missing"),
        Value::Null,
    )
    .await;
    let mut pipeline = graph();
    pipeline["states"][1]["id"] = json!("requirements");
    diagnostic(
        pipeline,
        "INVALID_STATE_ID",
        "/states/1/id",
        json!("requirements"),
        Value::Null,
    )
    .await;
}

#[tokio::test]
async fn pipeline_v2_rejects_unreachable_states() {
    let mut pipeline = graph();
    pipeline["states"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"isolated","kind":"end_failure"}));
    diagnostic(
        pipeline,
        "STATE_UNREACHABLE",
        "/states/2",
        json!("isolated"),
        Value::Null,
    )
    .await;
}

#[tokio::test]
async fn pipeline_v3_names_bad_targets_and_references() {
    let mut pipeline = graph();
    pipeline["transitions"][0]["to"] = json!("missing");
    diagnostic(
        pipeline.clone(),
        "TRANSITION_TARGET_MISSING",
        "/transitions/0/to",
        json!("missing"),
        json!("finish"),
    )
    .await;
    pipeline["transitions"][0]["retry"] =
        json!({"budget_id":"repairs","max":1,"then_to":"nowhere"});
    diagnostic(
        pipeline,
        "RETRY_TARGET_MISSING",
        "/transitions/0/retry/then_to",
        json!("nowhere"),
        json!("finish"),
    )
    .await;
}

#[tokio::test]
async fn pipeline_v4_requires_exits_and_keeps_ends_terminal() {
    let mut pipeline = graph();
    pipeline["transitions"] = json!([transition("back", "complete", "requirements", 20)]);
    diagnostic(
        pipeline.clone(),
        "STATE_NO_EXIT",
        "/states/0",
        json!("requirements"),
        Value::Null,
    )
    .await;
    diagnostic(
        pipeline,
        "TERMINAL_HAS_EXIT",
        "/transitions/0/from",
        json!("complete"),
        json!("back"),
    )
    .await;
}

#[tokio::test]
async fn pipeline_v5_rejects_unbounded_cycles_beside_bounded_retries() {
    let mut pipeline = graph();
    let mut bounded = transition("retry", "requirements", "requirements", 10);
    bounded["retry"] = json!({"budget_id":"repairs","max":3,"then_to":"complete"});
    pipeline["transitions"]
        .as_array_mut()
        .unwrap()
        .push(bounded);
    let (app, _dir) = app(None);
    assert_eq!(
        request(&app, "POST", VALIDATE, Some(json!({"pipeline":pipeline}))).await,
        (StatusCode::OK, json!({"errors":[],"warnings":[]}))
    );
    pipeline["transitions"]
        .as_array_mut()
        .unwrap()
        .push(transition("free", "requirements", "requirements", 11));
    diagnostic(
        pipeline.clone(),
        "UNBOUNDED_CYCLE",
        "/transitions/2/to",
        json!("requirements"),
        json!("free"),
    )
    .await;
    pipeline["transitions"].as_array_mut().unwrap().pop();
    pipeline["transitions"][1]["retry"]["then_to"] = json!("requirements");
    diagnostic(
        pipeline,
        "UNBOUNDED_CYCLE",
        "/transitions/1/retry/then_to",
        json!("requirements"),
        json!("retry"),
    )
    .await;
}

#[tokio::test]
async fn pipeline_v6_requires_reachable_success_and_a_way_out() {
    let mut pipeline = graph();
    pipeline["states"][1]["kind"] = json!("end_failure");
    diagnostic(
        pipeline.clone(),
        "SUCCESS_UNREACHABLE",
        "/start",
        json!("requirements"),
        Value::Null,
    )
    .await;
    pipeline["transitions"][0]["to"] = json!("requirements");
    diagnostic(
        pipeline,
        "NO_TERMINAL_PATH",
        "/states/0",
        json!("requirements"),
        Value::Null,
    )
    .await;
}

#[tokio::test]
async fn pipeline_v7_checks_capability_operator_and_enum_at_nested_path() {
    let mut pipeline = graph();
    pipeline["transitions"][0]["condition"] = json!({"all":[
        {"field":"review.verdict","op":"lt","value":"unknown"}
    ]});
    for (code, field) in [
        ("FIELD_NOT_ALLOWED", "field"),
        ("OP_NOT_ALLOWED", "op"),
        ("VALUE_TYPE_MISMATCH", "value"),
    ] {
        diagnostic(
            pipeline.clone(),
            code,
            &format!("/transitions/0/condition/all/0/{field}"),
            json!("requirements"),
            json!("finish"),
        )
        .await;
    }
    pipeline["states"][0]["kind"] = json!("unknown_kind");
    let (app, _dir) = app(None);
    assert_eq!(
        request(&app, "POST", VALIDATE, Some(json!({"pipeline":pipeline})))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn pipeline_vocabulary_uses_dashboard_auth_and_advertises_typed_capability() {
    let (app, _dir) = app(None);
    let path = "/server-campaigns/pipeline/condition-fields?kind=agent_task&executor=r8.review";
    assert_eq!(
        raw(&app, "GET", path, String::new(), None, false).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (status, body) = request(&app, "GET", path, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["fields"].as_array().unwrap().contains(&json!({
        "field":"review.verdict","type":"enum","operators":["eq","ne"],"enum_values":["clean","issues"],
        "source":"review","subject_binding":"candidate_producer","freshness":"current_candidate","available":true
    })));
}

fn save_body(key: &str, revision: u64, pipeline: Value) -> Option<Value> {
    Some(json!({"idempotency_key":key,"expected_definition_revision":revision,"pipeline":pipeline}))
}

#[tokio::test]
async fn pipeline_revision_cas_receipts_and_legacy_put_guard_pg() {
    let mut h = Harness::new().await;
    let mut create = create_body();
    create["definition"]["pipeline"] = graph();
    let (status, body) = request(&h.app, "POST", "/server-campaigns", Some(create)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let mut malformed = graph();
    malformed["start"] = json!("missing");
    let invalid = save_body("invalid", 1, malformed.clone());
    let (status, body) = request(&h.app, "PUT", PIPELINE, invalid.clone()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        body["context"]["definition_error"]["pipeline_errors"][0]["state_id"],
        "missing"
    );
    let mut full_invalid = edit_body("full-invalid", 1, "Campaign");
    full_invalid["definition"]["pipeline"] = malformed;
    assert_eq!(
        request(
            &h.app,
            "PUT",
            "/server-campaigns/campaign/definition",
            Some(full_invalid)
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let a = save_body("edit-a", 1, graph());
    let b = save_body("edit-b", 1, graph());
    let (a_result, b_result) = tokio::join!(
        request(&h.app, "PUT", PIPELINE, a.clone()),
        request(&h.app, "PUT", PIPELINE, b.clone())
    );
    let (ok, replay, stale) = if a_result.0 == StatusCode::OK {
        (a_result, a, b_result)
    } else {
        (b_result, b, a_result)
    };
    assert_eq!(ok.0, StatusCode::OK, "{ok:?}");
    assert_eq!(ok.1["result"]["definition_revision"], 2);
    assert_eq!(stale.0, StatusCode::CONFLICT);
    assert_eq!(stale.1["context"]["rejection"], "stale_definition");
    assert_eq!(
        request(&h.app, "PUT", PIPELINE, invalid).await,
        (status, body)
    );
    h.reconnect().await;
    assert_eq!(request(&h.app, "PUT", PIPELINE, replay).await, ok);
    let (status, body) = request(
        &h.app,
        "PUT",
        "/server-campaigns/campaign/definition",
        Some(edit_body("old", 2, "Campaign")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body["context"]["rejection"],
        "definition_schema_upgrade_required"
    );
    h.close().await;
}

#[tokio::test]
async fn pipeline_running_or_occupied_state_uses_fresh_transaction_snapshot_pg() {
    let h = Harness::new().await;
    let mut original = domain::running();
    original.nodes.get_mut("a").unwrap().stage = Stage::Requirements;
    original
        .run
        .as_mut()
        .unwrap()
        .attempts
        .get_mut("attempt")
        .unwrap()
        .outcome = Some(AttemptOutcome::Passed);
    h.seed_running_state(&original).await;
    let (status, body) = request(
        &h.app,
        "POST",
        &format!("{PIPELINE}/validate"),
        Some(json!({"pipeline":graph()})),
    )
    .await;
    assert_eq!(
        (status, body),
        (StatusCode::OK, json!({"errors":[],"warnings":[]}))
    );
    let (status, body) = request(
        &h.app,
        "PUT",
        PIPELINE,
        save_body("while-running", 1, graph()),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["context"]["rejection"], "invalid_state");
    original.nodes.get_mut("a").unwrap().stage = Stage::Review;
    sqlx::query("UPDATE server_campaign_definitions SET nodes = $1, control='\"stopped\"'::jsonb WHERE campaign_id='campaign'")
        .bind(sqlx::types::Json(&original.nodes)).execute(&h.pool).await.unwrap();
    let (status, body) = request(
        &h.app,
        "POST",
        &format!("{PIPELINE}/validate"),
        Some(json!({"pipeline":graph()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["errors"],
        json!([{"code":"STATE_IN_USE","state_id":"review","transition_id":null,"path":"/states"}])
    );
    let (status, body) = request(&h.app, "PUT", PIPELINE, save_body("occupied", 1, graph())).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["context"]["rejection"], "state_in_use");
    let mut full = edit_body("full-occupied", 1, "Campaign");
    full["definition"]["pipeline"] = graph();
    assert_eq!(
        request(
            &h.app,
            "PUT",
            "/server-campaigns/campaign/definition",
            Some(full)
        )
        .await
        .1["context"]["rejection"],
        "state_in_use"
    );
    assert_eq!(
        store::get(&h.pool, "campaign")
            .await
            .unwrap()
            .unwrap()
            .definition_revision,
        1
    );
    h.close().await;
}

#[tokio::test]
async fn pipeline_template_migration_preserves_stages_pins_and_scheduler_pg() {
    let mut h = Harness::new().await;
    let mut state = domain::running();
    use crate::domain::campaign::{
        decision_item::{ApprovalBinding, ApprovalTarget, ForumApproval},
        workflow_options::{ApprovalGate, WorkflowOptions},
    };
    let mut input = state.definition.input().clone();
    input.workflow_options = Some(WorkflowOptions {
        implementation: true,
        ..Default::default()
    });
    state.definition = crate::domain::campaign::definition::validate_definition(input).unwrap();
    let binding = ApprovalBinding {
        run_id: "run".into(),
        node_id: "a".into(),
        gate: ApprovalGate::Implementation,
        definition_revision: 1,
        policy_hash: "approval-pin".into(),
        target: ApprovalTarget::Candidate {
            base_sha: "base".into(),
            head_sha: "candidate".into(),
        },
    };
    let approvals = &mut state.run.as_mut().unwrap().approvals;
    approvals.open(binding.clone()).unwrap();
    approvals
        .approve(
            &binding,
            ForumApproval {
                decision_ref: "approval".into(),
                response_ref: "https://forum.example/decision/1".into(),
                response_at: domain::now(),
            },
            domain::now(),
        )
        .unwrap();
    state.control = CampaignControl::Stopped;
    state
        .run
        .as_mut()
        .unwrap()
        .attempts
        .get_mut("attempt")
        .unwrap()
        .outcome = Some(AttemptOutcome::Passed);
    state.nodes.get_mut("a").unwrap().stage = Stage::Implementation;
    h.seed_running_state(&state).await;
    let before = store::get(&h.pool, "campaign").await.unwrap().unwrap();
    let template_path = "/server-campaigns/pipeline/templates/campaign-development-9/1";
    let (status, template) = request(&h.app, "GET", template_path, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(template["pipeline"]["states"].as_array().unwrap().len(), 10);
    let (status, original) = request(&h.app, "GET", PIPELINE, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        original["execution"]["state_ids"],
        json!({"a":"implementation"})
    );
    assert_eq!(original["source"], "legacy_projection");
    let migrate = Some(json!({"idempotency_key":"migrate","expected_definition_revision":1}));
    let result = request(&h.app, "POST", MIGRATE, migrate.clone()).await;
    assert_eq!(result.0, StatusCode::OK, "{}", result.1);
    assert_eq!(result.1["result"]["definition_revision"], 2);
    h.reconnect().await;
    assert_eq!(request(&h.app, "POST", MIGRATE, migrate).await, result);
    let (_, migrated) = request(&h.app, "GET", PIPELINE, None).await;
    assert_eq!(migrated["pipeline"], template["pipeline"]);
    assert_eq!(migrated["execution_hash"], template["execution_hash"]);
    assert_eq!(migrated["execution"]["definition_revision"], 1);
    assert_eq!(migrated["execution"], original["execution"]);
    assert_eq!(migrated["migration_available"], false);
    let after = store::get(&h.pool, "campaign").await.unwrap().unwrap();
    assert_eq!(after.nodes, before.nodes);
    assert_eq!(after.run, before.run);
    assert_eq!(after.definition.hashes(), before.definition.hashes());
    let pin: sqlx::types::Json<Value> = sqlx::query_scalar(
        "SELECT definition_pin FROM server_campaign_runs WHERE campaign_id='campaign'",
    )
    .fetch_one(&h.pool)
    .await
    .unwrap();
    assert_eq!(pin.0, json!(before.definition));
    let event_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM server_campaign_run_history WHERE campaign_id='campaign'",
    )
    .fetch_one(&h.pool)
    .await
    .unwrap();
    assert_eq!(event_count, 0);
    for stage in [
        Stage::Requirements,
        Stage::Design,
        Stage::DesignReview,
        Stage::Implementation,
        Stage::Review,
        Stage::Landing,
        Stage::MainVerification,
        Stage::FinalVerification,
        Stage::Complete,
    ] {
        let mut reference = before.clone();
        let mut migrated = after.clone();
        for state in [&mut reference, &mut migrated] {
            state.control = CampaignControl::Running;
            state.nodes.get_mut("a").unwrap().stage = stage;
        }
        let legacy = next_action(&reference, "a");
        if stage == Stage::Implementation {
            use crate::domain::campaign::lifecycle::NextAction;
            assert!(matches!(legacy, NextAction::AssignWork { .. }));
        }
        assert_eq!(legacy, next_action(&migrated, "a"), "{stage:?}");
    }
    let mut prepared = create_body();
    prepared["id"] = json!("prepared");
    assert_eq!(
        request(&h.app, "POST", "/server-campaigns", Some(prepared))
            .await
            .0,
        StatusCode::CREATED
    );
    let (_, detail) = request(&h.app, "GET", "/server-campaigns/prepared/pipeline", None).await;
    assert_eq!(detail["execution"]["state_ids"], json!({"a":null}));
    assert_eq!(detail["migration_available"], true);
    let (status, result) = request(
        &h.app,
        "POST",
        "/server-campaigns/prepared/pipeline/migrate",
        Some(json!({"idempotency_key":"migrate", "expected_definition_revision":1})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["result"]["definition_revision"], 2);
    let (status, replay) = request(
        &h.app,
        "POST",
        "/server-campaigns/prepared/pipeline/migrate",
        Some(json!({"idempotency_key":"retry-current", "expected_definition_revision":2})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay, result);
    h.close().await;
}
