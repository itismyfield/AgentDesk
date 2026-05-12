//! Domain-split routes tests — `kanban` group.
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

#[tokio::test]
async fn kanban_create_card() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"title":"Test Card","priority":"high"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["card"]["title"], "Test Card");
    assert_eq!(json["card"]["priority"], "high");
    assert_eq!(json["card"]["status"], "backlog");
    assert!(json["card"]["id"].as_str().unwrap().len() > 10); // UUID
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kanban_create_card_pg_only_without_sqlite_mirror() {
    crate::pipeline::ensure_loaded();

    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

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
                .uri("/kanban-cards")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"title":"Test Card PG","priority":"high","repo_id":"repo-pg"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    if status != StatusCode::CREATED {
        panic!(
            "kanban_create_card_pg_only_without_sqlite_mirror status={} body={}",
            status,
            String::from_utf8_lossy(&body)
        );
    }

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let card_id = json["card"]["id"].as_str().unwrap();
    assert_eq!(json["card"]["title"], "Test Card PG");
    assert_eq!(json["card"]["priority"], "high");
    assert_eq!(json["card"]["status"], "backlog");
    assert_eq!(json["card"]["repo_id"], "repo-pg");

    let sqlite_card_count: i64 = db
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        sqlite_card_count, 0,
        "test must not rely on SQLite mirror state"
    );

    let row = sqlx::query(
        "SELECT title, status, repo_id, priority
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(row.try_get::<String, _>("title").unwrap(), "Test Card PG");
    assert_eq!(row.try_get::<String, _>("status").unwrap(), "backlog");
    assert_eq!(
        row.try_get::<Option<String>, _>("repo_id").unwrap(),
        Some("repo-pg".to_string())
    );
    assert_eq!(row.try_get::<String, _>("priority").unwrap(), "high");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn kanban_list_cards_empty() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/kanban-cards")
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
    assert!(json["cards"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn kanban_list_cards_with_filter() {
    let db = test_db();
    let engine = test_engine(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at) VALUES ('c1', 'Card1', 'backlog', 'medium', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
        conn.execute(
                "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at) VALUES ('c2', 'Card2', 'ready', 'high', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
    }

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/kanban-cards?status=ready")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let cards = json["cards"].as_array().unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0]["id"], "c2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kanban_list_cards_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, NOW(), NOW()
         )",
    )
    .bind("c-pg-list")
    .bind("Card PG List")
    .bind("ready")
    .bind("high")
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
                .uri("/kanban-cards?status=ready")
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
        "kanban_list_cards_pg_only_without_sqlite_mirror status={} body={}",
        status,
        body_text
    );

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let cards = json["cards"].as_array().unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0]["id"], "c-pg-list");

    let sqlite_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM kanban_cards WHERE id = 'c-pg-list'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sqlite_count, 0, "sqlite mirror should stay empty");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn link_dispatch_thread_pg_preserves_concurrent_channel_thread_updates() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id)
         VALUES ($1, $2, '111')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind("agent-thread-race")
    .bind("Agent Thread Race")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, channel_thread_map, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, '{}'::jsonb, NOW(), NOW()
         )",
    )
    .bind("card-thread-race")
    .bind("Thread Race")
    .bind("in_progress")
    .bind("high")
    .execute(&pg_pool)
    .await
    .unwrap();
    for idx in 0..12 {
        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
             ) VALUES (
                $1, $2, $3, $4, $5, $6, NOW(), NOW()
             )",
        )
        .bind(format!("dispatch-thread-race-{idx}"))
        .bind("card-thread-race")
        .bind("agent-thread-race")
        .bind("implementation")
        .bind("dispatched")
        .bind(format!("Thread race dispatch {idx}"))
        .execute(&pg_pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "CREATE OR REPLACE FUNCTION test_sleep_card_thread_map_update()
         RETURNS trigger
         LANGUAGE plpgsql
         AS $$
         BEGIN
             PERFORM pg_sleep(0.03);
             RETURN NEW;
         END;
         $$;
        ",
    )
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER test_sleep_card_thread_map_update
         BEFORE UPDATE OF channel_thread_map ON kanban_cards
         FOR EACH ROW
         EXECUTE FUNCTION test_sleep_card_thread_map_update();",
    )
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let responses = futures::future::join_all((0..12).map(|idx| {
        let app = app.clone();
        async move {
            let response = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/internal/link-dispatch-thread")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            json!({
                                "dispatch_id": format!("dispatch-thread-race-{idx}"),
                                "thread_id": format!("thread-race-{idx}"),
                                "channel_id": format!("{}", 9000 + idx),
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        }
    }))
    .await;
    assert_eq!(responses.len(), 12);

    let channel_thread_map: serde_json::Value = sqlx::query_scalar(
        "SELECT channel_thread_map
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind("card-thread-race")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    let map = channel_thread_map
        .as_object()
        .expect("channel_thread_map must be an object");
    assert_eq!(
        map.len(),
        12,
        "concurrent channel thread updates must merge instead of last-writer-wins"
    );
    for idx in 0..12 {
        assert_eq!(
            map.get(&(9000 + idx).to_string()),
            Some(&json!(format!("thread-race-{idx}")))
        );
    }

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn kanban_list_cards_filters_to_registered_repos_unless_repo_id_is_explicit() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_repo(&db, "repo-registered");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, repo_id, title, status, priority, created_at, updated_at
             ) VALUES (
                'c-registered', 'repo-registered', 'Registered Card', 'ready', 'medium',
                datetime('now'), datetime('now')
             )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, repo_id, title, status, priority, created_at, updated_at
             ) VALUES (
                'c-unregistered', 'repo-unregistered', 'Unregistered Card', 'ready', 'medium',
                datetime('now'), datetime('now')
             )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db, engine, None);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/kanban-cards")
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
    let cards = json["cards"].as_array().unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0]["id"], "c-registered");

    let response = app
        .oneshot(
            Request::builder()
                .uri("/kanban-cards?repo_id=repo-unregistered")
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
    let cards = json["cards"].as_array().unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0]["id"], "c-unregistered");
}

