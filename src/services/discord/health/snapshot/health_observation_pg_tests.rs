//! `/api/health` over real HTTP and a real database with stalled mailbox actors: the actor reads
//! and the database reads end by one deadline, and the existing gates still refuse the result.

use std::net::SocketAddr;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::tests::{Driver, mailbox, parked_runtime, parked_turn, read_failures, reasons};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::host_teardown_gate::test_support::nameless_turn;
use crate::services::health_diagnostics::pg_wire_proxy::PgWireProxy;

const RTT: Duration = Duration::from_millis(300);
const BUDGET: Duration = Duration::from_secs(2);

/// A migrated database and a pool that reaches it through a latency and stall proxy.
pub(in crate::services::discord::health) struct Db {
    db: TestPostgresDb,
    seed: sqlx::PgPool,
    pub(in crate::services::discord::health) proxy: PgWireProxy,
    pub(in crate::services::discord::health) pool: sqlx::PgPool,
}

impl Db {
    /// Opens `warm` connections before the proxy adds `delay`, so requests find them idle.
    pub(in crate::services::discord::health) async fn start(delay: Duration, warm: usize) -> Self {
        let db = TestPostgresDb::create().await;
        let seed = db.connect_and_migrate().await;
        let proxy = PgWireProxy::start(&db.database_url, Duration::ZERO).await;
        let pool = proxy.pool(8);
        let held = futures::future::join_all((0..warm).map(|_| pool.acquire())).await;
        drop(held);
        proxy.set_delay(delay);
        Self {
            db,
            seed,
            proxy,
            pool,
        }
    }

    pub(in crate::services::discord::health) async fn finish(self) {
        self.proxy.release();
        self.pool.close().await;
        self.seed.close().await;
        drop(self.proxy);
        self.db.drop().await;
    }
}

/// Serves the API router under `/api`, the path the self-watchdog probes.
pub(in crate::services::discord::health) async fn serve(
    pool: &sqlx::PgPool,
    registry: Arc<HealthRegistry>,
) -> SocketAddr {
    let mut engine_config = crate::config::Config::default();
    engine_config.policies.hot_reload = false;
    let engine = crate::engine::PolicyEngine::new(&engine_config).unwrap();
    let tx = crate::server::ws::new_broadcast();
    let buf = crate::server::ws::spawn_batch_flusher(tx.clone());
    let api = crate::server::routes::api_router_with_pg(
        engine,
        Default::default(),
        tx,
        buf,
        Some(registry),
        Some(pool.clone()),
    );
    let app = axum::Router::new().nest("/api", api);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = app.into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, service).await.unwrap() });
    addr
}

/// One GET over loopback; the elapsed time runs to the body's last byte.
pub(in crate::services::discord::health) async fn http_get(
    addr: SocketAddr,
    path: &str,
) -> (u16, Value, Duration) {
    let started = Instant::now();
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nhost: {addr}\r\nconnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(6), stream.read_to_end(&mut raw)).await;
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

pub(in crate::services::discord::health) fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
}

/// A connected gateway provider, the shape deploy readiness accepts when nothing else is wrong.
pub(in crate::services::discord::health) async fn gateway(
    registry: &HealthRegistry,
) -> Arc<crate::services::discord::SharedData> {
    let shared = crate::services::discord::make_shared_data_for_tests();
    shared
        .bot_connected
        .store(true, std::sync::atomic::Ordering::Relaxed);
    registry.register("claude".into(), shared.clone()).await;
    shared
}

/// An 800ms actor, a silent reconfirm and a 300ms database: the actor reads spend the deadline,
/// the late database read finds none left, and the response still ends under 2s.
#[test]
fn health_handler_actor_and_db_share_one_deadline() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let main = runtime();
    let registry = Arc::new(HealthRegistry::new());
    let claude = main.block_on(gateway(&registry));
    let base = 6_871_350_000;
    let (slow, reconfirm, normal) = (
        poise::serenity_prelude::ChannelId::new(base + 1),
        poise::serenity_prelude::ChannelId::new(base + 2),
        poise::serenity_prelude::ChannelId::new(base + 3),
    );
    let slow_runtime = parked_runtime();
    parked_turn(&slow_runtime, &claude, slow);
    let reconfirm_runtime = parked_runtime();
    parked_turn(&reconfirm_runtime, &claude, reconfirm);
    let (db, addr) = main.block_on(async {
        nameless_turn(&claude, normal).await;
        let db = Db::start(RTT, 8).await;
        let addr = serve(&db.pool, registry.clone()).await;
        (db, addr)
    });
    let handle = claude.mailbox_peek(reconfirm).unwrap();
    let _slow = Driver::answer_from(slow_runtime, Duration::from_millis(800));
    let _once = Driver::start(
        reconfirm_runtime,
        Duration::from_millis(300),
        |_| async move {
            let _ = handle.try_snapshot().await;
        },
    );
    main.block_on(async {
        let (status, body, elapsed) = http_get(addr, "/api/health/detail").await;
        assert!(elapsed < BUDGET, "{elapsed:?}: {status} {body}");
        assert_eq!(body["db"], true, "{body}");
        let entry = mailbox(&body, "claude", slow).expect("the 800ms actor answered in time");
        assert_eq!(entry["has_cancel_token"], true);
        let failures = read_failures(&body);
        assert!(
            failures.contains(&(
                "claude".to_string(),
                reconfirm.get(),
                "reconfirm".to_string()
            )),
            "{failures:?}"
        );
        assert_eq!(failures.len(), 1, "{failures:?}");
        // The actor reads ran to the deadline, so the bulk session read had nothing left.
        for channel in [slow, reconfirm, normal] {
            let row = mailbox(&body, "claude", channel).unwrap();
            let message = row["session_lookup_error"]["message"]
                .as_str()
                .unwrap_or_default();
            assert!(message.contains("before session lookup"), "{row}");
        }
        assert!(!reasons(&body).contains(&"db_slow".to_string()), "{body}");

        let (_, public, elapsed) = http_get(addr, "/api/health").await;
        assert!(elapsed < BUDGET, "{elapsed:?}: {public}");
        db.finish().await;
    });
}

