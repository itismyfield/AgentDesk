use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header},
    routing::post,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{path::PathBuf, sync::Arc};
use tower::ServiceExt;

struct ResetFixture {
    run_id: String,
    entry_id: String,
    dispatch_id: String,
}

async fn seed_reset_run(
    pool: &PgPool,
    suffix: &str,
    repo: &str,
    agent_id: &str,
    run_status: &str,
    entry_status: &str,
    with_live_cleanup: bool,
) -> ResetFixture {
    let run_id = format!("run-reset-{suffix}");
    let entry_id = format!("entry-reset-{suffix}");
    let dispatch_id = format!("dispatch-reset-{suffix}");
    let card_id = format!("card-reset-{suffix}");

    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(&run_id)
    .bind(repo)
    .bind(agent_id)
    .bind(run_status)
    .execute(pool)
    .await
    .expect("seed reset run");
    sqlx::query(
        "INSERT INTO kanban_cards (id, repo_id, title, status)
         VALUES ($1, $2, 'Reset card', 'in_progress')",
    )
    .bind(&card_id)
    .bind(repo)
    .execute(pool)
    .await
    .expect("seed reset card");

    if with_live_cleanup {
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, to_agent_id, dispatch_type, status, title)
             VALUES ($1, $2, $3, 'implementation', 'dispatched', 'Reset dispatch')",
        )
        .bind(&dispatch_id)
        .bind(&card_id)
        .bind(agent_id)
        .execute(pool)
        .await
        .expect("seed reset dispatch");
    }

    let dispatch = if with_live_cleanup {
        dispatch_id.as_str()
    } else {
        ""
    };
    let slot_index = if with_live_cleanup { 0_i64 } else { -1_i64 };
    sqlx::query(
        "INSERT INTO auto_queue_entries
            (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index)
         VALUES ($1, $2, $3, $4, $5, NULLIF($6, ''), NULLIF($7, -1))",
    )
    .bind(&entry_id)
    .bind(&run_id)
    .bind(&card_id)
    .bind(agent_id)
    .bind(entry_status)
    .bind(dispatch)
    .bind(slot_index)
    .execute(pool)
    .await
    .expect("seed reset entry");

    if with_live_cleanup {
        sqlx::query(
            "INSERT INTO auto_queue_slots
                (agent_id, slot_index, assigned_run_id, assigned_thread_group)
             VALUES ($1, 0, $2, 0)",
        )
        .bind(agent_id)
        .bind(&run_id)
        .execute(pool)
        .await
        .expect("seed reset slot");
        sqlx::query(
            "INSERT INTO auto_queue_phase_gates
                (run_id, phase, status, dispatch_id)
             VALUES ($1, 0, 'pending', NULLIF($2, ''))",
        )
        .bind(&run_id)
        .bind(dispatch)
        .execute(pool)
        .await
        .expect("seed reset phase gate");
    }

    ResetFixture {
        run_id,
        entry_id,
        dispatch_id,
    }
}

async fn reset_test_db() -> (crate::db::auto_queue::test_support::TestPostgresDb, PgPool) {
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    (pg_db, pool)
}

fn reset_test_app(pool: PgPool) -> Router {
    let mut config = crate::config::Config::default();
    config.server.host = "127.0.0.1".to_string();
    config.policies.dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
    let engine = crate::engine::PolicyEngine::new_with_pg(&config, Some(pool.clone()))
        .expect("create reset test policy engine");
    let broadcast_tx = crate::server::ws::new_broadcast();
    let batch_buffer = crate::server::ws::spawn_batch_flusher(broadcast_tx.clone());
    let state = crate::app_state::AppState {
        pg_pool: Some(pool),
        engine,
        config: Arc::new(config),
        broadcast_tx,
        batch_buffer,
        health_registry: None,
        cluster_instance_id: None,
    };
    Router::new()
        .route("/queue/reset", post(super::reset))
        .with_state(state)
}