#[tokio::test]
async fn kanban_get_card() {
    let db = test_db();
    let engine = test_engine(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at) VALUES ('c1', 'Card1', 'backlog', 'medium', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
    }

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/kanban-cards/c1")
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
    assert_eq!(json["card"]["id"], "c1");
    assert_eq!(json["card"]["title"], "Card1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kanban_get_card_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at)
         VALUES ($1, $2, $3, $4, NOW(), NOW())",
    )
    .bind("c1-pg-get")
    .bind("Card1 PG")
    .bind("backlog")
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
        .oneshot(
            Request::builder()
                .uri("/kanban-cards/c1-pg-get")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    if status != StatusCode::OK {
        panic!(
            "kanban_get_card_pg_only_without_sqlite_mirror status={} body={}",
            status,
            String::from_utf8_lossy(&body)
        );
    }

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["card"]["id"], "c1-pg-get");
    assert_eq!(json["card"]["title"], "Card1 PG");

    let sqlite_card_count: i64 = db
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM kanban_cards WHERE id = 'c1-pg-get'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        sqlite_card_count, 0,
        "test must not rely on SQLite mirror state"
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn kanban_list_and_get_include_latest_dispatch_result_summary() {
    let db = test_db();
    seed_test_agents(&db);
    let engine = test_engine(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at)
             VALUES ('card-summary', 'Card Summary', 'in_progress', 'medium', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at
             ) VALUES (
                'dispatch-rework-summary', 'card-summary', 'agent-1', 'rework', 'pending',
                'Rework requested', ?1, datetime('now'), datetime('now')
             )",
            [json!({
                "pm_decision": "rework",
                "comment": "Handle the race condition"
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "UPDATE kanban_cards SET latest_dispatch_id = 'dispatch-rework-summary' WHERE id = 'card-summary'",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine.clone(), None);
    let list_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/kanban-cards")
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
    let listed_card = list_json["cards"]
        .as_array()
        .unwrap()
        .iter()
        .find(|card| card["id"] == "card-summary")
        .expect("card-summary must be present in kanban list");
    assert_eq!(
        listed_card["latest_dispatch_result_summary"],
        "PM requested rework: Handle the race condition"
    );

    let get_response = app
        .oneshot(
            Request::builder()
                .uri("/kanban-cards/card-summary")
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
        get_json["card"]["latest_dispatch_result_summary"],
        "PM requested rework: Handle the race condition"
    );
    assert_eq!(get_json["card"]["latest_dispatch_type"], "rework");
}

#[tokio::test]
async fn kanban_get_card_not_found() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/kanban-cards/nonexistent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn kanban_update_card_status() {
    let db = test_db();
    let engine = test_engine(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at) VALUES ('c1', 'Card1', 'backlog', 'medium', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
    }

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/kanban-cards/c1")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"status":"ready"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["card"]["status"], "ready");
}

#[tokio::test]
async fn kanban_update_card_rejects_manual_non_backlog_transition_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at)
         VALUES ('c1', 'Card1', 'ready', 'medium', NOW(), NOW())",
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
                .uri("/kanban-cards/c1")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"status":"in_progress"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("backlog"),
        "error must explain the restricted manual transition rule"
    );
    let error = json["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("/api/kanban-cards/{id}/transition"),
        "error must point callers at the administrative transition endpoint: {error}"
    );
    assert!(
        error.contains("/api/kanban-cards/{id}/rereview"),
        "error must point callers at the rereview endpoint for review reruns: {error}"
    );

    let status: String = sqlx::query_scalar("SELECT status FROM kanban_cards WHERE id = 'c1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "ready");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn kanban_update_card_rejects_mixed_status_and_metadata_without_transition_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, priority, metadata, created_at, updated_at)
         VALUES ('c-mixed-status-metadata', 'Mixed update', 'backlog', 'medium', $1::jsonb, NOW(), NOW())",
    )
    .bind(r#"{"existing":true}"#)
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
                .method("PATCH")
                .uri("/kanban-cards/c-mixed-status-metadata")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"status":"ready","metadata_json":"not-json"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let error = json["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("cannot combine status changes with metadata or other field updates"),
        "error must explain mixed status/metadata updates are split: {error}"
    );

    let row = sqlx::query(
        "SELECT status, metadata::text AS metadata
         FROM kanban_cards
         WHERE id = 'c-mixed-status-metadata'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let status: String = row.try_get("status").unwrap();
    let metadata_raw: Option<String> = row.try_get("metadata").unwrap();
    let metadata: serde_json::Value =
        serde_json::from_str(metadata_raw.as_deref().expect("metadata should remain")).unwrap();
    assert_eq!(status, "backlog");
    assert_eq!(metadata, json!({"existing": true}));

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
#[ignore = "obsolete SQLite kanban backlog cleanup fixture; route is PG-only after #843/#868"]
async fn kanban_update_card_to_backlog_cleans_up_dispatches_auto_queue_and_turns() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-manual-backlog");
    seed_repo(&db, "test/repo");
    ensure_auto_queue_tables(&db);

    let tmux_name = format!("manual-backlog-{}", uuid::Uuid::new_v4().simple());
    let session_key = format!("host:{tmux_name}");
    let tmux_created = if crate::services::platform::tmux::is_available() {
        let output = crate::services::platform::tmux::create_session(&tmux_name, None, "sleep 120")
            .expect("tmux session should spawn");
        assert!(
            output.status.success(),
            "tmux session should start for turn cancellation test"
        );
        true
    } else {
        false
    };

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, latest_dispatch_id, review_status, review_round, review_notes,
                suggestion_pending_at, review_entered_at, awaiting_dod_at,
                created_at, updated_at, started_at
            ) VALUES (
                'card-manual-backlog', 'Manual Backlog Cleanup', 'in_progress', 'medium', 'agent-manual-backlog', 'test-repo',
                541, 'dispatch-manual-backlog', 'reviewing', 3, 'stale review state',
                datetime('now', '-12 minutes'), datetime('now', '-11 minutes'), datetime('now', '-10 minutes'),
                datetime('now', '-20 minutes'), datetime('now', '-20 minutes'), datetime('now', '-20 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-manual-backlog', 'card-manual-backlog', 'agent-manual-backlog', 'implementation', 'pending',
                'live impl', datetime('now', '-10 minutes'), datetime('now', '-10 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, agent_id, provider, status, active_dispatch_id, last_heartbeat, created_at
            ) VALUES (
                ?1, 'agent-manual-backlog', 'codex', 'turn_active', 'dispatch-manual-backlog',
                datetime('now', '-9 minutes'), datetime('now', '-9 minutes')
            )",
            sqlite_params![session_key],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-manual-backlog', 'test-repo', 'agent-manual-backlog', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-manual-backlog-pending', 'test-repo', 'agent-manual-backlog', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at
            ) VALUES (
                'entry-manual-backlog-dispatched', 'run-manual-backlog', 'card-manual-backlog', 'agent-manual-backlog',
                'dispatched', 'dispatch-manual-backlog', datetime('now', '-10 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-manual-backlog-2', 'test-repo', 'agent-manual-backlog', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status
            ) VALUES (
                'entry-manual-backlog-pending', 'run-manual-backlog-pending', 'card-manual-backlog', 'agent-manual-backlog', 'pending'
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO card_review_state (
                card_id, state, pending_dispatch_id, review_round, last_verdict, last_decision,
                approach_change_round, session_reset_round, review_entered_at, updated_at
            ) VALUES (
                'card-manual-backlog', 'suggestion_pending', 'dispatch-manual-backlog', 3, 'pass', 'approved',
                2, 3, datetime('now', '-11 minutes'), datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/kanban-cards/card-manual-backlog")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"status":"backlog"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["card"]["status"], "backlog");

    let conn = db.lock().unwrap();
    let (
        card_status,
        latest_dispatch_id,
        review_status,
        review_round,
        review_notes,
        suggestion_pending_at,
        review_entered_at,
        awaiting_dod_at,
    ): (
        String,
        Option<String>,
        Option<String>,
        i32,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT status, latest_dispatch_id, review_status, review_round, review_notes,
                    suggestion_pending_at, review_entered_at, awaiting_dod_at
             FROM kanban_cards WHERE id = 'card-manual-backlog'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(card_status, "backlog");
    assert!(latest_dispatch_id.is_none());
    assert!(review_status.is_none());
    assert_eq!(review_round, 0);
    assert!(review_notes.is_none());
    assert!(suggestion_pending_at.is_none());
    assert!(review_entered_at.is_none());
    assert!(awaiting_dod_at.is_none());

    let dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-manual-backlog'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dispatch_status, "cancelled");

    // #1235: stable-key (entry id) lookups instead of Vec equality.
    let entry_rows: std::collections::BTreeMap<String, (String, Option<String>)> = conn
        .prepare(
            "SELECT id, status, dispatch_id FROM auto_queue_entries
             WHERE kanban_card_id = 'card-manual-backlog'",
        )
        .unwrap()
        .query_map([], |row| {
            let id: String = row.get(0)?;
            let status: String = row.get(1)?;
            let dispatch_id: Option<String> = row.get(2)?;
            Ok((id, (status, dispatch_id)))
        })
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    let mb_dispatched = entry_rows
        .get("entry-manual-backlog-dispatched")
        .expect("seeded dispatched entry must still exist");
    let mb_pending = entry_rows
        .get("entry-manual-backlog-pending")
        .expect("seeded pending entry must still exist");
    assert_eq!(mb_dispatched.0, "skipped");
    assert!(mb_dispatched.1.is_none());
    assert_eq!(mb_pending.0, "skipped");
    assert!(mb_pending.1.is_none());

    let (session_status, active_dispatch_id): (String, Option<String>) = conn
        .query_row(
            "SELECT status, active_dispatch_id
             FROM sessions
             WHERE session_key = ?1",
            sqlite_params![session_key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(session_status, "disconnected");
    assert!(active_dispatch_id.is_none());

    let (
        review_state_round,
        review_state_status,
        review_state_pending_dispatch,
        review_state_verdict,
        review_state_decision,
    ): (i64, String, Option<String>, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT review_round, state, pending_dispatch_id, last_verdict, last_decision
             FROM card_review_state WHERE card_id = 'card-manual-backlog'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(review_state_round, 0);
    assert_eq!(review_state_status, "idle");
    assert!(review_state_pending_dispatch.is_none());
    assert!(review_state_verdict.is_none());
    assert!(review_state_decision.is_none());

    drop(conn);

    if tmux_created {
        assert!(
            !crate::services::platform::tmux::has_session(&tmux_name),
            "manual backlog revert must kill the live tmux turn"
        );
    }
}

#[tokio::test]
async fn kanban_update_card_not_found() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/kanban-cards/nonexistent")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"status":"ready"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kanban_get_card_review_state_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at)
         VALUES ($1, $2, $3, $4, NOW(), NOW())",
    )
    .bind("card-review-state-pg")
    .bind("Review State PG")
    .bind("review")
    .bind("medium")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO card_review_state (
            card_id, review_round, state, pending_dispatch_id, last_verdict, last_decision,
            decided_by, decided_at, review_entered_at, updated_at
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, NOW(), NOW(), NOW())",
    )
    .bind("card-review-state-pg")
    .bind(2_i64)
    .bind("reviewing")
    .bind("dispatch-review-pg")
    .bind("accept")
    .bind("ship")
    .bind("agent-reviewer")
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
                .uri("/kanban-cards/card-review-state-pg/review-state")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    if status != StatusCode::OK {
        panic!(
            "kanban_get_card_review_state_pg_only_without_sqlite_mirror status={} body={}",
            status,
            String::from_utf8_lossy(&body)
        );
    }

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["card_id"], "card-review-state-pg");
    assert_eq!(json["review_round"], 2);
    assert_eq!(json["state"], "reviewing");
    assert_eq!(json["pending_dispatch_id"], "dispatch-review-pg");
    assert_eq!(json["last_verdict"], "accept");
    assert_eq!(json["last_decision"], "ship");
    assert_eq!(json["decided_by"], "agent-reviewer");

    let sqlite_state_count: i64 = db
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM card_review_state WHERE card_id = 'card-review-state-pg'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        sqlite_state_count, 0,
        "test must not rely on SQLite mirror state"
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kanban_list_card_reviews_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at)
         VALUES ($1, $2, $3, $4, NOW(), NOW())",
    )
    .bind("card-reviews-pg")
    .bind("Reviews PG")
    .bind("review")
    .bind("medium")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO review_decisions (
            kanban_card_id, dispatch_id, item_index, decision, decided_at
         ) VALUES ($1, $2, $3, $4, NOW()), ($1, $5, $6, $7, NOW())",
    )
    .bind("card-reviews-pg")
    .bind("dispatch-review-1")
    .bind(0_i64)
    .bind("accept")
    .bind("dispatch-review-2")
    .bind(1_i64)
    .bind("rework")
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
                .uri("/kanban-cards/card-reviews-pg/reviews")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    if status != StatusCode::OK {
        panic!(
            "kanban_list_card_reviews_pg_only_without_sqlite_mirror status={} body={}",
            status,
            String::from_utf8_lossy(&body)
        );
    }

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let reviews = json["reviews"].as_array().unwrap();
    assert_eq!(reviews.len(), 2);
    assert_eq!(reviews[0]["kanban_card_id"], "card-reviews-pg");
    assert_eq!(reviews[0]["dispatch_id"], "dispatch-review-1");
    assert_eq!(reviews[0]["item_index"], 0);
    assert_eq!(reviews[0]["decision"], "accept");
    assert_eq!(reviews[1]["dispatch_id"], "dispatch-review-2");
    assert_eq!(reviews[1]["item_index"], 1);
    assert_eq!(reviews[1]["decision"], "rework");

    let sqlite_review_count: i64 = db
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM review_decisions WHERE kanban_card_id = 'card-reviews-pg'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        sqlite_review_count, 0,
        "test must not rely on SQLite mirror state"
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn kanban_assign_card() {
    let db = test_db();
    let engine = test_engine(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO agents (id, name, provider, status, xp) VALUES ('ch-td', 'Agent TD', 'claude', 'idle', 0)",
                [],
            ).unwrap();
        conn.execute(
                "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at) VALUES ('c1', 'Card1', 'backlog', 'medium', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
    }

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/c1/assign")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"agent_id":"ch-td"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // #255: assign walks through free transitions to the dispatchable state (requested)
    assert_eq!(json["card"]["status"], "requested");
    assert_eq!(json["card"]["assigned_agent_id"], "ch-td");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kanban_assign_card_pg_only_without_sqlite_mirror() {
    crate::pipeline::ensure_loaded();

    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id, discord_channel_alt)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind("ch-td")
    .bind("Agent TD")
    .bind("claude")
    .bind("111")
    .bind("222")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at)
         VALUES ($1, $2, $3, $4, NOW(), NOW())",
    )
    .bind("c1-pg")
    .bind("Card1 PG")
    .bind("backlog")
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
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/c1-pg/assign")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"agent_id":"ch-td"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    if status != StatusCode::OK {
        panic!(
            "kanban_assign_card_pg_only_without_sqlite_mirror status={} body={}",
            status,
            String::from_utf8_lossy(&body)
        );
    }
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["card"]["status"], "requested");
    assert_eq!(json["card"]["assigned_agent_id"], "ch-td");
    assert_eq!(json["assignment"]["ok"], true);
    assert_eq!(json["assignment"]["agent_id"], "ch-td");
    assert_eq!(json["transition"]["attempted"], true);
    assert_eq!(json["transition"]["ok"], true);
    assert_eq!(json["transition"]["from"], "backlog");
    assert_eq!(json["transition"]["to"], "requested");
    assert_eq!(json["transition"]["target"], "requested");
    assert_eq!(json["transition"]["target_status"], "requested");
    assert_eq!(json["transition"]["next_action"], "none_required");
    assert!(json["transition"]["error"].is_null());

    let sqlite_card_count: i64 = db
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM kanban_cards WHERE id = 'c1-pg'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        sqlite_card_count, 0,
        "test must not rely on SQLite mirror state"
    );

    let row = sqlx::query(
        "SELECT status, assigned_agent_id
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind("c1-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(row.try_get::<String, _>("status").unwrap(), "requested");
    assert_eq!(
        row.try_get::<Option<String>, _>("assigned_agent_id")
            .unwrap(),
        Some("ch-td".to_string())
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kanban_assign_card_reports_transition_failure_in_response() {
    crate::pipeline::ensure_loaded();

    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id, discord_channel_alt)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind("ch-td")
    .bind("Agent TD")
    .bind("claude")
    .bind("111")
    .bind("222")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at)
         VALUES ($1, $2, $3, $4, NOW(), NOW())",
    )
    .bind("c1-pg-transition-fail")
    .bind("Card with terminal status")
    .bind("done")
    .bind("medium")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/c1-pg-transition-fail/assign")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"agent_id":"ch-td"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["card"]["status"], "done");
    assert_eq!(json["card"]["assigned_agent_id"], "ch-td");
    assert_eq!(json["assignment"]["ok"], true);
    assert_eq!(json["transition"]["attempted"], true);
    assert_eq!(json["transition"]["ok"], false);
    assert_eq!(json["transition"]["from"], "done");
    assert_eq!(json["transition"]["to"], "done");
    assert_eq!(json["transition"]["target"], "requested");
    assert_eq!(json["transition"]["target_status"], "requested");
    assert_eq!(
        json["transition"]["next_action"],
        "inspect_transition_error"
    );
    assert_eq!(json["transition"]["steps"], json!(["requested"]));
    assert_eq!(json["transition"]["failed_step"], "requested");
    assert!(
        json["transition"]["error"]
            .as_str()
            .is_some_and(|message| !message.is_empty()),
        "transition failure must be visible in the response body"
    );

    let row = sqlx::query(
        "SELECT status, assigned_agent_id
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind("c1-pg-transition-fail")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(row.try_get::<String, _>("status").unwrap(), "done");
    assert_eq!(
        row.try_get::<Option<String>, _>("assigned_agent_id")
            .unwrap(),
        Some("ch-td".to_string())
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kanban_assign_issue_pg_upserts_without_duplicates() {
    crate::pipeline::ensure_loaded();

    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO agents (id, name) VALUES ($1, $2)")
        .bind("agent-issue")
        .bind("Agent Issue")
        .execute(&pg_pool)
        .await
        .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );

    let request_body = json!({
        "github_repo": "owner/issue-sync",
        "github_issue_number": 77,
        "github_issue_url": "https://github.com/owner/issue-sync/issues/77",
        "title": "Issue sync via assign route",
        "description": "Assign route must reuse the same card for the same issue.",
        "assignee_agent_id": "agent-issue"
    })
    .to_string();

    let first_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/assign-issue")
                .header("content-type", "application/json")
                .body(Body::from(request_body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first_response.status(), StatusCode::CREATED);
    let first_body = axum::body::to_bytes(first_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let first_json: serde_json::Value = serde_json::from_slice(&first_body).unwrap();
    assert_eq!(first_json["deduplicated"], false);
    assert_eq!(first_json["card"]["assigned_agent_id"], "agent-issue");
    assert_eq!(first_json["card"]["github_issue_number"], 77);
    assert_eq!(first_json["card"]["status"], "requested");
    assert_eq!(first_json["assignment"]["ok"], true);
    assert_eq!(first_json["assignment"]["agent_id"], "agent-issue");
    assert_eq!(first_json["transition"]["attempted"], true);
    assert_eq!(first_json["transition"]["ok"], true);
    let first_card_id = first_json["card"]["id"].as_str().unwrap().to_string();

    let second_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/assign-issue")
                .header("content-type", "application/json")
                .body(Body::from(request_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second_response.status(), StatusCode::OK);
    let second_body = axum::body::to_bytes(second_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let second_json: serde_json::Value = serde_json::from_slice(&second_body).unwrap();
    assert_eq!(second_json["deduplicated"], true);
    assert_eq!(second_json["card"]["id"], first_card_id);
    assert_eq!(second_json["assignment"]["ok"], true);
    assert_eq!(second_json["transition"]["attempted"], false);
    assert_eq!(second_json["transition"]["ok"], true);

    let row = sqlx::query(
        "SELECT COUNT(*)::BIGINT AS card_count, MIN(id) AS card_id, MIN(status) AS status
         FROM kanban_cards
         WHERE repo_id = $1 AND github_issue_number = $2",
    )
    .bind("owner/issue-sync")
    .bind(77_i64)
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(row.try_get::<i64, _>("card_count").unwrap(), 1);
    assert_eq!(
        row.try_get::<Option<String>, _>("card_id").unwrap(),
        Some(first_card_id)
    );
    assert_eq!(
        row.try_get::<Option<String>, _>("status").unwrap(),
        Some("requested".to_string())
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kanban_assign_issue_reports_transition_failure_in_response() {
    crate::pipeline::ensure_loaded();

    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO agents (id, name) VALUES ($1, $2)")
        .bind("agent-issue-fail")
        .bind("Agent Issue Fail")
        .execute(&pg_pool)
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO kanban_cards (
             id, repo_id, title, status, priority, github_issue_url,
             github_issue_number, created_at, updated_at
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, NOW(), NOW())",
    )
    .bind("c-issue-transition-fail")
    .bind("owner/issue-sync")
    .bind("Terminal issue card")
    .bind("done")
    .bind("medium")
    .bind("https://github.com/owner/issue-sync/issues/78")
    .bind(78_i64)
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/assign-issue")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "github_repo": "owner/issue-sync",
                        "github_issue_number": 78,
                        "github_issue_url": "https://github.com/owner/issue-sync/issues/78",
                        "title": "Terminal issue card updated by assign",
                        "description": "Assignment can succeed even when transition is blocked.",
                        "assignee_agent_id": "agent-issue-fail"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["deduplicated"], true);
    assert_eq!(json["card"]["id"], "c-issue-transition-fail");
    assert_eq!(json["card"]["status"], "done");
    assert_eq!(json["card"]["assigned_agent_id"], "agent-issue-fail");
    assert_eq!(json["assignment"]["ok"], true);
    assert_eq!(json["assignment"]["agent_id"], "agent-issue-fail");
    assert_eq!(json["transition"]["attempted"], true);
    assert_eq!(json["transition"]["ok"], false);
    assert_eq!(json["transition"]["from"], "done");
    assert_eq!(json["transition"]["to"], "done");
    assert_eq!(json["transition"]["target"], "requested");
    assert_eq!(json["transition"]["target_status"], "requested");
    assert_eq!(
        json["transition"]["next_action"],
        "inspect_transition_error"
    );
    assert_eq!(json["transition"]["steps"], json!(["requested"]));
    assert_eq!(json["transition"]["failed_step"], "requested");
    assert!(
        json["transition"]["error"]
            .as_str()
            .is_some_and(|message| !message.is_empty()),
        "assign-issue transition failure must be visible in the response body"
    );

    let row = sqlx::query(
        "SELECT status, assigned_agent_id
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind("c-issue-transition-fail")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(row.try_get::<String, _>("status").unwrap(), "done");
    assert_eq!(
        row.try_get::<Option<String>, _>("assigned_agent_id")
            .unwrap(),
        Some("agent-issue-fail".to_string())
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn kanban_assign_card_not_found() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/nonexistent/assign")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"agent_id":"ch-td"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kanban_delete_card_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at)
         VALUES ($1, $2, $3, $4, NOW(), NOW())",
    )
    .bind("c1-pg-delete")
    .bind("Delete PG")
    .bind("backlog")
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
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/kanban-cards/c1-pg-delete")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    if status != StatusCode::OK {
        panic!(
            "kanban_delete_card_pg_only_without_sqlite_mirror status={} body={}",
            status,
            String::from_utf8_lossy(&body)
        );
    }

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);

    let sqlite_card_count: i64 = db
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM kanban_cards WHERE id = 'c1-pg-delete'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        sqlite_card_count, 0,
        "test must not rely on SQLite mirror state"
    );

    let pg_card_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM kanban_cards WHERE id = $1")
            .bind("c1-pg-delete")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(pg_card_count, 0);

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kanban_terminal_status_fires_hook() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
            dir.path().join("test-hooks.js"),
            r#"
            var p = {
                name: "test-hooks",
                priority: 1,
                onCardTransition: function(payload) {
                    agentdesk.db.execute(
                        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('transition', '" + payload.from + "->" + payload.to + "')",
                        []
                    );
                },
                onCardTerminal: function(payload) {
                    agentdesk.db.execute(
                        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('terminal', '" + payload.card_id + ":" + payload.status + "')",
                        []
                    );
                }
            };
            agentdesk.registerPolicy(p);
            "#,
        ).unwrap();

    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let config = crate::config::Config {
        policies: crate::config::PoliciesConfig {
            dir: dir.path().to_path_buf(),
            hot_reload: false,
        },
        ..crate::config::Config::default()
    };
    let engine = PolicyEngine::new_with_pg(&config, Some(pg_pool.clone())).unwrap();

    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at)
         VALUES ('c1', 'Card1', 'requested', 'medium', NOW(), NOW())",
    )
    .execute(&pg_pool)
    .await
    .unwrap();

    // Use force transition: requested → done (no rule, force bypasses)
    let result = crate::kanban::transition_status_with_opts_pg_only(
        &pg_pool,
        &engine,
        "c1",
        "done",
        "pmd",
        crate::engine::transition::ForceIntent::OperatorOverride,
    )
    .await;
    assert!(
        result.is_ok(),
        "force transition should succeed: {:?}",
        result
    );

    let transition: String = sqlx::query_scalar("SELECT value FROM kv_meta WHERE key = $1")
        .bind("transition")
        .fetch_one(&pg_pool)
        .await
        .unwrap();
    assert_eq!(transition, "requested->done");

    let terminal: String = sqlx::query_scalar("SELECT value FROM kv_meta WHERE key = $1")
        .bind("terminal")
        .fetch_one(&pg_pool)
        .await
        .unwrap();
    assert_eq!(terminal, "c1:done");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn kanban_repos_pg_create_update_delete_round_trip() {
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );

    let create_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-repos")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"repo":"itismyfield/AgentDesk"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_response.status(), StatusCode::CREATED);

    let patch_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/kanban-repos/itismyfield/AgentDesk")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"default_agent_id":"pg-agent"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(patch_response.status(), StatusCode::OK);

    let list_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/kanban-repos")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let list_body = axum::body::to_bytes(list_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let list_json: serde_json::Value = serde_json::from_slice(&list_body).unwrap();
    assert_eq!(list_json["repos"][0]["default_agent_id"], "pg-agent");

    let delete_response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/kanban-repos/itismyfield/AgentDesk")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(delete_response.status(), StatusCode::OK);

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn force_transition_succeeds_with_correct_channel_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    seed_card_with_status_pg(&pool, "card-ft3", "requested").await;

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
                .uri("/kanban-cards/card-ft3/transition")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(r#"{"status":"done"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["forced"], true);

    pool.close().await;
    pg_db.drop().await;
}

/// #1442 — `/redispatch` response must surface `new_dispatch_id`,
/// `cancelled_dispatch_id`, and `next_action` so callers do not chain
/// `/transition` or `/queue/generate` and create duplicates.
#[tokio::test]
async fn redispatch_response_includes_dispatch_ids_and_next_action_pg_1442() {
    let (_repo, _repo_guard) = setup_test_repo();
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pool.clone());
    seed_repo_pg(&pool, "test-repo").await;
    seed_agent_pg(&pool, "agent-redispatch-1442").await;

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, repo_id,
            github_issue_number, latest_dispatch_id, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, NOW(), NOW()
         )",
    )
    .bind("card-redispatch-1442")
    .bind("Redispatch 1442")
    .bind("in_progress")
    .bind("medium")
    .bind("agent-redispatch-1442")
    .bind("test-repo")
    .bind(1442_i64)
    .bind("dispatch-redispatch-1442-old")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, NOW() - INTERVAL '5 minutes', NOW() - INTERVAL '5 minutes'
         )",
    )
    .bind("dispatch-redispatch-1442-old")
    .bind("card-redispatch-1442")
    .bind("agent-redispatch-1442")
    .bind("implementation")
    .bind("dispatched")
    .bind("[Impl] Issue #1442")
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
                .uri("/kanban-cards/card-redispatch-1442/redispatch")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"reason":"test #1442"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body).to_string();
    assert_eq!(
        status,
        StatusCode::OK,
        "redispatch must succeed; got {body_text}"
    );
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json.get("card").is_some(),
        "response must keep existing 'card' field for backward compat: {body_text}"
    );
    assert_eq!(
        json["next_action"], "none_required",
        "happy-path redispatch must report next_action=none_required: {body_text}"
    );
    assert_eq!(
        json["cancelled_dispatch_id"], "dispatch-redispatch-1442-old",
        "redispatch must echo the cancelled dispatch id: {body_text}"
    );
    let new_dispatch_id = json["new_dispatch_id"]
        .as_str()
        .expect("new_dispatch_id must be a string when create_dispatch succeeds");
    assert!(
        !new_dispatch_id.is_empty(),
        "new_dispatch_id must not be empty when a dispatch was created"
    );
    assert_ne!(
        new_dispatch_id, "dispatch-redispatch-1442-old",
        "new_dispatch_id must be a brand-new UUID, not the cancelled one"
    );

    pool.close().await;
    pg_db.drop().await;
}