/// Every executable on PATH except jq, so the readiness predicate takes its jq-less path.
fn path_without_jq(farm: &Path) -> std::ffi::OsString {
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap()) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let link = farm.join(entry.file_name());
            if entry.file_name() != "jq" && !link.exists() {
                let _ = std::os::unix::fs::symlink(entry.path(), link);
            }
        }
    }
    farm.as_os_str().to_owned()
}

/// Whether the deploy readiness predicate accepts `body`, with or without jq.
fn deploy_ready(body: &Value, jq: bool) -> bool {
    let farm = tempfile::tempdir().unwrap();
    let mut command = Command::new("bash");
    if !jq {
        command.env("PATH", path_without_jq(farm.path()));
    }
    let script = r#"
        [ "$(command -v jq >/dev/null 2>&1 && echo 1 || echo 0)" = "$WANT_JQ" ] || exit 90
        . "$DEFAULTS" || exit 91
        health_json_is_ready "$BODY" 1 1 1 1 >/dev/null 2>&1"#;
    let status = command
        .args(["-c", script])
        .env("WANT_JQ", if jq { "1" } else { "0" })
        .env(
            "DEFAULTS",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/_defaults.sh"),
        )
        .env("BODY", body.to_string())
        .status()
        .unwrap();
    match status.code() {
        Some(0) => true,
        Some(1) => false,
        other => panic!("readiness harness failed ({other:?}, jq={jq}) for {body}"),
    }
}

/// Deploy readiness refuses a stalled actor's public body with and without jq, which names no
/// channel; the same node without that actor is ready, so the new reason is what refuses it.
#[test]
fn health_timeout_consumers_keep_existing_gates() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let main = runtime();
    let parked = parked_runtime();
    let stalled_registry = Arc::new(HealthRegistry::new());
    let ready_registry = Arc::new(HealthRegistry::new());
    let stalled = poise::serenity_prelude::ChannelId::new(6_871_360_001);
    let shared = main.block_on(async {
        gateway(&ready_registry).await;
        gateway(&stalled_registry).await
    });
    parked_turn(&parked, &shared, stalled);
    main.block_on(async {
        let db = Db::start(Duration::ZERO, 4).await;
        let mut bodies = Vec::new();
        for registry in [&ready_registry, &stalled_registry] {
            let addr = serve(&db.pool, registry.clone()).await;
            let (_, mut body, elapsed) = http_get(addr, "/api/health").await;
            assert!(elapsed < BUDGET, "{elapsed:?}: {body}");
            // No dashboard is built in tests; the axis is not what this test exercises.
            body["dashboard"] = true.into();
            bodies.push(body);
        }
        let [ready, refused] = <[Value; 2]>::try_from(bodies).unwrap();
        for jq in [true, false] {
            assert!(
                deploy_ready(&ready, jq),
                "control must be ready (jq={jq}): {ready}"
            );
            assert!(
                !deploy_ready(&refused, jq),
                "stalled actor body was ready (jq={jq}): {refused}"
            );
        }
        assert!(
            reasons(&refused).contains(&"mailbox_snapshot_unavailable".to_string()),
            "{refused}"
        );
        assert_eq!(refused["degraded"], true, "{refused}");
        for key in [
            "mailboxes",
            "mailbox_observation_failures",
            "queue_park_observation_failures",
        ] {
            assert!(
                refused.get(key).is_none(),
                "public body carries {key}: {refused}"
            );
        }
        assert!(
            !refused.to_string().contains(&stalled.get().to_string()),
            "{refused}"
        );

        db.finish().await;
    });
}
