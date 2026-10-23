//! `/api/health` against a real database behind a latency and stall proxy: the acquire budget,
//! the shared deadline, and which blocks a timeout may drop.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use poise::serenity_prelude::ChannelId;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::health::HealthRegistry;
use crate::services::hang_forensics;
use crate::services::health_diagnostics::pg_wire_proxy::PgWireProxy;

mod amplification_claim_tests;
mod amplification_policy_tests;
mod amplification_profile_tests;
mod amplification_records_tests;
mod amplification_verdict_tests;
mod resource_regression_tests;

const RTT: Duration = Duration::from_millis(300);
const BUDGET: Duration = Duration::from_secs(2);

/// A migrated database, a direct pool for seeding, and a proxied pool for the handler.
struct Db {
    db: TestPostgresDb,
    seed: sqlx::PgPool,
    proxy: PgWireProxy,
    pool: sqlx::PgPool,
}

impl Db {
    async fn start(delay: Duration, max_connections: u32) -> Self {
        let db = TestPostgresDb::create().await;
        let seed = db.connect_and_migrate().await;
        let proxy = PgWireProxy::start(&db.database_url, delay).await;
        let pool = proxy.pool(max_connections);
        Self {
            db,
            seed,
            proxy,
            pool,
        }
    }

    async fn finish(self) {
        self.proxy.release();
        self.pool.close().await;
        self.seed.close().await;
        drop(self.proxy);
        self.db.drop().await;
    }

    /// kv_meta reports, an outbox past the age threshold and cleanup rows, so every DB block
    /// is non-empty when its query succeeds.
    async fn seed_blocks(&self) {
        for (key, value) in [
            ("config_audit_report", r#"{"checked":3}"#),
            (
                "pipeline_override_health_report",
                r#"{"warnings_count":0,"checked":2}"#,
            ),
            ("runtime-config", r#"{"dispatchRateLimitGateDangerPct":77}"#),
        ] {
            sqlx::query("INSERT INTO kv_meta (key, value) VALUES ($1, $2)")
                .bind(key)
                .bind(value)
                .execute(&self.seed)
                .await
                .unwrap();
        }
        sqlx::query(
            "INSERT INTO dispatch_outbox (dispatch_id, action, status, retry_count, created_at)
             VALUES ('d1', 'notify', 'pending', 1, NOW() - INTERVAL '5 minutes'),
                    ('d2', 'notify', 'failed', 3, NOW())",
        )
        .execute(&self.seed)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO auto_queue_run_cleanup_tasks (run_ids, dead_lettered_at)
             VALUES (ARRAY['r1'], NULL), (ARRAY['r2'], NOW()), (ARRAY['r3'], NOW())",
        )
        .execute(&self.seed)
        .await
        .unwrap();
    }

    /// Opens `count` connections at once and returns them, so later acquires find them idle.
    async fn warm(&self, count: usize) {
        let mut held = Vec::new();
        for _ in 0..count {
            held.push(self.pool.acquire());
        }
        let held = futures::future::join_all(held).await;
        for connection in held {
            let mut connection = connection.unwrap();
            sqlx::query("SELECT 1")
                .execute(&mut *connection)
                .await
                .unwrap();
        }
        tokio::time::sleep(RTT * 2 + Duration::from_millis(100)).await;
    }

    /// Waits until every pooled connection is idle again: sqlx pings a connection on release,
    /// so for one round trip after a response its connections cannot be reused.
    async fn settle(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.pool.num_idle() < self.pool.size() as usize {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("every connection returns to the idle queue");
    }
}

fn app(
    pool: &sqlx::PgPool,
    registry: Option<Arc<HealthRegistry>>,
    config: crate::config::Config,
) -> axum::Router {
    let mut engine_config = crate::config::Config::default();
    engine_config.policies.hot_reload = false;
    let engine = crate::engine::PolicyEngine::new(&engine_config).unwrap();
    let tx = crate::server::ws::new_broadcast();
    let buf = crate::server::ws::spawn_batch_flusher(tx.clone());
    crate::server::routes::api_router_with_pg(engine, config, tx, buf, registry, Some(pool.clone()))
}

async fn serve(app: axum::Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = app.into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, service).await.unwrap() });
    addr
}

