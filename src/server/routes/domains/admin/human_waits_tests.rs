use std::{net::SocketAddr, sync::Arc};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::{Request, StatusCode, header},
    response::Response,
};
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

use crate::{
    db::server_campaigns as store,
    dispatch::test_support::DispatchPostgresTestDb,
    domain::campaign::{
        decision_item::{ApprovalBinding, ForumApproval},
        workflow_options::ApprovalGate,
    },
    services::campaign_execution::{
        commands,
        control::{self, tests as domain},
        model::{CampaignControl, Command, CommandEnvelope},
    },
};

use super::super::AppState;

const LIST: &str = "/api/human-waits";
const TOKEN: &str = "human-waits-test-token";

fn app(pool: Option<PgPool>) -> (Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.data.dir = dir.path().join("data");
    config.policies.dir = dir.path().join("policies");
    config.policies.hot_reload = false;
    config.server.auth_token = Some(TOKEN.into());
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
    let admin = super::super::router(state.clone()).with_state(state);
    (Router::new().nest("/api", admin), dir)
}

async fn get(app: &Router, path: &str, authorized: bool) -> Response {
    let mut request = Request::get(path);
    if authorized {
        request = request.header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
    }
    let mut request = request.body(Body::empty()).unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo("192.0.2.2:1234".parse::<SocketAddr>().unwrap()));
    app.clone().oneshot(request).await.unwrap()
}

async fn expect_json(response: Response, expected: StatusCode) -> Value {
    let status = response.status();
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    assert_eq!(status, expected, "{}", String::from_utf8_lossy(&body));
    serde_json::from_slice(&body).unwrap()
}

async fn waits(app: &Router) -> Vec<Value> {
    let response = get(app, LIST, true).await;
    let cache_control = response.headers().get(header::CACHE_CONTROL).cloned();
    let body = expect_json(response, StatusCode::OK).await;
    assert_eq!(cache_control.unwrap(), "no-store");
    body["waits"].as_array().unwrap().clone()
}

struct Harness {
    fixture: DispatchPostgresTestDb,
    pool: PgPool,
    app: Router,
    _dir: tempfile::TempDir,
}

impl Harness {
    async fn new() -> Self {
        let fixture = DispatchPostgresTestDb::create("human_waits", "human waits HTTP tests").await;
        let pool = fixture.connect_and_migrate_with_max_connections(4).await;
        let (app, dir) = app(Some(pool.clone()));
        Self {
            fixture,
            pool,
            app,
            _dir: dir,
        }
    }

    async fn close(self) {
        self.pool.close().await;
        self.fixture.drop().await;
    }
}

