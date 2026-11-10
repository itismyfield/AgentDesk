//! Postgres idempotency, dispatch-outbox claims, and advisory-lock exclusivity.
//!
//! Runs when `POSTGRES_TEST_DATABASE_URL_BASE` is set. `STATE_PROPERTIES_REQUIRE_PG=1`
//! turns a missing server into a failure instead of a skip.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection};

use crate::db::dispatches::outbox::{
    mark_dispatch_outbox_claimed_pg, schedule_outbox_retry_pg,
    select_pending_dispatch_outbox_claim_candidates_pg,
};
use crate::db::idempotency::{IdempotencyOutcome, claim, record_response};
use crate::db::postgres::AdvisoryLockLease;

fn require_pg() -> bool {
    if std::env::var("POSTGRES_TEST_DATABASE_URL_BASE")
        .ok()
        .is_some_and(|value| !value.trim().is_empty())
    {
        return true;
    }
    assert!(
        std::env::var("STATE_PROPERTIES_REQUIRE_PG").ok().as_deref() != Some("1"),
        "POSTGRES_TEST_DATABASE_URL_BASE is required for state property DB tests"
    );
    eprintln!("state property DB tests skipped: POSTGRES_TEST_DATABASE_URL_BASE unset");
    false
}

async fn pool() -> (String, sqlx::PgPool) {
    let base = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE").expect("postgres base url");
    let name = format!("agentdesk_state_prop_{}", uuid::Uuid::new_v4().simple());
    let admin = format!("{base}/postgres");
    let mut conn = PgConnection::connect(&admin).await.expect("admin connection");
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&mut conn)
        .await
        .expect("create database");
    drop(conn);
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&format!("{base}/{name}"))
        .await
        .expect("connect database");
    crate::db::postgres::migrate(&pool)
        .await
        .expect("migrate");
    (name, pool)
}

async fn drop_db(name: &str) {
    let base = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE").expect("postgres base url");
    let Ok(mut conn) = PgConnection::connect(&format!("{base}/postgres")).await else {
        return;
    };
    let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE {name} WITH (FORCE)"
    )))
        .execute(&mut conn)
        .await;
}

async fn guard(
    name: &str,
    test: impl std::future::Future<Output = ()> + Send + 'static,
) -> Option<String> {
    match tokio::spawn(test).await {
        Ok(()) => None,
        Err(error) => Some(format!("{name}: {error}")),
    }
}

pub(super) async fn run() -> Vec<String> {
    let mut failures = Vec::new();
    for failure in [
        guard(
            "idempotency_claim_is_single_owner_and_replay_stable",
            idempotency_claim_is_single_owner_and_replay_stable(),
        )
        .await,
        guard(
            "dispatch_claim_is_exclusive_and_retry_is_fenced",
            dispatch_claim_is_exclusive_and_retry_is_fenced(),
        )
        .await,
        guard(
            "advisory_lock_lease_is_exclusive",
            advisory_lock_lease_is_exclusive(),
        )
        .await,
    ]
    .into_iter()
    .flatten()
    {
        failures.push(failure);
    }
    failures
}

async fn idempotency_claim_is_single_owner_and_replay_stable() {
    if !require_pg() {
        return;
    }
    let (name, pool) = pool().await;
    let pool = Arc::new(pool);
    for round in 0..12u32 {
        let key = format!("turn-{round}");
        let mut joins = Vec::new();
        for worker in 0..6 {
            let pool = Arc::clone(&pool);
            let key = key.clone();
            joins.push(tokio::spawn(async move {
                claim(
                    &pool,
                    "state-properties",
                    &key,
                    "fingerprint-a",
                    Some(&format!("worker-{worker}")),
                    Duration::from_secs(60),
                )
                .await
                .expect("claim")
            }));
        }
        let mut created = 0;
        let mut inflight = 0;
        for join in joins {
            match join.await.expect("join") {
                IdempotencyOutcome::Created { .. } => created += 1,
                IdempotencyOutcome::InFlight => inflight += 1,
                other => panic!("round {round}: unexpected {other:?}"),
            }
        }
        assert_eq!((created, inflight), (1, 5), "round {round}");
        record_response(&pool, "state-properties", &key, 200, &json!({"ok": round}))
            .await
            .expect("record");
        record_response(&pool, "state-properties", &key, 500, &json!({"ok": "replaced"}))
            .await
            .expect("second record");
        match claim(
            &pool,
            "state-properties",
            &key,
            "fingerprint-a",
            None,
            Duration::from_secs(60),
        )
        .await
        .expect("replay")
        {
            IdempotencyOutcome::Replay { status, body, .. } => {
                assert_eq!(status, 200, "round {round}: later record_response changed the replay");
                assert_eq!(body, json!({"ok": round}));
            }
            other => panic!("round {round}: replay became {other:?}"),
        }
        match claim(
            &pool,
            "state-properties",
            &key,
            "fingerprint-b",
            None,
            Duration::from_secs(60),
        )
        .await
        .expect("mismatch")
        {
            IdempotencyOutcome::FingerprintMismatch { .. } => {}
            other => panic!("round {round}: mismatched retry became {other:?}"),
        }
    }
    drop(pool);
    drop_db(&name).await;
}