/// One GET over a real loopback connection; the elapsed time runs to the body's last byte.
async fn http_get(addr: SocketAddr, path: &str) -> (u16, Value, Duration) {
    let started = Instant::now();
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nhost: {addr}\r\nconnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(20), stream.read_to_end(&mut raw)).await;
    let elapsed = started.elapsed();
    read.expect("the health response must finish").unwrap();
    let raw = String::from_utf8_lossy(&raw);
    let status = raw[9..12].parse().unwrap();
    let body = raw.split_once("\r\n\r\n").map_or("", |(_, body)| body);
    (
        status,
        serde_json::from_str(body).unwrap_or(Value::Null),
        elapsed,
    )
}

/// A provider registry whose detail body lists one busy mailbox per channel.
async fn registry_with_mailboxes(channels: &[u64]) -> Arc<HealthRegistry> {
    let registry = Arc::new(HealthRegistry::new());
    let shared = crate::services::discord::make_shared_data_for_tests();
    registry
        .register("claude".to_string(), shared.clone())
        .await;
    for channel in channels {
        crate::services::discord::host_teardown_gate::test_support::nameless_turn(
            &shared,
            ChannelId::new(*channel),
        )
        .await;
    }
    registry
}

/// A gateway node in an enabled cluster, so an empty registry takes the standby worker lookup.
fn standby_config() -> crate::config::Config {
    let mut config = crate::config::Config::default();
    config.cluster.enabled = true;
    config.cluster.instance_id = Some("node-standby".to_string());
    config
}

