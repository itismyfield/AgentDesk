use super::*;
use sqlx::Connection;

fn capture() -> (tracing::subscriber::DefaultGuard, std::fs::File) {
    let file = tempfile::tempfile().unwrap();
    let reader = file.try_clone().unwrap();
    crate::logging::test_capture::pin_callsite_interest();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || file.try_clone().unwrap())
        .finish();
    (tracing::subscriber::set_default(subscriber), reader)
}

fn logs(reader: &mut std::fs::File) -> String {
    use std::io::{Read, Seek, SeekFrom};
    reader.seek(SeekFrom::Start(0)).unwrap();
    let mut logs = String::new();
    reader.read_to_string(&mut logs).unwrap();
    logs
}

async fn database() -> (
    crate::db::auto_queue::test_support::TestPostgresDb,
    sqlx::PgPool,
) {
    let fixture = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    (fixture, pool)
}

async fn hold(pool: &sqlx::PgPool, key: i64) -> sqlx::PgConnection {
    let mut connection = sqlx::PgConnection::connect_with(&pool.connect_options())
        .await
        .unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(key)
        .execute(&mut connection)
        .await
        .unwrap();
    connection
}

fn allow_next_snapshot(diagnostics: &Diagnostics) {
    diagnostics
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .next_snapshot = None;
}

async fn observe(
    diagnostics: &Diagnostics,
    pool: &sqlx::PgPool,
    key: i64,
    result: LeaseResult,
    short: bool,
) {
    diagnostics.record(result);
    diagnostics.failure(pool, key, result, short).await;
}