/// #1733 — `/retry` response must surface dispatch ids and `next_action` with
/// field presence even when the previous failed dispatch did not need cancel.
#[tokio::test]
async fn retry_response_includes_dispatch_ids_and_next_action_pg_1733() {
    let (_repo, _repo_guard) = setup_test_repo();
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pool.clone());
    seed_repo_pg(&pool, "test-repo").await;
    seed_agent_pg(&pool, "agent-retry-1733").await;

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, repo_id,
            github_issue_number, latest_dispatch_id, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, NOW(), NOW()
         )",
    )
    .bind("card-retry-1733")
    .bind("Retry 1733")
    .bind("requested")
    .bind("medium")
    .bind("agent-retry-1733")
    .bind("test-repo")
    .bind(1733_i64)
    .bind("dispatch-retry-1733-old")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, NOW() - INTERVAL '5 minutes', NOW() - INTERVAL '5 minutes'
         )",
    )
    .bind("dispatch-retry-1733-old")
    .bind("card-retry-1733")
    .bind("agent-retry-1733")
    .bind("implementation")
    .bind("failed")
    .bind("[Impl] Issue #1733")
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
                .uri("/kanban-cards/card-retry-1733/retry")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"request_now":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body).to_string();
    assert_eq!(
        status,
        StatusCode::OK,
        "retry must succeed; got {body_text}"
    );
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json.get("card").is_some(),
        "response must keep existing 'card' field for backward compat: {body_text}"
    );
    assert_eq!(
        json["next_action"], "none_required",
        "happy-path retry must report next_action=none_required: {body_text}"
    );
    assert!(
        json.get("cancelled_dispatch_id").is_some(),
        "cancelled_dispatch_id key must be present even when null: {body_text}"
    );
    assert!(
        json["cancelled_dispatch_id"].is_null(),
        "failed old dispatch should not be reported as newly cancelled: {body_text}"
    );
    let new_dispatch_id = json["new_dispatch_id"]
        .as_str()
        .expect("new_dispatch_id must be a string when create_dispatch succeeds");
    assert!(
        !new_dispatch_id.is_empty(),
        "new_dispatch_id must not be empty when a dispatch was created"
    );
    assert_ne!(
        new_dispatch_id, "dispatch-retry-1733-old",
        "new_dispatch_id must be a brand-new UUID, not the failed one"
    );

    pool.close().await;
    pg_db.drop().await;
}

