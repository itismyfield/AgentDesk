//! Auto-queue routes that add entries to an existing run, raced against its completion.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header},
};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};
use tower::ServiceExt;

use super::{AppState, domains};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::auto_queue::{
    RunCompletionBlockedReason, RunCompletionOutcome, finalize_run_if_ready_on_pg_tx,
    finalize_run_if_ready_pg,
};

const AUTH_TOKEN: &str = "auto-queue-admission-test-token";
const DEADLINE: Duration = Duration::from_secs(5);

fn test_router(pool: PgPool) -> Router {
    let mut config = crate::config::Config::default();
    config.server.auth_token = Some(AUTH_TOKEN.to_string());
    let engine = crate::engine::PolicyEngine::new(&config).expect("construct policy engine");
    let broadcast_tx = crate::eventbus::new_broadcast();
    let batch_buffer = crate::eventbus::spawn_batch_flusher(broadcast_tx.clone());
    let state = AppState {
        pg_pool: Some(pool),
        engine,
        config: Arc::new(config),
        broadcast_tx,
        batch_buffer,
        health_registry: None,
        cluster_instance_id: None,
    };
    domains::ops::router(state.clone()).with_state(state)
}

async fn request(app: &Router, method: Method, path: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {AUTH_TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("build request");
    let response = tokio::time::timeout(DEADLINE, app.clone().oneshot(request))
        .await
        .expect("route answers before the deadline")
        .expect("send request");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read response");
    (
        status,
        serde_json::from_slice(&bytes).expect("decode response"),
    )
}

fn spawn_request(
    app: &Router,
    method: Method,
    path: &str,
    body: Value,
) -> tokio::task::JoinHandle<(StatusCode, Value)> {
    let app = app.clone();
    let path = path.to_string();
    tokio::spawn(async move { request(&app, method, &path, body).await })
}

async fn setup_pool(pg_db: &TestPostgresDb, run_status: &str) -> PgPool {
    let pool = pg_db.connect_and_migrate_with_max_connections(6).await;
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id)
         VALUES ('agent-1', 'Agent 1', 'claude', '123')",
    )
    .execute(&pool)
    .await
    .expect("seed agent");
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ('run-1', 'repo-1', 'agent-1', $1)",
    )
    .bind(run_status)
    .execute(&pool)
    .await
    .expect("seed run");
    pool
}

async fn seed_card_in(pool: &PgPool, card_id: &str, issue_number: i64, status: &str) {
    sqlx::query(
        "INSERT INTO kanban_cards
            (id, title, status, assigned_agent_id, repo_id, github_issue_number)
         VALUES ($1, $1, $3, 'agent-1', 'repo-1', $2)",
    )
    .bind(card_id)
    .bind(issue_number)
    .bind(status)
    .execute(pool)
    .await
    .expect("seed card");
}

async fn seed_card(pool: &PgPool, card_id: &str, issue_number: i64) {
    seed_card_in(pool, card_id, issue_number, "ready").await;
}

/// A card the order callback accepts: the first dispatchable state of the default pipeline.
async fn seed_orderable_card(pool: &PgPool, card_id: &str, issue_number: i64) {
    crate::pipeline::ensure_loaded();
    let status = crate::pipeline::get().dispatchable_states()[0].to_string();
    seed_card_in(pool, card_id, issue_number, &status).await;
}

async fn seed_entry(pool: &PgPool, entry_id: &str, status: &str) {
    let card_id = format!("card-{entry_id}");
    seed_card(pool, &card_id, 900).await;
    sqlx::query(
        "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status)
         VALUES ($1, 'run-1', $2, 'agent-1', $3)",
    )
    .bind(entry_id)
    .bind(&card_id)
    .bind(status)
    .execute(pool)
    .await
    .expect("seed entry");
}

async fn scalar_i64(pool: &PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(pool)
        .await
        .expect("count")
}

async fn run_status(pool: &PgPool) -> String {
    sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = 'run-1'")
        .fetch_one(pool)
        .await
        .expect("run status")
}

async fn live_entries(pool: &PgPool) -> i64 {
    scalar_i64(
        pool,
        "SELECT COUNT(*)::BIGINT FROM auto_queue_entries
         WHERE run_id = 'run-1' AND status IN ('pending', 'dispatched')",
    )
    .await
}

