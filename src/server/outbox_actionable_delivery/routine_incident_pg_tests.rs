//! The routine failure incident row through the real outbox drain against PostgreSQL: posting
//! before the post, ack on a real message id, and no repost once the post may have been sent.
use std::sync::{Arc, Mutex};

use chrono::{Duration, Utc};
use serde_json::json;
use sqlx::PgPool;

use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::server::drain_message_outbox_batch_once;
use crate::services::message_outbox::{self, OutboxMessage, routine_incident};
use crate::services::routines::execution_mode::{ExecutionDefinition, RoutineExecutionMode};
use crate::services::routines::store::{NewRoutine, RoutineStore};

const INCIDENT: &str = "routine_failure_incident";

async fn setup() -> (TestPostgresDb, PgPool, RoutineStore) {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_cc)
         VALUES ('boot-agent', 'boot agent', 'claude', '6551001')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let store = RoutineStore::new_with_timezone_and_checkpoint_limit(
        Arc::new(pool.clone()),
        "Asia/Seoul",
        256 * 1024,
    );
    (db, pool, store)
}

/// A headless N = 1 routine failed twice: `(routine id, first failed run, incident row id)`.
async fn incident(pool: &PgPool, store: &RoutineStore, name: &str) -> (String, String, i64) {
    let definition = ExecutionDefinition {
        mode: RoutineExecutionMode::Headless,
        runner: json!({"kind": "quickjs", "script_ref": format!("{name}.js")}),
    };
    let routine = NewRoutine {
        agent_id: Some("boot-agent".into()),
        fallback_agent_id: None,
        max_retries: Some(0),
        script_ref: format!("{name}.js"),
        name: name.into(),
        status: None,
        execution_strategy: "fresh".into(),
        schedule: None,
        next_due_at: Some(Utc::now() - Duration::minutes(1)),
        checkpoint: None,
        discord_thread_id: None,
        timeout_secs: None,
    };
    let id = store
        .attach_routine_with_execution(routine, Some(&definition), 1)
        .await
        .unwrap()
        .id;
    let mut runs = Vec::new();
    for _ in 0..2 {
        let run = store.claim_due_runs(1).await.unwrap().pop().unwrap().run_id;
        let due = Some(Utc::now() - Duration::minutes(1));
        assert!(store.fail_run(&run, "boom", None, due).await.unwrap());
        runs.push(run);
    }
    let row: i64 = sqlx::query_scalar(
        "SELECT id FROM message_outbox WHERE reason_code = $1 AND session_key = $2",
    )
    .bind(INCIDENT)
    .bind(format!("routine:{id}:failure_incident:{}", runs[0]))
    .fetch_one(pool)
    .await
    .unwrap();
    store.pause_routine(&id).await.unwrap();
    (id, runs.swap_remove(0), row)
}

/// One drain whose incident sends answer `(status, body)`; returns the transport state each
/// incident send saw in PostgreSQL. `lose_lease` moves the claim so the ack cannot be stored.
async fn drain(pool: &PgPool, status: &str, body: &str, lose_lease: bool) -> Vec<Option<String>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let reply = (status.to_string(), body.to_string());
    let (observed, db) = (seen.clone(), pool.clone());
    drain_message_outbox_batch_once(pool, Some("incident-test"), move |row| {
        let (reply, observed, db) = (reply.clone(), observed.clone(), db.clone());
        async move {
            if row.reason_code.as_deref() != Some(INCIDENT) {
                return ("200 OK".to_string(), String::new());
            }
            let state: Option<String> =
                sqlx::query_scalar("SELECT transport_state FROM message_outbox WHERE id = $1")
                    .bind(row.id)
                    .fetch_one(&db)
                    .await
                    .unwrap();
            observed.lock().unwrap().push(state);
            if lose_lease {
                sqlx::query(
                    "UPDATE message_outbox SET claimed_at = claimed_at - INTERVAL '1 hour'
                     WHERE id = $1",
                )
                .bind(row.id)
                .execute(&db)
                .await
                .unwrap();
            }
            reply
        }
    })
    .await;
    seen.lock().unwrap().clone()
}