#[tokio::test]
async fn snapshot_only_classifies_current_short_circuit_free_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (fixture, pool) = database().await;
    let key = -914802601;
    let mut peer = hold(&pool, key).await;
    let diagnostics = Diagnostics::default();
    diagnostics.context(&pool, "standby", "claude", key);
    let actual = crate::db::postgres::AdvisoryLockLease::try_acquire_named_expiring_observed(
        &pool,
        key,
        "real false try before free snapshot",
        "diagnostic-test",
        None,
        |event| diagnostics.attempt(event),
    )
    .await
    .unwrap();
    assert!(actual.is_none());
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(key)
        .execute(&mut peer)
        .await
        .unwrap();
    diagnostics
        .failure(&pool, key, LeaseResult::Held, false)
        .await;
    let view = diagnostics.projection();
    assert_eq!(view.snapshot_status, SnapshotStatus::Free);
    assert_eq!(
        view.last_result,
        Some(LeaseResult::Held),
        "a real false try followed by free is not a short circuit"
    );
    assert!(!view.results.contains_key(&LeaseResult::RefusedWhileFree));

    let yielding = Diagnostics::default();
    yielding.context(&pool, "yield-wait", "claude", key);
    observe(&yielding, &pool, key, LeaseResult::Waiting, true).await;
    assert_eq!(
        yielding.projection().last_result,
        Some(LeaseResult::RefusedWhileFree)
    );
    assert_eq!(yielding.projection().tries, 0);
    observe(&yielding, &pool, key, LeaseResult::Waiting, true).await;
    assert_eq!(
        yielding.projection().last_result,
        Some(LeaseResult::Waiting),
        "a previous free snapshot cannot classify a new skipped try"
    );
    assert_eq!(
        yielding.projection().results[&LeaseResult::RefusedWhileFree],
        1
    );
    assert_eq!(yielding.projection().snapshot_attempts, 1);
    peer.close().await.unwrap();
    if std::env::var_os("I7924_PREPARED_HOLDER").is_some() {
        let mut connection = sqlx::PgConnection::connect_with(&pool.connect_options())
            .await
            .unwrap();
        sqlx::query("BEGIN").execute(&mut connection).await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(key)
            .execute(&mut connection)
            .await
            .unwrap();
        sqlx::query("PREPARE TRANSACTION 'i7924_prepared_diagnostic'")
            .execute(&mut connection)
            .await
            .unwrap();
        connection.close().await.unwrap();
        let prepared = Diagnostics::default();
        prepared.context(&pool, "yield-wait", "claude", key);
        observe(&prepared, &pool, key, LeaseResult::Waiting, true).await;
        let view = prepared.projection();
        sqlx::query("ROLLBACK PREPARED 'i7924_prepared_diagnostic'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(view.snapshot_status, SnapshotStatus::Held);
        assert_eq!(view.last_result, Some(LeaseResult::Waiting));
        assert!(view.holder.unwrap().pid.is_none());
        eprintln!("PREPARED_HOLDER status=held pid=null refused_while_free=0");
    }
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test]
async fn snapshot_transition_pending_identity_and_new_streak_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (fixture, pool) = database().await;
    let key = -914802602;
    let (_capture, mut reader) = capture();
    let peer = hold(&pool, key).await;
    let diagnostics = Diagnostics::default();
    diagnostics.context(&pool, "standby", "claude", key);
    observe(&diagnostics, &pool, key, LeaseResult::Held, false).await;
    let first = diagnostics.projection().holder.unwrap();
    assert!(first.backend_start.is_some());
    observe(&diagnostics, &pool, key, LeaseResult::Err, false).await;
    observe(&diagnostics, &pool, key, LeaseResult::Held, false).await;
    assert!(
        diagnostics
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
    );
    assert_eq!(diagnostics.projection().snapshot_attempts, 1);
    allow_next_snapshot(&diagnostics);
    observe(&diagnostics, &pool, key, LeaseResult::Held, false).await;
    assert_eq!(
        logs(&mut reader)
            .matches("GATEWAY-LEASE: holder snapshot")
            .count(),
        2,
        "result transitions pending during the cap must trigger a snapshot event even with the same holder"
    );
    allow_next_snapshot(&diagnostics);
    observe(&diagnostics, &pool, key, LeaseResult::Held, false).await;
    assert_eq!(
        logs(&mut reader)
            .matches("GATEWAY-LEASE: holder snapshot")
            .count(),
        2,
        "unchanged identity and result do not duplicate snapshot events"
    );
    // Simulate a cached PID reused with an older backend_start; the new sample is real PG data.
    diagnostics
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .view
        .holder
        .as_mut()
        .unwrap()
        .backend_start = first
        .backend_start
        .map(|at| at - chrono::Duration::microseconds(1));
    allow_next_snapshot(&diagnostics);
    observe(&diagnostics, &pool, key, LeaseResult::Held, false).await;
    assert_eq!(
        logs(&mut reader)
            .matches("GATEWAY-LEASE: holder snapshot")
            .count(),
        3,
        "backend_start alone distinguishes a reused PID"
    );
    peer.close().await.unwrap();
    let replacement = hold(&pool, key).await;
    allow_next_snapshot(&diagnostics);
    observe(&diagnostics, &pool, key, LeaseResult::Held, false).await;
    assert_ne!(diagnostics.projection().holder.unwrap().pid, first.pid);
    assert_eq!(
        logs(&mut reader)
            .matches("GATEWAY-LEASE: holder snapshot")
            .count(),
        4
    );
    diagnostics.record(LeaseResult::Acquired);
    assert_eq!(
        diagnostics
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failure_result,
        Some(LeaseResult::Held),
        "raw true must not clear the failure streak before adoption"
    );
    diagnostics.adopted();
    allow_next_snapshot(&diagnostics);
    observe(&diagnostics, &pool, key, LeaseResult::Held, false).await;
    assert_eq!(
        logs(&mut reader)
            .matches("GATEWAY-LEASE: holder snapshot")
            .count(),
        5
    );
    replacement.close().await.unwrap();
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test]
async fn snapshot_filters_database_bigint_namespace_and_exact_key_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (fixture, pool) = database().await;
    let base = crate::db::postgres::postgres_test_database_url_base().unwrap();
    let other_pool = sqlx::PgPool::connect(&format!("{base}/postgres"))
        .await
        .unwrap();
    let key = -914802603;
    let foreign_database = hold(&other_pool, key).await;
    let adjacent = hold(&pool, key + 1).await;
    let mut two_int = sqlx::PgConnection::connect_with(&pool.connect_options())
        .await
        .unwrap();
    sqlx::query("SELECT pg_advisory_lock($1::int, $2::int)")
        .bind((key >> 32) as i32)
        .bind(key as i32)
        .execute(&mut two_int)
        .await
        .unwrap();
    let mut actual = hold(&pool, key).await;
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut actual)
        .await
        .unwrap();
    let diagnostics = Diagnostics::default();
    diagnostics.context(&pool, "standby", "claude", key);
    observe(&diagnostics, &pool, key, LeaseResult::Held, false).await;
    assert_eq!(diagnostics.projection().holder.unwrap().pid, Some(pid));
    actual.close().await.unwrap();
    allow_next_snapshot(&diagnostics);
    observe(&diagnostics, &pool, key, LeaseResult::Held, false).await;
    assert!(
        diagnostics.projection().holder.is_none(),
        "other database, two-int namespace, and adjacent key cannot become the holder"
    );
    assert_eq!(
        diagnostics.projection().snapshot_status,
        SnapshotStatus::Free
    );
    foreign_database.close().await.unwrap();
    adjacent.close().await.unwrap();
    two_int.close().await.unwrap();
    other_pool.close().await;
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test]
async fn snapshot_concurrency_and_cancellation_keep_reserved_budget_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (fixture, pool) = database().await;
    let key = 914802604;
    let diagnostics = Diagnostics::default();
    diagnostics.context(&pool, "standby", "claude", key);
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            observe(&diagnostics, &pool, 914802604, LeaseResult::Held, false),
            observe(&diagnostics, &pool, 914802604, LeaseResult::Err, false),
        );
    })
    .await
    .unwrap();
    assert_eq!(diagnostics.projection().snapshot_attempts, 1);
    assert!(
        diagnostics
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
    );
    let saturated = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&fixture.database_url)
        .await
        .unwrap();
    let connection = saturated.acquire().await.unwrap();
    let cancelled = Diagnostics::default();
    cancelled.context(&saturated, "standby", "claude", key);
    assert!(
        tokio::time::timeout(
            Duration::from_millis(20),
            observe(&cancelled, &saturated, 914802604, LeaseResult::Held, false)
        )
        .await
        .is_err()
    );
    assert!(
        !cancelled
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .in_flight
    );
    assert!(
        cancelled
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
    );
    drop(connection);
    observe(&cancelled, &saturated, 914802604, LeaseResult::Held, false).await;
    assert_eq!(
        cancelled.projection().snapshot_attempts,
        1,
        "cancellation cannot refund the minute budget"
    );
    saturated.close().await;
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test(start_paused = true)]
async fn monotonic_minute_budget_precedes_transitions_and_summary_ticks() {
    let diagnostics = Diagnostics::default();
    let (_capture, mut reader) = capture();
    let start = Instant::now();
    diagnostics.record(LeaseResult::Held);
    assert!(diagnostics.reserve(start, LeaseResult::Held).is_some());
    diagnostics.record(LeaseResult::Err);
    assert!(
        diagnostics
            .reserve(start + MINUTE * 2, LeaseResult::Err)
            .is_none(),
        "a query still in flight excludes a second query"
    );
    diagnostics
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .in_flight = false;
    for n in 0..60 {
        let result = if n % 2 == 0 {
            LeaseResult::Err
        } else {
            LeaseResult::Held
        };
        diagnostics.record(result);
        assert!(
            diagnostics
                .reserve(start + Duration::from_millis(59_999), result)
                .is_none()
        );
    }
    assert_eq!(diagnostics.projection().snapshot_attempts, 1);
    assert!(
        diagnostics
            .reserve(start + MINUTE, LeaseResult::Held)
            .unwrap()
            .2
    );
    assert_eq!(diagnostics.projection().snapshot_attempts, 2);
    diagnostics.summary();
    tokio::time::advance(Duration::from_millis(59_999)).await;
    diagnostics.summary();
    assert_eq!(
        logs(&mut reader)
            .matches("⏳ GATEWAY-LEASE: waiting for adoption")
            .count(),
        1
    );
    tokio::time::advance(Duration::from_millis(1)).await;
    diagnostics.summary();
    assert_eq!(
        logs(&mut reader)
            .matches("⏳ GATEWAY-LEASE: waiting for adoption")
            .count(),
        2
    );
}

#[tokio::test(start_paused = true)]
async fn delayed_completion_cannot_bunch_two_reads_into_one_minute() {
    let diagnostics = Diagnostics::default();
    let start = Instant::now();
    assert!(diagnostics.reserve(start, LeaseResult::Held).is_some());
    let reservation = Reservation {
        diagnostics: &diagnostics,
        completed: true,
    };
    tokio::time::advance(MINUTE * 2).await;
    drop(reservation);
    assert!(
        diagnostics
            .reserve(Instant::now(), LeaseResult::Held)
            .is_none()
    );
    tokio::time::advance(Duration::from_millis(59_999)).await;
    assert!(
        diagnostics
            .reserve(Instant::now(), LeaseResult::Held)
            .is_none()
    );
    tokio::time::advance(Duration::from_millis(1)).await;
    assert!(
        diagnostics
            .reserve(Instant::now(), LeaseResult::Held)
            .is_some()
    );
}
