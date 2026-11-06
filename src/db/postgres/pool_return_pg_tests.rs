use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::relay_dead_letter::{self as dlq, RelayDeadLetterRecord};
use crate::services::health_diagnostics::pg_wire_proxy::PgWireProxy;
use sqlx::{Connection, PgConnection};

fn runtime_options() -> PgPoolOptions {
    pool_options(PoolConnectSettings {
        max_connections: 1,
        acquire_timeout: Duration::from_secs(2),
        idle_timeout: Duration::from_secs(300),
        max_lifetime: Duration::from_secs(1800),
        test_before_acquire: true,
    })
}

async fn observer(db: &TestPostgresDb) -> PgConnection {
    let mut observer = PgConnection::connect(&db.database_url).await.unwrap();
    sqlx::raw_sql("CREATE TABLE pool_return_markers (nonce TEXT PRIMARY KEY)")
        .execute(&mut observer)
        .await
        .unwrap();
    observer
}

async fn idle(pool: &PgPool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while pool.num_idle() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("one-slot pool returned its connection");
}

async fn insert_marker(pool: &PgPool) {
    sqlx::query("INSERT INTO pool_return_markers VALUES ('after-return')")
        .execute(pool)
        .await
        .unwrap();
}

async fn marker_count(observer: &mut PgConnection) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM pool_return_markers")
        .fetch_one(observer)
        .await
        .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn dead_letter_timeout_during_begin_does_not_poison_next_borrower_pg() {
    let db = TestPostgresDb::create().await;
    let migrated = db.connect_and_migrate().await;
    migrated.close().await;
    let proxy = PgWireProxy::start(&db.database_url, Duration::ZERO).await;
    let pool = runtime_options()
        .connect_with(proxy.connect_options())
        .await
        .unwrap();
    idle(&pool).await;
    proxy.stall("BEGIN");
    let operation = tokio::spawn({
        let pool = pool.clone();
        async move {
            dlq::record_too_old(
                Some(&pool),
                RelayDeadLetterRecord {
                    kind: dlq::KIND_CATCH_UP_TOO_OLD.into(),
                    channel_id: "7844".into(),
                    author_id: None,
                    message_id: Some("1".into()),
                    content: "cancelled input".into(),
                    reason: "too old".into(),
                },
            )
            .await
        }
    });
    proxy
        .wait_held(tokio::time::Instant::now() + Duration::from_secs(2))
        .await;
    assert_eq!(operation.await.unwrap(), None, "real DLQ timeout fired");
    proxy.release();
    sqlx::query("SELECT COUNT(*) FROM intake_outbox")
        .execute(&pool)
        .await
        .unwrap();
    let owner = crate::services::tui_o::ownership::execution::read_observation(
        &pool,
        "codex",
        7844,
        "pool-return-test",
        || Ok(false),
    )
    .await;
    let next = sqlx::query("SELECT 1").execute(&pool).await;
    eprintln!(
        "owner_error={:?}; next_consumer={next:?}",
        owner.as_ref().err().map(|e| &e.reason)
    );
    assert!(owner.is_ok(), "owner read must start a fresh transaction");
    assert!(next.is_ok(), "next borrower must not receive 25P02");
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn write_after_cancelled_begin_is_visible_to_other_sessions_pg() {
    let db = TestPostgresDb::create().await;
    let mut observer = observer(&db).await;
    let proxy = PgWireProxy::start(&db.database_url, Duration::ZERO).await;
    let pool = runtime_options()
        .connect_with(proxy.connect_options())
        .await
        .unwrap();
    idle(&pool).await;
    proxy.stall("BEGIN");
    let operation = tokio::spawn({
        let pool = pool.clone();
        async move { pool.begin().await }
    });
    proxy
        .wait_held(tokio::time::Instant::now() + Duration::from_secs(2))
        .await;
    operation.abort();
    assert!(operation.await.unwrap_err().is_cancelled());
    proxy.release();
    insert_marker(&pool).await;
    let count = marker_count(&mut observer).await;
    eprintln!("observer_count={count}");
    assert_eq!(
        count, 1,
        "successful INSERT must be committed for another session"
    );
    pool.close().await;
    observer.close().await.unwrap();
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn isolated_bridge_pool_cleans_returned_connection_pg() {
    let db = TestPostgresDb::create().await;
    let mut observer = observer(&db).await;
    let source = runtime_options().connect(&db.database_url).await.unwrap();
    let bridge_source = source.clone();
    std::thread::spawn(move || {
        crate::utils::async_bridge::block_on_pg_result(
            &bridge_source,
            |pool| async move {
                let mut connection = pool.acquire().await.map_err(|e| e.to_string())?;
                sqlx::raw_sql("BEGIN")
                    .execute(&mut *connection)
                    .await
                    .map_err(|e| e.to_string())?;
                drop(connection);
                insert_marker(&pool).await;
                Ok::<_, String>(())
            },
            |e| e,
        )
    })
    .join()
    .unwrap()
    .unwrap();
    let count = marker_count(&mut observer).await;
    eprintln!("bridge_observer_count={count}");
    assert_eq!(count, 1, "same isolated bridge pool must clean each return");
    source.close().await;
    observer.close().await.unwrap();
    db.drop().await;
}

#[derive(Clone, Default)]
struct CapturingWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for CapturingWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturingWriter {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn returned_connection_disposition_pg() {
    let db = TestPostgresDb::create().await;
    let mut observer = observer(&db).await;
    let writer = CapturingWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(writer.clone())
        .with_env_filter(format!(
            "{},sqlx::postgres::notice=warn",
            crate::logging::DEFAULT_TRACING_DIRECTIVE
        ))
        .finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);
    for state in ["I", "T", "E", "stall"] {
        let proxy = PgWireProxy::start(&db.database_url, Duration::ZERO).await;
        let pool = super::pool_return::with_return_cleanup(
            PgPoolOptions::new()
                .max_connections(1)
                .test_before_acquire(true)
                .acquire_timeout(Duration::from_secs(1)),
            Duration::from_millis(300),
        )
        .connect_lazy_with(proxy.connect_options());
        let mut connection = pool.acquire().await.unwrap();
        let before: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *connection)
            .await
            .unwrap();
        match state {
            "T" | "E" => {
                sqlx::raw_sql("BEGIN")
                    .execute(&mut *connection)
                    .await
                    .unwrap();
                if state == "E" {
                    let error = sqlx::query("SELECT 1/0")
                        .execute(&mut *connection)
                        .await
                        .unwrap_err();
                    assert_eq!(
                        error.as_database_error().and_then(|e| e.code()).as_deref(),
                        Some("22012")
                    );
                    connection.ping().await.unwrap();
                }
            }
            "stall" => proxy.stall("BEGIN; ROLLBACK;"),
            _ => {}
        }
        writer.0.lock().unwrap().clear();
        drop(connection);
        if state == "stall" {
            proxy
                .wait_held(tokio::time::Instant::now() + Duration::from_secs(1))
                .await;
        }
        let replacement = pool.acquire().await;
        eprintln!(
            "state={state}; acquire_error={:?}",
            replacement.as_ref().err()
        );
        if replacement.is_err() {
            proxy.release();
        }
        let mut connection =
            replacement.expect("return cleanup must finish or discard without releasing the stall");
        let after: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *connection)
            .await
            .unwrap();
        let logs = String::from_utf8(writer.0.lock().unwrap().clone()).unwrap();
        eprintln!("state={state}; before_pid={before}; after_pid={after}; logs={logs}");
        if matches!(state, "I" | "T") {
            assert_eq!(before, after, "{state}: healthy connection is reused");
        } else {
            assert_ne!(before, after, "{state}: unsafe connection is discarded");
        }
        match state {
            "I" => assert!(!logs.contains("WARN"), "idle return must be quiet: {logs}"),
            "T" => {
                assert_eq!(
                    logs.matches("open transaction; rolled back").count(),
                    1,
                    "{logs}"
                );
                sqlx::query("INSERT INTO pool_return_markers VALUES ('after-return')")
                    .execute(&mut *connection)
                    .await
                    .unwrap();
                assert_eq!(
                    marker_count(&mut observer).await,
                    1,
                    "T: write is autocommitted"
                );
            }
            "E" => {
                sqlx::query("SELECT 1")
                    .execute(&mut *connection)
                    .await
                    .unwrap();
                assert_eq!(
                    logs.matches("discarding returned connection").count(),
                    1,
                    "{logs}"
                );
                assert!(logs.contains("25P02"), "discard includes SQLSTATE: {logs}");
            }
            "stall" => assert_eq!(
                logs.matches("discarding returned connection").count(),
                1,
                "{logs}"
            ),
            _ => unreachable!(),
        }
        drop(connection);
        proxy.release();
        pool.close().await;
    }
    observer.close().await.unwrap();
    db.drop().await;
}