/// `(status, transport_state, transport_message_id)`.
async fn state(pool: &PgPool, id: i64) -> (String, Option<String>, Option<String>) {
    sqlx::query_as(
        "SELECT status, transport_state, transport_message_id FROM message_outbox WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn projection(store: &RoutineStore, routine_id: &str, run_id: &str) -> Option<String> {
    let runs = store.list_runs(routine_id, 10).await.unwrap();
    let run = runs.into_iter().find(|run| run.id == run_id).unwrap();
    run.failure_incident
}

fn posting() -> Vec<Option<String>> {
    vec![Some("posting".into())]
}

fn held(state: &str) -> (String, Option<String>, Option<String>) {
    ("held".into(), Some(state.into()), None)
}

#[tokio::test(flavor = "current_thread")]
async fn incident_ack_stores_the_message_id_and_never_reads_as_a_linked_turn_pg() {
    let (db, pool, store) = setup().await;
    let (routine, first, row) = incident(&pool, &store, "acked").await;
    assert_eq!(
        projection(&store, &routine, &first).await.as_deref(),
        Some("reserved")
    );
    let runs_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM routine_runs")
        .fetch_one(&pool)
        .await
        .unwrap();

    let ack = r#"{"ok":true,"message_id":"1500"}"#;
    assert_eq!(drain(&pool, "200 OK", ack, false).await, posting());
    assert_eq!(
        state(&pool, row).await,
        ("sent".into(), Some("acked".into()), Some("1500".into()))
    );
    // A require-mention channel refuses the post: Discord acked it, but no owner or run started.
    assert_eq!(
        projection(&store, &routine, &first).await.as_deref(),
        Some("posted_turn_unconfirmed")
    );
    let owners: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM intake_session_owners")
        .fetch_one(&pool)
        .await
        .unwrap();
    let runs_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM routine_runs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((owners, runs_after), (0, runs_before));
    assert_eq!(drain(&pool, "200 OK", ack, false).await, []);

    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_post_that_may_have_been_sent_closes_unconfirmed_and_keeps_its_fence_pg() {
    let (db, pool, store) = setup().await;
    let (routine, first, row) = incident(&pool, &store, "unknown").await;
    assert_eq!(
        drain(&pool, "500 Internal Server Error", "timeout", false).await,
        posting()
    );
    assert_eq!(state(&pool, row).await, held("unknown"));
    // 200 without a real message id is not an ack.
    let (_, _, skipped) = incident(&pool, &store, "skipped").await;
    let skip = r#"{"ok":true,"message_id":"","delivery":"skipped"}"#;
    assert_eq!(drain(&pool, "200 OK", skip, false).await, posting());
    assert_eq!(state(&pool, skipped).await, held("unknown"));

    // No drain, activation, cancel, GC or failure write reopens or drops the fence.
    sqlx::query("UPDATE message_outbox SET next_attempt_at = NULL")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(drain(&pool, "200 OK", "{}", false).await, []);
    assert!(
        !message_outbox::activate_staged_outbox_pg(&pool, row)
            .await
            .unwrap()
    );
    assert!(
        !message_outbox::activate_or_confirm_staged_outbox_pg(&pool, row)
            .await
            .unwrap()
    );
    assert!(
        !message_outbox::cancel_staged_outbox_pg(&pool, row)
            .await
            .unwrap()
    );
    message_outbox::gc_stale_outbox_rows(&pool).await.unwrap();
    for write in [
        "UPDATE message_outbox SET status = 'failed' WHERE id = $1",
        "UPDATE message_outbox SET status = 'pending', transport_state = NULL WHERE id = $1",
        "UPDATE message_outbox SET dedupe_key = NULL WHERE id = $1",
        "DELETE FROM message_outbox WHERE id = $1",
    ] {
        let changed = sqlx::query(write).bind(row).execute(&pool).await.unwrap();
        assert_eq!(changed.rows_affected(), 0, "{write}");
    }
    assert_eq!(state(&pool, row).await, held("unknown"));
    let (target, session) = (
        "agent:boot-agent".to_string(),
        format!("routine:{routine}:failure_incident:{first}"),
    );
    let again = message_outbox::enqueue_outbox_pg_returning_id_with_persistent_dedupe(
        &pool,
        OutboxMessage {
            target: &target,
            content: "again",
            bot: "announce",
            source: "routine-runtime",
            reason_code: Some(INCIDENT),
            session_key: Some(&session),
        },
    )
    .await
    .unwrap();
    assert_eq!(again, row);
    assert_eq!(
        projection(&store, &routine, &first).await.as_deref(),
        Some("completion_unconfirmed")
    );

    // An ack that cannot be stored leaves `posting`; the abandoned sweep closes it as unknown.
    let (routine, first, lost) = incident(&pool, &store, "lost-ack").await;
    let ack = r#"{"ok":true,"message_id":"1700"}"#;
    assert_eq!(drain(&pool, "200 OK", ack, true).await, posting());
    assert_eq!(state(&pool, lost).await, held("posting"));
    assert_eq!(drain(&pool, "200 OK", ack, false).await, []);
    assert_eq!(
        routine_incident::close_abandoned_posting(&pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(state(&pool, lost).await, held("unknown"));
    assert_eq!(
        projection(&store, &routine, &first).await.as_deref(),
        Some("completion_unconfirmed")
    );
    // Only the worker holding the lease may still confirm it; any other claim cannot.
    let (claim_owner, claimed_at): (String, chrono::DateTime<Utc>) =
        sqlx::query_as("SELECT claim_owner, claimed_at FROM message_outbox WHERE id = $1")
            .bind(lost)
            .fetch_one(&pool)
            .await
            .unwrap();
    let lease = |claim_owner| routine_incident::Lease {
        id: lost,
        claim_owner,
        claimed_at,
    };
    assert!(
        !routine_incident::ack(&pool, lease("other"), "1701")
            .await
            .unwrap()
    );
    assert!(
        routine_incident::ack(&pool, lease(&claim_owner), "1700")
            .await
            .unwrap()
    );
    assert_eq!(
        state(&pool, lost).await,
        ("sent".into(), Some("acked".into()), Some("1700".into()))
    );

    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_refusal_before_the_post_retries_the_same_incident_row_pg() {
    let (db, pool, store) = setup().await;
    let (_, _, row) = incident(&pool, &store, "refused").await;
    assert_eq!(
        drain(&pool, "409 Conflict", "withheld", false).await,
        posting()
    );
    let (status, transport, retries): (String, Option<String>, i64) = sqlx::query_as(
        "SELECT status, transport_state, retry_count FROM message_outbox WHERE id = $1",
    )
    .bind(row)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((status.as_str(), transport, retries), ("pending", None, 1));
    sqlx::query("UPDATE message_outbox SET next_attempt_at = NULL WHERE id = $1")
        .bind(row)
        .execute(&pool)
        .await
        .unwrap();
    let ack = r#"{"ok":true,"message_id":"1800"}"#;
    assert_eq!(drain(&pool, "200 OK", ack, false).await, posting());
    assert_eq!(
        state(&pool, row).await,
        ("sent".into(), Some("acked".into()), Some("1800".into()))
    );

    pool.close().await;
    db.drop().await;
}

async fn claim(pool: &PgPool, id: i64) -> (String, chrono::DateTime<Utc>) {
    sqlx::query_as("SELECT claim_owner, claimed_at FROM message_outbox WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn a_lost_lease_never_posts_and_only_an_aged_posting_row_is_swept_pg() {
    let (db, pool, store) = setup().await;
    let (_, _, row) = incident(&pool, &store, "lease").await;
    // Another worker reclaimed the row; the first worker still holds its old lease.
    sqlx::query(
        "UPDATE message_outbox SET status = 'processing', claim_owner = 'other', claimed_at = NOW()
         WHERE id = $1",
    )
    .bind(row)
    .execute(&pool)
    .await
    .unwrap();
    let (_, claimed_at) = claim(&pool, row).await;
    let stale = super::super::PendingMessageOutboxRow {
        id: row,
        target: "agent:boot-agent".into(),
        content: "incident".into(),
        bot: "announce".into(),
        source: "routine-runtime".into(),
        reason_code: Some(INCIDENT.into()),
        session_key: None,
        retry_count: 0,
        attachment: Ok(None),
        claim_owner: "stale".into(),
        claimed_at: claimed_at - Duration::hours(1),
    };
    let sends = Arc::new(Mutex::new(0));
    let sent = sends.clone();
    super::routine_incident::deliver(&pool, &stale, || async move {
        *sent.lock().unwrap() += 1;
        ("200 OK".to_string(), r#"{"message_id":"1900"}"#.to_string())
    })
    .await;
    assert_eq!(*sends.lock().unwrap(), 0);
    assert_eq!(state(&pool, row).await, ("processing".into(), None, None));

    // The live lease posts; the sweep leaves a post that is still within its window.
    let lease = routine_incident::Lease {
        id: row,
        claim_owner: "other",
        claimed_at,
    };
    assert!(routine_incident::begin_posting(&pool, lease).await.unwrap());
    let swept = || async {
        routine_incident::close_abandoned_posting(&pool)
            .await
            .unwrap()
    };
    assert_eq!(swept().await, 0);
    assert_eq!(state(&pool, row).await, held("posting"));
    sqlx::query(
        "UPDATE message_outbox SET claimed_at = claimed_at - INTERVAL '11 minutes' WHERE id = $1",
    )
    .bind(row)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(swept().await, 1);
    assert_eq!(state(&pool, row).await, held("unknown"));

    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_redriven_incident_keeps_its_fence_and_its_key_reserves_the_same_row_pg() {
    let (db, pool, store) = setup().await;
    let (routine, first, row) = incident(&pool, &store, "redriven").await;
    // An unresolvable target is refused before any post: the same row retries, then fails.
    assert_eq!(
        drain(&pool, "404 Not Found", "unknown target", false).await,
        posting()
    );
    let retries: i64 = sqlx::query_scalar("SELECT retry_count FROM message_outbox WHERE id = $1")
        .bind(row)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        (state(&pool, row).await, retries),
        (("pending".into(), None, None), 1)
    );
    sqlx::query(
        "UPDATE message_outbox SET retry_count = 1000, next_attempt_at = NULL WHERE id = $1",
    )
    .bind(row)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        drain(&pool, "403 Forbidden", "not in role map", false).await,
        posting()
    );
    assert_eq!(state(&pool, row).await, ("failed".into(), None, None));
    assert_eq!(
        projection(&store, &routine, &first).await.as_deref(),
        Some("delivery_failed")
    );

    // An operator redrive gives the row a finite fence; its next post may have been sent.
    let ids = [row];
    crate::services::message_outbox_recovery::redrive_failed_rows(&pool, &ids, "k1", "test", false)
        .await
        .unwrap();
    let finite: bool = sqlx::query_scalar(
        "SELECT dedupe_expires_at IS NOT NULL FROM message_outbox WHERE id = $1",
    )
    .bind(row)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(finite);
    assert_eq!(
        drain(&pool, "500 Internal Server Error", "timeout", false).await,
        posting()
    );
    assert_eq!(state(&pool, row).await, held("unknown"));

    let (target, session) = (
        "agent:boot-agent".to_string(),
        format!("routine:{routine}:failure_incident:{first}"),
    );
    let again = message_outbox::enqueue_outbox_pg_returning_id_with_persistent_dedupe(
        &pool,
        OutboxMessage {
            target: &target,
            content: "again",
            bot: "announce",
            source: "routine-runtime",
            reason_code: Some(INCIDENT),
            session_key: Some(&session),
        },
    )
    .await
    .unwrap();
    assert_eq!(again, row);
    // The re-reservation made the fence permanent and reopened nothing.
    message_outbox::gc_stale_outbox_rows(&pool).await.unwrap();
    assert_eq!(state(&pool, row).await, held("unknown"));
    assert_eq!(drain(&pool, "200 OK", "{}", false).await, []);
    let rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox WHERE session_key = $1")
            .bind(&session)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rows, 1);

    pool.close().await;
    db.drop().await;
}