fn reasons(body: &Value) -> Vec<String> {
    body["degraded_reasons"]
        .as_array()
        .map(|reasons| {
            reasons
                .iter()
                .filter_map(|reason| reason.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// A label, the arm's registry and config, the path, and the acquires it may make.
type BudgetCase = (
    &'static str,
    Option<Arc<HealthRegistry>>,
    crate::config::Config,
    &'static str,
    u64,
);

/// The global probe counters are shared, so their tests hold the counter lock on a plain thread.
fn counted_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
}

/// Acquires per request, counted by `ProbedPool` and, independently, by the statements the
/// proxy saw executed, so a query outside a probe bracket still counts.
#[test]
fn health_handler_acquire_budget_public_and_detail() {
    let _serial = hang_forensics::counter_test_lock();
    counted_runtime().block_on(health_handler_acquire_budget_public_and_detail_body());
}

async fn health_handler_acquire_budget_public_and_detail_body() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = Db::start(Duration::ZERO, 8).await;
    db.seed_blocks().await;
    let mailboxes = registry_with_mailboxes(&[6_871_200_001, 6_871_200_002, 6_871_200_003]).await;
    let empty = || Some(Arc::new(HealthRegistry::new()));
    let cases: [BudgetCase; 6] = [
        ("standalone public", None, Default::default(), "/health", 4),
        (
            "standalone detail",
            None,
            Default::default(),
            "/health/detail",
            4,
        ),
        (
            "registry public",
            Some(mailboxes.clone()),
            Default::default(),
            "/health",
            4,
        ),
        (
            "registry detail",
            Some(mailboxes),
            Default::default(),
            "/health/detail",
            5,
        ),
        ("standby public", empty(), standby_config(), "/health", 5),
        (
            "standby detail",
            empty(),
            standby_config(),
            "/health/detail",
            5,
        ),
    ];
    let mut seen = Vec::new();
    for (label, registry, config, path, expected) in cases {
        let addr = serve(app(&db.pool, registry, config)).await;
        db.proxy.reset_executions();
        let before = hang_forensics::snapshot();
        let (status, body, _) = http_get(addr, path).await;
        let after = hang_forensics::snapshot();
        let probes = after.db_probes_started - before.db_probes_started;
        let executed = db.proxy.executions();
        assert!(status == 200 || status == 503, "{label}: {status} {body}");
        assert_eq!(after.db_in_flight, before.db_in_flight, "{label}");
        seen.push((label, probes, executed.len() as u64, expected));
        assert!(
            !reasons(&body).iter().any(|reason| reason == "db_slow"),
            "{label}: a local database must answer inside the deadline: {body}"
        );
        let _ = executed;
    }
    for (label, probes, executed, expected) in &seen {
        assert!(
            *probes <= 5 && *executed <= 5,
            "{label}: over the N=5 budget: {seen:?}"
        );
        assert_eq!(
            (probes, executed),
            (expected, expected),
            "{label}: {seen:?}"
        );
    }
    db.finish().await;
}

/// Public health over a 300 ms round trip is fully answered, last byte included, in under 2 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn health_handler_pg_rtt_300ms_completes_under_2s() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = Db::start(Duration::ZERO, 8).await;
    db.seed_blocks().await;
    db.warm(6).await;
    db.proxy.set_delay(RTT);
    let addr = serve(app(
        &db.pool,
        Some(Arc::new(HealthRegistry::new())),
        Default::default(),
    ))
    .await;
    let (status, body, elapsed) = http_get(addr, "/health").await;
    assert!(elapsed < BUDGET, "{elapsed:?}: {body}");
    assert_eq!(body["db"], true, "{status} {body}");
    assert!(
        !reasons(&body).iter().any(|reason| reason == "db_slow"),
        "{body}"
    );
    assert_eq!(body["auto_queue_cleanup"]["dead_lettered"], 2, "{body}");
    db.finish().await;
}

fn state(
    pool: &sqlx::PgPool,
    registry: Option<Arc<HealthRegistry>>,
    config: crate::config::Config,
) -> super::AppState {
    let mut engine_config = crate::config::Config::default();
    engine_config.policies.hot_reload = false;
    let broadcast_tx = crate::server::ws::new_broadcast();
    super::AppState {
        pg_pool: Some(pool.clone()),
        engine: crate::engine::PolicyEngine::new(&engine_config).unwrap(),
        config: Arc::new(config),
        batch_buffer: crate::server::ws::spawn_batch_flusher(broadcast_tx.clone()),
        broadcast_tx,
        health_registry: registry,
        cluster_instance_id: None,
    }
}

/// The registry arm with its snapshot held back by `delay`, timed from the request start.
async fn registry_body_after(
    state: &super::AppState,
    detailed: bool,
    delay: Duration,
    injected: Option<Value>,
) -> (u16, Value, Duration) {
    let started = Instant::now();
    let registry = state.health_registry.clone().expect("registry arm");
    let snapshot = async {
        tokio::time::sleep(delay).await;
        match injected {
            Some(json) => (
                crate::services::discord::health::HealthStatus::Healthy,
                json,
            ),
            None if detailed => {
                let snapshot =
                    crate::services::discord::health::build_health_snapshot(&registry).await;
                (snapshot.status(), serde_json::to_value(snapshot).unwrap())
            }
            None => {
                let snapshot =
                    crate::services::discord::health::build_public_health_snapshot(&registry).await;
                (snapshot.status(), serde_json::to_value(snapshot).unwrap())
            }
        }
    };
    let deadline = super::db_collection::deadline();
    let response =
        super::response::registry_response(state, &registry, detailed, deadline, snapshot).await;
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap(),
        started.elapsed(),
    )
}

fn mailbox_rows(body: &Value) -> &Vec<Value> {
    body["mailboxes"].as_array().expect("detail mailboxes")
}

/// Three mailbox channels per test, so no two tests share a channel actor.
fn mailboxes(test: u64) -> [u64; 3] {
    let base = 6_871_200_000 + test * 10;
    [base + 1, base + 2, base + 3]
}