/// #1442 — `/transition` response must include `cancelled_dispatch_ids`
/// (array) and `next_action_hint` so callers can confirm the cleanup
/// outcome without chaining additional calls.
#[tokio::test]
async fn transition_response_includes_cancelled_ids_and_next_action_hint_pg_1442() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pool.clone());
    seed_repo_pg(&pool, "test-repo").await;
    seed_agent_pg(&pool, "agent-transition-1442").await;

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, repo_id,
            github_issue_number, latest_dispatch_id, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, NOW(), NOW()
         )",
    )
    .bind("card-transition-1442")
    .bind("Transition 1442")
    .bind("in_progress")
    .bind("medium")
    .bind("agent-transition-1442")
    .bind("test-repo")
    .bind(14422_i64)
    .bind("dispatch-transition-1442")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, NOW() - INTERVAL '5 minutes', NOW() - INTERVAL '5 minutes'
         )",
    )
    .bind("dispatch-transition-1442")
    .bind("card-transition-1442")
    .bind("agent-transition-1442")
    .bind("implementation")
    .bind("dispatched")
    .bind("[Impl] Issue #14422")
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
    // #1442 (codex P2): send x-channel-id so this test isn't 401-flaked when
    // it runs in parallel with the `kanban_manager_channel_id` PMD-channel
    // tests that mutate the global AGENTDESK_CONFIG.
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-transition-1442/transition")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(r#"{"status":"ready","cancel_dispatches":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body).to_string();
    assert_eq!(
        status,
        StatusCode::OK,
        "transition must succeed; got {body_text}"
    );
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["forced"], true);
    let ids = json["cancelled_dispatch_ids"]
        .as_array()
        .expect("cancelled_dispatch_ids must be an array");
    assert!(
        ids.iter()
            .any(|id| id.as_str() == Some("dispatch-transition-1442")),
        "cancelled_dispatch_ids must list the previously-active dispatch id: {body_text}"
    );
    let hint = json["next_action_hint"]
        .as_str()
        .expect("next_action_hint must be a string");
    assert!(
        !hint.is_empty(),
        "next_action_hint must not be empty: {body_text}"
    );
    // created_dispatch_id is allowed to be null when no new dispatch was kicked
    // off by hooks; we just assert the field is present.
    assert!(
        json.get("created_dispatch_id").is_some(),
        "response must include created_dispatch_id key (null is OK): {body_text}"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn transition_to_ready_with_active_dispatch_returns_409_pg_1444() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pool.clone());
    seed_repo_pg(&pool, "test-repo").await;
    seed_agent_pg(&pool, "agent-tx-1444").await;

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, repo_id,
            github_issue_number, latest_dispatch_id, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, NOW(), NOW()
         )",
    )
    .bind("card-tx-1444")
    .bind("Transition guard 1444")
    .bind("in_progress")
    .bind("medium")
    .bind("agent-tx-1444")
    .bind("test-repo")
    .bind(1444001_i64)
    .bind("dispatch-tx-1444-active")
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
    .bind("dispatch-tx-1444-active")
    .bind("card-tx-1444")
    .bind("agent-tx-1444")
    .bind("implementation")
    .bind("dispatched")
    .bind("[Impl] active 1444")
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
                .uri("/kanban-cards/card-tx-1444/transition")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(r#"{"status":"ready"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body).to_string();
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "transition to ready with active dispatch must 409: {body_text}"
    );
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["active_dispatch_id"], "dispatch-tx-1444-active",
        "409 body must echo the blocking dispatch id: {body_text}"
    );
    let hint = json["next_action_hint"]
        .as_str()
        .expect("next_action_hint must be a string");
    assert!(
        hint.contains("force=true"),
        "next_action_hint must point callers at force=true: {body_text}"
    );
    let error = json["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("force=true"),
        "error must mention force=true: {body_text}"
    );

    // Verify the active dispatch was NOT cancelled by the rejected call.
    let dispatch_status: String =
        sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
            .bind("dispatch-tx-1444-active")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        dispatch_status, "dispatched",
        "guarded transition must leave the live dispatch untouched"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn transition_to_ready_with_active_dispatch_force_true_proceeds_pg_1444() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pool.clone());
    seed_repo_pg(&pool, "test-repo").await;
    seed_agent_pg(&pool, "agent-tx-force-1444").await;

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, repo_id,
            github_issue_number, latest_dispatch_id, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, NOW(), NOW()
         )",
    )
    .bind("card-tx-force-1444")
    .bind("Force transition 1444")
    .bind("in_progress")
    .bind("medium")
    .bind("agent-tx-force-1444")
    .bind("test-repo")
    .bind(1444002_i64)
    .bind("dispatch-tx-force-1444-active")
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
    .bind("dispatch-tx-force-1444-active")
    .bind("card-tx-force-1444")
    .bind("agent-tx-force-1444")
    .bind("implementation")
    .bind("dispatched")
    .bind("[Impl] force 1444")
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
                .uri("/kanban-cards/card-tx-force-1444/transition")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(r#"{"status":"ready","force":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body).to_string();
    assert_eq!(
        status,
        StatusCode::OK,
        "force=true must bypass the #1444 guard and succeed: {body_text}"
    );
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["forced"], true);
    let cancelled_ids = json["cancelled_dispatch_ids"]
        .as_array()
        .expect("cancelled_dispatch_ids must be an array");
    assert!(
        cancelled_ids
            .iter()
            .any(|id| id.as_str() == Some("dispatch-tx-force-1444-active")),
        "cancelled_dispatch_ids must include the previously-active dispatch: {body_text}"
    );

    let dispatch_status: String =
        sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
            .bind("dispatch-tx-force-1444-active")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        dispatch_status, "cancelled",
        "force=true must cancel the previously-active dispatch"
    );

    pool.close().await;
    pg_db.drop().await;
}

/// #1444 codex iter-1 P2 regression: when a card is already in `ready` and
/// the caller retries the transition with `force=true` to clean up a still-
/// active dispatch, the FSM short-circuits with NoOp and the cleanup path
/// is bypassed. The route handler must explicitly cancel the active
/// dispatches in that case so the documented force-recovery actually
/// resolves the duplicate-dispatch incident.
#[tokio::test]
async fn transition_to_ready_force_true_cleans_up_active_dispatch_when_already_ready_pg_1444() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pool.clone());
    seed_repo_pg(&pool, "test-repo").await;
    seed_agent_pg(&pool, "agent-tx-noop-1444").await;

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, repo_id,
            github_issue_number, latest_dispatch_id, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, NOW(), NOW()
         )",
    )
    .bind("card-tx-noop-1444")
    .bind("Ready+active 1444 noop")
    .bind("ready")
    .bind("medium")
    .bind("agent-tx-noop-1444")
    .bind("test-repo")
    .bind(1444003_i64)
    .bind("dispatch-tx-noop-1444-active")
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
    .bind("dispatch-tx-noop-1444-active")
    .bind("card-tx-noop-1444")
    .bind("agent-tx-noop-1444")
    .bind("implementation")
    .bind("dispatched")
    .bind("[Impl] noop 1444")
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
                .uri("/kanban-cards/card-tx-noop-1444/transition")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(r#"{"status":"ready","force":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body).to_string();
    assert_eq!(
        status,
        StatusCode::OK,
        "force ready→ready must succeed: {body_text}"
    );
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let cancelled_ids = json["cancelled_dispatch_ids"]
        .as_array()
        .expect("cancelled_dispatch_ids must be an array");
    assert!(
        cancelled_ids
            .iter()
            .any(|id| id.as_str() == Some("dispatch-tx-noop-1444-active")),
        "force=true on ready→ready must cancel the active dispatch even though FSM is NoOp: {body_text}"
    );

    let dispatch_status: String =
        sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
            .bind("dispatch-tx-noop-1444-active")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        dispatch_status, "cancelled",
        "active dispatch must be cancelled by the no-op force path"
    );

    pool.close().await;
    pg_db.drop().await;
}