async fn uncommitted_completion(pool: &PgPool) -> Transaction<'static, Postgres> {
    let mut completion = pool.begin().await.expect("begin completion");
    assert_eq!(
        finalize_run_if_ready_on_pg_tx(&mut completion, "run-1")
            .await
            .expect("complete run-1"),
        RunCompletionOutcome::Completed
    );
    completion
}

/// True once a backend of this database waits on a lock; false if `task` finished first.
async fn wait_for_lock_waiter<T>(pool: &PgPool, task: &tokio::task::JoinHandle<T>) -> bool {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let waiting = scalar_i64(
            pool,
            "SELECT COUNT(*)::BIGINT FROM pg_stat_activity
             WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .await;
        if waiting > 0 {
            return true;
        }
        if task.is_finished() {
            return false;
        }
        assert!(
            Instant::now() < deadline,
            "request neither waited nor finished"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn join<T>(task: tokio::task::JoinHandle<T>) -> T {
    tokio::time::timeout(DEADLINE, task)
        .await
        .expect("request finishes before the deadline")
        .expect("join request")
}

/// Contract: PATCH reopening an entry of a completed run answers 409 naming the run state,
/// and leaves the entry and its transition audit untouched.
#[tokio::test]
async fn patch_reopen_of_completed_run_entry_answers_conflict_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = setup_pool(&pg_db, "active").await;
    seed_entry(&pool, "entry-1", "failed").await;
    assert_eq!(
        finalize_run_if_ready_pg(&pool, "run-1")
            .await
            .expect("complete run-1"),
        RunCompletionOutcome::Completed
    );
    let app = test_router(pool.clone());
    let (status, body) = request(
        &app,
        Method::PATCH,
        "/queue/entries/entry-1",
        json!({"status": "pending"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let error = body["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("run is completed: refusing to reopen"),
        "{body}"
    );
    assert_eq!(run_status(&pool).await, "completed");
    assert_eq!(live_entries(&pool).await, 0);
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*)::BIGINT FROM auto_queue_entry_transitions WHERE to_status = 'pending'",
        )
        .await,
        0
    );
    pool.close().await;
    pg_db.drop().await;
}

/// Contract: a manual add that passed the route's active check refuses once a concurrent
/// completion of the run commits first, instead of appending to the completed run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_add_waits_for_uncommitted_completion_and_refuses_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = setup_pool(&pg_db, "active").await;
    seed_entry(&pool, "entry-done", "failed").await;
    seed_card(&pool, "card-add", 101).await;
    let app = test_router(pool.clone());
    let completion = uncommitted_completion(&pool).await;
    let add = spawn_request(
        &app,
        Method::POST,
        "/queue/runs/run-1/entries",
        json!({"issue_number": 101}),
    );
    let waited = wait_for_lock_waiter(&pool, &add).await;
    completion.commit().await.expect("commit completion");
    let (status, body) = join(add).await;
    assert_eq!(run_status(&pool).await, "completed");
    assert_eq!(live_entries(&pool).await, 0);
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("run is completed: refusing to add"),
        "{body}"
    );
    assert!(waited, "add did not wait for the completion");
    pool.close().await;
    pg_db.drop().await;
}

/// Contract: an order callback racing an uncommitted completion of its pending run neither
/// inserts entries nor reactivates the completed run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn order_callback_waits_for_uncommitted_completion_and_refuses_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = setup_pool(&pg_db, "pending").await;
    seed_orderable_card(&pool, "card-order", 201).await;
    let app = test_router(pool.clone());
    let completion = uncommitted_completion(&pool).await;
    let order = spawn_request(
        &app,
        Method::POST,
        "/queue/runs/run-1/order",
        json!({"order": [201]}),
    );
    let waited = wait_for_lock_waiter(&pool, &order).await;
    completion.commit().await.expect("commit completion");
    let (status, body) = join(order).await;
    assert_eq!(run_status(&pool).await, "completed");
    assert_eq!(live_entries(&pool).await, 0);
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(waited, "order callback did not wait for the completion");
    pool.close().await;
    pg_db.drop().await;
}