/// Over the 300 ms link with a warm pool, every block arrives and nothing reads as slow: the
/// 2 s bound comes from concurrency, not from the deadline cutting reads short.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn health_handler_parallel_db_preserves_complete_results() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = Db::start(Duration::ZERO, 10).await;
    db.seed_blocks().await;
    db.warm(8).await;
    db.proxy.set_delay(RTT);
    let channels = mailboxes(1);
    let registry = registry_with_mailboxes(&channels).await;
    let addr = serve(app(&db.pool, Some(registry), Default::default())).await;
    for path in ["/health/detail", "/health"] {
        // Each request starts on the warm pool, not on new connections opened over the slow link.
        db.settle().await;
        let (_, body, elapsed) = http_get(addr, path).await;
        assert!(elapsed < BUDGET, "{path} {elapsed:?}: {body}");
        assert_eq!(
            (&body["db"], &body["server_up"]),
            (&Value::Bool(true), &Value::Bool(true))
        );
        assert!(
            !reasons(&body).iter().any(|reason| reason == "db_slow"),
            "{path}: {body}"
        );
        assert_eq!(
            body["auto_queue_cleanup"]["dead_lettered"], 2,
            "{path}: {body}"
        );
        if path == "/health/detail" {
            assert_eq!(body["dispatch_outbox"]["permanent_failures"], 1, "{body}");
            assert_eq!(body["config_audit"]["checked"], 3, "{body}");
            assert_eq!(body["pipeline_overrides"]["checked"], 2, "{body}");
            assert_eq!(body["rate_limit_dispatch_gate"]["danger_pct"], 77, "{body}");
            assert!(body.get("active_session_audit").is_none(), "{body}");
            assert_eq!(mailbox_rows(&body).len(), channels.len(), "{body}");
            for row in mailbox_rows(&body) {
                assert!(row["session_lookup_error"].is_null(), "{row}");
                assert_eq!(row["session_record_present"], false, "{row}");
            }
        }
    }
    db.finish().await;
}

/// A pool wait, a stalled statement and a late read after a slow snapshot each still answer
/// inside 2 s with `db_slow`, keeping every read that did finish.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn health_handler_db_deadline_covers_late_queries() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let registry = registry_with_mailboxes(&mailboxes(2)).await;

    // The only connection is checked out, so every acquire waits.
    let db = Db::start(Duration::ZERO, 1).await;
    let held = db.pool.acquire().await.unwrap();
    let addr = serve(app(&db.pool, Some(registry.clone()), Default::default())).await;
    let (status, body, elapsed) = http_get(addr, "/health/detail").await;
    assert!(elapsed < BUDGET, "pool wait {elapsed:?}: {body}");
    assert_eq!((status, &body["db"]), (503, &Value::Bool(false)), "{body}");
    assert!(reasons(&body).contains(&"db_slow".to_string()), "{body}");
    assert!(body["release_source"].is_object() && body["delivery_record_rollout"].is_object());
    for row in mailbox_rows(&body) {
        assert_eq!(
            row["session_lookup_error"]["safety_gate"], "measurement_unavailable",
            "{row}"
        );
    }
    drop(held);
    db.finish().await;

    // One stalled statement costs only its own block.
    let db = Db::start(Duration::ZERO, 8).await;
    db.seed_blocks().await;
    db.proxy.stall("FROM dispatch_outbox");
    let addr = serve(app(&db.pool, Some(registry.clone()), Default::default())).await;
    let (_, body, elapsed) = http_get(addr, "/health/detail").await;
    assert!(elapsed < BUDGET, "stalled outbox {elapsed:?}: {body}");
    assert_eq!(body["db"], true, "{body}");
    assert!(reasons(&body).contains(&"db_slow".to_string()), "{body}");
    assert!(body.get("dispatch_outbox").is_none(), "{body}");
    assert_eq!(body["config_audit"]["checked"], 3, "{body}");
    assert_eq!(body["auto_queue_cleanup"]["pending"], 1, "{body}");
    assert!(body.get("active_session_audit").is_none(), "{body}");
    for row in mailbox_rows(&body) {
        assert!(row["session_lookup_error"].is_null(), "{row}");
    }
    db.finish().await;

    // A snapshot that takes 1 s leaves the late reads only the remaining 0.5 s.
    let db = Db::start(Duration::ZERO, 8).await;
    db.seed_blocks().await;
    db.proxy.stall("DISTINCT ON (thread_channel_id)");
    let detail = state(&db.pool, Some(registry.clone()), Default::default());
    let (_, body, elapsed) = registry_body_after(&detail, true, Duration::from_secs(1), None).await;
    assert!(elapsed < BUDGET, "late session read {elapsed:?}: {body}");
    assert!(reasons(&body).contains(&"db_slow".to_string()), "{body}");
    assert_eq!(body["dispatch_outbox"]["pending"], 1, "{body}");
    assert_eq!(body["config_audit"]["checked"], 3, "{body}");
    for row in mailbox_rows(&body) {
        let message = row["session_lookup_error"]["message"]
            .as_str()
            .unwrap_or_default();
        assert!(message.contains("during session lookup"), "{row}");
    }
    db.proxy.release();
    db.proxy.stall("FROM worker_nodes");
    let standby = state(
        &db.pool,
        Some(Arc::new(HealthRegistry::new())),
        standby_config(),
    );
    let (_, body, elapsed) =
        registry_body_after(&standby, false, Duration::from_secs(1), None).await;
    assert!(elapsed < BUDGET, "late standby read {elapsed:?}: {body}");
    assert!(reasons(&body).contains(&"db_slow".to_string()), "{body}");
    assert_eq!(
        (&body["db"], &body["auto_queue_cleanup"]["pending"]),
        (&Value::Bool(true), &json_one())
    );
    db.finish().await;
}

