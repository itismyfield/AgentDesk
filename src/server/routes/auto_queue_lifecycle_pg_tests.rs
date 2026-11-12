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

    async fn scalar_i64(pool: &sqlx::PgPool, query: &'static str) -> i64 {
        sqlx::query_scalar(query)
            .fetch_one(pool)
            .await
            .expect("load scalar")
    }

    /// Seed one entry of `run_id` on its own card, optionally linked to a
    /// dispatch, the way the generator and dispatcher write it.
    async fn seed_entry(
        pool: &sqlx::PgPool,
        run_id: &str,
        entry_id: &str,
        status: &str,
        dispatch_id: Option<&str>,
    ) {
        let card_id = format!("card-{entry_id}");
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
             VALUES ($1, 'Reset Route Card', 'todo', 'agent-1')",
        )
        .bind(&card_id)
        .execute(pool)
        .await
        .expect("seed reset card");
        if let Some(dispatch_id) = dispatch_id {
            sqlx::query(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status)
                 VALUES ($1, $2, 'agent-1', 'implementation', 'pending')",
            )
            .bind(dispatch_id)
            .bind(&card_id)
            .execute(pool)
            .await
            .expect("seed reset dispatch");
        }
        sqlx::query(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id)
             VALUES ($1, $2, $3, 'agent-1', $4, $5)",
        )
        .bind(entry_id)
        .bind(run_id)
        .bind(&card_id)
        .bind(status)
        .bind(dispatch_id)
        .execute(pool)
        .await
        .expect("seed reset entry");
    }

    async fn entry_count(pool: &sqlx::PgPool, run_id: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM auto_queue_entries WHERE run_id = $1")
            .bind(run_id)
            .fetch_one(pool)
            .await
            .expect("count run entries")
    }

    async fn wait_for_rebind_advisory_lock_waiter(pool: &sqlx::PgPool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let blocked = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (
                     SELECT 1
                     FROM pg_stat_activity
                     WHERE datname = current_database()
                       AND state = 'active'
                       AND wait_event_type = 'Lock'
                       AND query LIKE '%SELECT pg_advisory_lock(hashtext($1), hashtext($2))%'
                 )",
            )
            .fetch_one(pool)
            .await
            .expect("inspect blocked slot rebind advisory lock");
            if blocked {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "slot rebind did not reach its advisory lock after reading run status"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
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

    // The real activate route preserves grace, then completes and notifies exactly once.
    #[tokio::test]
    async fn dispatch_next_route_completes_drained_run_through_canonical_writer_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(6).await;
        seed_agent(&pool).await;
        seed_run(&pool, "run-drained", "active").await;
        sqlx::query(
            "INSERT INTO auto_queue_slots
            (agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map)
         VALUES ('agent-1', 0, 'run-drained', 0, '{}'::jsonb)",
        )
        .execute(&pool)
        .await
        .expect("seed drained slot");
        seed_entry(&pool, "run-drained", "entry-drained", "failed", None).await;
        sqlx::query(
            "UPDATE auto_queue_runs
         SET phase_gate_grace_until = NOW() + INTERVAL '30 seconds'
         WHERE id = 'run-drained'",
        )
        .execute(&pool)
        .await
        .expect("open grace window");
        let app = test_router(pool.clone());
        let body = Some(json!({"run_id": "run-drained"}));

        let status = request_json(&app, Method::POST, "/queue/dispatch-next", body.clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(run_status(&pool, "run-drained").await, "active");
        assert_eq!(slot_run(&pool).await.as_deref(), Some("run-drained"));
        assert_eq!(
            scalar_i64(&pool, "SELECT COUNT(*)::BIGINT FROM message_outbox").await,
            0
        );

        sqlx::query(
            "UPDATE auto_queue_runs SET phase_gate_grace_until = NULL WHERE id = 'run-drained'",
        )
        .execute(&pool)
        .await
        .expect("close grace window");
        let status = request_json(&app, Method::POST, "/queue/dispatch-next", body.clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(run_status(&pool, "run-drained").await, "completed");
        assert_eq!(slot_run(&pool).await, None);
        assert_eq!(
            scalar_i64(&pool, "SELECT COUNT(*)::BIGINT FROM message_outbox").await,
            1
        );

        let status = request_json(&app, Method::POST, "/queue/dispatch-next", body).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            scalar_i64(&pool, "SELECT COUNT(*)::BIGINT FROM message_outbox").await,
            1
        );
        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn patch_completed_is_rejected_and_performs_no_lifecycle_side_effects_pg() {
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

    #[tokio::test]
    async fn patch_retired_deploy_phases_returns_client_error_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
        seed_agent(&pool).await;
        seed_run(&pool, "run-retired-deploy-phases", "active").await;
        let app = test_router(pool.clone());

        let (status, response) = request_json_response(
            &app,
            Method::PATCH,
            "/queue/runs/run-retired-deploy-phases",
            Some(json!({"deploy_phases": [1, 3]})),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(response["error"], "no fields to update");
        assert_eq!(
            run_status(&pool, "run-retired-deploy-phases").await,
            "active"
        );
        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn stale_manual_rebind_cannot_reclaim_slot_after_canonical_completion_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(6).await;
        seed_agent(&pool).await;
        seed_run(&pool, "run-stale-rebind", "active").await;
        sqlx::query(
            "INSERT INTO auto_queue_slots
                (agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map)
             VALUES ('agent-1', 0, 'run-stale-rebind', 0, '{}'::jsonb)",
        )
        .execute(&pool)
        .await
        .expect("seed stale rebind slot");

        let mut blocker = pool.acquire().await.expect("acquire rebind blocker");
        sqlx::query("SELECT pg_advisory_lock(hashtext($1), hashtext($2))")
            .bind("aq_slot_rebind:agent-1")
            .bind("0")
            .execute(&mut *blocker)
            .await
            .expect("hold route rebind advisory lock");

        let app = test_router(pool.clone());
        let rebind_task = tokio::spawn(async move {
            request_json_response(
                &app,
                Method::POST,
                "/queue/slots/agent-1/0/rebind",
                Some(json!({"run_id": "run-stale-rebind", "thread_group": 0})),
            )
            .await
        });
        wait_for_rebind_advisory_lock_waiter(&pool).await;

        assert!(
            crate::db::auto_queue::finalize_run_if_ready_pg(&pool, "run-stale-rebind")
                .await
                .expect("canonically complete run after route status read")
                .is_completed()
        );
        assert_eq!(run_status(&pool, "run-stale-rebind").await, "completed");
        assert_eq!(slot_run(&pool).await, None);

        sqlx::query("SELECT pg_advisory_unlock(hashtext($1), hashtext($2))")
            .bind("aq_slot_rebind:agent-1")
            .bind("0")
            .execute(&mut *blocker)
            .await
            .expect("release route rebind advisory lock");
        let (status, _response) = rebind_task.await.expect("join stale route continuation");

        assert_eq!(status, StatusCode::OK);
        assert_eq!(run_status(&pool, "run-stale-rebind").await, "completed");
        assert_eq!(
            slot_run(&pool).await,
            None,
            "terminal run must not regain slot authority from the stale route continuation"
        );
        drop(blocker);
        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn manual_rebind_accepts_every_canonical_live_status_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
        seed_agent(&pool).await;
        seed_run(&pool, "run-live-rebind", "active").await;
        sqlx::query(
            "INSERT INTO auto_queue_slots
                (agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map)
             VALUES ('agent-1', 0, NULL, NULL, '{}'::jsonb)",
        )
        .execute(&pool)
        .await
        .expect("seed live rebind slot");
        let app = test_router(pool.clone());

        for status in crate::db::auto_queue::run_status::LIVE_RUN_STATUSES {
            sqlx::query("UPDATE auto_queue_runs SET status = $1 WHERE id = 'run-live-rebind'")
                .bind(status)
                .execute(&pool)
                .await
                .expect("set canonical live status");
            sqlx::query(
                "UPDATE auto_queue_slots
                 SET assigned_run_id = NULL, assigned_thread_group = NULL
                 WHERE agent_id = 'agent-1' AND slot_index = 0",
            )
            .execute(&pool)
            .await
            .expect("clear slot before live rebind");

            let (response_status, response) = request_json_response(
                &app,
                Method::POST,
                "/queue/slots/agent-1/0/rebind",
                Some(json!({"run_id": "run-live-rebind", "thread_group": 0})),
            )
            .await;

            assert_eq!(
                response_status,
                StatusCode::OK,
                "canonical live status {status} must remain eligible: {response}"
            );
            assert_eq!(run_status(&pool, "run-live-rebind").await, *status);
            assert_eq!(
                slot_run(&pool).await.as_deref(),
                Some("run-live-rebind"),
                "canonical live status {status} must retain rebind authority"
            );
        }

        pool.close().await;
        pg_db.drop().await;
    }

    /// The body `resetAutoQueue` sends: `run_id` always, `repo`/`agent_id`
    /// only when set, since `JSON.stringify` drops `undefined` fields.
    #[tokio::test]
    async fn reset_http_resets_only_the_selected_run_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
        seed_agent(&pool).await;
        seed_run(&pool, "run-reset-x", "generated").await;
        seed_run(&pool, "run-reset-w", "pending").await;
        seed_run(&pool, "run-reset-y", "active").await;
        seed_entry(&pool, "run-reset-x", "entry-x-1", "pending", None).await;
        seed_entry(&pool, "run-reset-x", "entry-x-2", "pending", None).await;
        seed_entry(&pool, "run-reset-w", "entry-w-1", "pending", None).await;
        seed_entry(&pool, "run-reset-y", "entry-y-1", "pending", None).await;
        let app = test_router(pool.clone());

        let (status, response) = request_json_response(
            &app,
            Method::POST,
            "/queue/reset",
            Some(json!({"run_id": "run-reset-x", "repo": "repo-1", "agent_id": "agent-1"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_eq!(response["run_id"], "run-reset-x");
        assert_eq!(response["deleted_entries"], 2);
        assert_eq!(response["completed_runs"], 1);

        let (status, response) = request_json_response(
            &app,
            Method::POST,
            "/queue/reset",
            Some(json!({"run_id": "run-reset-w", "agent_id": "agent-1"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_eq!(response["deleted_entries"], 1);

        for run_id in ["run-reset-x", "run-reset-w"] {
            assert_eq!(entry_count(&pool, run_id).await, 0, "{run_id}");
            assert_eq!(run_status(&pool, run_id).await, "completed", "{run_id}");
        }
        assert_eq!(entry_count(&pool, "run-reset-y").await, 1);
        assert_eq!(run_status(&pool, "run-reset-y").await, "active");
        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn reset_http_rejects_scope_mismatch_and_unknown_run_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
        seed_agent(&pool).await;
        seed_run(&pool, "run-reset-scope", "generated").await;
        seed_entry(&pool, "run-reset-scope", "entry-scope-1", "pending", None).await;
        let app = test_router(pool.clone());

        for body in [
            json!({"run_id": "run-reset-scope", "repo": "repo-1", "agent_id": "agent-other"}),
            json!({"run_id": "run-reset-scope", "repo": "repo-other", "agent_id": "agent-1"}),
        ] {
            let (status, response) =
                request_json_response(&app, Method::POST, "/queue/reset", Some(body.clone())).await;
            assert_eq!(status, StatusCode::CONFLICT, "{body} -> {response}");
            assert!(
                response["context"].get("status").is_none(),
                "a scope refusal carries no run status: {response}"
            );
        }
        let (status, _response) = request_json_response(
            &app,
            Method::POST,
            "/queue/reset",
            Some(json!({"run_id": "run-reset-absent", "agent_id": "agent-1"})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        assert_eq!(entry_count(&pool, "run-reset-scope").await, 1);
        assert_eq!(run_status(&pool, "run-reset-scope").await, "generated");
        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn reset_http_rejects_malformed_body_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
        seed_agent(&pool).await;
        seed_run(&pool, "run-reset-body", "generated").await;
        seed_entry(&pool, "run-reset-body", "entry-body-1", "pending", None).await;
        let app = test_router(pool.clone());

        for body in [
            json!({"repo": "repo-1", "agent_id": "agent-1"}),
            json!({"run_id": "   ", "agent_id": "agent-1"}),
            json!({"run_id": "run-reset-body", "agent": "agent-1"}),
        ] {
            let (status, response) =
                request_json_response(&app, Method::POST, "/queue/reset", Some(body.clone())).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body} -> {response}");
        }
        let (_status, response) = request_json_response(
            &app,
            Method::POST,
            "/queue/reset",
            Some(json!({"run_id": "   "})),
        )
        .await;
        assert_eq!(response["error"], "run_id is required for reset");

        assert_eq!(entry_count(&pool, "run-reset-body").await, 1);
        assert_eq!(run_status(&pool, "run-reset-body").await, "generated");
        pool.close().await;
        pg_db.drop().await;
    }

    /// A live run is refused with no writes, and the `end` it points at is the
    /// canonical exit: once it has run, the same reset request succeeds.
    #[tokio::test]
    async fn reset_http_refuses_live_run_until_it_is_ended_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
        seed_agent(&pool).await;
        seed_run(&pool, "run-reset-live", "active").await;
        seed_entry(
            &pool,
            "run-reset-live",
            "entry-live-1",
            "dispatched",
            Some("dispatch-live-1"),
        )
        .await;
        sqlx::query(
            "INSERT INTO auto_queue_slots
                (agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map)
             VALUES ('agent-1', 0, 'run-reset-live', 0, '{}'::jsonb)",
        )
        .execute(&pool)
        .await
        .expect("seed live reset slot");
        let app = test_router(pool.clone());
        let reset_body =
            json!({"run_id": "run-reset-live", "repo": "repo-1", "agent_id": "agent-1"});
        let dispatch_state = || {
            scalar_i64(
                &pool,
                "SELECT COUNT(*)::BIGINT FROM task_dispatches
                 WHERE id = 'dispatch-live-1' AND status = 'pending'",
            )
        };

        let (status, response) =
            request_json_response(&app, Method::POST, "/queue/reset", Some(reset_body.clone()))
                .await;
        assert_eq!(status, StatusCode::CONFLICT, "{response}");
        assert_eq!(response["context"]["run_id"], "run-reset-live");
        assert_eq!(response["context"]["status"], "active");
        let error = response["error"].as_str().expect("refusal message");
        assert!(
            error.contains("POST /api/queue/runs/run-reset-live/end"),
            "refusal must point at end: {error}"
        );
        assert_eq!(entry_count(&pool, "run-reset-live").await, 1);
        assert_eq!(run_status(&pool, "run-reset-live").await, "active");
        assert_eq!(
            dispatch_state().await,
            1,
            "refused reset left the dispatch live"
        );
        assert_eq!(slot_run(&pool).await.as_deref(), Some("run-reset-live"));

        let status = request_json(&app, Method::POST, "/queue/runs/run-reset-live/end", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(run_status(&pool, "run-reset-live").await, "completed");
        assert_eq!(dispatch_state().await, 0, "end cancelled the live dispatch");
        assert_eq!(slot_run(&pool).await, None);

        let (status, response) =
            request_json_response(&app, Method::POST, "/queue/reset", Some(reset_body)).await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_eq!(response["deleted_entries"], 1);
        assert_eq!(response["completed_runs"], 0);
        pool.close().await;
        pg_db.drop().await;
    }

    /// `end` refuses a restoring run, so the refusal points at per-run cancel.
    #[tokio::test]
    async fn reset_http_points_restoring_run_at_cancel_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
        seed_agent(&pool).await;
        seed_run(&pool, "run-reset-restoring", "restoring").await;
        seed_entry(
            &pool,
            "run-reset-restoring",
            "entry-restoring-1",
            "pending",
            None,
        )
        .await;
        let app = test_router(pool.clone());
        let reset_body = json!({"run_id": "run-reset-restoring", "agent_id": "agent-1"});

        let (status, response) =
            request_json_response(&app, Method::POST, "/queue/reset", Some(reset_body.clone()))
                .await;
        assert_eq!(status, StatusCode::CONFLICT, "{response}");
        assert_eq!(response["context"]["run_id"], "run-reset-restoring");
        assert_eq!(response["context"]["status"], "restoring");
        let error = response["error"].as_str().expect("refusal message");
        assert!(
            error.contains("POST /api/queue/cancel?run_id=run-reset-restoring")
                && !error.contains("/end"),
            "restoring refusal must point at cancel only: {error}"
        );
        assert_eq!(entry_count(&pool, "run-reset-restoring").await, 1);

        let status = request_json(
            &app,
            Method::POST,
            "/queue/runs/run-reset-restoring/end",
            None,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "end does not select restoring runs"
        );
        let status = request_json(
            &app,
            Method::POST,
            "/queue/cancel?run_id=run-reset-restoring",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(run_status(&pool, "run-reset-restoring").await, "cancelled");

        let (status, response) =
            request_json_response(&app, Method::POST, "/queue/reset", Some(reset_body)).await;
        assert_eq!(status, StatusCode::OK, "{response}");
        pool.close().await;
        pg_db.drop().await;
    }

    async fn seed_slot(pool: &sqlx::PgPool, run_id: &str) {
        sqlx::query(
            "INSERT INTO auto_queue_slots
                (agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map)
             VALUES ('agent-1', 0, $1, 0, CAST('{}' AS jsonb))",
        )
        .bind(run_id)
        .execute(pool)
        .await
        .expect("seed slot");
    }

    async fn entry_status(pool: &sqlx::PgPool, entry_id: &str) -> String {
        sqlx::query_scalar("SELECT status FROM auto_queue_entries WHERE id = $1")
            .bind(entry_id)
            .fetch_one(pool)
            .await
            .expect("load entry status")
    }

    async fn notice_targets(pool: &sqlx::PgPool) -> Vec<String> {
        sqlx::query_scalar("SELECT target FROM message_outbox WHERE bot = 'notify' ORDER BY id")
            .fetch_all(pool)
            .await
            .expect("load notices")
    }

    /// Makes every outbox INSERT fail until [`allow_outbox`].
    async fn reject_outbox(pool: &sqlx::PgPool) {
        sqlx::raw_sql(
            "CREATE OR REPLACE FUNCTION reject_outbox_insert() RETURNS trigger AS $$
             BEGIN RAISE EXCEPTION 'outbox rejected by test'; END $$ LANGUAGE plpgsql;
             CREATE TRIGGER reject_outbox BEFORE INSERT ON message_outbox
             FOR EACH ROW EXECUTE FUNCTION reject_outbox_insert();",
        )
        .execute(pool)
        .await
        .expect("install outbox rejection");
    }

    async fn allow_outbox(pool: &sqlx::PgPool) {
        sqlx::query("DROP TRIGGER reject_outbox ON message_outbox")
            .execute(pool)
            .await
            .expect("drop outbox rejection");
    }

    /// One active run holding the slot: a finished entry plus `entry-last` in `last_status`.
    async fn seed_run_with_last_entry(pool: &sqlx::PgPool, last_status: &str) {
        seed_agent(pool).await;
        seed_run(pool, "run-1", "active").await;
        seed_slot(pool, "run-1").await;
        seed_entry(pool, "run-1", "entry-done", "done", None).await;
        seed_entry(pool, "run-1", "entry-last", last_status, None).await;
    }

    /// The skip route's entry write completes the run in the same tx: a failed notice keeps
    /// the entry pending and the slot held, and the successful skip queues one notice.
    #[tokio::test]
    async fn skip_route_completes_run_with_entry_write_in_one_tx_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_run_with_last_entry(&pool, "pending").await;
        let app = test_router(pool.clone());

        reject_outbox(&pool).await;
        let (status, body) =
            request_json_response(&app, Method::PATCH, "/queue/entries/entry-last/skip", None)
                .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        assert_eq!(entry_status(&pool, "entry-last").await, "pending");
        assert_eq!(run_status(&pool, "run-1").await, "active");
        assert_eq!(slot_run(&pool).await.as_deref(), Some("run-1"));
        assert!(notice_targets(&pool).await.is_empty());

        allow_outbox(&pool).await;
        let (status, body) =
            request_json_response(&app, Method::PATCH, "/queue/entries/entry-last/skip", None)
                .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(entry_status(&pool, "entry-last").await, "skipped");
        assert_eq!(run_status(&pool, "run-1").await, "completed");
        assert_eq!(slot_run(&pool).await, None);
        assert_eq!(notice_targets(&pool).await, vec!["channel:123".to_string()]);
        pool.close().await;
        pg_db.drop().await;
    }

    /// A plain status change through the entry PATCH route completes the run with the write.
    #[tokio::test]
    async fn entry_patch_status_change_completes_run_with_entry_write_in_one_tx_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_run_with_last_entry(&pool, "pending").await;
        let app = test_router(pool.clone());
        let skip = json!({"status": "skipped"});

        reject_outbox(&pool).await;
        let (status, body) = request_json_response(
            &app,
            Method::PATCH,
            "/queue/entries/entry-last",
            Some(skip.clone()),
        )
        .await;
        assert_ne!(status, StatusCode::OK, "{body}");
        assert_eq!(entry_status(&pool, "entry-last").await, "pending");
        assert_eq!(run_status(&pool, "run-1").await, "active");
        assert!(notice_targets(&pool).await.is_empty());

        allow_outbox(&pool).await;
        let (status, body) =
            request_json_response(&app, Method::PATCH, "/queue/entries/entry-last", Some(skip))
                .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(entry_status(&pool, "entry-last").await, "skipped");
        assert_eq!(run_status(&pool, "run-1").await, "completed");
        assert_eq!(slot_run(&pool).await, None);
        assert_eq!(notice_targets(&pool).await, vec!["channel:123".to_string()]);
        pool.close().await;
        pg_db.drop().await;
    }

    /// The entry PATCH route's manual `done` reconciles a failed entry whose card finished,
    /// with its transition recorded; a `done` write never completes the run by itself.
    #[tokio::test]
    async fn entry_patch_done_reconciles_failed_entry_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_run_with_last_entry(&pool, "failed").await;
        sqlx::raw_sql(
            "UPDATE kanban_cards SET status = 'done' WHERE id = 'card-entry-last';
             INSERT INTO task_dispatches
                 (id, kanban_card_id, to_agent_id, dispatch_type, status, result)
             VALUES ('d-last', 'card-entry-last', 'agent-1', 'implementation', 'completed',
                     CAST('{\"completed_commit\":\"abc123\"}' AS jsonb));
             UPDATE auto_queue_entries SET dispatch_id = 'd-last' WHERE id = 'entry-last';",
        )
        .execute(&pool)
        .await
        .expect("seed finished card evidence");
        let app = test_router(pool.clone());

        let (status, body) = request_json_response(
            &app,
            Method::PATCH,
            "/queue/entries/entry-last",
            Some(json!({"status": "done"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(entry_status(&pool, "entry-last").await, "done");
        assert_eq!(
            scalar_i64(
                &pool,
                "SELECT COUNT(*)::BIGINT FROM auto_queue_entry_transitions
                  WHERE entry_id = 'entry-last' AND from_status = 'failed'
                    AND to_status = 'done' AND trigger_source = 'manual_terminal_reconcile'",
            )
            .await,
            1
        );
        assert_eq!(run_status(&pool, "run-1").await, "active");
        assert!(notice_targets(&pool).await.is_empty());
        pool.close().await;
        pg_db.drop().await;
    }

    /// The repair route clears a passed final gate, completes the drained run and queues its
    /// notice in the repair tx; a failed notice leaves the gate and run as they were.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repair_route_clears_gate_and_completes_run_in_one_tx_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_agent(&pool).await;
        seed_run(&pool, "run-1", "active").await;
        seed_slot(&pool, "run-1").await;
        seed_entry(&pool, "run-1", "entry-done", "done", None).await;
        let mut declaration = crate::phase_gate::resolve_declaration_value("pr-confirm")
            .expect("pr-confirm declaration");
        let gate = declaration.as_object_mut().expect("declaration object");
        gate.insert("run_id".to_string(), json!("run-1"));
        gate.insert("batch_phase".to_string(), json!(0));
        gate.insert("final_phase".to_string(), json!(true));
        let context = json!({"phase_gate": declaration});
        let result = json!({"verdict": "phase_gate_passed", "checks": {
            "merge_verified": "pass", "issue_closed": "pass", "build_passed": "pass"}});
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, to_agent_id, dispatch_type, status, title, context, result)
             VALUES ('d-gate', 'agent-1', 'phase-gate', 'completed', 'gate',
                     CAST($1 AS jsonb), CAST($2 AS jsonb))",
        )
        .bind(context.to_string())
        .bind(result.to_string())
        .execute(&pool)
        .await
        .expect("seed gate dispatch");
        crate::db::auto_queue::save_phase_gate_state_on_pg(
            &pool,
            "run-1",
            0,
            &crate::db::auto_queue::PhaseGateStateWrite {
                status: "failed".into(),
                pass_verdict: "phase_gate_passed".into(),
                dispatch_ids: vec!["d-gate".into()],
                next_phase: None,
                final_phase: true,
                failure_reason: Some("stale failed verdict".into()),
                ..Default::default()
            },
        )
        .await
        .expect("seed failed gate");
        let gates = "SELECT COUNT(*)::BIGINT FROM auto_queue_phase_gates WHERE run_id = 'run-1'";
        let app = test_router(pool.clone());
        let path = "/queue/runs/run-1/phase-gates/repair";

        reject_outbox(&pool).await;
        let (status, body) = request_json_response(&app, Method::POST, path, Some(json!({}))).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        assert_eq!(scalar_i64(&pool, gates).await, 1);
        assert_eq!(run_status(&pool, "run-1").await, "active");
        assert_eq!(slot_run(&pool).await.as_deref(), Some("run-1"));

        allow_outbox(&pool).await;
        let (status, body) = request_json_response(&app, Method::POST, path, Some(json!({}))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["cleared_gates"], 1, "{body}");
        assert_eq!(body["outcomes"][0]["run_finalized"], true, "{body}");
        assert_eq!(scalar_i64(&pool, gates).await, 0);
        assert_eq!(run_status(&pool, "run-1").await, "completed");
        assert_eq!(slot_run(&pool).await, None);
        assert_eq!(notice_targets(&pool).await, vec!["channel:123".to_string()]);
        pool.close().await;
        pg_db.drop().await;
    }

    async fn transition_count(pool: &sqlx::PgPool, entry: &str, to: &str, source: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT FROM auto_queue_entry_transitions
              WHERE entry_id = $1 AND to_status = $2 AND trigger_source = $3",
        )
        .bind(entry)
        .bind(to)
        .bind(source)
        .fetch_one(pool)
        .await
        .expect("count entry transitions")
    }

    /// Restore writes each entry's decision in one tx: `done` for a finished card, `pending`
    /// for a card needing a new dispatch, and `pending` then an eager re-attach for a card
    /// with a live dispatch. A failed write leaves the run cancelled and every entry skipped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restore_route_writes_done_existing_and_new_dispatch_entries_atomically_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_agent(&pool).await;
        seed_run(&pool, "run-1", "cancelled").await;
        seed_entry(&pool, "run-1", "entry-a", "skipped", None).await;
        seed_entry(&pool, "run-1", "entry-b", "skipped", None).await;
        seed_entry(&pool, "run-1", "entry-c", "skipped", None).await;
        sqlx::raw_sql(
            "UPDATE kanban_cards SET status = 'done' WHERE id = 'card-entry-a';
             INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status)
             VALUES ('d-a', 'card-entry-a', 'agent-1', 'implementation', 'completed'),
                    ('d-live', 'card-entry-b', 'agent-1', 'implementation', 'dispatched'),
                    ('d-c-old', 'card-entry-c', 'agent-1', 'implementation', 'cancelled');
             UPDATE kanban_cards SET latest_dispatch_id = 'd-live' WHERE id = 'card-entry-b';
             INSERT INTO auto_queue_entry_dispatch_history (entry_id, dispatch_id)
             VALUES ('entry-a', 'd-a'), ('entry-b', 'd-live'), ('entry-c', 'd-c-old');
             CREATE FUNCTION reject_entry_c() RETURNS trigger AS $$
             BEGIN
                 IF OLD.id = 'entry-c' THEN RAISE EXCEPTION 'entry-c rejected by test'; END IF;
                 RETURN NEW;
             END $$ LANGUAGE plpgsql;
             CREATE TRIGGER reject_entry_c BEFORE UPDATE ON auto_queue_entries
             FOR EACH ROW EXECUTE FUNCTION reject_entry_c();",
        )
        .execute(&pool)
        .await
        .expect("seed restore fixture");
        let app = test_router(pool.clone());
        let path = "/queue/runs/run-1/restore";

        let (status, body) = request_json_response(&app, Method::POST, path, Some(json!({}))).await;
        assert_ne!(status, StatusCode::OK, "{body}");
        assert_eq!(run_status(&pool, "run-1").await, "cancelled");
        for entry in ["entry-a", "entry-b", "entry-c"] {
            assert_eq!(entry_status(&pool, entry).await, "skipped", "{entry}");
        }

        sqlx::query("DROP TRIGGER reject_entry_c ON auto_queue_entries")
            .execute(&pool)
            .await
            .expect("drop entry rejection");
        let (status, body) = request_json_response(&app, Method::POST, path, Some(json!({}))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            transition_count(&pool, "entry-a", "done", "restore_run_done").await,
            1
        );
        assert_eq!(entry_status(&pool, "entry-a").await, "done");
        assert_eq!(
            transition_count(
                &pool,
                "entry-c",
                "pending",
                "restore_run_pending_new_dispatch"
            )
            .await,
            1
        );
        assert_eq!(
            transition_count(
                &pool,
                "entry-b",
                "dispatched",
                "restore_run_attach_existing_dispatch"
            )
            .await,
            1,
            "{body}"
        );
        let attached: (String, Option<String>, Option<i64>) = sqlx::query_as(
            "SELECT status, dispatch_id, slot_index::BIGINT FROM auto_queue_entries
              WHERE id = 'entry-b'",
        )
        .fetch_one(&pool)
        .await
        .expect("load re-attached entry");
        assert_eq!(attached.0, "dispatched");
        assert_eq!(attached.1.as_deref(), Some("d-live"));
        assert!(
            attached.2.is_some(),
            "the re-attached entry holds a slot: {body}"
        );
        pool.close().await;
        pg_db.drop().await;
    }
}
