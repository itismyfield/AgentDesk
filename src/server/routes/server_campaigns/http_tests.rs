use super::*;
use crate::{
    dispatch::test_support::DispatchPostgresTestDb,
    services::campaign_execution::{control::tests as domain, model::*},
};
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::{Request, header},
};
use std::sync::Arc;
use tower::ServiceExt;

fn app(pool: Option<sqlx::PgPool>) -> (axum::Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.policies.dir = dir.path().join("policies");
    config.policies.hot_reload = false;
    config.data.dir = dir.path().join("data");
    config.server.auth_token = Some("server-campaign-test-token".into());
    std::fs::create_dir_all(&config.policies.dir).unwrap();
    let state = AppState {
        engine: crate::engine::PolicyEngine::new_with_pg(&config, None).unwrap(),
        config: Arc::new(config),
        pg_pool: pool,
        broadcast_tx: crate::eventbus::new_broadcast(),
        batch_buffer: Default::default(),
        health_registry: None,
        cluster_instance_id: None,
    };
    (
        super::super::domains::admin::router(state.clone())
            .merge(super::super::domains::ops::router(state.clone()))
            .with_state(state),
        dir,
    )
}

async fn raw(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: String,
    content_type: Option<&str>,
    auth: bool,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(path);
    if auth {
        request = request.header(header::AUTHORIZATION, "Bearer server-campaign-test-token");
    }
    if let Some(content_type) = content_type {
        request = request.header(header::CONTENT_TYPE, content_type);
    }
    let mut request = request.body(Body::from(body)).unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "192.0.2.2:1234".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    assert!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("application/json")
    );
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn request(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    raw(
        app,
        method,
        path,
        body.map(|value| value.to_string()).unwrap_or_default(),
        Some("application/json"),
        true,
    )
    .await
}

fn definition() -> Value {
    json!({"repository":"adk","title":"Campaign","goal":"Verified work","profile_version":"v1","nodes":[{"id":"a","title":"A","issue_url":"https://github.com/itismyfield/AgentDesk/issues/7438","acceptance":[{"id":"ac-a","description":"Prove A","verification":"test"}],"dependencies":[],"group":null}]})
}

fn create_body() -> Value {
    json!({"id":"campaign","idempotency_key":"create","definition":definition()})
}
fn edit_body(key: &str, revision: u64, title: &str) -> Value {
    let mut definition = definition();
    definition["title"] = json!(title);
    json!({"idempotency_key":key,"expected_definition_revision":revision,"definition":definition})
}
fn command_body(key: &str, command: Value) -> Value {
    json!({"idempotency_key":key,"command":command})
}
fn stop_body(key: &str, mode: &str) -> Value {
    command_body(
        key,
        json!({"command":"stop","expected_definition_revision":1,"expected_run":"run","mode":mode}),
    )
}

struct Harness {
    fixture: DispatchPostgresTestDb,
    pool: sqlx::PgPool,
    app: axum::Router,
    _dir: tempfile::TempDir,
}

impl Harness {
    async fn new() -> Self {
        let fixture =
            DispatchPostgresTestDb::create("server_campaign_http", "server campaign HTTP tests")
                .await;
        let pool = fixture.connect_and_migrate_with_max_connections(4).await;
        let (app, dir) = app(Some(pool.clone()));
        Self {
            fixture,
            pool,
            app,
            _dir: dir,
        }
    }
    async fn reconnect(&mut self) {
        self.pool.close().await;
        self.pool = self
            .fixture
            .connect_and_migrate_with_max_connections(4)
            .await;
        let (app, dir) = app(Some(self.pool.clone()));
        self.app = app;
        self._dir = dir;
    }
    async fn close(self) {
        self.pool.close().await;
        self.fixture.drop().await;
    }

