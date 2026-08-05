#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        Router,
        body::{Body, to_bytes},
        http::{Method, Request, StatusCode, header},
    };
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use super::super::{AppState, domains};
    use crate::db::auto_queue::test_support::TestPostgresDb;

    const AUTH_TOKEN: &str = "auto-queue-lifecycle-test-token";

    fn test_state(pool: sqlx::PgPool) -> AppState {
        let mut config = crate::config::Config::default();
        config.server.auth_token = Some(AUTH_TOKEN.to_string());
        let engine = crate::engine::PolicyEngine::new(&config).expect("construct policy engine");
        let broadcast_tx = crate::eventbus::new_broadcast();
        let batch_buffer = crate::eventbus::spawn_batch_flusher(broadcast_tx.clone());
        AppState {
            pg_pool: Some(pool),
            engine,
            config: Arc::new(config),
            broadcast_tx,
            batch_buffer,
            health_registry: None,
            cluster_instance_id: None,
        }
    }

    fn test_router(pool: sqlx::PgPool) -> Router {
        let state = test_state(pool);
        domains::ops::router(state.clone()).with_state(state)
    }

    async fn request_json_response(
        app: &Router,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {AUTH_TOKEN}"));
        let body = match body {
            Some(body) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        let response = app
            .clone()
            .oneshot(builder.body(body).expect("build lifecycle request"))
            .await
            .expect("send lifecycle request");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read lifecycle response");
        let body = serde_json::from_slice(&bytes).expect("decode lifecycle response");
        (status, body)
    }

    async fn request_json(
        app: &Router,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> StatusCode {
        request_json_response(app, method, path, body).await.0
    }

    async fn seed_agent(pool: &sqlx::PgPool) {
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id)
         VALUES ('agent-1', 'Agent 1', 'claude', '123')",
        )
        .execute(pool)
        .await
        .expect("seed agent");
    }

    async fn seed_run(pool: &sqlx::PgPool, run_id: &str, status: &str) {
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ($1, 'repo-1', 'agent-1', $2)",
        )
        .bind(run_id)
        .bind(status)
        .execute(pool)
        .await
        .expect("seed run");
    }

    async fn run_status(pool: &sqlx::PgPool, run_id: &str) -> String {
        sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = $1")
            .bind(run_id)
            .fetch_one(pool)
            .await
            .expect("load run status")
    }

    async fn slot_run(pool: &sqlx::PgPool) -> Option<String> {
        sqlx::query_scalar(
            "SELECT assigned_run_id
         FROM auto_queue_slots
         WHERE agent_id = 'agent-1' AND slot_index = 0",
        )
        .fetch_one(pool)
        .await
        .expect("load slot run")
    }

    async fn scalar_i64(pool: &sqlx::PgPool, query: &str) -> i64 {
        sqlx::query_scalar(query)
            .fetch_one(pool)
            .await
            .expect("load scalar")
    }

    #[tokio::test]
    async fn postgres_auto_queue_lifecycle_http_routes_use_canonical_writers_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
        seed_agent(&pool).await;
        seed_run(&pool, "run-route", "active").await;
        sqlx::query(
            "INSERT INTO auto_queue_slots
            (agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map)
         VALUES ('agent-1', 0, 'run-route', 0, '{}'::jsonb)",
        )
        .execute(&pool)
        .await
        .expect("seed route slot");
        sqlx::query(
            "INSERT INTO auto_queue_phase_gates (run_id, phase, status)
         VALUES ('run-route', 0, 'pending')",
        )
        .execute(&pool)
        .await
        .expect("seed route phase gate");
        let app = test_router(pool.clone());
        let status = request_json(&app, Method::POST, "/queue/runs/run-route/pause", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(run_status(&pool, "run-route").await, "paused");
        assert_eq!(slot_run(&pool).await, None);
        let (status, response) =
            request_json_response(&app, Method::POST, "/queue/runs/run-route/resume", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response["resumed_runs"], 0);
        assert_eq!(response["blocked_runs"], 1);
        assert_eq!(response["message"], "No resumable runs");
        assert_eq!(run_status(&pool, "run-route").await, "paused");
        sqlx::query("DELETE FROM auto_queue_phase_gates WHERE run_id = 'run-route'")
            .execute(&pool)
            .await
            .expect("clear route phase gate before resume");
        let status = request_json(&app, Method::POST, "/queue/runs/run-route/resume", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(run_status(&pool, "run-route").await, "active");
        sqlx::query(
            "INSERT INTO auto_queue_phase_gates (run_id, phase, status)
         VALUES ('run-route', 0, 'pending')",
        )
        .execute(&pool)
        .await
        .expect("reseed route phase gate before end");
        sqlx::query(
            "UPDATE auto_queue_slots
         SET assigned_run_id = 'run-route', assigned_thread_group = 0
         WHERE agent_id = 'agent-1' AND slot_index = 0",
        )
        .execute(&pool)
        .await
        .expect("reseed route slot before end");
        let status = request_json(&app, Method::POST, "/queue/runs/run-route/end", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(run_status(&pool, "run-route").await, "completed");
        assert_eq!(slot_run(&pool).await, None);
        let gate_count = scalar_i64(
            &pool,
            "SELECT COUNT(*)::BIGINT FROM auto_queue_phase_gates WHERE run_id = 'run-route'",
        )
        .await;
        assert_eq!(gate_count, 0);
        let outbox_count = scalar_i64(&pool, "SELECT COUNT(*)::BIGINT FROM message_outbox").await;
        assert_eq!(outbox_count, 1);
        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn patch_completed_no_longer_skips_lifecycle_side_effects_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
        seed_agent(&pool).await;
        seed_run(&pool, "run-patch", "active").await;
        sqlx::query(
            "INSERT INTO auto_queue_slots
            (agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map)
         VALUES ('agent-1', 0, 'run-patch', 0, '{}'::jsonb)",
        )
        .execute(&pool)
        .await
        .expect("seed patch slot");
        sqlx::query(
            "INSERT INTO auto_queue_phase_gates (run_id, phase, status)
         VALUES ('run-patch', 0, 'pending')",
        )
        .execute(&pool)
        .await
        .expect("seed patch phase gate");
        let app = test_router(pool.clone());
        let status = request_json(
            &app,
            Method::PATCH,
            "/queue/runs/run-patch",
            Some(json!({"status": "completed"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(run_status(&pool, "run-patch").await, "active");
        assert_eq!(slot_run(&pool).await.as_deref(), Some("run-patch"));
        let gate_count = scalar_i64(
            &pool,
            "SELECT COUNT(*)::BIGINT FROM auto_queue_phase_gates WHERE run_id = 'run-patch'",
        )
        .await;
        assert_eq!(gate_count, 1);
        let outbox_count = scalar_i64(&pool, "SELECT COUNT(*)::BIGINT FROM message_outbox").await;
        assert_eq!(outbox_count, 0);
        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn patch_pending_start_and_metadata_updates_remain_supported_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
        seed_agent(&pool).await;
        seed_run(&pool, "run-start", "pending").await;
        let app = test_router(pool.clone());
        let status = request_json(
            &app,
            Method::PATCH,
            "/queue/runs/run-start",
            Some(json!({"status": "active"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(run_status(&pool, "run-start").await, "active");
        let status = request_json(
            &app,
            Method::PATCH,
            "/queue/runs/run-start",
            Some(json!({"max_concurrent_threads": 3})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let max_concurrent_threads = scalar_i64(
            &pool,
            "SELECT max_concurrent_threads::BIGINT FROM auto_queue_runs WHERE id = 'run-start'",
        )
        .await;
        assert_eq!(max_concurrent_threads, 3);
        let status = request_json(
            &app,
            Method::PATCH,
            "/queue/runs/run-missing",
            Some(json!({"max_concurrent_threads": 3})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        sqlx::query("UPDATE auto_queue_runs SET status = 'paused' WHERE id = 'run-start'")
            .execute(&pool)
            .await
            .expect("pause run before invalid legacy resume");
        let status = request_json(
            &app,
            Method::PATCH,
            "/queue/runs/run-start",
            Some(json!({"status": "active"})),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(run_status(&pool, "run-start").await, "paused");
        pool.close().await;
        pg_db.drop().await;
    }
}