fn json_one() -> Value {
    Value::from(1)
}

/// A SELECT 1 that never answers is an unready database, not a slow diagnostic: 503 with
/// `db_slow` and without `db_unavailable`, on both arms. The watchdog still reads it as an answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn health_handler_select1_timeout_is_unhealthy() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = Db::start(Duration::ZERO, 8).await;
    db.proxy.stall("SELECT 1");
    let check = |status: u16, body: &Value, elapsed: Duration| {
        assert!(elapsed < BUDGET, "{elapsed:?}: {body}");
        assert_eq!(status, 503, "{body}");
        assert_eq!(
            (&body["db"], &body["server_up"]),
            (&Value::Bool(false), &Value::Bool(false))
        );
        let reasons = reasons(body);
        assert!(reasons.contains(&"db_slow".to_string()), "{body}");
        assert!(!reasons.contains(&"db_unavailable".to_string()), "{body}");
    };
    let addr = serve(app(&db.pool, None, Default::default())).await;
    let (status, body, elapsed) = http_get(addr, "/health").await;
    check(status, &body, elapsed);

    let healthy =
        serde_json::json!({"status": "healthy", "degraded_reasons": [], "fully_recovered": true});
    let registry = state(
        &db.pool,
        Some(Arc::new(HealthRegistry::new())),
        Default::default(),
    );
    let (status, body, elapsed) =
        registry_body_after(&registry, false, Duration::ZERO, Some(healthy)).await;
    check(status, &body, elapsed);

    let api = axum::Router::new().nest("/api", app(&db.pool, None, Default::default()));
    let addr = serve(api).await.to_string();
    let outcome = tokio::task::spawn_blocking(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        hang_forensics::probe_health_once(&addr, "127.0.0.1", deadline).outcome
    })
    .await
    .unwrap();
    let crumbs = hang_forensics::snapshot();
    let verdict = hang_forensics::classify_health(&outcome, crumbs.runtime_liveness(), &crumbs);
    assert_eq!(
        verdict,
        hang_forensics::HealthVerdict::Responsive,
        "{outcome:?}"
    );
    db.finish().await;
}

