//! Domain-split routes tests — `dispatch` group.
//! Extracted verbatim from the original `routes_tests.rs` (test bodies unchanged).
//! Helpers and the `sqlite_params!` macro live in `super::common` and the parent
//! `routes_tests` module respectively (the macro is in lexical scope through
//! the parent `mod.rs`).

#![allow(unused_imports)]

// Reach into the parent `routes` module for the symbols the original
// `use super::*;` brought in, plus the shared test helpers in `common`.
use super::super::*;
use super::common::*;

use axum::body::{Body, HttpBody as _};
use axum::http::{Request, StatusCode};
use serde_json::json;
use sqlx::Row;
use std::ffi::OsString;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::pin::Pin;
use std::process::Command;
use std::sync::Arc;
use std::sync::MutexGuard;
use tower::ServiceExt;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_list_pending_dispatches_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id)
         VALUES ($1, $2, '111')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind("agent-pg-queue-list")
    .bind("Agent PG Queue List")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, NOW(), NOW()
         )",
    )
    .bind("card-pg-queue-list")
    .bind("Queue PG List")
    .bind("ready")
    .bind("medium")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, retry_count, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, NOW(), NOW()
         )",
    )
    .bind("dispatch-pg-queue-list")
    .bind("card-pg-queue-list")
    .bind("agent-pg-queue-list")
    .bind("review")
    .bind("pending")
    .bind("PG queue list dispatch")
    .bind(0_i32)
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri("/dispatches/pending")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body);
    assert_eq!(
        status,
        StatusCode::OK,
        "queue_list_pending_dispatches_pg_only_without_sqlite_mirror status={} body={}",
        status,
        body_text
    );

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["count"], 1);
    assert_eq!(json["dispatches"][0]["id"], "dispatch-pg-queue-list");
    assert_eq!(
        json["dispatches"][0]["kanban_card_id"],
        "card-pg-queue-list"
    );

    let sqlite_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches WHERE id = 'dispatch-pg-queue-list'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sqlite_count, 0, "sqlite mirror should stay empty");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_cancel_dispatch_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id)
         VALUES ($1, $2, '111')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind("agent-pg-queue-cancel")
    .bind("Agent PG Queue Cancel")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, active_thread_id, channel_thread_map, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6::jsonb, NOW(), NOW()
         )",
    )
    .bind("card-pg-queue-cancel")
    .bind("Queue PG Cancel")
    .bind("in_progress")
    .bind("high")
    .bind("thread-review-cancelled")
    .bind(
        json!({
            "111": "thread-review-cancelled",
            "222": "thread-work-active"
        })
        .to_string(),
    )
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, thread_id, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, NOW(), NOW()
         )",
    )
    .bind("dispatch-pg-queue-cancel")
    .bind("card-pg-queue-cancel")
    .bind("agent-pg-queue-cancel")
    .bind("implementation")
    .bind("pending")
    .bind("PG queue cancel dispatch")
    .bind("thread-review-cancelled")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query("INSERT INTO kv_meta (key, value) VALUES ($1, $2)")
        .bind("dispatch_notified:dispatch-pg-queue-cancel")
        .bind("1")
        .execute(&pg_pool)
        .await
        .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatches/dispatch-pg-queue-cancel/cancel")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body);
    assert_eq!(
        status,
        StatusCode::OK,
        "queue_cancel_dispatch_pg_only_without_sqlite_mirror status={} body={}",
        status,
        body_text
    );

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["dispatch_id"], "dispatch-pg-queue-cancel");

    let dispatch_status: String =
        sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
            .bind("dispatch-pg-queue-cancel")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(dispatch_status, "cancelled");

    let kv_guard_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM kv_meta WHERE key = $1")
            .bind("dispatch_notified:dispatch-pg-queue-cancel")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(
        kv_guard_count, 0,
        "dispatch_notified guard should be cleared"
    );

    let (channel_thread_map, active_thread_id): (Option<serde_json::Value>, Option<String>) =
        sqlx::query_as(
            "SELECT channel_thread_map, active_thread_id
             FROM kanban_cards
             WHERE id = $1",
        )
        .bind("card-pg-queue-cancel")
        .fetch_one(&pg_pool)
        .await
        .unwrap();
    assert_eq!(
        channel_thread_map,
        Some(json!({"222": "thread-work-active"})),
        "cancelled dispatch thread must be removed without dropping sibling work thread"
    );
    assert_eq!(
        active_thread_id.as_deref(),
        Some("thread-work-active"),
        "active_thread_id should move away from the cancelled review thread"
    );

    let sqlite_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches WHERE id = 'dispatch-pg-queue-cancel'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sqlite_count, 0, "sqlite mirror should stay empty");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_cancel_dispatch_cancels_matching_active_turn_pg() {
    let _obs_guard = crate::services::observability::test_runtime_lock();
    crate::services::observability::reset_for_tests();
    crate::services::observability::init_observability(None);

    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());
    let harness = crate::services::discord::health::TestHealthHarness::new_with_provider(
        crate::services::provider::ProviderKind::Codex,
    )
    .await;
    let channel_id = "1485506232256168022";
    let channel_num = channel_id.parse::<u64>().unwrap();
    let tmux_name = "AgentDesk-codex-dispatch-cancel-1552";
    let session_key = format!("mac-mini:{tmux_name}");

    sqlx::query(
        "INSERT INTO agents (
            id, name, provider, discord_channel_cdx, created_at, updated_at
         ) VALUES (
            $1, $2, 'codex', $3, NOW(), NOW()
         )",
    )
    .bind("agent-dispatch-cancel-turn")
    .bind("Agent Dispatch Cancel Turn")
    .bind(channel_id)
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, assigned_agent_id, created_at, updated_at
         ) VALUES (
            $1, $2, 'in_progress', $3, NOW(), NOW()
         )",
    )
    .bind("card-dispatch-cancel-turn")
    .bind("Dispatch Cancel Turn")
    .bind("agent-dispatch-cancel-turn")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
         ) VALUES (
            $1, $2, $3, 'implementation', 'dispatched', $4, NOW(), NOW()
         )",
    )
    .bind("dispatch-cancel-turn-1552")
    .bind("card-dispatch-cancel-turn")
    .bind("agent-dispatch-cancel-turn")
    .bind("Cancel active turn too")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO sessions (
            session_key, agent_id, provider, status, active_dispatch_id,
            thread_channel_id, last_heartbeat, created_at
         ) VALUES (
            $1, $2, 'codex', 'turn_active', $3, $4, NOW(), NOW()
         )",
    )
    .bind(&session_key)
    .bind("agent-dispatch-cancel-turn")
    .bind("dispatch-cancel-turn-1552")
    .bind(channel_id)
    .execute(&pg_pool)
    .await
    .unwrap();

    harness
        .seed_channel_session(
            channel_num,
            "dispatch-cancel-1552",
            Some("session-dispatch-cancel-1552"),
        )
        .await;
    let token = harness
        .start_active_turn(channel_num, 15, 1552, Some(tmux_name))
        .await;
    harness
        .seed_queue(channel_num, &[(2_552, "preserve dispatch cancel queue")])
        .await;

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        Some(harness.registry()),
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatches/dispatch-cancel-turn-1552/cancel")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body);
    assert_eq!(
        status,
        StatusCode::OK,
        "queue_cancel_dispatch_cancels_matching_active_turn_pg status={} body={}",
        status,
        body_text
    );

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["dispatch_id"], "dispatch-cancel-turn-1552");
    assert_eq!(json["active_turn_cancelled"], true);
    assert_eq!(json["turn_session_key"], session_key);
    assert_eq!(json["turn_tmux_session"], tmux_name);
    assert_eq!(json["turn_channel_id"], channel_id);
    assert_eq!(json["turn_agent_id"], "agent-dispatch-cancel-turn");
    assert_eq!(json["turn_status"], "cancelled");
    assert!(json["turn_completed_at"].as_str().is_some());
    assert_eq!(json["turn_lifecycle_path"], "runtime-fallback");
    assert_eq!(json["turn_tmux_killed"], false);
    assert_eq!(json["turn_queue_preserved"], true);
    assert_eq!(json["turn_inflight_cleared"], false);
    assert_eq!(json["turn_queued_remaining"], 1);

    assert!(
        token.cancelled.load(std::sync::atomic::Ordering::Relaxed),
        "dispatch cancel must signal the active turn token"
    );
    let (has_active_turn, queue_depth, session_id) = harness.mailbox_state(channel_num).await;
    assert!(!has_active_turn);
    assert_eq!(queue_depth, 1);
    assert_eq!(session_id, None);

    // #1672 P2: dispatch cancel must mirror the `/turns/{id}/cancel`
    // surface and schedule the deferred idle-queue drain so the
    // preserved pending_queue item resumes without waiting for the
    // next user message. We observe this through the harness'
    // `deferred_hook_backlog()` getter, which surfaces the counter
    // that `schedule_deferred_idle_queue_kickoff` increments
    // *synchronously* before spawning the 2s-delayed drain task.
    let backlog_after_cancel = harness.deferred_hook_backlog();
    assert!(
        backlog_after_cancel >= 1,
        "dispatch cancel must schedule the post-cancel queue drain (backlog={backlog_after_cancel}) — issue #1672 P2"
    );

    let event = crate::services::observability::events::recent(10)
        .into_iter()
        .find(|event| event.event_type == "turn_cancelled")
        .expect("turn_cancelled event should be recorded");
    assert_eq!(event.channel_id, Some(channel_num));
    assert_eq!(event.provider.as_deref(), Some("codex"));
    assert_eq!(
        event.payload["reason"],
        "queue-api cancel_dispatch (preserve)"
    );
    assert_eq!(event.payload["surface"], "queue_cancel_preserve");
    assert_eq!(event.payload["dispatch_id"], "dispatch-cancel-turn-1552");
    assert_eq!(event.payload["session_key"], session_key);

    let dispatch_status: String =
        sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
            .bind("dispatch-cancel-turn-1552")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(dispatch_status, "cancelled");

    let (session_status, active_dispatch_id): (String, Option<String>) = sqlx::query_as(
        "SELECT status, active_dispatch_id
         FROM sessions
         WHERE session_key = $1",
    )
    .bind(&session_key)
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(session_status, "disconnected");
    assert_eq!(active_dispatch_id, None);

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn dispatch_pg_list_empty() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .uri("/dispatches")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["dispatches"].as_array().unwrap().is_empty());

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn dispatch_pg_create_and_get() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('ch-td', 'TD', '111', '222')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at) VALUES ('c1', 'Card1', 'ready', 'medium', NOW(), NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine.clone(),
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatches")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"kanban_card_id":"c1","to_agent_id":"ch-td","title":"Do it"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, StatusCode::CREATED, "unexpected response: {json}");
    let dispatch_id = json["dispatch"]["id"].as_str().unwrap().to_string();
    assert_eq!(json["dispatch"]["status"], "pending");
    assert_eq!(json["dispatch"]["kanban_card_id"], "c1");

    // #255: ready→requested is free, so dispatch from ready kicks off to "in_progress"
    let card_status: String = sqlx::query_scalar("SELECT status FROM kanban_cards WHERE id = 'c1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(card_status, "in_progress");
    let notify_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT FROM dispatch_outbox WHERE dispatch_id = $1 AND action = 'notify'",
    )
    .bind(&dispatch_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(notify_count, 1, "API create must persist notify outbox");

    // GET single dispatch
    let app2 = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let response2 = app2
        .oneshot(
            Request::builder()
                .uri(&format!("/dispatches/{dispatch_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response2.status(), StatusCode::OK);
    let body2 = axum::body::to_bytes(response2.into_body(), usize::MAX)
        .await
        .unwrap();
    let json2: serde_json::Value = serde_json::from_slice(&body2).unwrap();
    assert_eq!(json2["dispatch"]["id"], dispatch_id);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_routes_allow_same_agent_parallel_delivery_on_different_provider_channels_pg() {
    let _env_lock = env_lock();
    let (base_url, state, server_handle) = spawn_mock_dispatch_delivery_server().await;
    let _api_base = EnvVarGuard::set("AGENTDESK_DISCORD_API_BASE_URL", &base_url);
    let runtime_root = tempfile::tempdir().unwrap();
    write_announce_token(runtime_root.path());
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());

    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (
                id, name, provider, discord_channel_id, discord_channel_alt,
                discord_channel_cc, discord_channel_cdx, created_at, updated_at
             ) VALUES (
                'agent-parallel-provider', 'Agent Parallel Provider', 'claude', '111', '222',
                '111', '222', datetime('now'), datetime('now')
             )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, created_at, updated_at
             ) VALUES (
                'card-parallel-impl', 'Parallel implementation', 'ready', 'medium',
                'agent-parallel-provider', datetime('now'), datetime('now')
             )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, created_at, updated_at
             ) VALUES (
                'card-parallel-consult', 'Parallel consultation', 'ready', 'medium',
                'agent-parallel-provider', datetime('now'), datetime('now')
             )",
            [],
        )
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO agents (
            id, name, provider, discord_channel_id, discord_channel_alt,
            discord_channel_cc, discord_channel_cdx, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, NOW(), NOW()
         )",
    )
    .bind("agent-parallel-provider")
    .bind("Agent Parallel Provider")
    .bind("claude")
    .bind("111")
    .bind("222")
    .bind("111")
    .bind("222")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, NOW(), NOW()
         )",
    )
    .bind("card-parallel-impl")
    .bind("Parallel implementation")
    .bind("ready")
    .bind("medium")
    .bind("agent-parallel-provider")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, NOW(), NOW()
         )",
    )
    .bind("card-parallel-consult")
    .bind("Parallel consultation")
    .bind("ready")
    .bind("medium")
    .bind("agent-parallel-provider")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let impl_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatches")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"kanban_card_id":"card-parallel-impl","to_agent_id":"agent-parallel-provider","dispatch_type":"implementation","title":"Parallel implementation"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(impl_response.status(), StatusCode::CREATED);
    let impl_body = axum::body::to_bytes(impl_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let impl_json: serde_json::Value = serde_json::from_slice(&impl_body).unwrap();
    let impl_dispatch_id = impl_json["dispatch"]["id"]
        .as_str()
        .expect("implementation dispatch id")
        .to_string();

    let consult_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatches")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"kanban_card_id":"card-parallel-consult","to_agent_id":"agent-parallel-provider","dispatch_type":"consultation","title":"Parallel consultation"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(consult_response.status(), StatusCode::CREATED);
    let consult_body = axum::body::to_bytes(consult_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let consult_json: serde_json::Value = serde_json::from_slice(&consult_body).unwrap();
    let consult_dispatch_id = consult_json["dispatch"]["id"]
        .as_str()
        .expect("consultation dispatch id")
        .to_string();

    let pending_notify_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT
           FROM dispatch_outbox
          WHERE dispatch_id = ANY($1)
            AND action = 'notify'
            AND status = 'pending'",
    )
    .bind(vec![impl_dispatch_id.clone(), consult_dispatch_id.clone()])
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(
        pending_notify_count, 2,
        "route create must enqueue both notify rows before delivery"
    );

    let processed = crate::server::routes::dispatches::process_outbox_batch_with_real_notifier(
        Some(&db),
        &pg_pool,
    )
    .await;
    assert_eq!(processed, 2, "outbox worker should drain both notify rows");

    server_handle.abort();

    let state = state.lock().unwrap();
    assert!(
        state
            .calls
            .contains(&"POST /channels/111/threads".to_string()),
        "implementation dispatch must use the primary provider channel: {:?}",
        state.calls
    );
    assert!(
        state
            .calls
            .contains(&"POST /channels/thread-111/messages".to_string()),
        "implementation dispatch must post into its primary-thread mailbox: {:?}",
        state.calls
    );
    assert!(
        state
            .calls
            .contains(&"POST /channels/222/threads".to_string()),
        "consultation dispatch must use the counter-model provider channel: {:?}",
        state.calls
    );
    assert!(
        state
            .calls
            .contains(&"POST /channels/thread-222/messages".to_string()),
        "consultation dispatch must post into its counter-model thread mailbox: {:?}",
        state.calls
    );
    drop(state);

    let impl_thread_id: Option<String> = sqlx::query_scalar(
        "SELECT thread_id
           FROM task_dispatches
          WHERE id = $1",
    )
    .bind(&impl_dispatch_id)
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(impl_thread_id.as_deref(), Some("thread-111"));
    let impl_status: String = sqlx::query_scalar(
        "SELECT status
           FROM task_dispatches
          WHERE id = $1",
    )
    .bind(&impl_dispatch_id)
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(impl_status, "dispatched");
    let consult_thread_id: Option<String> = sqlx::query_scalar(
        "SELECT thread_id
           FROM task_dispatches
          WHERE id = $1",
    )
    .bind(&consult_dispatch_id)
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(consult_thread_id.as_deref(), Some("thread-222"));
    let consult_status: String = sqlx::query_scalar(
        "SELECT status
           FROM task_dispatches
          WHERE id = $1",
    )
    .bind(&consult_dispatch_id)
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(consult_status, "dispatched");

    let done_notify_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT
           FROM dispatch_outbox
          WHERE dispatch_id = ANY($1)
            AND action = 'notify'
            AND status = 'done'",
    )
    .bind(vec![impl_dispatch_id.clone(), consult_dispatch_id.clone()])
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(done_notify_count, 2, "notify rows must complete via outbox");
    let pending_status_reactions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT
           FROM dispatch_outbox
          WHERE dispatch_id = ANY($1)
            AND action = 'status_reaction'
            AND status = 'pending'",
    )
    .bind(vec![impl_dispatch_id, consult_dispatch_id])
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(
        pending_status_reactions, 2,
        "notify delivery must enqueue one status_reaction follow-up per dispatch"
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn dispatch_pg_create_for_terminal_card_returns_conflict_with_reason() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '555', '666')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, priority, assigned_agent_id, created_at, updated_at)
         VALUES ('c-terminal', 'Terminal Card', 'done', 'medium', 'agent-1', NOW(), NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatches")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"kanban_card_id":"c-terminal","to_agent_id":"agent-1","dispatch_type":"review","title":"Review Terminal"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("terminal card c-terminal (status: done)"),
        "expected terminal-card detail, got {json}"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn dispatch_create_with_skip_outbox_omits_notify_row_pg() {
    let db = test_db();
    seed_test_agents(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at) VALUES ('c1-skip', 'Card1 Skip', 'ready', 'medium', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
    }

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
         VALUES ($1, $2, $3, $4)",
    )
    .bind("ch-td")
    .bind("TD")
    .bind("111")
    .bind("222")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, priority)
         VALUES ($1, $2, $3, $4)",
    )
    .bind("c1-skip")
    .bind("Card1 Skip")
    .bind("ready")
    .bind("medium")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatches")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"kanban_card_id":"c1-skip","to_agent_id":"ch-td","title":"Bookkeeping only","skip_outbox":true}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, StatusCode::CREATED, "unexpected response: {json}");
    let dispatch_id = json["dispatch"]["id"].as_str().unwrap().to_string();

    let verify_pool = sqlx::PgPool::connect(&pg_db.database_url).await.unwrap();
    let notify_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint
         FROM dispatch_outbox
         WHERE dispatch_id = $1 AND action = 'notify'",
    )
    .bind(&dispatch_id)
    .fetch_one(&verify_pool)
    .await
    .unwrap();
    assert_eq!(
        notify_count, 0,
        "skip_outbox=true must suppress notify outbox persistence"
    );

    verify_pool.close().await;
    drop(app);
    pg_pool.close().await;
    pg_db.drop().await;
}