    // Only this isolated fixture supplies execution state until run creation is available.
    async fn seed_running(&self) -> CampaignState {
        let state = domain::with_action(ActionState::StartPending, false);
        let body = json!({"id":"campaign","idempotency_key":"create","definition":state.definition.input()});
        assert_eq!(
            request(&self.app, "POST", "/server-campaigns", Some(body))
                .await
                .0,
            StatusCode::CREATED
        );
        let run = state.run.as_ref().unwrap();
        let mut tx = self.pool.begin().await.unwrap();
        sqlx::query("INSERT INTO server_campaign_runs (campaign_id, run_id, document, nodes, definition_pin) VALUES ($1,$2,$3,$4,$5)")
            .bind(&state.id).bind(&run.id).bind(sqlx::types::Json(run)).bind(sqlx::types::Json(&state.nodes)).bind(sqlx::types::Json(&state.definition)).execute(&mut *tx).await.unwrap();
        sqlx::query("UPDATE server_campaign_definitions SET control=$2, run_id=$3, nodes=$4 WHERE campaign_id=$1")
            .bind(&state.id).bind(sqlx::types::Json(state.control)).bind(&run.id).bind(sqlx::types::Json(&state.nodes)).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        state
    }
}

#[tokio::test]
async fn authenticated_admin_registration_and_missing_pool_have_json_errors() {
    let (app, _dir) = app(None);
    for (method, path, body) in [
        ("GET", "/server-campaigns", json!(null)),
        ("GET", "/server-campaigns/campaign", json!(null)),
        ("GET", "/server-campaigns/campaign/events", json!(null)),
        ("POST", "/server-campaigns", create_body()),
        (
            "PUT",
            "/server-campaigns/campaign/definition",
            edit_body("edit", 1, "Edited"),
        ),
        (
            "POST",
            "/server-campaigns/campaign/commands",
            stop_body("stop", "immediate"),
        ),
    ] {
        let (status, _) = raw(
            &app,
            method,
            path,
            body.to_string(),
            Some("application/json"),
            false,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}");
        let (status, error) = request(&app, method, path, Some(body)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{method} {path}");
        assert_eq!(error["code"], "database");
        assert!(error.get("campaigns").is_none());
    }
}

#[tokio::test]
async fn read_envelope_matches_f8_fixture_and_definition_view_share_identity_pg() {
    let h = Harness::new().await;
    let before = Utc::now();
    assert_eq!(
        request(&h.app, "POST", "/server-campaigns", Some(create_body()))
            .await
            .0,
        StatusCode::CREATED
    );
    let (status, mut detail) = request(&h.app, "GET", "/server-campaigns/campaign", None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, mut list) =
        request(&h.app, "GET", "/server-campaigns?limit=1&offset=0", None).await;
    assert_eq!(status, StatusCode::OK);
    println!("F1B_HTTP_DETAIL_JSON={detail}");
    println!("F1B_HTTP_LIST_JSON={list}");
    for document in [&mut detail["campaign"], &mut list["campaigns"][0]] {
        assert_eq!(document["definition"], definition());
        assert!(document["definition"].get("input").is_none());
        assert!(document["definition"].get("hashes").is_none());
        let node_id = document["definition"]["nodes"][0]["id"].clone();
        let view = &mut document["view"];
        assert_eq!(view["campaign_id"], "campaign");
        assert_eq!(view["definition_revision"], 1);
        assert_eq!(view["nodes"][0]["id"], node_id);
        let as_of = chrono::DateTime::parse_from_rfc3339(view["as_of"].as_str().unwrap())
            .unwrap()
            .with_timezone(&Utc);
        assert!(as_of >= before && as_of <= Utc::now());
        let last_event_at =
            chrono::DateTime::parse_from_rfc3339(view["last_event_at"].as_str().unwrap())
                .unwrap()
                .with_timezone(&Utc);
        assert!(last_event_at >= before && last_event_at <= as_of);
        // Normalize server timestamps only; all other fields are the actual HTTP body.
        view["as_of"] = json!("2026-10-28T00:00:00Z");
        view["last_event_at"] = json!("2026-10-28T00:00:00Z");
    }
    let expected: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/server_campaigns/detail-prepared.json"
    ))
    .unwrap();
    assert_eq!(detail, expected);
    assert_eq!(list, json!({"campaigns":[expected["campaign"].clone()]}));
    h.close().await;
}