/// A snapshot that runs past the deadline is not a slow database: no `db_slow`, every mailbox
/// row unmeasured rather than absent, and the dispatch-gate read from the initial reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn health_handler_snapshot_delay_marks_bulk_unmeasured() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = Db::start(Duration::ZERO, 8).await;
    db.seed_blocks().await;
    let channels = mailboxes(3);
    let registry = registry_with_mailboxes(&channels).await;
    let state = state(&db.pool, Some(registry), Default::default());
    for detailed in [true, false] {
        let (_, body, elapsed) =
            registry_body_after(&state, detailed, Duration::from_millis(1_600), None).await;
        assert!(elapsed < BUDGET, "{elapsed:?}: {body}");
        assert!(
            !reasons(&body).iter().any(|reason| reason == "db_slow"),
            "{body}"
        );
        assert_eq!(body["db"], true, "{body}");
        if detailed {
            assert_eq!(body["rate_limit_dispatch_gate"]["danger_pct"], 77, "{body}");
            assert_eq!(mailbox_rows(&body).len(), channels.len(), "{body}");
            for row in mailbox_rows(&body) {
                let error = &row["session_lookup_error"];
                assert_eq!(error["safety_gate"], "measurement_unavailable", "{row}");
                let message = error["message"].as_str().unwrap_or_default();
                assert!(message.contains("before session lookup"), "{row}");
            }
        }
    }
    db.finish().await;
}

/// Repeated requests add no connection or thread; a cut or cancelled one leaves no probe in flight,
/// and the next reads correctly. It reruns alone in a new process, so it counts only its threads.
#[test]
fn health_handler_timeout_drops_all_db_guards() {
    if resource_regression_tests::rerun_alone(concat!(
        module_path!(),
        "::health_handler_timeout_drops_all_db_guards"
    )) {
        return;
    }
    let _serial = hang_forensics::counter_test_lock();
    counted_runtime().block_on(health_handler_timeout_drops_all_db_guards_body());
}

async fn health_handler_timeout_drops_all_db_guards_body() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = Db::start(Duration::ZERO, 8).await;
    db.seed_blocks().await;
    let registry = registry_with_mailboxes(&mailboxes(4)).await;
    let addr = serve(app(&db.pool, Some(registry.clone()), Default::default())).await;
    resource_regression_tests::repeated_requests_keep_resources(&db, addr).await;
    db.proxy.stall("FROM dispatch_outbox");

    let before = hang_forensics::snapshot();
    let (_, body, _) = http_get(addr, "/health/detail").await;
    assert!(reasons(&body).contains(&"db_slow".to_string()), "{body}");
    let answered = hang_forensics::snapshot();
    assert_eq!(
        answered.db_in_flight, before.db_in_flight,
        "a cut read must release its slot"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    let later = hang_forensics::snapshot();
    assert_eq!(
        later.db_probes_started, answered.db_probes_started,
        "nothing runs on after"
    );
    assert_eq!(later.db_in_flight, before.db_in_flight);

    // Dropping the handler future mid-read, as a closed client connection does.
    let mut request = axum::http::Request::builder()
        .uri("/health/detail")
        .body(axum::body::Body::empty())
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.1:8791".parse::<SocketAddr>().unwrap(),
    ));
    let handler =
        tower::ServiceExt::oneshot(app(&db.pool, Some(registry), Default::default()), request);
    let cut = tokio::time::timeout(Duration::from_millis(300), handler).await;
    assert!(
        cut.is_err(),
        "the stalled read must still be pending at 300 ms"
    );
    assert_eq!(
        hang_forensics::snapshot().db_in_flight,
        before.db_in_flight,
        "cancelled"
    );

    db.proxy.release();
    let (status, body, _) = http_get(addr, "/health/detail").await;
    assert_eq!(body["db"], true, "{status} {body}");
    assert!(
        !reasons(&body).iter().any(|reason| reason == "db_slow"),
        "{body}"
    );
    assert_eq!(body["dispatch_outbox"]["pending"], 1, "{body}");
    assert_eq!(body["dispatch_outbox"]["permanent_failures"], 1, "{body}");
    resource_regression_tests::connections_return_after_cuts(&db).await;
    db.finish().await;
}

/// With no open connection and a 600 ms link, connecting alone outlasts the deadline: the
/// answer is still bounded and says `db_slow`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn health_handler_cold_pool_is_bounded_and_marked_slow() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = Db::start(RTT * 2, 8).await;
    let addr = serve(app(&db.pool, None, Default::default())).await;
    let (_, body, elapsed) = http_get(addr, "/health").await;
    assert!(elapsed < BUDGET, "{elapsed:?}: {body}");
    assert!(reasons(&body).contains(&"db_slow".to_string()), "{body}");
    db.finish().await;
}