/// #761: A crafted `POST /api/dispatches` call that preseeds review-target
/// fields (`reviewed_commit`, `worktree_path`, `branch`, `target_repo`) must
/// NOT be able to steer the review dispatch at an arbitrary commit/path. The
/// fields are stripped before `build_review_context` runs, and the
/// validation/refresh chain resolves the real target from the card's history.
#[tokio::test]
async fn dispatch_create_review_strips_untrusted_review_target_fields_from_context_pg() {
    let db = test_db();
    seed_test_agents(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    // Real repo exists on the card, but the caller injects a foreign
    // external target_repo. The hardened path must fail closed instead of
    // silently falling back to the card repo and reviewing unrelated code.
    let (repo, _repo_override) = setup_test_repo();
    let real_worktree_path = repo.path().to_string_lossy().into_owned();

    // Card in the review-ready state (pre-review), linked to a real repo
    // path while the caller injects a conflicting foreign target_repo.
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, github_issue_number,
                created_at, updated_at
             ) VALUES (
                'card-761', 'Preseed review target', 'in_progress', 'medium', 'ch-td', 761,
                datetime('now'), datetime('now')
             )",
            [],
        )
        .unwrap();
    }

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
         VALUES ($1, $2, $3, $4)",
    )
    .bind("ch-td")
    .bind("TD")
    .bind("111")
    .bind("222")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, github_issue_number, repo_id
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7
         )",
    )
    .bind("card-761")
    .bind("Preseed review target")
    .bind("in_progress")
    .bind("medium")
    .bind("ch-td")
    .bind(761_i64)
    .bind(&real_worktree_path)
    .execute(&pg_pool)
    .await
    .unwrap();

    // Simulate a malicious / buggy caller preseeding review-target fields.
    // The injected commit SHA is syntactically valid but points at nothing
    // in this repo; the injected worktree path doesn't exist either.
    //
    // #761 (Codex round-2): also set `_trusted_review_target: true` in the
    // context to prove the flag is inert. The API-sourced code path MUST
    // ignore any JSON-supplied trust signal and always treat review-target
    // fields as untrusted.
    let injected_commit = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    let injected_worktree = "/tmp/agentdesk-761-attacker-controlled-worktree";
    let injected_target_repo = "/tmp/agentdesk-761-attacker-controlled-repo";
    let body = serde_json::json!({
        "kanban_card_id": "card-761",
        "to_agent_id": "ch-td",
        "dispatch_type": "review",
        "title": "[Review R1] card-761",
        "context": {
            "reviewed_commit": injected_commit,
            "worktree_path": injected_worktree,
            "branch": "attacker/controlled-branch",
            "target_repo": injected_target_repo,
            "_trusted_review_target": true,
        }
    })
    .to_string();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatches")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "unexpected response: {json}"
    );
    let error = json["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("external_target_repo_unrecoverable"),
        "expected fail-closed external_target_repo guard, got {json}"
    );
    assert!(
        error.contains(injected_target_repo),
        "error should point at the rejected injected target_repo, got {json}"
    );
    assert!(
        !json.to_string().contains(injected_commit),
        "error response must not echo the injected reviewed_commit: {json}"
    );
    assert!(
        !json.to_string().contains(injected_worktree),
        "error response must not echo the injected worktree_path: {json}"
    );
    assert!(
        !json.to_string().contains("attacker/controlled-branch"),
        "error response must not echo the injected branch: {json}"
    );

    let dispatch_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM task_dispatches WHERE kanban_card_id = $1",
    )
    .bind("card-761")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(
        dispatch_count, 0,
        "fail-closed review-target validation must not persist a dispatch row"
    );

    drop(app);
    pg_pool.close().await;
    pg_db.drop().await;
}