/// #1444 incident regression: replay today's bug — call /redispatch (which
/// is single-call complete and creates one new dispatch), then chain the
/// redundant /transition status:ready and /queue/generate calls. The
/// guards must reject the follow-ups so the second dispatch is NEVER
/// created.
#[tokio::test]
async fn idempotency_guards_block_duplicate_dispatch_after_redispatch_pg_1444() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pool.clone());
    seed_repo_pg(&pool, "test-repo").await;
    seed_agent_pg(&pool, "agent-incident-1444").await;

    // Seed the failed-impl card the way redispatch needs it: in_progress
    // with a previously-failed dispatch row. /redispatch will cancel that
    // and create a brand-new dispatch_id.
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, repo_id,
            github_issue_number, latest_dispatch_id, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, NOW(), NOW()
         )",
    )
    .bind("card-incident-1444")
    .bind("Incident 1444")
    .bind("in_progress")
    .bind("medium")
    .bind("agent-incident-1444")
    .bind("test-repo")
    .bind(1444100_i64)
    .bind("dispatch-incident-1444-old")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, NOW() - INTERVAL '5 minutes', NOW() - INTERVAL '5 minutes'
         )",
    )
    .bind("dispatch-incident-1444-old")
    .bind("card-incident-1444")
    .bind("agent-incident-1444")
    .bind("implementation")
    .bind("dispatched")
    .bind("[Impl] incident-old 1444")
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

    // Step 1 — /redispatch (the only call that should have happened).
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-incident-1444/redispatch")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"reason":"test #1444"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let new_dispatch_id = json["new_dispatch_id"]
        .as_str()
        .expect("redispatch must report a new_dispatch_id")
        .to_string();
    assert_ne!(
        new_dispatch_id, "dispatch-incident-1444-old",
        "redispatch must mint a fresh dispatch_id"
    );

    let after_redispatch_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT FROM task_dispatches WHERE kanban_card_id = $1",
    )
    .bind("card-incident-1444")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        after_redispatch_count, 2,
        "after /redispatch we expect 2 task_dispatches rows: the old (cancelled) and the new"
    );

    // Step 2 — the wrongly chained /transition status:ready. Must 409.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-incident-1444/transition")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(r#"{"status":"ready"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::CONFLICT,
        "redundant transition→ready must 409 because the redispatched dispatch is still active"
    );

    // Step 3 — the wrongly chained /queue/generate. Card must be
    // reported under skipped_due_to_active_dispatch and NOT enter a run.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"repo":"test-repo","issue_numbers":[1444100]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body).to_string();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let skipped_active = json["skipped_due_to_active_dispatch"]
        .as_array()
        .expect("skipped_due_to_active_dispatch must be an array");
    assert!(
        skipped_active
            .iter()
            .any(|entry| entry["issue_number"] == 1444100),
        "issue 1444100 must be reported as skipped_due_to_active_dispatch: {body_text}"
    );
    let entries = json["entries"]
        .as_array()
        .map(|e| e.as_slice())
        .unwrap_or(&[]);
    assert!(
        entries
            .iter()
            .all(|entry| entry["card_id"].as_str().unwrap_or_default() != "card-incident-1444"),
        "card-incident-1444 must NOT enter the generated run: {body_text}"
    );

    // Final invariant: only ONE active (pending|dispatched) dispatch row
    // exists for the card after redispatch + the rejected follow-ups.
    let active_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND status IN ('pending', 'dispatched')",
    )
    .bind("card-incident-1444")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        active_count, 1,
        "exactly one active dispatch must exist after the rejected follow-ups; the guard prevents a duplicate"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn force_transition_rejects_mismatched_channel_when_pmd_channel_is_configured() {
    let _lock = env_lock();
    let db = test_db();
    let engine = test_engine(&db);
    seed_card_with_status(&db, "card-ft4", "requested");

    let config_dir = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.kanban.manager_channel_id = Some("pmd-chan-123".to_string());
    let config_path = config_dir.path().join("agentdesk.yaml");
    crate::config::save_to_path(&config_path, &config).unwrap();
    let _config_guard = EnvVarGuard::set_path("AGENTDESK_CONFIG", &config_path);

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-ft4/transition")
                .header("content-type", "application/json")
                .header("x-channel-id", "wrong-channel")
                .body(Body::from(r#"{"status":"done"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["error"],
        "force-transition requires PMD channel authorization"
    );
}

// #1342 ci-red: this test exercises the JS bridge's setStatus path during
// the OnCardTerminal hook, which on PG holds a write tx across pipeline
// resolution. With the default `#[tokio::test]` (current_thread) runtime
// the hook executor and the source pool's drivers contend for the only
// worker thread, so a multi-thread runtime is required for the test to
// drive the pool while the actor thread is dispatching JS hooks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn force_transition_to_done_tracks_pr_from_live_work_dispatch_and_cleans_it_up_pg() {
    crate::pipeline::ensure_loaded();
    let (repo, _repo_override) = setup_test_repo();
    let _gh = install_mock_gh_pr_tracking(
        "test/repo",
        "wt/card-575-force",
        905,
        "feature-sha-575-force",
    );
    let policy_dir = tempfile::tempdir().unwrap();
    let source_policies = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
    for entry in std::fs::read_dir(&source_policies).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("js") {
            continue;
        }
        std::fs::copy(&path, policy_dir.path().join(entry.file_name())).unwrap();
    }
    std::fs::write(
        policy_dir.path().join("zz-ft-terminal-marker.js"),
        r#"
        agentdesk.registerPolicy({
          name: "ft-terminal-marker",
          priority: 9999,
          onCardTerminal: function(payload) {
            agentdesk.db.execute(
              "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('test_force_terminal_marker', ?1)",
              [payload.card_id + ":" + payload.status]
            );
          }
        });
        "#,
    )
    .unwrap();
    let origin = tempfile::tempdir().unwrap();
    run_git(origin.path(), &["init", "--bare"]);
    run_git(
        repo.path(),
        &["remote", "add", "origin", origin.path().to_str().unwrap()],
    );
    run_git(repo.path(), &["push", "-u", "origin", "main"]);

    let worktrees_dir = repo.path().join("worktrees");
    std::fs::create_dir_all(&worktrees_dir).unwrap();
    run_git(repo.path(), &["branch", "wt/card-575-force"]);

    let worktree_path = worktrees_dir.join("card-575-force");
    run_git(
        repo.path(),
        &[
            "worktree",
            "add",
            worktree_path.to_str().unwrap(),
            "wt/card-575-force",
        ],
    );
    std::fs::write(
        worktree_path.join("feature.txt"),
        "force transition merge\n",
    )
    .unwrap();
    run_git(worktree_path.as_path(), &["add", "feature.txt"]);
    run_git(
        worktree_path.as_path(),
        &["commit", "-m", "fix: force-transition merge target (#575)"],
    );
    std::fs::write(
        worktree_path.join("merge-proof.txt"),
        "second force transition merge\n",
    )
    .unwrap();
    run_git(worktree_path.as_path(), &["add", "merge-proof.txt"]);
    run_git(
        worktree_path.as_path(),
        &[
            "commit",
            "-m",
            "fix: second force-transition merge target (#575)",
        ],
    );
    run_git(repo.path(), &["push", "-u", "origin", "wt/card-575-force"]);
    let feature_commit = run_git_output(worktree_path.as_path(), &["rev-parse", "HEAD"]);

    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let mut config = crate::config::Config::default();
    config.policies.dir = policy_dir.path().to_path_buf();
    config.policies.hot_reload = false;
    let engine = PolicyEngine::new_with_pg(&config, Some(pool.clone())).unwrap();
    seed_agent_pg(&pool, "agent-ft-terminal").await;
    seed_repo_pg(&pool, "test/repo").await;
    sqlx::query(
        "INSERT INTO kv_meta (key, value) VALUES ('kanban_manager_channel_id', $1)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind("pmd-chan-123")
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, repo_id,
            github_issue_number, latest_dispatch_id, created_at, updated_at, started_at
        ) VALUES (
            'card-ft-terminal', 'Issue #575', 'in_progress', 'medium', 'agent-ft-terminal', 'test/repo',
            575, 'dispatch-ft-terminal', NOW() - INTERVAL '5 minutes', NOW() - INTERVAL '5 minutes', NOW() - INTERVAL '5 minutes'
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
            created_at, updated_at
        ) VALUES (
            'dispatch-ft-terminal', 'card-ft-terminal', 'agent-ft-terminal', 'implementation', 'dispatched',
            'live impl', $1, NOW() - INTERVAL '4 minutes', NOW() - INTERVAL '4 minutes'
        )",
    )
    .bind(
        serde_json::json!({
            "worktree_path": worktree_path.to_string_lossy().to_string(),
            "worktree_branch": "wt/card-575-force"
        })
        .to_string(),
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kv_meta (key, value) VALUES ('merge_automation_enabled', 'true')
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kv_meta (key, value) VALUES ('merge_strategy_mode', 'pr-always')
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions (
            session_key, agent_id, provider, status, cwd, active_dispatch_id, last_heartbeat, created_at
        ) VALUES (
            'session-ft-terminal', 'agent-ft-terminal', 'codex', 'turn_active', $1, 'dispatch-ft-terminal',
            NOW() - INTERVAL '4 minutes', NOW() - INTERVAL '4 minutes'
        )",
    )
    .bind(worktree_path.to_string_lossy().to_string())
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(db.clone(), engine, config, None, pool.clone());
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-ft-terminal/transition")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(r#"{"status":"done"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["forced"], true);
    assert_eq!(
        json["cancelled_dispatches"],
        serde_json::json!(1),
        "force-transition to done must cancel the live work dispatch before terminal hooks"
    );

    let hook_marker: Option<String> =
        sqlx::query_scalar("SELECT value FROM kv_meta WHERE key = 'test_force_terminal_marker'")
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert_eq!(
        hook_marker.as_deref(),
        Some("card-ft-terminal:done"),
        "force-transition to done must still fire OnCardTerminal hooks"
    );

    let merge_debug: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT state, last_error FROM pr_tracking WHERE card_id = 'card-ft-terminal'",
    )
    .fetch_optional(&pool)
    .await
    .unwrap()
    .unwrap_or((None, None));

    run_git(
        repo.path(),
        &["fetch", "origin", "main", "wt/card-575-force"],
    );
    let merged = Command::new("git")
        .args([
            "merge-base",
            "--is-ancestor",
            &feature_commit,
            "origin/main",
        ])
        .current_dir(repo.path())
        .status()
        .unwrap();
    let pushed_feature = Command::new("git")
        .args(["show", "origin/wt/card-575-force:feature.txt"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    let pushed_proof = Command::new("git")
        .args(["show", "origin/wt/card-575-force:merge-proof.txt"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(
        !merged.success(),
        "force-transition terminal path must not direct-merge into origin/main when PR+CI is required; pr_tracking={:?}",
        merge_debug
    );
    assert!(
        pushed_feature.status.success()
            && pushed_proof.status.success()
            && String::from_utf8_lossy(&pushed_feature.stdout) == "force transition merge\n"
            && String::from_utf8_lossy(&pushed_proof.stdout) == "second force transition merge\n",
        "force-transition terminal path must still push the tracked worktree branch for PR creation; pr_tracking={:?}",
        merge_debug
    );

    let mut card_status = String::new();
    let mut latest_dispatch_id: Option<String> = None;
    let mut blocked_reason: Option<String> = None;
    let mut dispatch_status = String::new();
    let mut pr_tracking_state: Option<String> = None;
    let mut pr_tracking_pr_number: Option<i64> = None;
    let mut pr_tracking_last_error: Option<String> = None;

    let tracking_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let card_row: (String, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT status, latest_dispatch_id, blocked_reason FROM kanban_cards WHERE id = 'card-ft-terminal'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let observed_dispatch_status: String = sqlx::query_scalar(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-ft-terminal'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        // #1342 ci-red: pr_tracking.pr_number is INT8/BIGINT in the PG
        // schema (sqlx 0.8 strict typing), so it must be decoded as i64.
        let pr_row: (Option<String>, Option<i64>, Option<String>) = sqlx::query_as(
            "SELECT state, pr_number, last_error FROM pr_tracking WHERE card_id = 'card-ft-terminal'",
        )
        .fetch_optional(&pool)
        .await
        .unwrap()
        .unwrap_or((None, None, None));

        card_status = card_row.0;
        latest_dispatch_id = card_row.1;
        blocked_reason = card_row.2;
        dispatch_status = observed_dispatch_status;
        pr_tracking_state = pr_row.0;
        pr_tracking_pr_number = pr_row.1;
        pr_tracking_last_error = pr_row.2;

        if pr_tracking_state.as_deref() == Some("wait-ci")
            && pr_tracking_pr_number == Some(905)
            && blocked_reason.as_deref() == Some("ci:waiting")
        {
            break;
        }

        if std::time::Instant::now() >= tracking_deadline {
            break;
        }

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    assert_eq!(card_status, "done");
    assert!(
        latest_dispatch_id.is_none(),
        "force-transition terminal cleanup must clear stale latest_dispatch_id"
    );
    assert_eq!(
        dispatch_status, "cancelled",
        "live implementation dispatch must not survive a force-transition to done"
    );
    assert_eq!(
        pr_tracking_state.as_deref(),
        Some("wait-ci"),
        "force-transition terminal cleanup should track the created PR and wait for CI"
    );
    assert_eq!(pr_tracking_pr_number, Some(905));
    assert_eq!(pr_tracking_last_error, None);
    assert_eq!(blocked_reason.as_deref(), Some("ci:waiting"));

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn rereview_reactivates_done_card_with_fresh_review_dispatch() {
    crate::pipeline::ensure_loaded();
    let _env_lock = env_lock();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-rereview");
    set_pmd_channel(&db, "pmd-chan-123");
    ensure_auto_queue_tables(&db);

    let repo = tempfile::tempdir().unwrap();
    run_git(repo.path(), &["init", "-b", "main"]);
    run_git(repo.path(), &["config", "user.email", "test@test.com"]);
    run_git(repo.path(), &["config", "user.name", "Test"]);
    run_git(repo.path(), &["commit", "--allow-empty", "-m", "initial"]);
    let expected_commit = git_commit(repo.path(), "fix: review target (#269)");
    let _repo_dir = EnvVarGuard::set_path("AGENTDESK_REPO_DIR", repo.path());
    let _config_dir = write_repo_mapping_config(&[("test-repo", repo.path())]);
    let _config = EnvVarGuard::set_path(
        "AGENTDESK_CONFIG",
        &_config_dir.path().join("agentdesk.yaml"),
    );

    let completed_commit = "1111111111111111111111111111111111111269";
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, latest_dispatch_id, created_at, updated_at, completed_at
            ) VALUES (
                'card-rereview', 'Issue #269', 'done', 'medium', 'agent-rereview', 'test-repo',
                269, 'rd-old', datetime('now'), datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, result,
                created_at, updated_at
            ) VALUES (
                'impl-rereview', 'card-rereview', 'agent-rereview', 'implementation', 'completed',
                'impl', ?1, datetime('now', '-2 minutes'), datetime('now', '-2 minutes')
            )",
            [serde_json::json!({
                "completed_commit": completed_commit,
                "completed_branch": "main"
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                created_at, updated_at
            ) VALUES (
                'review-old', 'card-rereview', 'agent-rereview', 'review', 'completed',
                'old review', ?1, datetime('now', '-1 minutes'), datetime('now', '-1 minutes')
            )",
            [serde_json::json!({
                "reviewed_commit": "wrong-review-target"
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                created_at, updated_at
            ) VALUES (
                'rd-old', 'card-rereview', 'agent-rereview', 'review-decision', 'completed',
                'old rd', datetime('now', '-30 seconds'), datetime('now', '-30 seconds')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-rereview', 'test-repo', 'agent-rereview', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, completed_at
            ) VALUES (
                'entry-rereview', 'run-rereview', 'card-rereview', 'agent-rereview',
                'done', 'rd-old', datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-rereview/rereview")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(
                    r#"{"reason":"repair wrong review target in unified thread"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["rereviewed"], true);

    let review_dispatch_id = json["review_dispatch_id"]
        .as_str()
        .expect("response must include new review dispatch id")
        .to_string();

    let conn = db.lock().unwrap();
    let card_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-rereview'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(card_status, "review");

    let dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = ?1",
            [&review_dispatch_id],
            |row| row.get(0),
        )
        .unwrap();
    let reviewed_commit = conn
        .query_row(
            "SELECT context FROM task_dispatches WHERE id = ?1",
            [&review_dispatch_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .unwrap()
        .and_then(|context| {
            serde_json::from_str::<serde_json::Value>(&context)
                .ok()
                .and_then(|value| {
                    value
                        .get("reviewed_commit")
                        .and_then(|entry| entry.as_str())
                        .map(str::to_string)
                })
        });
    assert_eq!(dispatch_status, "pending");
    assert_eq!(
        reviewed_commit.as_deref(),
        Some(expected_commit.as_str()),
        "reviewed_commit should be recovered from the repo fallback chain"
    );

    let (entry_status, entry_dispatch_id): (String, String) = conn
        .query_row(
            "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-rereview'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(entry_status, "dispatched");
    assert_eq!(entry_dispatch_id, review_dispatch_id);
}

#[tokio::test]
async fn dispute_repeat_pg_does_not_reuse_poisoned_review_target() {
    crate::pipeline::ensure_loaded();
    let _env_lock = env_lock();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    seed_agent_pg(&pool, "agent-dispute-repeat").await;
    seed_repo_pg(&pool, "test-repo").await;

    let repo = tempfile::tempdir().unwrap();
    run_git(repo.path(), &["init", "-b", "main"]);
    run_git(repo.path(), &["config", "user.email", "test@test.com"]);
    run_git(repo.path(), &["config", "user.name", "Test"]);
    run_git(repo.path(), &["commit", "--allow-empty", "-m", "initial"]);
    let safe_commit = git_commit(repo.path(), "fix: safe review target (#472)");
    let worktree_dir = repo.path().join("wt-472");
    run_git(
        repo.path(),
        &[
            "worktree",
            "add",
            worktree_dir.to_str().unwrap(),
            "-b",
            "wt/472-poison",
        ],
    );
    let worktree_path = worktree_dir.to_string_lossy().to_string();
    let poisoned_commit = git_commit(&worktree_dir, "chore: stale target (#482)");
    let _repo_dir = EnvVarGuard::set_path("AGENTDESK_REPO_DIR", repo.path());
    let _config_dir = write_repo_mapping_config(&[("test-repo", repo.path())]);
    let _config = EnvVarGuard::set_path(
        "AGENTDESK_CONFIG",
        &_config_dir.path().join("agentdesk.yaml"),
    );

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, repo_id,
            github_issue_number, latest_dispatch_id, review_status, created_at, updated_at
        ) VALUES (
            'card-dispute-repeat', 'Issue #472', 'review', 'medium', 'agent-dispute-repeat', 'test-repo',
            472, 'rd-dispute-1', 'suggestion_pending', NOW(), NOW()
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result,
            created_at, updated_at, completed_at
        ) VALUES (
            'impl-dispute-repeat', 'card-dispute-repeat', 'agent-dispute-repeat', 'implementation',
            'completed', 'impl', $1, $2,
            NOW() - INTERVAL '10 minutes', NOW() - INTERVAL '10 minutes', NOW() - INTERVAL '10 minutes'
        )",
    )
    .bind(
        serde_json::json!({
            "worktree_path": worktree_path,
            "branch": "wt/472-poison"
        })
        .to_string(),
    )
    .bind(
        serde_json::json!({
            "completed_worktree_path": worktree_path,
            "completed_branch": "wt/472-poison",
            "completed_commit": poisoned_commit.clone(),
        })
        .to_string(),
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title,
            created_at, updated_at
        ) VALUES (
            'rd-dispute-1', 'card-dispute-repeat', 'agent-dispute-repeat', 'review-decision',
            'pending', '[Review Decision]', NOW() - INTERVAL '1 minutes', NOW() - INTERVAL '1 minutes'
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO card_review_state (
            card_id, state, pending_dispatch_id, review_round, updated_at
        ) VALUES (
            'card-dispute-repeat', 'suggestion_pending', 'rd-dispute-1', 1, NOW()
        )",
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
    let dispute_request = |dispatch_id: &str| {
        Request::builder()
            .method("POST")
            .uri("/reviews/decision")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "card_id": "card-dispute-repeat",
                    "decision": "dispute",
                    "dispatch_id": dispatch_id,
                })
                .to_string(),
            ))
            .unwrap()
    };

    let response1 = app
        .clone()
        .oneshot(dispute_request("rd-dispute-1"))
        .await
        .unwrap();
    assert_eq!(response1.status(), StatusCode::OK);

    let body1 = axum::body::to_bytes(response1.into_body(), usize::MAX)
        .await
        .unwrap();
    let json1: serde_json::Value = serde_json::from_slice(&body1).unwrap();
    let first_review_dispatch_id = json1["review_dispatch_id"]
        .as_str()
        .expect("dispute response must include first review dispatch id")
        .to_string();
    let first_reviewed_commit = json1["reviewed_commit"]
        .as_str()
        .expect("dispute response must include first reviewed commit")
        .to_string();
    assert_eq!(first_reviewed_commit, safe_commit);
    assert_ne!(first_reviewed_commit, poisoned_commit);

    let first_rd_status = sqlx::query_scalar::<_, String>(
        "SELECT status FROM task_dispatches WHERE id = 'rd-dispute-1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(first_rd_status, "completed");

    sqlx::query(
        "UPDATE task_dispatches
         SET status = 'completed', completed_at = NOW(), updated_at = NOW()
         WHERE id = $1",
    )
    .bind(&first_review_dispatch_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title,
            created_at, updated_at
        ) VALUES (
            'rd-dispute-2', 'card-dispute-repeat', 'agent-dispute-repeat', 'review-decision',
            'pending', '[Review Decision 2]', NOW(), NOW()
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE kanban_cards
         SET latest_dispatch_id = 'rd-dispute-2', review_status = 'suggestion_pending', updated_at = NOW()
         WHERE id = 'card-dispute-repeat'",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE card_review_state
         SET state = 'suggestion_pending', pending_dispatch_id = 'rd-dispute-2', updated_at = NOW()
         WHERE card_id = 'card-dispute-repeat'",
    )
    .execute(&pool)
    .await
    .unwrap();

    let response2 = app.oneshot(dispute_request("rd-dispute-2")).await.unwrap();
    assert_eq!(response2.status(), StatusCode::OK);

    let body2 = axum::body::to_bytes(response2.into_body(), usize::MAX)
        .await
        .unwrap();
    let json2: serde_json::Value = serde_json::from_slice(&body2).unwrap();
    let second_review_dispatch_id = json2["review_dispatch_id"]
        .as_str()
        .expect("dispute response must include second review dispatch id")
        .to_string();
    let second_reviewed_commit = json2["reviewed_commit"]
        .as_str()
        .expect("dispute response must include second reviewed commit")
        .to_string();
    assert_ne!(second_review_dispatch_id, first_review_dispatch_id);
    assert_eq!(second_reviewed_commit, safe_commit);
    assert_ne!(second_reviewed_commit, poisoned_commit);

    let second_context_raw = sqlx::query_scalar::<_, Option<String>>(
        "SELECT context FROM task_dispatches WHERE id = $1",
    )
    .bind(&second_review_dispatch_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let second_context: serde_json::Value =
        serde_json::from_str(second_context_raw.as_deref().unwrap_or("{}"))
            .expect("second review dispatch must persist context");
    assert_eq!(second_context["reviewed_commit"], safe_commit);
    let actual_worktree_path = std::fs::canonicalize(
        second_context["worktree_path"]
            .as_str()
            .expect("review dispatch must persist worktree_path"),
    )
    .unwrap()
    .to_string_lossy()
    .to_string();
    let expected_worktree_path = std::fs::canonicalize(repo.path())
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert_eq!(actual_worktree_path, expected_worktree_path);
    assert_eq!(second_context["branch"], "main");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn reopen_reactivates_done_card_without_deadlocking_review_tuning_fixup() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());
    seed_agent_pg(&pg_pool, "agent-reopen").await;
    seed_repo_pg(&pg_pool, "test-repo").await;
    set_pmd_channel(&db, "pmd-chan-123");
    let reopen_target = crate::pipeline::get()
        .dispatchable_states()
        .into_iter()
        .next()
        .expect("default pipeline should expose at least one dispatchable state")
        .to_string();

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, repo_id,
            review_status, created_at, updated_at, completed_at
        ) VALUES (
            'card-reopen', 'Issue #270', 'done', 'medium', 'agent-reopen', 'test-repo',
            'pass', NOW(), NOW(), NOW()
        )",
    )
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ('run-reopen', 'test-repo', 'agent-reopen', 'active')",
    )
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, completed_at
        ) VALUES (
            'entry-reopen', 'run-reopen', 'card-reopen', 'agent-reopen',
            'done', NOW()
        )",
    )
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO review_tuning_outcomes (
            card_id, dispatch_id, review_round, verdict, decision, outcome
        ) VALUES (
            'card-reopen', 'review-pass', 1, 'pass', 'approved', 'true_negative'
        )",
    )
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
                .uri("/kanban-cards/card-reopen/reopen")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(
                    r#"{"reason":"retry after incorrect pass","review_status":"queued"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["reopened"], true);
    assert_eq!(json["to"], reopen_target);

    let (status, review_status, completed_at): (String, Option<String>, Option<String>) =
        sqlx::query_as(
            "SELECT status, review_status, completed_at::text
             FROM kanban_cards WHERE id = 'card-reopen'",
        )
        .fetch_one(&pg_pool)
        .await
        .unwrap();
    assert_eq!(status, reopen_target);
    assert_eq!(review_status.as_deref(), Some("queued"));
    assert!(completed_at.is_none());

    let entry_status: String =
        sqlx::query_scalar("SELECT status FROM auto_queue_entries WHERE id = 'entry-reopen'")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(entry_status, "dispatched");

    let outcome: String = sqlx::query_scalar(
        "SELECT outcome FROM review_tuning_outcomes
             WHERE card_id = 'card-reopen'
             ORDER BY review_round DESC, id DESC
             LIMIT 1",
    )
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(outcome, "false_negative");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn transition_to_done_records_true_negative_in_postgres_review_tuning() {
    crate::pipeline::ensure_loaded();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = crate::engine::PolicyEngine::new_with_pg(
        &crate::config::Config::default(),
        Some(pg_pool.clone()),
    )
    .unwrap();

    seed_agent_pg(&pg_pool, "agent-pg-tn").await;
    seed_repo_pg(&pg_pool, "test-repo").await;

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, assigned_agent_id, review_status, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, NOW(), NOW()
         )",
    )
    .bind("card-pg-tn")
    .bind("test-repo")
    .bind("PG TN")
    .bind("review")
    .bind("medium")
    .bind("agent-pg-tn")
    .bind("pass")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO card_review_state (card_id, review_round, last_verdict, updated_at)
         VALUES ($1, $2, $3, NOW())",
    )
    .bind("card-pg-tn")
    .bind(2_i32)
    .bind("pass")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, dispatch_type, status, result, created_at, updated_at, completed_at
         )
         VALUES ($1, $2, 'review', 'completed', $3, NOW(), NOW(), NOW())",
    )
    .bind("dispatch-pg-tn")
    .bind("card-pg-tn")
    .bind(
        serde_json::json!({
            "items": [
                {"category": "logic"},
                {"category": "tests"}
            ]
        })
        .to_string(),
    )
    .execute(&pg_pool)
    .await
    .unwrap();

    let result = crate::kanban::transition_status_with_opts_pg_only(
        &pg_pool,
        &engine,
        "card-pg-tn",
        "done",
        "review",
        crate::engine::transition::ForceIntent::OperatorOverride,
    )
    .await;
    assert!(result.is_ok(), "transition to done should succeed");

    let row = sqlx::query(
        "SELECT review_round::BIGINT AS review_round, verdict, decision, outcome, finding_categories
         FROM review_tuning_outcomes
         WHERE card_id = $1
         ORDER BY id DESC
         LIMIT 1",
    )
    .bind("card-pg-tn")
    .fetch_one(&pg_pool)
    .await
    .unwrap();

    assert_eq!(row.try_get::<i64, _>("review_round").unwrap(), 2);
    assert_eq!(row.try_get::<String, _>("verdict").unwrap(), "pass");
    assert_eq!(row.try_get::<String, _>("decision").unwrap(), "done");
    assert_eq!(
        row.try_get::<String, _>("outcome").unwrap(),
        "true_negative"
    );
    assert_eq!(
        row.try_get::<String, _>("finding_categories").unwrap(),
        "[\"logic\",\"tests\"]"
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn reopen_skips_preflight_already_applied_for_api_reopen() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-reopen-skip");
    seed_repo(&db, "test-repo");
    set_pmd_channel(&db, "pmd-chan-123");

    let reopen_target = crate::pipeline::get()
        .dispatchable_states()
        .into_iter()
        .next()
        .expect("default pipeline should expose at least one dispatchable state")
        .to_string();

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                created_at, updated_at, completed_at
            ) VALUES (
                'card-reopen-skip', 'Issue #272', 'done', 'medium', 'agent-reopen-skip', 'test-repo',
                datetime('now'), datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                created_at, updated_at, completed_at
            ) VALUES (
                'impl-reopen-skip', 'card-reopen-skip', 'agent-reopen-skip', 'implementation',
                'completed', 'stale impl', datetime('now', '-1 hour'), datetime('now', '-1 hour'),
                datetime('now', '-1 hour')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-reopen-skip/reopen")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(r#"{"reason":"skip preflight on API reopen"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["reopened"], true);
    assert_eq!(json["to"], reopen_target);

    let conn = db.lock().unwrap();
    let (status, metadata_raw): (String, Option<String>) = conn
        .query_row(
            "SELECT status, metadata FROM kanban_cards WHERE id = 'card-reopen-skip'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        status, reopen_target,
        "API reopen must skip already_applied preflight and keep card reopened"
    );
    let metadata: serde_json::Value =
        serde_json::from_str(metadata_raw.as_deref().unwrap_or("{}")).unwrap();
    assert_eq!(metadata["preflight_status"], "skipped");
    assert_eq!(metadata["preflight_summary"], "Skipped for API reopen");
    assert!(
        metadata.get("skip_preflight_once").is_none(),
        "skip_preflight_once must be consumed during reopen transition"
    );
}