#[tokio::test]
async fn create_edit_command_receipts_replay_exact_status_body_after_new_pool_pg() {
    let mut h = Harness::new().await;
    let created = request(&h.app, "POST", "/server-campaigns", Some(create_body())).await;
    assert_eq!(created.0, StatusCode::CREATED);
    fn old_renderer(outcome: &store::CommandOutcome) -> store::StoreResult<store::ReceiptResponse> {
        let mut response = protocol::commanded(outcome)?;
        response.body["context"]["response_format"] = json!("v1");
        Ok(response)
    }
    let old_command = CommandEnvelope {
        idempotency_key: "old-start".into(),
        observed_at: Utc::now(),
        command: Command::Start {
            expected_revision: 1,
        },
    };
    let old_response = store::execute(&h.pool, "campaign", &old_command, old_renderer)
        .await
        .unwrap()
        .response;
    let edit = edit_body("edit", 1, "Edited");
    let edited = request(
        &h.app,
        "PUT",
        "/server-campaigns/campaign/definition",
        Some(edit.clone()),
    )
    .await;
    assert_eq!(edited.0, StatusCode::OK);
    let command = command_body(
        "start",
        json!({"command":"start","expected_definition_revision":2}),
    );
    let unavailable = request(
        &h.app,
        "POST",
        "/server-campaigns/campaign/commands",
        Some(command.clone()),
    )
    .await;
    assert_eq!(unavailable.0, StatusCode::CONFLICT);
    assert_eq!(
        unavailable.1["context"]["rejection"],
        "execution_unavailable"
    );
    assert_eq!(
        request(
            &h.app,
            "PUT",
            "/server-campaigns/campaign/definition",
            Some(edit_body("edit-2", 2, "Newest"))
        )
        .await
        .0,
        StatusCode::OK
    );
    h.reconnect().await;
    assert_eq!(
        request(
            &h.app,
            "POST",
            "/server-campaigns/campaign/commands",
            Some(command_body(
                "old-start",
                json!({"command":"start","expected_definition_revision":1})
            ))
        )
        .await,
        (
            StatusCode::from_u16(old_response.status).unwrap(),
            old_response.body
        )
    );
    assert_eq!(
        request(&h.app, "POST", "/server-campaigns", Some(create_body())).await,
        created
    );
    assert_eq!(
        request(
            &h.app,
            "PUT",
            "/server-campaigns/campaign/definition",
            Some(edit)
        )
        .await,
        edited
    );
    assert_eq!(
        request(
            &h.app,
            "POST",
            "/server-campaigns/campaign/commands",
            Some(command)
        )
        .await,
        unavailable
    );
    let mut changed = create_body();
    changed["definition"]["title"] = json!("Different");
    assert_eq!(
        request(&h.app, "POST", "/server-campaigns", Some(changed))
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        request(
            &h.app,
            "PUT",
            "/server-campaigns/campaign/definition",
            Some(edit_body("stale", 1, "Old"))
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let current = request(&h.app, "GET", "/server-campaigns/campaign", None)
        .await
        .1;
    assert_eq!(current["campaign"]["definition"]["title"], "Newest");
    assert_eq!(current["campaign"]["view"]["definition_revision"], 3);
    assert_eq!(
        request(
            &h.app,
            "PUT",
            "/server-campaigns/campaign/definition",
            Some(edit_body("edit", 1, "Different body"))
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        request(
            &h.app,
            "POST",
            "/server-campaigns/campaign/commands",
            Some(command_body(
                "start",
                json!({"command":"start","expected_definition_revision":3})
            ))
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    h.close().await;
}

#[tokio::test]
async fn internal_commands_and_unknown_nested_execution_fields_are_400_without_mutation_pg() {
    let h = Harness::new().await;
    assert_eq!(
        request(&h.app, "POST", "/server-campaigns", Some(create_body()))
            .await
            .0,
        StatusCode::CREATED
    );
    let state = domain::with_action(ActionState::Planned, false);
    let binding = domain::binding(&state);
    let internal = [
        Command::ConfirmStopFence,
        Command::ConfirmStopped,
        Command::AcknowledgeFence {
            expected_run: "run".into(),
            target: binding.session.gate.clone(),
            control_version: 2,
        },
        Command::ObserveSession {
            identity: binding.session.clone(),
            observation: SessionObservation::Gone {
                observed_at: domain::now(),
                evidence: "forged".into(),
            },
        },
        Command::PlanAction {
            action: Box::new(state.run.unwrap().actions["action"].clone()),
        },
        Command::ObserveAction {
            action_id: "action".into(),
            binding: Box::new(binding.clone()),
            observation: ActionObservation::Started {
                receipt: "forged".into(),
            },
        },
        Command::AdvanceAction {
            action_id: "action".into(),
            binding: Box::new(binding),
            state: ActionState::StartPending,
        },
    ];
    for command in internal {
        let body = command_body("forged", serde_json::to_value(command).unwrap());
        assert_eq!(
            request(
                &h.app,
                "POST",
                "/server-campaigns/campaign/commands",
                Some(body)
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    for variant in [
        "report_stopped",
        "submit_evidence",
        "accept_stage_result",
        "complete",
        "complete_node",
        "complete_run",
    ] {
        assert_eq!(
            request(
                &h.app,
                "POST",
                "/server-campaigns/campaign/commands",
                Some(command_body("forged", json!({"command":variant})))
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    let mut bodies = Vec::new();
    for field in [
        "auto_queue",
        "status",
        "stage",
        "evidence",
        "observed_at",
        "control_version",
        "execution_available",
        "PASS",
        "unknown",
    ] {
        let mut body = create_body();
        body[field] = json!(true);
        bodies.push(body);
        let mut body = create_body();
        body["definition"][field] = json!(true);
        bodies.push(body);
        let mut body = create_body();
        body["definition"]["nodes"][0][field] = json!(true);
        bodies.push(body);
        let mut body = create_body();
        body["definition"]["nodes"][0]["acceptance"][0][field] = json!(true);
        bodies.push(body);
        let mut body = command_body(
            "forged",
            json!({"command":"start","expected_definition_revision":1}),
        );
        body["command"][field] = json!(true);
        assert_eq!(
            request(
                &h.app,
                "POST",
                "/server-campaigns/campaign/commands",
                Some(body)
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        let mut body = edit_body("forged", 1, "Unsafe");
        body[field] = json!(true);
        assert_eq!(
            request(
                &h.app,
                "PUT",
                "/server-campaigns/campaign/definition",
                Some(body)
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        let mut body = command_body(
            "forged",
            json!({"command":"start","expected_definition_revision":1}),
        );
        body[field] = json!(true);
        assert_eq!(
            request(
                &h.app,
                "POST",
                "/server-campaigns/campaign/commands",
                Some(body)
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    for body in bodies {
        assert_eq!(
            request(&h.app, "POST", "/server-campaigns", Some(body))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    let events = store::events(&h.pool, "campaign", None, 0, 100)
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    let receipts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM server_campaign_receipts")
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(receipts, 1);
    h.close().await;
}

#[tokio::test]
async fn invalid_definition_returns_400_and_durable_failure_receipt_pg() {
    let mut h = Harness::new().await;
    let mut cases = Vec::new();
    let mut invalid = definition();
    invalid["nodes"][0]["dependencies"] = json!(["a"]);
    cases.push((
        "cycle",
        invalid,
        json!({"code":"cycle","target":null,"path":["a","a"]}),
    ));
    let mut invalid = definition();
    invalid["nodes"][0]["acceptance"] = json!([]);
    cases.push((
        "empty_ac",
        invalid,
        json!({"code":"empty_acceptance","target":"a","path":[]}),
    ));
    let mut invalid = definition();
    invalid["nodes"][0]["dependencies"] = json!(["absent"]);
    cases.push((
        "missing_dependency",
        invalid,
        json!({"code":"missing_dependency","target":"a","path":["absent"]}),
    ));
    let mut invalid = definition();
    let duplicate = invalid["nodes"][0].clone();
    invalid["nodes"].as_array_mut().unwrap().push(duplicate);
    cases.push((
        "duplicate_node",
        invalid,
        json!({"code":"duplicate_node","target":"a","path":[]}),
    ));
    let mut invalid = definition();
    let duplicate = invalid["nodes"][0]["acceptance"][0].clone();
    invalid["nodes"][0]["acceptance"]
        .as_array_mut()
        .unwrap()
        .push(duplicate);
    cases.push((
        "duplicate_ac",
        invalid,
        json!({"code":"duplicate_acceptance","target":"ac-a","path":[]}),
    ));
    let mut invalid = definition();
    invalid["profile_version"] = json!("unsupported");
    cases.push((
        "unsupported_profile",
        invalid,
        json!({"code":"unsupported_profile","target":"profile_version","path":[]}),
    ));
    assert_eq!(
        request(&h.app, "POST", "/server-campaigns", Some(create_body()))
            .await
            .0,
        StatusCode::CREATED
    );
    for (index, (case, definition, expected)) in cases.iter().enumerate() {
        let post =
            json!({"id":format!("bad-{index}"),"idempotency_key":"bad","definition":definition});
        let put = json!({"idempotency_key":format!("bad-{index}"),"expected_definition_revision":1,"definition":definition});
        for (method, path, body) in [
            ("POST", "/server-campaigns", post),
            ("PUT", "/server-campaigns/campaign/definition", put),
        ] {
            let failed = request(&h.app, method, path, Some(body.clone())).await;
            assert_eq!(failed.0, StatusCode::BAD_REQUEST, "{method} {case}");
            assert_eq!(
                failed.1["context"]["definition_error"], *expected,
                "typed definition error must survive {method} {case}"
            );
            assert_eq!(failed.1["context"]["rejection"], "invalid_definition");
            assert_eq!(failed.1["context"]["target"], expected["target"]);
            println!(
                "F1B_API_R2_ERROR_JSON={}",
                json!({"method":method,"case":case,"status":failed.0.as_u16(),"body":failed.1})
            );
            h.reconnect().await;
            assert_eq!(
                request(&h.app, method, path, Some(body)).await,
                failed,
                "saved response must replay after reconnect: {method} {case}"
            );
        }
    }
    let receipts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM server_campaign_receipts")
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(receipts, i64::try_from(cases.len()).unwrap() * 2 + 1);

    // Invalid serde input never reaches domain validation or creates a receipt.
    let mut invalid = definition();
    invalid["nodes"][0]["acceptance"][0]["verification"] = json!("PASS");
    for (method, path, body) in [
        (
            "POST",
            "/server-campaigns",
            json!({"id":"bad-serde","idempotency_key":"bad","definition":invalid}),
        ),
        (
            "PUT",
            "/server-campaigns/campaign/definition",
            json!({"idempotency_key":"bad-serde","expected_definition_revision":1,"definition":invalid}),
        ),
    ] {
        let failed = request(&h.app, method, path, Some(body)).await;
        assert_eq!(failed.0, StatusCode::BAD_REQUEST);
        assert!(failed.1["context"].get("definition_error").is_none());
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM server_campaign_receipts")
            .fetch_one(&h.pool)
            .await
            .unwrap(),
        receipts
    );
    assert_eq!(store::list(&h.pool, 100, 0).await.unwrap().len(), 1);
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
async fn request_ids_keys_revision_bounds_and_change_reason_are_closed_pg() {
    let h = Harness::new().await;
    let mut invalid_id = create_body();
    invalid_id["id"] = json!("bad id");
    assert_eq!(
        request(&h.app, "POST", "/server-campaigns", Some(invalid_id))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let mut empty_key = create_body();
    empty_key["idempotency_key"] = json!(" ");
    assert_eq!(
        request(&h.app, "POST", "/server-campaigns", Some(empty_key))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let mut missing_key = create_body();
    missing_key
        .as_object_mut()
        .unwrap()
        .remove("idempotency_key");
    assert_eq!(
        request(&h.app, "POST", "/server-campaigns", Some(missing_key))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&h.app, "POST", "/server-campaigns", Some(create_body()))
            .await
            .0,
        StatusCode::CREATED
    );
    for revision in [0, u64::MAX] {
        assert_eq!(
            request(
                &h.app,
                "PUT",
                "/server-campaigns/campaign/definition",
                Some(edit_body("bad-revision", revision, "Unsafe"))
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            request(
                &h.app,
                "POST",
                "/server-campaigns/campaign/commands",
                Some(command_body(
                    "bad-revision",
                    json!({"command":"start","expected_definition_revision":revision})
                ))
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    let mut reason = edit_body("bad-change", 1, "Unsafe");
    reason["change"] =
        json!({"reference":"https://example.test/change","explanation":"Change","accepted":true});
    assert_eq!(
        request(
            &h.app,
            "PUT",
            "/server-campaigns/campaign/definition",
            Some(reason)
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let mut changed = edit_body("missing-reason", 1, "Changed");
    changed["definition"]["goal"] = json!("New requirement");
    let rejected = request(
        &h.app,
        "PUT",
        "/server-campaigns/campaign/definition",
        Some(changed),
    )
    .await;
    assert_eq!(rejected.0, StatusCode::BAD_REQUEST);
    assert_eq!(rejected.1["context"]["rejection"], "missing_change_reason");
    h.close().await;
}

#[tokio::test]
async fn strict_pagination_paths_missing_targets_and_empty_event_pages_pg() {
    let h = Harness::new().await;
    assert_eq!(
        request(&h.app, "POST", "/server-campaigns", Some(create_body()))
            .await
            .0,
        StatusCode::CREATED
    );
    for path in [
        "/server-campaigns?limit=0",
        "/server-campaigns?limit=101",
        "/server-campaigns?offset=-1",
        "/server-campaigns?limit=no",
        "/server-campaigns?extra=1",
        "/server-campaigns?limit=1&limit=2",
        "/server-campaigns/campaign/events?after=-1",
        "/server-campaigns/campaign/events?limit=501",
        "/server-campaigns/campaign/events?limit=0",
        "/server-campaigns/campaign/events?after=9223372036854775808",
        "/server-campaigns/campaign/events?run_id=bad%20id",
        "/server-campaigns/campaign/events?extra=1",
        "/server-campaigns/campaign?extra=1",
        "/server-campaigns/bad%20id",
    ] {
        assert_eq!(
            request(&h.app, "GET", path, None).await.0,
            StatusCode::BAD_REQUEST,
            "{path}"
        );
    }
    for path in [
        "/server-campaigns/missing",
        "/server-campaigns/missing/events",
    ] {
        assert_eq!(
            request(&h.app, "GET", path, None).await.0,
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        request(
            &h.app,
            "PUT",
            "/server-campaigns/missing/definition",
            Some(edit_body("edit", 1, "Missing"))
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            &h.app,
            "POST",
            "/server-campaigns/missing/commands",
            Some(stop_body("stop", "immediate"))
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            &h.app,
            "GET",
            "/server-campaigns/campaign/events?after=1&run_id=missing&limit=500",
            None
        )
        .await,
        (StatusCode::OK, json!({"events":[]}))
    );
    let page = request(
        &h.app,
        "GET",
        "/server-campaigns/campaign/events?limit=1",
        None,
    )
    .await;
    assert_eq!(page.0, StatusCode::OK);
    assert_eq!(page.1["events"][0]["sequence"], 1);
    h.close().await;
}

#[tokio::test]
async fn json_rejection_preserves_413_415_and_maps_syntax_to_400() {
    let (app, _dir) = app(None);
    assert_eq!(
        raw(
            &app,
            "POST",
            "/server-campaigns",
            create_body().to_string(),
            None,
            true
        )
        .await
        .0,
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    assert_eq!(
        raw(
            &app,
            "POST",
            "/server-campaigns",
            "{".into(),
            Some("application/json"),
            true
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let mut body = create_body();
    body["definition"]["goal"] = json!("x".repeat(BODY_LIMIT_BYTES));
    for (method, path, body) in [
        ("POST", "/server-campaigns", body),
        (
            "PUT",
            "/server-campaigns/campaign/definition",
            json!({"filler":"x".repeat(BODY_LIMIT_BYTES)}),
        ),
        (
            "POST",
            "/server-campaigns/campaign/commands",
            json!({"filler":"x".repeat(BODY_LIMIT_BYTES)}),
        ),
    ] {
        assert_eq!(
            request(&app, method, path, Some(body)).await.0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }
}

#[tokio::test]
async fn http_stop_targets_run_definition_after_events_and_replays_original_receipt_pg() {
    let mut h = Harness::new().await;
    let state = h.seed_running().await;
    let binding = domain::binding(&state);
    let envelope = CommandEnvelope {
        idempotency_key: "observe".into(),
        observed_at: domain::now(),
        command: Command::ObserveAction {
            action_id: "action".into(),
            binding: Box::new(binding),
            observation: ActionObservation::Started {
                receipt: "fake-start".into(),
            },
        },
    };
    store::execute(&h.pool, "campaign", &envelope, protocol::commanded)
        .await
        .unwrap();
    let stopped = request(
        &h.app,
        "POST",
        "/server-campaigns/campaign/commands",
        Some(stop_body("stop", "finish_stage")),
    )
    .await;
    assert_eq!(stopped.0, StatusCode::ACCEPTED);
    assert_eq!(stopped.1["result"]["control_version"], 2);
    assert_eq!(
        request(
            &h.app,
            "PUT",
            "/server-campaigns/campaign/definition",
            Some(edit_body("edit-running", 1, "Unsafe"))
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    for (field, value, rejection) in [
        ("expected_run", json!("old-run"), "stale_run"),
        ("expected_definition_revision", json!(2), "stale_definition"),
    ] {
        let mut body = stop_body(field, "immediate");
        body["command"][field] = value;
        let response = request(
            &h.app,
            "POST",
            "/server-campaigns/campaign/commands",
            Some(body),
        )
        .await;
        assert_eq!(response.0, StatusCode::CONFLICT);
        assert_eq!(response.1["context"]["rejection"], rejection);
    }
    let escalated = request(
        &h.app,
        "POST",
        "/server-campaigns/campaign/commands",
        Some(stop_body("escalate", "immediate")),
    )
    .await;
    assert_eq!(escalated.0, StatusCode::ACCEPTED);
    assert_eq!(escalated.1["result"]["control_version"], 3);
    h.reconnect().await;
    assert_eq!(
        request(
            &h.app,
            "POST",
            "/server-campaigns/campaign/commands",
            Some(stop_body("stop", "finish_stage"))
        )
        .await,
        stopped
    );
    let current = store::get(&h.pool, "campaign").await.unwrap().unwrap();
    assert_eq!(current.run.unwrap().control_version, 3);
    h.close().await;
}

#[tokio::test]
async fn public_execution_is_unavailable_without_legacy_queue_or_card_effects_pg() {
    let h = Harness::new().await;
    let legacy_input = serde_json::from_value(
        json!({"title":"Legacy","status":"active","round":1,"nodes":[],"auto_queue":false}),
    )
    .unwrap();
    let legacy = crate::db::campaigns::create(&h.pool, "campaign".into(), legacy_input)
        .await
        .unwrap();
    h.seed_running().await;
    for name in ["start", "resume", "retry"] {
        let mut command = json!({"command":name,"expected_definition_revision":1});
        if name != "start" {
            command["expected_run"] = json!("run");
        }
        if name == "retry" {
            command["node_id"] = json!("a");
        }
        let response = request(
            &h.app,
            "POST",
            "/server-campaigns/campaign/commands",
            Some(command_body(name, command)),
        )
        .await;
        assert_eq!(response.0, StatusCode::CONFLICT);
        assert_eq!(response.1["context"]["rejection"], "execution_unavailable");
    }
    let unchanged = crate::db::campaigns::get(&h.pool, "campaign")
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(unchanged).unwrap(),
        serde_json::to_value(legacy).unwrap()
    );
    for sql in [
        "SELECT COUNT(*) FROM auto_queue_entries",
        "SELECT COUNT(*) FROM task_dispatches",
        "SELECT COUNT(*) FROM dispatch_outbox",
        "SELECT COUNT(*) FROM kanban_cards",
        "SELECT COUNT(*) FROM worker_nodes",
    ] {
        let count: i64 = sqlx::query_scalar(sql).fetch_one(&h.pool).await.unwrap();
        assert_eq!(count, 0);
    }
    h.close().await;
}

#[tokio::test]
async fn closed_pool_returns_503_without_sql_details_pg() {
    let h = Harness::new().await;
    h.pool.close().await;
    let (status, body) = request(&h.app, "GET", "/server-campaigns", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body,
        json!({"error":"canonical PostgreSQL pool unavailable","code":"database","context":{}})
    );
    h.fixture.drop().await;
}

#[tokio::test]
async fn shipped_docs_router_advertises_all_server_campaign_endpoints() {
    let (app, _dir) = app(None);
    let (status, body) = request(&app, "GET", "/docs/automation/campaigns", None).await;
    assert_eq!(status, StatusCode::OK);
    let endpoints = body["endpoints"].as_array().unwrap();
    for (method, path) in [
        ("GET", "/api/server-campaigns"),
        ("POST", "/api/server-campaigns"),
        ("GET", "/api/server-campaigns/{id}"),
        ("PUT", "/api/server-campaigns/{id}/definition"),
        ("POST", "/api/server-campaigns/{id}/commands"),
        ("GET", "/api/server-campaigns/{id}/events"),
    ] {
        assert!(
            endpoints
                .iter()
                .any(|endpoint| endpoint["method"] == method && endpoint["path"] == path),
            "{method} {path}"
        );
    }
}
