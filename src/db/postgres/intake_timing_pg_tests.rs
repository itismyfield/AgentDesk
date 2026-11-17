use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::logging::intake_timing::{self as timing, tests as support};
use tracing::Instrument as _;

fn runtime_options(acquire_timeout: Duration) -> PgPoolOptions {
    pool_options(PoolConnectSettings {
        max_connections: 1,
        acquire_timeout,
        idle_timeout: Duration::from_secs(300),
        max_lifetime: Duration::from_secs(1800),
        test_before_acquire: true,
    })
}

#[tokio::test(flavor = "current_thread")]
async fn runtime_pool_work_under_a_trace_counts_without_sql_or_binds_and_timeouts_count_pg() {
    let db = TestPostgresDb::create().await;
    let pool = runtime_options(Duration::from_millis(300))
        .connect(&db.database_url)
        .await
        .unwrap();
    let lines = support::Lines::default();
    let dispatch = support::dispatch(&lines, true);
    let default = tracing::dispatcher::set_default(&dispatch);

    let traced = timing::begin(1, Instant::now());
    async {
        let echoed: String = sqlx::query_scalar("SELECT $1::text")
            .bind("bind-secret-value")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(echoed, "bind-secret-value");
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SELECT 1").execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
    }
    .instrument(traced.clone())
    .await;
    traced.in_scope(|| timing::settle(None));

    let held = pool.acquire().await.unwrap();
    let starved = timing::begin(2, Instant::now());
    let error = sqlx::query("SELECT 1")
        .execute(&pool)
        .instrument(starved)
        .await
        .unwrap_err();
    assert!(matches!(error, sqlx::Error::PoolTimedOut), "{error}");
    timing::finish_message(2, &Err(format!("owner front: {error}")));
    drop(held);
    drop(default);

    let found = lines.timing();
    assert_eq!(found.len(), 2, "{found:?}");
    let (ok, timed_out) = (&found[0], &found[1]);
    assert_eq!(support::field(ok, "db_ops"), "4", "{ok}");
    assert_eq!(support::field(ok, "tx_controls"), "2", "{ok}");
    assert_eq!(support::field(ok, "acquires"), "2", "{ok}");
    assert_eq!(support::field(ok, "acquire_errors"), "0", "{ok}");
    for secret in ["bind-secret-value", "SELECT", "postgres", &db.database_url] {
        assert!(!ok.contains(secret), "{secret} leaked: {ok}");
    }
    assert_eq!(
        support::field(timed_out, "outcome"),
        "failed",
        "{timed_out}"
    );
    assert_eq!(
        support::field(timed_out, "acquire_errors"),
        "1",
        "{timed_out}"
    );
    assert_eq!(support::field(timed_out, "db_ops"), "0", "{timed_out}");
    pool.close().await;
    db.drop().await;
}