#[tokio::test]
async fn reopen_returns_bad_gateway_when_github_reopen_fails_before_response() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-reopen-ghfail");
    set_pmd_channel(&db, "pmd-chan-123");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_url, created_at, updated_at, completed_at
            ) VALUES (
                'card-reopen-ghfail', 'Issue #271', 'done', 'medium', 'agent-reopen-ghfail',
                'test-repo', 'https://example.com/not-github', datetime('now'),
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-reopen-ghfail/reopen")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(r#"{"reason":"gh reopen failure test"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["reopened"], false);
    assert_eq!(json["github_issue_url"], "https://example.com/not-github");
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("not a github url"),
        "expected invalid github url parse error, got {json}"
    );
}

#[tokio::test]
async fn reopen_reset_full_clears_review_thread_and_preflight_state() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-reopen-reset");
    seed_repo(&db, "test-repo");
    set_pmd_channel(&db, "pmd-chan-123");
    ensure_auto_queue_tables(&db);

    let reopen_target = crate::pipeline::get()
        .dispatchable_states()
        .into_iter()
        .next()
        .expect("default pipeline should expose at least one dispatchable state")
        .to_string();

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                latest_dispatch_id, review_status, review_round, review_notes,
                suggestion_pending_at, review_entered_at, awaiting_dod_at,
                metadata, channel_thread_map, active_thread_id,
                created_at, updated_at, completed_at
            ) VALUES (
                'card-reopen-reset', 'Issue #273', 'done', 'medium', 'agent-reopen-reset', 'test-repo',
                'dispatch-reopen-reset', 'suggestion_pending', 4, 'stale review notes',
                datetime('now', '-12 minutes'), datetime('now', '-11 minutes'), datetime('now', '-10 minutes'),
                '{\"keep\":\"yes\",\"preflight_status\":\"already_applied\",\"preflight_summary\":\"stale\",\"preflight_checked_at\":\"2026-04-01T00:00:00Z\",\"consultation_status\":\"completed\",\"consultation_result\":{\"summary\":\"stale\"}}',
                '{\"111\":\"222\"}', '222',
                datetime('now', '-20 minutes'), datetime('now', '-20 minutes'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-reopen-reset', 'card-reopen-reset', 'agent-reopen-reset', 'consultation',
                'pending', 'stale consult', datetime('now', '-9 minutes'), datetime('now', '-9 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, agent_id, provider, status, active_dispatch_id, last_heartbeat, created_at
            ) VALUES (
                'session-reopen-reset', 'agent-reopen-reset', 'codex', 'turn_active', 'dispatch-reopen-reset',
                datetime('now', '-9 minutes'), datetime('now', '-9 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-reopen-reset', 'test-repo', 'agent-reopen-reset', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-reopen-reset-history', 'test-repo', 'agent-reopen-reset', 'completed')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at
            ) VALUES (
                'entry-reopen-live', 'run-reopen-reset', 'card-reopen-reset', 'agent-reopen-reset',
                'dispatched', 'dispatch-reopen-reset', datetime('now', '-9 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, completed_at
            ) VALUES (
                'entry-reopen-done', 'run-reopen-reset-history', 'card-reopen-reset', 'agent-reopen-reset',
                'done', datetime('now', '-30 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO card_review_state (
                card_id, state, pending_dispatch_id, review_round, last_verdict, last_decision,
                approach_change_round, session_reset_round, review_entered_at, updated_at
            ) VALUES (
                'card-reopen-reset', 'suggestion_pending', 'dispatch-reopen-reset', 4, 'pass', 'approved',
                3, 4, datetime('now', '-11 minutes'), datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-reopen-reset/reopen")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(
                    r#"{"reason":"full reset reopen","reset_full":true,"review_status":"queued"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["reopened"], true);
    assert_eq!(json["reset_full"], true);
    assert_eq!(json["cancelled_dispatches"], 1);
    assert_eq!(json["skipped_auto_queue_entries"], 1);

    let conn = db.lock().unwrap();
    let (
        status,
        latest_dispatch_id,
        review_status,
        review_round,
        review_notes,
        suggestion_pending_at,
        review_entered_at,
        awaiting_dod_at,
        metadata_raw,
        channel_thread_map,
        active_thread_id,
        completed_at,
    ): (
        String,
        Option<String>,
        Option<String>,
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT status, latest_dispatch_id, review_status, review_round, review_notes,
                    suggestion_pending_at, review_entered_at, awaiting_dod_at,
                    metadata, channel_thread_map, active_thread_id, completed_at
             FROM kanban_cards WHERE id = 'card-reopen-reset'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(status, reopen_target);
    assert!(latest_dispatch_id.is_none());
    assert_eq!(review_status.as_deref(), Some("queued"));
    assert_eq!(review_round, 0);
    assert!(review_notes.is_none());
    assert!(suggestion_pending_at.is_none());
    assert!(review_entered_at.is_none());
    assert!(awaiting_dod_at.is_none());
    assert!(channel_thread_map.is_none());
    assert!(active_thread_id.is_none());
    assert!(completed_at.is_none());

    let metadata: serde_json::Value =
        serde_json::from_str(metadata_raw.as_deref().unwrap_or("{}")).unwrap();
    assert_eq!(metadata["keep"], "yes");
    assert!(
        metadata.get("preflight_status").is_none(),
        "reset_full must clear stale preflight status"
    );
    assert!(
        metadata.get("preflight_summary").is_none(),
        "reset_full must clear stale preflight summary"
    );
    assert!(
        metadata.get("consultation_status").is_none(),
        "reset_full must clear stale consultation status"
    );
    assert!(
        metadata.get("consultation_result").is_none(),
        "reset_full must clear stale consultation result"
    );
    assert!(
        metadata.get("skip_preflight_once").is_none(),
        "reset_full must not leave a preflight skip marker behind"
    );

    let (
        review_state_round,
        review_state_status,
        review_state_pending_dispatch,
        review_state_verdict,
        review_state_decision,
        review_state_approach_change_round,
        review_state_session_reset_round,
        review_state_entered_at,
    ): (
        i64,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT review_round, state, pending_dispatch_id, last_verdict, last_decision,
                    approach_change_round, session_reset_round, review_entered_at
             FROM card_review_state WHERE card_id = 'card-reopen-reset'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(review_state_round, 0);
    assert_eq!(review_state_status, "idle");
    assert!(review_state_pending_dispatch.is_none());
    assert!(review_state_verdict.is_none());
    assert!(review_state_decision.is_none());
    assert!(review_state_approach_change_round.is_none());
    assert!(review_state_session_reset_round.is_none());
    assert!(review_state_entered_at.is_none());

    let dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-reopen-reset'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dispatch_status, "cancelled");

    let (session_status, active_dispatch_id): (String, Option<String>) = conn
        .query_row(
            "SELECT status, active_dispatch_id
             FROM sessions
             WHERE session_key = 'session-reopen-reset'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(session_status, "idle");
    assert!(active_dispatch_id.is_none());

    let entry_rows: Vec<(String, Option<String>)> = conn
        .prepare(
            "SELECT status, dispatch_id FROM auto_queue_entries
             WHERE kanban_card_id = 'card-reopen-reset'
             ORDER BY id ASC",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(
        entry_rows,
        vec![
            ("dispatched".to_string(), None),
            ("skipped".to_string(), None),
        ],
        "reset_full must reactivate done entries but skip stale live entries"
    );
}

#[tokio::test]
async fn retry_preserves_review_dispatch_type() {
    let (_repo, _repo_guard) = setup_test_repo();
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-review-retry");
    seed_repo(&db, "test-repo");
    ensure_auto_queue_tables(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, latest_dispatch_id, created_at, updated_at
            ) VALUES (
                'card-review-retry', 'Issue #331 retry', 'review', 'medium', 'agent-review-retry', 'test-repo',
                331, 'dispatch-review-old', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                created_at, updated_at
            ) VALUES (
                'dispatch-review-old', 'card-review-retry', 'agent-review-retry', 'review', 'pending',
                '[Review] Issue #331 retry', datetime('now', '-10 minutes'), datetime('now', '-10 minutes')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-review-retry/retry")
                .header("content-type", "application/json")
                .body(Body::from(r#"{}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["card"]["latest_dispatch_type"], "review");

    let conn = db.lock().unwrap();
    let old_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-review-old'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(old_status, "cancelled");

    let latest_dispatch_id: String = conn
        .query_row(
            "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-review-retry'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_ne!(latest_dispatch_id, "dispatch-review-old");

    let (dispatch_type, status, title): (String, String, String) = conn
        .query_row(
            "SELECT dispatch_type, status, title FROM task_dispatches WHERE id = ?1",
            [&latest_dispatch_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(dispatch_type, "review");
    assert_eq!(status, "pending");
    assert_eq!(title, "[Review] Issue #331 retry");
}

#[tokio::test]
async fn redispatch_preserves_review_dispatch_type() {
    let (_repo, _repo_guard) = setup_test_repo();
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-review-redispatch");
    seed_repo(&db, "test-repo");
    ensure_auto_queue_tables(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, latest_dispatch_id, review_status, created_at, updated_at
            ) VALUES (
                'card-review-redispatch', 'Issue #331 redispatch', 'review', 'medium', 'agent-review-redispatch', 'test-repo',
                331, 'dispatch-review-redispatch-old', 'queued', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                created_at, updated_at
            ) VALUES (
                'dispatch-review-redispatch-old', 'card-review-redispatch', 'agent-review-redispatch', 'review', 'dispatched',
                '[Review] Issue #331 redispatch', datetime('now', '-10 minutes'), datetime('now', '-10 minutes')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-review-redispatch/redispatch")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"reason":"requeue review"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["card"]["latest_dispatch_type"], "review");

    let conn = db.lock().unwrap();
    let old_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-review-redispatch-old'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(old_status, "cancelled");

    let latest_dispatch_id: String = conn
        .query_row(
            "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-review-redispatch'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_ne!(latest_dispatch_id, "dispatch-review-redispatch-old");

    let (dispatch_type, status, title, review_status): (String, String, String, Option<String>) =
        conn.query_row(
            "SELECT td.dispatch_type, td.status, td.title, kc.review_status
             FROM task_dispatches td
             JOIN kanban_cards kc ON kc.latest_dispatch_id = td.id
             WHERE td.id = ?1",
            [&latest_dispatch_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(dispatch_type, "review");
    assert_eq!(status, "pending");
    assert_eq!(title, "[Review] Issue #331 redispatch");
    assert!(
        review_status.is_none(),
        "redispatch should clear stale review_status before creating the new review dispatch"
    );
}

#[tokio::test]
async fn rereview_clears_stale_review_fields() {
    let (_repo, _repo_guard) = setup_test_repo();
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-stale-cleanup");
    set_pmd_channel(&db, "pmd-chan-123");
    ensure_auto_queue_tables(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, review_status, suggestion_pending_at,
                review_entered_at, awaiting_dod_at,
                created_at, updated_at
            ) VALUES (
                'card-stale', 'Issue #300', 'review', 'medium', 'agent-stale-cleanup', 'test-repo',
                300, 'suggestion_pending', datetime('now', '-10 minutes'),
                datetime('now', '-20 minutes'), datetime('now', '-5 minutes'),
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                created_at, updated_at
            ) VALUES (
                'impl-stale', 'card-stale', 'agent-stale-cleanup', 'implementation', 'completed',
                'impl', datetime('now', '-30 minutes'), datetime('now', '-30 minutes')
            )",
            [],
        )
        .unwrap();
        // Seed card_review_state with stale data
        conn.execute(
            "INSERT INTO card_review_state (card_id, state, pending_dispatch_id, review_round, updated_at)
             VALUES ('card-stale', 'suggestion_pending', 'old-dispatch-id', 1, datetime('now'))",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-stale/rereview")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(r#"{"reason":"stale cleanup test"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let conn = db.lock().unwrap();
    let (review_status, suggestion_pending_at, awaiting_dod_at): (
        Option<String>,
        Option<String>,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT review_status, suggestion_pending_at, awaiting_dod_at
             FROM kanban_cards WHERE id = 'card-stale'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    // After cleanup + OnReviewEnter hook: review_status is refreshed to "reviewing"
    // (not the stale "suggestion_pending"), and stale timestamps are cleared.
    assert_ne!(
        review_status.as_deref(),
        Some("suggestion_pending"),
        "stale review_status 'suggestion_pending' should be cleared by rereview"
    );
    assert!(
        suggestion_pending_at.is_none(),
        "suggestion_pending_at should be NULL after rereview"
    );
    assert!(
        awaiting_dod_at.is_none(),
        "awaiting_dod_at should be NULL after rereview"
    );

    // card_review_state should NOT be stale "suggestion_pending" with old pending_dispatch_id
    let (rs_state, rs_pending): (String, Option<String>) = conn
        .query_row(
            "SELECT state, pending_dispatch_id FROM card_review_state WHERE card_id = 'card-stale'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_ne!(
        rs_state, "suggestion_pending",
        "card_review_state.state should not be stale 'suggestion_pending'"
    );
    assert_ne!(
        rs_pending.as_deref(),
        Some("old-dispatch-id"),
        "card_review_state.pending_dispatch_id should not be the old stale value"
    );
}

#[tokio::test]
async fn rereview_resets_repeated_finding_round_markers() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-acr-reset");
    set_pmd_channel(&db, "pmd-chan-123");
    ensure_auto_queue_tables(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, created_at, updated_at
            ) VALUES (
                'card-acr', 'Issue #272', 'review', 'medium', 'agent-acr-reset', 'test-repo',
                272, datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                created_at, updated_at
            ) VALUES (
                'impl-acr', 'card-acr', 'agent-acr-reset', 'implementation', 'completed',
                'impl', datetime('now', '-30 minutes'), datetime('now', '-30 minutes')
            )",
            [],
        )
        .unwrap();
        // Seed card_review_state with non-null repeated-finding markers from a previous cycle
        conn.execute(
            "INSERT INTO card_review_state (
                card_id, state, review_round, approach_change_round, session_reset_round, updated_at
             ) VALUES ('card-acr', 'reviewing', 3, 2, 3, datetime('now'))",
            [],
        )
        .unwrap();
    }

    // Verify repeated-finding markers are set before rereview
    {
        let conn = db.lock().unwrap();
        let (acr, reset_round): (Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT approach_change_round, session_reset_round
                 FROM card_review_state WHERE card_id = 'card-acr'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            acr,
            Some(2),
            "approach_change_round should be 2 before rereview"
        );
        assert_eq!(
            reset_round,
            Some(3),
            "session_reset_round should be 3 before rereview"
        );
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-acr/rereview")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(
                    r#"{"reason":"repeated-finding marker reset test"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    // repeated-finding markers should be NULL after rereview
    let conn = db.lock().unwrap();
    let (acr, reset_round): (Option<i64>, Option<i64>) = conn
        .query_row(
            "SELECT approach_change_round, session_reset_round
             FROM card_review_state WHERE card_id = 'card-acr'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(
        acr.is_none(),
        "approach_change_round should be NULL after rereview, got {:?}",
        acr
    );
    assert!(
        reset_round.is_none(),
        "session_reset_round should be NULL after rereview, got {:?}",
        reset_round
    );
}

#[tokio::test]
async fn idle_sync_preserves_repeated_finding_round_markers() {
    // Regression test for #272/#420: generic idle sync (timeout, gate-failure, pass)
    // must NOT clear repeated-finding markers — only the explicit rereview path does.
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at)
             VALUES ('card-preserve', 'preserve test', 'review', 'medium', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO card_review_state (
                card_id, state, review_round, approach_change_round, session_reset_round, updated_at
             ) VALUES ('card-preserve', 'reviewing', 3, 2, 3, datetime('now'))",
            [],
        )
        .unwrap();

        // Simulate a non-rereview idle sync (e.g. pass/approved, timeout fallback)
        conn.execute(
            "UPDATE card_review_state
             SET state = 'idle',
                 last_verdict = 'pass',
                 updated_at = datetime('now')
             WHERE card_id = 'card-preserve'",
            [],
        )
        .unwrap();

        let (acr, reset_round): (Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT approach_change_round, session_reset_round
                 FROM card_review_state WHERE card_id = 'card-preserve'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            acr,
            Some(2),
            "approach_change_round must be preserved on generic idle sync, got {:?}",
            acr
        );
        assert_eq!(
            reset_round,
            Some(3),
            "session_reset_round must be preserved on generic idle sync, got {:?}",
            reset_round
        );
    }
}

#[tokio::test]
async fn rereview_backlog_card_transitions_to_review_with_dispatch_pg() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    seed_agent_pg(&pool, "agent-backlog-rr").await;
    seed_repo_pg(&pool, "test-repo").await;

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, repo_id,
            github_issue_number, created_at, updated_at
        ) VALUES (
            'card-backlog-rr', 'Issue #301', 'backlog', 'medium', 'agent-backlog-rr', 'test-repo',
            301, NOW(), NOW()
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title,
            created_at, updated_at
        ) VALUES (
            'impl-backlog-rr', 'card-backlog-rr', 'agent-backlog-rr', 'implementation', 'completed',
            'impl', NOW() - INTERVAL '30 minutes', NOW() - INTERVAL '30 minutes'
        )",
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
                .method("POST")
                .uri("/kanban-cards/card-backlog-rr/rereview")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(r#"{"reason":"backlog rereview test"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["rereviewed"], true);
    assert!(
        json["review_dispatch_id"].as_str().is_some(),
        "should have a dispatch id"
    );

    let card_status: String =
        sqlx::query_scalar("SELECT status FROM kanban_cards WHERE id = 'card-backlog-rr'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(card_status, "review", "card should transition to review");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn rereview_returns_bad_gateway_when_github_reopen_fails_before_response() {
    let (_repo, _repo_guard) = setup_test_repo();
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-rereview-ghfail");
    set_pmd_channel(&db, "pmd-chan-123");
    ensure_auto_queue_tables(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_url, created_at, updated_at, completed_at
            ) VALUES (
                'card-rereview-ghfail', 'Issue #336', 'done', 'medium', 'agent-rereview-ghfail',
                'test-repo', 'https://example.com/not-github', datetime('now'),
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                created_at, updated_at, completed_at
            ) VALUES (
                'impl-rereview-ghfail', 'card-rereview-ghfail', 'agent-rereview-ghfail',
                'implementation', 'completed', 'impl', datetime('now', '-30 minutes'),
                datetime('now', '-30 minutes'), datetime('now', '-30 minutes')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-rereview-ghfail/rereview")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(r#"{"reason":"gh reopen failure test"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["rereviewed"], false);
    assert_eq!(json["github_issue_url"], "https://example.com/not-github");
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("not a github url"),
        "expected invalid github url parse error, got {json}"
    );
}

#[tokio::test]
async fn batch_rereview_processes_multiple_issues() {
    let (_repo, _repo_guard) = setup_test_repo();
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-batch-rr");
    set_pmd_channel(&db, "pmd-chan-123");
    ensure_auto_queue_tables(&db);

    {
        let conn = db.lock().unwrap();
        // Card for issue #401
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, created_at, updated_at
            ) VALUES (
                'card-batch-1', 'Issue #401', 'done', 'medium', 'agent-batch-rr', 'test-repo',
                401, datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                created_at, updated_at
            ) VALUES (
                'impl-batch-1', 'card-batch-1', 'agent-batch-rr', 'implementation', 'completed',
                'impl 401', datetime('now', '-30 minutes'), datetime('now', '-30 minutes')
            )",
            [],
        )
        .unwrap();
        // Card for issue #402
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, created_at, updated_at
            ) VALUES (
                'card-batch-2', 'Issue #402', 'done', 'medium', 'agent-batch-rr', 'test-repo',
                402, datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                created_at, updated_at
            ) VALUES (
                'impl-batch-2', 'card-batch-2', 'agent-batch-rr', 'implementation', 'completed',
                'impl 402', datetime('now', '-30 minutes'), datetime('now', '-30 minutes')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/batch-rereview")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(
                    serde_json::json!({
                        "issues": [401, 402, 999],
                        "reason": "batch test"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let results = json["results"].as_array().expect("results should be array");
    assert_eq!(results.len(), 3, "should have 3 results");

    // Issue 401 should succeed
    assert_eq!(results[0]["issue"], 401);
    assert_eq!(results[0]["ok"], true);
    assert!(results[0]["dispatch_id"].as_str().is_some());

    // Issue 402 should succeed
    assert_eq!(results[1]["issue"], 402);
    assert_eq!(results[1]["ok"], true);
    assert!(results[1]["dispatch_id"].as_str().is_some());

    // Issue 999 should fail (not found)
    assert_eq!(results[2]["issue"], 999);
    assert_eq!(results[2]["ok"], false);
    assert!(
        results[2]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("not found")
    );

    // Verify both cards transitioned to review
    let conn = db.lock().unwrap();
    let status_1: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-batch-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status_1, "review");

    let status_2: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-batch-2'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status_2, "review");
}