/// Contract: a completion arriving while an order callback holds the run token defers, and
/// the callback publishes its entries and the active status together.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_defers_behind_order_callback_holding_the_token_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = setup_pool(&pg_db, "pending").await;
    seed_orderable_card(&pool, "card-order", 202).await;
    let app = test_router(pool.clone());
    let mut card_holder = pool.begin().await.expect("begin card holder");
    sqlx::query("SELECT id FROM kanban_cards WHERE id = 'card-order' FOR UPDATE")
        .execute(&mut *card_holder)
        .await
        .expect("hold card row");
    let order = spawn_request(
        &app,
        Method::POST,
        "/queue/runs/run-1/order",
        json!({"order": [202]}),
    );
    assert!(wait_for_lock_waiter(&pool, &order).await);
    let mut attempt = pool.begin().await.expect("begin completion attempt");
    let first = finalize_run_if_ready_on_pg_tx(&mut attempt, "run-1")
        .await
        .expect("completion attempt");
    attempt.commit().await.expect("commit completion attempt");
    card_holder.rollback().await.expect("release card row");
    let (status, body) = join(order).await;

    assert_eq!(
        first,
        RunCompletionOutcome::Blocked(RunCompletionBlockedReason::TokenContended)
    );
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["created"], 1, "{body}");
    assert_eq!(
        finalize_run_if_ready_pg(&pool, "run-1")
            .await
            .expect("retry completion"),
        RunCompletionOutcome::Blocked(RunCompletionBlockedReason::RunnableEntry)
    );
    assert_eq!(run_status(&pool).await, "active");
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*)::BIGINT FROM auto_queue_runs
             WHERE id = 'run-1' AND completed_at IS NOT NULL",
        )
        .await,
        0
    );
    pool.close().await;
    pg_db.drop().await;
}

/// Contract: an order callback whose entry insert fails publishes nothing: no entry, no
/// activation, an error response.
#[tokio::test]
async fn order_callback_rolls_back_every_entry_on_insert_failure_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = setup_pool(&pg_db, "pending").await;
    seed_orderable_card(&pool, "card-ok", 301).await;
    seed_orderable_card(&pool, "card-bad", 302).await;
    sqlx::query(
        "CREATE FUNCTION reject_bad_entry() RETURNS trigger AS $$
         BEGIN RAISE EXCEPTION 'entry rejected by test'; END $$ LANGUAGE plpgsql",
    )
    .execute(&pool)
    .await
    .expect("create reject function");
    sqlx::query(
        "CREATE TRIGGER reject_bad_entry BEFORE INSERT ON auto_queue_entries
         FOR EACH ROW WHEN (NEW.kanban_card_id = 'card-bad') EXECUTE FUNCTION reject_bad_entry()",
    )
    .execute(&pool)
    .await
    .expect("install reject trigger");
    let app = test_router(pool.clone());
    let (status, body) = request(
        &app,
        Method::POST,
        "/queue/runs/run-1/order",
        json!({"order": [301, 302]}),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*)::BIGINT FROM auto_queue_entries WHERE run_id = 'run-1'",
        )
        .await,
        0
    );
    assert_eq!(run_status(&pool).await, "pending");
    pool.close().await;
    pg_db.drop().await;
}

/// Contract: an order with no dispatchable card completes the empty run and records the
/// rationale in one answer, without waiting on its own run token.
#[tokio::test]
async fn empty_order_completes_run_and_records_rationale_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = setup_pool(&pg_db, "pending").await;
    let app = test_router(pool.clone());
    let (status, body) = request(
        &app,
        Method::POST,
        "/queue/runs/run-1/order",
        json!({"order": [999], "rationale": "nothing ready"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["created"], 0, "{body}");
    assert_eq!(run_status(&pool).await, "completed");
    let rationale: Option<String> =
        sqlx::query_scalar("SELECT ai_rationale FROM auto_queue_runs WHERE id = 'run-1'")
            .fetch_one(&pool)
            .await
            .expect("load rationale");
    assert_eq!(
        rationale.as_deref(),
        Some("nothing ready (no ready cards — auto-completed)")
    );
    pool.close().await;
    pg_db.drop().await;
}