async fn seed(pool: &PgPool, id: &str, control: CampaignControl) {
    let mut input = domain::input();
    input.title = format!("Campaign {id}");
    input.nodes[2].title = format!("Node {id}");
    input.nodes[2].issue_url = format!("https://github.com/itismyfield/AgentDesk/issues/{id}");
    input.workflow_options.as_mut().unwrap().start = true;
    let mut decision =
        control::create_definition(id.into(), input, "create", domain::now()).unwrap();
    if control != CampaignControl::Prepared {
        let fixture = domain::running();
        let mut run = fixture.run.unwrap();
        run.attempts.get_mut("attempt").unwrap().binding.campaign_id = id.into();
        decision.state.control = control;
        decision.state.run = Some(run);
        decision.state.nodes = fixture.nodes;
    }
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO server_campaign_keys (campaign_id) VALUES ($1)")
        .bind(id)
        .execute(&mut *tx)
        .await
        .unwrap();
    store::persist_for_test(&mut tx, None, &decision)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

async fn command(pool: &PgPool, id: &str, key: &str, at: DateTime<Utc>, command: Command) {
    let reply = commands::execute(
        pool,
        id,
        &CommandEnvelope {
            idempotency_key: key.into(),
            observed_at: at,
            command,
        },
        |_| {
            Ok(store::ReceiptResponse {
                status: 202,
                body: json!({}),
            })
        },
    )
    .await
    .unwrap();
    assert!(
        matches!(reply.outcome, store::CommandOutcome::Decided(Ok(_))),
        "{reply:?}"
    );
}

fn approval_binding(head: &str) -> ApprovalBinding {
    serde_json::from_value(json!({
        "run_id": "run", "node_id": "c", "gate": ApprovalGate::Start, "definition_revision": 1,
        "target": {"kind": "candidate", "base_sha": "a".repeat(40), "head_sha": head.repeat(40)},
        "policy_hash": "c".repeat(64)
    }))
    .unwrap()
}

async fn open(pool: &PgPool, key: &str, at: DateTime<Utc>, binding: &ApprovalBinding) {
    command(
        pool,
        "7438",
        key,
        at,
        Command::OpenApprovalGate {
            binding: Box::new(binding.clone()),
        },
    )
    .await;
}

#[tokio::test]
async fn human_waits_reads_beyond_first_campaign_page_pg() {
    let h = Harness::new().await;
    for n in 1..=201 {
        let control = match n {
            9 | 201 => CampaignControl::Running,
            101 => CampaignControl::Stopped,
            _ => CampaignControl::Prepared,
        };
        seed(&h.pool, &format!("{n:03}"), control).await;
    }
    let rows = waits(&h.app).await;
    let expected = ["009", "201"].map(|id| {
        json!({
            "source": "server", "campaign_id": id, "node_id": "c",
            "title": format!("Node {id}"), "campaign_title": format!("Campaign {id}"),
            "issue_url": format!("https://github.com/itismyfield/AgentDesk/issues/{id}"),
            "human_status": "decision_required", "wait_started_at": null
        })
    });
    assert_eq!(rows.as_slice(), expected.as_slice());
    h.close().await;
}

#[tokio::test]
async fn human_waits_matches_current_gate_and_snapshot_sequence_pg() {
    let h = Harness::new().await;
    seed(&h.pool, "7438", CampaignControl::Running).await;
    let first = waits(&h.app).await;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0]["node_id"], "c");
    assert_eq!(first[0]["human_status"], "decision_required");
    assert_eq!(first[0].get("wait_started_at"), Some(&Value::Null));

    let binding = approval_binding("b");
    let first_at = domain::now() + TimeDelta::nanoseconds(123);
    open(&h.pool, "first", first_at, &binding).await;

    let other_at = domain::now() + TimeDelta::seconds(2);
    open(&h.pool, "other", other_at, &approval_binding("d")).await;
    assert_eq!(waits(&h.app).await[0]["wait_started_at"], json!(other_at));

    let reopened_at = domain::now() + TimeDelta::seconds(3);
    open(&h.pool, "reopen", reopened_at, &binding).await;
    let detail = expect_json(
        get(&h.app, "/api/server-campaigns/7438", true).await,
        StatusCode::OK,
    )
    .await;
    let runtime = &detail["campaign"]["view"]["nodes"][2]["runtime"];
    let current = waits(&h.app).await;
    assert_eq!(current.len(), 1);
    assert_eq!(current[0]["human_status"], "awaiting_approval");
    assert_eq!(current[0]["human_status"], runtime["human_status"]);
    assert_eq!(current[0]["wait_started_at"], json!(reopened_at));
    assert_eq!(current[0]["wait_started_at"], runtime["wait_started_at"]);

    command(
        &h.pool,
        "7438",
        "approve",
        reopened_at,
        Command::ApproveGate {
            binding: Box::new(binding),
            evidence: Box::new(ForumApproval {
                decision_ref: "start-7438".into(),
                response_ref: "https://forum.example/decisions/start-7438#response-2".into(),
                response_at: reopened_at,
            }),
        },
    )
    .await;
    let approved = waits(&h.app).await;
    assert_eq!(approved.len(), 1);
    assert_eq!(approved[0]["human_status"], "awaiting_start");
    assert_eq!(approved[0].get("wait_started_at"), Some(&Value::Null));
    h.close().await;
}

#[tokio::test]
async fn human_waits_reports_empty_and_query_failure_pg() {
    let h = Harness::new().await;
    assert!(waits(&h.app).await.is_empty());
    seed(&h.pool, "7438", CampaignControl::Prepared).await;
    sqlx::query(
        "UPDATE server_campaign_definitions SET nodes = 'null'::jsonb WHERE campaign_id = $1",
    )
    .bind("7438")
    .execute(&h.pool)
    .await
    .unwrap();
    let failed = expect_json(
        get(&h.app, LIST, true).await,
        StatusCode::INTERNAL_SERVER_ERROR,
    )
    .await;
    assert!(failed.get("waits").is_none());

    h.pool.close().await;
    let unavailable = expect_json(
        get(&h.app, LIST, true).await,
        StatusCode::SERVICE_UNAVAILABLE,
    )
    .await;
    assert!(unavailable.get("waits").is_none());
    h.close().await;
}