async fn post_reset(app: &Router, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(Method::POST)
        .uri("/queue/reset")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("build reset request");
    let response = app
        .clone()
        .oneshot(request)
        .await
        .expect("send reset request");
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("read reset response");
    let body = serde_json::from_slice(&body).expect("decode reset response");
    (status, body)
}

#[tokio::test]
async fn reset_run_scope_preserves_same_agent_other_repo_run_pg() {
    let (pg_db, pool) = reset_test_db().await;
    let target = seed_reset_run(
        &pool,
        "scope-a",
        "repo-reset-a",
        "agent-reset",
        "active",
        "pending",
        false,
    )
    .await;
    let untouched = seed_reset_run(
        &pool,
        "scope-b",
        "repo-reset-b",
        "agent-reset",
        "active",
        "pending",
        false,
    )
    .await;
    let app = reset_test_app(pool.clone());

    let (status, body) = post_reset(
        &app,
        json!({
            "run_id": target.run_id.as_str(),
            "repo": "repo-reset-a",
            "agent_id": "agent-reset"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "reset response: {body}");
    assert_eq!(body["cancelled_runs"], 1);

    let untouched_state = sqlx::query_as::<_, (String, String)>(
        "SELECT r.status, e.status
         FROM auto_queue_runs r
         JOIN auto_queue_entries e ON e.run_id = r.id
         WHERE r.id = $1",
    )
    .bind(&untouched.run_id)
    .fetch_one(&pool)
    .await
    .expect("load untouched run");
    assert_eq!(
        untouched_state,
        ("active".to_string(), "pending".to_string()),
        "same-agent run in another repo must be untouched"
    );
    let target_status: String =
        sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = $1")
            .bind(&target.run_id)
            .fetch_one(&pool)
            .await
            .expect("load target status");
    assert_eq!(target_status, "cancelled");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn reset_repo_and_agent_ownership_mismatch_returns_conflict_pg() {
    let (pg_db, pool) = reset_test_db().await;
    let target = seed_reset_run(
        &pool,
        "ownership",
        "repo-reset-a",
        "agent-reset",
        "active",
        "pending",
        false,
    )
    .await;
    let app = reset_test_app(pool.clone());

    let (repo_status, repo_body) = post_reset(
        &app,
        json!({
            "run_id": target.run_id.as_str(),
            "repo": "repo-reset-other",
            "agent_id": "agent-reset"
        }),
    )
    .await;
    assert_eq!(
        repo_status,
        StatusCode::CONFLICT,
        "repo mismatch: {repo_body}"
    );

    let (agent_status, agent_body) = post_reset(
        &app,
        json!({
            "run_id": target.run_id.as_str(),
            "repo": "repo-reset-a",
            "agent_id": "agent-reset-other"
        }),
    )
    .await;
    assert_eq!(
        agent_status,
        StatusCode::CONFLICT,
        "agent mismatch: {agent_body}"
    );

    let run_status: String = sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = $1")
        .bind(&target.run_id)
        .fetch_one(&pool)
        .await
        .expect("load ownership target status");
    assert_eq!(run_status, "active", "ownership rejection must not mutate");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn reset_unknown_missing_and_terminal_run_contracts_are_pg() {
    let (pg_db, pool) = reset_test_db().await;
    let terminal = seed_reset_run(
        &pool,
        "terminal",
        "repo-reset",
        "agent-reset",
        "completed",
        "skipped",
        false,
    )
    .await;
    let app = reset_test_app(pool.clone());

    let (unknown_status, unknown_body) = post_reset(
        &app,
        json!({
            "run_id": "run-unknown-field",
            "unexpected": true
        }),
    )
    .await;
    assert_eq!(
        unknown_status,
        StatusCode::BAD_REQUEST,
        "unknown field: {unknown_body}"
    );

    let (missing_status, missing_body) =
        post_reset(&app, json!({ "run_id": "run-does-not-exist" })).await;
    assert_eq!(
        missing_status,
        StatusCode::NOT_FOUND,
        "missing run: {missing_body}"
    );

    let (terminal_status, terminal_body) = post_reset(
        &app,
        json!({
            "run_id": terminal.run_id.as_str(),
            "repo": "repo-reset",
            "agent_id": "agent-reset"
        }),
    )
    .await;
    assert_eq!(
        terminal_status,
        StatusCode::CONFLICT,
        "terminal run: {terminal_body}"
    );

    for (suffix, status) in [("generated", "generated"), ("pending", "pending")] {
        let run = seed_reset_run(
            &pool,
            suffix,
            &format!("repo-reset-{suffix}"),
            &format!("agent-reset-{suffix}"),
            status,
            "pending",
            false,
        )
        .await;
        let (status, body) = post_reset(&app, json!({ "run_id": run.run_id.as_str() })).await;
        assert_eq!(status, StatusCode::OK, "cancel {suffix}: {body}");
        assert_eq!(body["cancelled_runs"], 1);
        assert_eq!(body["cancelled_entries"], 1);
        let state = sqlx::query_as::<_, (String, String)>(
            "SELECT r.status, e.status
             FROM auto_queue_runs r JOIN auto_queue_entries e ON e.run_id = r.id
             WHERE r.id = $1",
        )
        .bind(&run.run_id)
        .fetch_one(&pool)
        .await
        .expect("load generated/pending reset state");
        assert_eq!(state, ("cancelled".to_string(), "skipped".to_string()));
    }

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn reset_canonical_cleanup_summarizes_dispatch_gate_slot_and_entry_transition_pg() {
    let (pg_db, pool) = reset_test_db().await;
    let target = seed_reset_run(
        &pool,
        "cleanup",
        "repo-reset",
        "agent-reset",
        "active",
        "dispatched",
        true,
    )
    .await;
    let app = reset_test_app(pool.clone());

    let (status, body) = post_reset(&app, json!({ "run_id": target.run_id.as_str() })).await;
    assert_eq!(status, StatusCode::OK, "cleanup response: {body}");
    assert_eq!(body["cancelled_dispatches"], 1);
    assert_eq!(body["deleted_phase_gates"], 1);
    assert_eq!(body["released_slots"], 1);
    assert_eq!(body["cancelled_entries"], 1);
    assert_eq!(body["entry_transition_summary"]["pending_to_skipped"], 1);

    let state = sqlx::query_as::<_, (String, String, String, Option<String>, Option<String>, i64)>(
        "SELECT r.status, e.status, d.status, e.dispatch_id, s.assigned_run_id,
                (SELECT COUNT(*) FROM auto_queue_phase_gates WHERE run_id = $1)
         FROM auto_queue_runs r
         JOIN auto_queue_entries e ON e.id = $2
         JOIN task_dispatches d ON d.id = $3
         JOIN auto_queue_slots s ON s.agent_id = 'agent-reset' AND s.slot_index = 0
         WHERE r.id = $1",
    )
    .bind(&target.run_id)
    .bind(&target.entry_id)
    .bind(&target.dispatch_id)
    .fetch_one(&pool)
    .await
    .expect("load canonical reset state");
    assert_eq!(
        state,
        (
            "cancelled".to_string(),
            "skipped".to_string(),
            "cancelled".to_string(),
            None,
            None,
            0,
        )
    );
    let transition = sqlx::query_as::<_, (String, String, String)>(
        "SELECT from_status, to_status, trigger_source
         FROM auto_queue_entry_transitions
         WHERE entry_id = $1
         ORDER BY id DESC
         LIMIT 1",
    )
    .bind(&target.entry_id)
    .fetch_one(&pool)
    .await
    .expect("load reset entry transition");
    assert_eq!(
        transition,
        (
            "pending".to_string(),
            "skipped".to_string(),
            "run_cancel".to_string()
        )
    );

    pool.close().await;
    pg_db.drop().await;
}