async fn dispatch_claim_is_exclusive_and_retry_is_fenced() {
    if !require_pg() {
        return;
    }
    let (name, pool) = pool().await;
    let pool = Arc::new(pool);
    for round in 0..8u32 {
        let dispatch_id = format!("state-prop-{round}");
        sqlx::query(
            "INSERT INTO dispatch_outbox (dispatch_id, action, status, retry_count)
             VALUES ($1, 'notify', 'pending', 0)",
        )
        .bind(&dispatch_id)
        .execute(&*pool)
        .await
        .expect("insert outbox");

        let mut joins = Vec::new();
        for worker in 0..6 {
            let pool = Arc::clone(&pool);
            let owner = format!("worker-{worker}");
            let wanted = dispatch_id.clone();
            joins.push(tokio::spawn(async move {
                let mut tx = pool.begin().await.expect("begin");
                let rows = select_pending_dispatch_outbox_claim_candidates_pg(&mut tx, &owner)
                    .await
                    .expect("select");
                let Some(row) = rows.into_iter().find(|row| row.dispatch_id == wanted) else {
                    tx.rollback().await.expect("rollback");
                    return None;
                };
                let claimed_at = mark_dispatch_outbox_claimed_pg(&mut tx, row.id, &owner)
                    .await
                    .expect("mark");
                tx.commit().await.expect("commit");
                Some((row.id, owner, claimed_at))
            }));
        }
        let mut winners = Vec::new();
        for join in joins {
            if let Some(winner) = join.await.expect("join") {
                winners.push(winner);
            }
        }
        assert_eq!(winners.len(), 1, "round {round}: {winners:?}");
        let (outbox_id, owner, claimed_at) = winners.pop().unwrap();

        let stolen = {
            let mut tx = pool.begin().await.expect("begin");
            let claimed = mark_dispatch_outbox_claimed_pg(&mut tx, outbox_id, "thief").await;
            tx.commit().await.expect("commit");
            claimed
        };
        assert!(
            stolen.is_err(),
            "round {round}: a second mark stole the live claim: {stolen:?}"
        );

        let stale = schedule_outbox_retry_pg(
            &pool,
            outbox_id,
            "nope",
            1,
            0,
            "not-the-owner",
            claimed_at,
        )
        .await;
        assert!(stale.is_err(), "round {round}: foreign retry reset the row");

        schedule_outbox_retry_pg(&pool, outbox_id, "retry", 1, 0, &owner, claimed_at)
            .await
            .expect("owner retry");
        let mut tx = pool.begin().await.expect("begin");
        let again = select_pending_dispatch_outbox_claim_candidates_pg(&mut tx, "retry-owner")
            .await
            .expect("reselect");
        assert!(
            again.iter().any(|row| row.id == outbox_id),
            "round {round}: owner retry did not make the row claimable"
        );
        tx.rollback().await.expect("rollback");
    }
    drop(pool);
    drop_db(&name).await;
}

async fn advisory_lock_lease_is_exclusive() {
    if !require_pg() {
        return;
    }
    let (name, pool) = pool().await;
    let pool = Arc::new(pool);
    for round in 0..16i64 {
        let barrier = Arc::new(tokio::sync::Barrier::new(6));
        let mut joins = Vec::new();
        for worker in 0..6 {
            let pool = Arc::clone(&pool);
            let barrier = Arc::clone(&barrier);
            joins.push(tokio::spawn(async move {
                let lease = AdvisoryLockLease::try_acquire(
                    &pool,
                    80_000 + round,
                    format!("w{worker}"),
                )
                .await
                .expect("try_acquire");
                let held = lease.is_some();
                barrier.wait().await;
                drop(lease);
                held
            }));
        }
        let mut held = 0;
        for join in joins {
            if join.await.expect("join") {
                held += 1;
            }
        }
        assert_eq!(held, 1, "round {round}");
    }
    drop(pool);
    drop_db(&name).await;
}