/// #761 (Codex round-2): Focused negative test for the trust-boundary
/// redesign. The previous round's `_trusted_review_target` context flag was
/// client-controlled, so an attacker could set it alongside injected
/// review-target fields and bypass stripping entirely. The fix replaces the
/// flag with an out-of-band Rust enum parameter on `build_review_context`,
/// and the API-sourced path
/// (`POST /api/dispatches` → `create_dispatch_core_internal`) always uses
/// `ReviewTargetTrust::Untrusted`. This test asserts the flag cannot bypass
/// stripping on its own, even without any "real" review target existing for
/// the card — the injected values must be dropped and never resurrected
/// from the context payload.
#[tokio::test]
async fn dispatch_create_review_ignores_client_trusted_review_target_flag_pg() {
    let db = test_db();
    seed_test_agents(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    // Deliberately do NOT seed any work dispatch or pr_tracking row for this
    // card — the validation/refresh chain has nothing to resolve. If the
    // flag were honored, the injected fields would slip straight into the
    // persisted context. The fix means they get stripped and remain absent.
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, github_issue_number,
                created_at, updated_at
             ) VALUES (
                'card-761-flag', 'Ignore trust flag', 'in_progress', 'medium', 'ch-td', 999999,
                datetime('now'), datetime('now')
             )",
            [],
        )
        .unwrap();
    }

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
         VALUES ($1, $2, $3, $4)",
    )
    .bind("ch-td")
    .bind("TD")
    .bind("111")
    .bind("222")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, github_issue_number
         ) VALUES (
            $1, $2, $3, $4, $5, $6
         )",
    )
    .bind("card-761-flag")
    .bind("Ignore trust flag")
    .bind("in_progress")
    .bind("medium")
    .bind("ch-td")
    .bind(999999_i64)
    .execute(&pg_pool)
    .await
    .unwrap();

    let injected_commit = "cafef00dcafef00dcafef00dcafef00dcafef00d";
    let injected_worktree = "/tmp/agentdesk-761-flag-attacker-worktree";
    let injected_target_repo = "/tmp/agentdesk-761-flag-attacker-repo";
    let body = serde_json::json!({
        "kanban_card_id": "card-761-flag",
        "to_agent_id": "ch-td",
        "dispatch_type": "review",
        "title": "[Review R1] trust-flag bypass attempt",
        "context": {
            "reviewed_commit": injected_commit,
            "worktree_path": injected_worktree,
            "branch": "attacker/trust-flag-bypass",
            "target_repo": injected_target_repo,
            // The crux: client explicitly asserts trust. The server must ignore it.
            "_trusted_review_target": true,
        }
    })
    .to_string();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatches")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    // Dispatch creation may succeed (validation chain has nothing to inject
    // but that's not fatal for review dispatches — noop-style contexts are
    // valid). What matters is that the INJECTED fields did not propagate.
    // Some routes return CREATED on success or CONFLICT if the worktree
    // recovery chain finds nothing usable; accept either, only assert on
    // the persisted context if the row was created.
    let status = response.status();
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    if status == StatusCode::CREATED {
        let context = &json["dispatch"]["context"];
        assert_ne!(
            context["reviewed_commit"].as_str(),
            Some(injected_commit),
            "client-supplied trust flag must NOT bypass reviewed_commit stripping"
        );
        assert_ne!(
            context["worktree_path"].as_str(),
            Some(injected_worktree),
            "client-supplied trust flag must NOT bypass worktree_path stripping"
        );
        assert_ne!(
            context["branch"].as_str(),
            Some("attacker/trust-flag-bypass"),
            "client-supplied trust flag must NOT bypass branch stripping"
        );
        assert_ne!(
            context["target_repo"].as_str(),
            Some(injected_target_repo),
            "client-supplied trust flag must NOT bypass target_repo stripping"
        );
        assert!(
            context.get("_trusted_review_target").is_none(),
            "client-supplied _trusted_review_target flag must not persist into the dispatch context"
        );
    } else {
        // If creation failed, the injected values clearly didn't end up
        // anywhere — test passes vacuously. But the response JSON must NOT
        // echo them back (and the dispatch service doesn't echo request
        // bodies on error, so this is a sanity guard only).
        assert!(
            !json.to_string().contains(injected_commit),
            "error response must not echo the injected reviewed_commit: {json}"
        );
    }

    drop(app);
    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn dispatch_pg_create_card_not_found() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatches")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"kanban_card_id":"nope","to_agent_id":"ch-td","title":"Do it"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn dispatch_pg_complete() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('ch-td', 'TD', '111', '222')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at) VALUES ('c1', 'Card1', 'ready', 'medium', NOW(), NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Create dispatch
    let app = test_api_router_with_pg(
        db.clone(),
        engine.clone(),
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatches")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"kanban_card_id":"c1","to_agent_id":"ch-td","title":"Do it"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let dispatch_id = json["dispatch"]["id"].as_str().unwrap().to_string();

    // Complete dispatch
    let app2 = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let response2 = app2
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(&format!("/dispatches/{dispatch_id}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"status":"completed","result":{"ok":true,"agent_response_present":true}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response2.status(), StatusCode::OK);
    let body2 = axum::body::to_bytes(response2.into_body(), usize::MAX)
        .await
        .unwrap();
    let json2: serde_json::Value = serde_json::from_slice(&body2).unwrap();
    assert_eq!(json2["dispatch"]["status"], "completed");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn dispatch_pg_get_not_found() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .uri("/dispatches/nonexistent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn dispatch_pg_list_with_filter() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at) VALUES ('c1', 'Card1', 'ready', 'medium', NOW(), NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, status, title, created_at, updated_at) VALUES ('d1', 'c1', 'ag1', 'pending', 'T1', NOW(), NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, status, title, created_at, updated_at) VALUES ('d2', 'c1', 'ag1', 'completed', 'T2', NOW(), NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri("/dispatches?status=pending")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let dispatches = json["dispatches"].as_array().unwrap();
    assert_eq!(dispatches.len(), 1);
    assert_eq!(dispatches[0]["id"], "d1");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn dispatch_pg_endpoints_include_normalized_result_summary() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '555', '666')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at)
         VALUES ('card-dispatch-summary', 'Dispatch Summary Card', 'review', 'medium', NOW(), NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, result, created_at, updated_at
         ) VALUES (
            'dispatch-cancel-summary', 'card-dispatch-summary', 'agent-1', 'implementation', 'cancelled',
            'Cancelled dispatch', $1, NOW() - INTERVAL '1 minute', NOW() - INTERVAL '1 minute'
         )",
    )
    .bind(
        json!({
            "reason": "auto_cancelled_on_terminal_card"
        })
        .to_string(),
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, result, created_at, updated_at
         ) VALUES (
            'dispatch-review-summary', 'card-dispatch-summary', 'agent-1', 'review-decision', 'completed',
            'Review decision', $1, NOW(), NOW()
         )",
    )
    .bind(
        json!({
            "decision": "accept",
            "comment": "Looks good"
        })
        .to_string(),
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine.clone(),
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let list_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/dispatches")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(list_response.status(), StatusCode::OK);
    let list_body = axum::body::to_bytes(list_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let list_json: serde_json::Value = serde_json::from_slice(&list_body).unwrap();
    let dispatches = list_json["dispatches"].as_array().unwrap();

    let cancelled = dispatches
        .iter()
        .find(|dispatch| dispatch["id"] == "dispatch-cancel-summary")
        .expect("cancelled dispatch must be returned");
    assert_eq!(
        cancelled["result_summary"],
        "Cancelled: terminal card cleanup"
    );
    assert_eq!(
        cancelled["result"]["reason"],
        serde_json::Value::String("auto_cancelled_on_terminal_card".to_string())
    );

    let review_decision = dispatches
        .iter()
        .find(|dispatch| dispatch["id"] == "dispatch-review-summary")
        .expect("review decision dispatch must be returned");
    assert_eq!(
        review_decision["result_summary"],
        "Accepted review feedback: Looks good"
    );
    assert_eq!(review_decision["result"]["decision"], "accept");

    let get_response = app
        .oneshot(
            Request::builder()
                .uri("/dispatches/dispatch-review-summary")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(get_response.status(), StatusCode::OK);
    let get_body = axum::body::to_bytes(get_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let get_json: serde_json::Value = serde_json::from_slice(&get_body).unwrap();
    assert_eq!(
        get_json["dispatch"]["result_summary"],
        "Accepted review feedback: Looks good"
    );
    assert_eq!(get_json["dispatch"]["result"]["comment"], "Looks good");

    pool.close().await;
    pg_db.drop().await;
}

/// #1444: when /api/queue/dispatch-next encounters an entry whose card
/// already has an active dispatch, it must reuse the existing dispatch
/// (no duplicate creation) and emit the DISPATCH-NEXT skip log marker. We
/// verify the no-duplicate property — the card's latest_dispatch_id must
/// not change, and dispatch row count for the card stays at 1.
#[tokio::test]
async fn dispatch_next_skips_card_with_active_dispatch_pg_1444() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pool.clone());
    seed_repo_pg(&pool, "test-repo").await;
    seed_agent_pg(&pool, "agent-dn-1444").await;

    // Seed a card already pointing at a live dispatch.
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, repo_id,
            github_issue_number, latest_dispatch_id, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, NOW(), NOW()
         )",
    )
    .bind("card-dn-1444")
    .bind("Dispatch-next guard 1444")
    .bind("ready")
    .bind("medium")
    .bind("agent-dn-1444")
    .bind("test-repo")
    .bind(1444020_i64)
    .bind("dispatch-dn-1444-existing")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, NOW(), NOW()
         )",
    )
    .bind("dispatch-dn-1444-existing")
    .bind("card-dn-1444")
    .bind("agent-dn-1444")
    .bind("implementation")
    .bind("dispatched")
    .bind("[Impl] existing dn 1444")
    .execute(&pool)
    .await
    .unwrap();

    // Build an active auto_queue run with one pending entry pointing at
    // this card so dispatch-next has something to iterate over.
    sqlx::query(
        "INSERT INTO auto_queue_runs (
            id, repo, agent_id, status, max_concurrent_threads, thread_group_count
         ) VALUES (
            $1, $2, $3, 'active', 1, 1
         )",
    )
    .bind("run-dn-1444")
    .bind("test-repo")
    .bind("agent-dn-1444")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, thread_group, batch_phase
         ) VALUES (
            $1, $2, $3, $4, 'pending', 0, 0
         )",
    )
    .bind("entry-dn-1444")
    .bind("run-dn-1444")
    .bind("card-dn-1444")
    .bind("agent-dn-1444")
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"repo":"test-repo"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body).to_string();
    assert!(
        status.is_success(),
        "dispatch-next should respond 2xx even when the entry is skipped: status={status} body={body_text}"
    );

    // No new dispatch must have been created — count stays at 1.
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT FROM task_dispatches WHERE kanban_card_id = $1",
    )
    .bind("card-dn-1444")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        count, 1,
        "dispatch-next must NOT create a duplicate dispatch when the card already has an active one: body={body_text}"
    );
    let latest: Option<String> =
        sqlx::query_scalar("SELECT latest_dispatch_id FROM kanban_cards WHERE id = $1")
            .bind("card-dn-1444")
            .fetch_optional(&pool)
            .await
            .unwrap()
            .flatten();
    assert_eq!(
        latest.as_deref(),
        Some("dispatch-dn-1444-existing"),
        "card must still point at the original dispatch"
    );

    pool.close().await;
    pg_db.drop().await;
}

/// #265: PATCH /dispatches/:id with an invalid status like "done" must return
/// 400 and must NOT modify the dispatch or its associated card state.
#[tokio::test]
async fn patch_dispatch_pg_rejects_invalid_status() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('ch-td', 'TD', '111', '222')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at)
         VALUES ('card-265', 'Stuck Card', 'in_progress', 'ch-td', 'dispatch-265', NOW(), NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
         VALUES ('dispatch-265', 'card-265', 'ch-td', 'rework', 'dispatched', 'Rework task', NOW(), NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/dispatches/dispatch-265")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"status":"done"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "invalid status 'done' must be rejected with 400"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["error"]
            .as_str()
            .unwrap()
            .contains("invalid dispatch status"),
        "error message must mention invalid status"
    );

    // Verify dispatch status is unchanged (pipeline invariant)
    let dispatch_status: String =
        sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = 'dispatch-265'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        dispatch_status, "dispatched",
        "dispatch status must be unchanged after rejected update"
    );

    // Verify card state is also unchanged (pipeline invariant)
    let card_status: String =
        sqlx::query_scalar("SELECT status FROM kanban_cards WHERE id = 'card-265'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        card_status, "in_progress",
        "card status must be unchanged after rejected dispatch update"
    );

    pool.close().await;
    pg_db.drop().await;
}

/// #265: Valid statuses like "cancelled" must still work through the generic path.
#[tokio::test]
#[ignore = "obsolete SQLite dispatch route fixture; PR #868 runtime path is PostgreSQL-only"]
async fn patch_dispatch_accepts_valid_status_cancelled() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_test_agents(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at)
             VALUES ('card-265v', 'Valid Card', 'in_progress', 'ch-td', 'dispatch-265v', datetime('now'), datetime('now'))",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
             VALUES ('dispatch-265v', 'card-265v', 'ch-td', 'rework', 'dispatched', 'Rework task', datetime('now'), datetime('now'))",
            [],
        ).unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/dispatches/dispatch-265v")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"status":"cancelled"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "valid status 'cancelled' must be accepted"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["dispatch"]["status"], "cancelled");
}
