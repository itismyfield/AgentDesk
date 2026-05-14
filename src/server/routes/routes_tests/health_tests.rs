//! Domain-split routes tests — `health` group.
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
async fn health_detail_and_stale_mailbox_repair_pg_require_bearer_when_auth_enabled() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let mut config = crate::config::Config::default();
    config.server.host = "0.0.0.0".to_string();
    config.server.auth_token = Some("secret-token".to_string());
    let app = test_api_router_with_pg(db, engine, config, None, pool.clone());

    let mut detail_request = Request::builder()
        .uri("/health/detail")
        .body(Body::empty())
        .unwrap();
    detail_request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(
            "10.0.0.5:8791".parse::<std::net::SocketAddr>().unwrap(),
        ));
    let detail_response = app.clone().oneshot(detail_request).await.unwrap();
    assert_eq!(detail_response.status(), StatusCode::UNAUTHORIZED);

    let mut repair_request = Request::builder()
        .method("POST")
        .uri("/doctor/stale-mailbox/repair")
        .body(Body::from("{}"))
        .unwrap();
    repair_request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(
            "10.0.0.5:8791".parse::<std::net::SocketAddr>().unwrap(),
        ));
    let repair_response = app.clone().oneshot(repair_request).await.unwrap();
    assert_eq!(repair_response.status(), StatusCode::UNAUTHORIZED);

    let mut startup_doctor_request = Request::builder()
        .uri("/doctor/startup/latest")
        .body(Body::empty())
        .unwrap();
    startup_doctor_request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(
            "10.0.0.5:8791".parse::<std::net::SocketAddr>().unwrap(),
        ));
    let startup_doctor_response = app.clone().oneshot(startup_doctor_request).await.unwrap();
    assert_eq!(startup_doctor_response.status(), StatusCode::UNAUTHORIZED);

    let mut authorized_detail_request = Request::builder()
        .uri("/health/detail")
        .header("authorization", "Bearer secret-token")
        .body(Body::empty())
        .unwrap();
    authorized_detail_request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(
            "10.0.0.5:8791".parse::<std::net::SocketAddr>().unwrap(),
        ));
    let authorized_detail_response = app.oneshot(authorized_detail_request).await.unwrap();
    assert_eq!(authorized_detail_response.status(), StatusCode::OK);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn dispatch_outbox_failed_acknowledge_endpoint_marks_rows_non_permanent() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );

    sqlx::query(
        "INSERT INTO task_dispatches (id, status, title, created_at, updated_at)
         VALUES ($1, 'completed', 'ack test dispatch', NOW(), NOW())",
    )
    .bind("dispatch-ack-test")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO dispatch_outbox (dispatch_id, action, status, retry_count, error, created_at)
         VALUES ($1, 'notify', 'failed', 5, 'delivery exhausted', NOW())
         RETURNING id",
    )
    .bind("dispatch-ack-test")
    .fetch_one(&pool)
    .await
    .unwrap();

    let list_response = app
        .clone()
        .oneshot(local_get_request("/dispatch-outbox/failed"))
        .await
        .unwrap();
    assert_eq!(list_response.status(), StatusCode::OK);
    let list_body = axum::body::to_bytes(list_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let list_json: serde_json::Value = serde_json::from_slice(&list_body).unwrap();
    assert_eq!(list_json["count"], 1);
    let row_id = list_json["rows"][0]["id"].as_i64().unwrap();
    assert_eq!(list_json["rows"][0]["dispatch_status"], "completed");
    assert_eq!(list_json["rows"][0]["retry_count"], 5);

    let dry_run_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatch-outbox/failed")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"ids": [row_id], "dry_run": true}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(dry_run_response.status(), StatusCode::OK);
    let still_failed: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM dispatch_outbox WHERE status = 'failed'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(still_failed, 1, "dry_run must not mutate failed rows");

    let missing_ids_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatch-outbox/failed")
                .header("content-type", "application/json")
                .body(Body::from(json!({}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_ids_response.status(), StatusCode::BAD_REQUEST);

    let ack_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatch-outbox/failed")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"ids": [row_id], "reason": "obsolete notification"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ack_response.status(), StatusCode::OK);
    let ack_body = axum::body::to_bytes(ack_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let ack_json: serde_json::Value = serde_json::from_slice(&ack_body).unwrap();
    assert_eq!(ack_json["acknowledged"], 1);
    assert_eq!(ack_json["acknowledged_ids"][0], row_id);

    let status: String = sqlx::query_scalar("SELECT status FROM dispatch_outbox WHERE id = $1")
        .bind(row_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "acknowledged");
    let remaining_failed: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM dispatch_outbox WHERE status = 'failed'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(remaining_failed, 0);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn health_surfaces_latest_startup_doctor_summary_without_raw_checks() {
    let _lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
    let artifact_path =
        seed_startup_doctor_artifact(runtime_root.path(), sample_startup_doctor_artifact());

    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "SQLite-only test harness has no PostgreSQL server signal, but the health body must still surface startup doctor context"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let doctor = &json["latest_startup_doctor"];
    assert_eq!(doctor["available"], true);
    assert_eq!(doctor["doctor_status"], "failed");
    assert!(
        doctor["artifact_path"].is_null(),
        "artifact_path must not be exposed on the public /api/health endpoint"
    );
    assert_eq!(doctor["failed_count"], 1);
    assert_eq!(doctor["warned_count"], 1);
    assert_eq!(doctor["detail_endpoint"], "/api/doctor/startup/latest");
    assert!(doctor.get("failed_checks").is_none());
    assert!(doctor.get("warned_checks").is_none());
    assert!(doctor.get("checks").is_none());
}

#[tokio::test]
async fn startup_doctor_latest_endpoint_returns_json_artifact_envelope() {
    let _lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
    let artifact_path =
        seed_startup_doctor_artifact(runtime_root.path(), sample_startup_doctor_artifact());

    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(local_get_request("/doctor/startup/latest"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        content_type.starts_with("application/json"),
        "latest startup doctor endpoint must return JSON, got {content_type}"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["available"], true);
    assert_eq!(json["artifact_path"], artifact_path.display().to_string());
    assert_eq!(json["detail_source"], "startup_doctor_artifact");
    assert_eq!(json["followup_context"], "restart_followup");
    assert_eq!(json["summary"]["failed"], 1);
    assert_eq!(json["artifact"]["checks"][2]["id"], "dispatch_outbox");
}

#[tokio::test]
async fn startup_doctor_latest_endpoint_reports_missing_artifact_as_json() {
    let _lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
    fs::create_dir_all(runtime_root.path().join("runtime")).unwrap();
    fs::write(
        runtime_root.path().join("runtime").join("dcserver.pid"),
        "4242\n",
    )
    .unwrap();

    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(local_get_request("/doctor/startup/latest"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["available"], false);
    assert_eq!(json["reason"], "startup_doctor_artifact_missing");
    assert_eq!(json["artifact"], serde_json::Value::Null);
}

#[tokio::test]
async fn startup_doctor_latest_endpoint_reports_corrupt_artifact_as_parse_error() {
    let _lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());

    let runtime_dir = runtime_root.path().join("runtime");
    fs::create_dir_all(&runtime_dir).unwrap();
    fs::write(runtime_dir.join("dcserver.pid"), "4242\n").unwrap();

    let boot_id = crate::cli::doctor::startup::current_boot_id().unwrap();
    let artifact_dir = runtime_dir.join("doctor").join("startup");
    fs::create_dir_all(&artifact_dir).unwrap();
    fs::write(
        artifact_dir.join(format!("{boot_id}.json")),
        b"{ not valid json {{",
    )
    .unwrap();

    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(local_get_request("/doctor/startup/latest"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], false);
    assert_eq!(json["available"], false);
    assert_eq!(json["error"], "invalid_startup_doctor_artifact");
    assert!(
        !json["detail"].is_null(),
        "parse error detail string must be present"
    );
    assert_eq!(json["artifact"], serde_json::Value::Null);
}

#[tokio::test]
async fn health_detail_includes_latest_startup_doctor_detailed_fields() {
    let _lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
    let artifact_path =
        seed_startup_doctor_artifact(runtime_root.path(), sample_startup_doctor_artifact());

    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(local_get_request("/health/detail"))
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "SQLite-only test harness has no PostgreSQL server signal, but detail health must still surface startup doctor context"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let doctor = &json["latest_startup_doctor"];
    assert_eq!(doctor["available"], true);
    assert_eq!(doctor["artifact_path"], artifact_path.display().to_string());
    assert_eq!(
        doctor["failed_checks"]
            .as_array()
            .expect("failed_checks must be an array in detail")
            .len(),
        1,
        "sample artifact has 1 failed check"
    );
    assert_eq!(
        doctor["warned_checks"]
            .as_array()
            .expect("warned_checks must be an array in detail")
            .len(),
        1,
        "sample artifact has 1 warned check"
    );
    assert_eq!(doctor["run_context"], "startup_once");
    assert_eq!(doctor["non_fatal"], true);
    assert_eq!(doctor["followup_context"], "restart_followup");
    assert!(
        doctor.get("checks").is_none(),
        "raw checks array must not be present at top level of doctor object"
    );
}

#[tokio::test]
async fn health_detail_and_latest_endpoint_share_same_artifact_contract() {
    let _lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
    let artifact_path =
        seed_startup_doctor_artifact(runtime_root.path(), sample_startup_doctor_artifact());
    let artifact_path_str = artifact_path.display().to_string();

    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let health_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let health_json: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(health_response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();

    let detail_response = app
        .clone()
        .oneshot(local_get_request("/health/detail"))
        .await
        .unwrap();
    let detail_json: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(detail_response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();

    let latest_response = app
        .clone()
        .oneshot(local_get_request("/doctor/startup/latest"))
        .await
        .unwrap();
    let latest_json: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(latest_response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();

    assert!(
        health_json["latest_startup_doctor"]["artifact_path"].is_null(),
        "artifact_path must not appear in the public /api/health summary"
    );
    assert_eq!(
        detail_json["latest_startup_doctor"]["artifact_path"], artifact_path_str,
        "detail health must report correct artifact_path"
    );
    assert_eq!(
        latest_json["artifact_path"], artifact_path_str,
        "latest endpoint must report correct artifact_path"
    );
    assert_eq!(
        health_json["latest_startup_doctor"]["detail_endpoint"], "/api/doctor/startup/latest",
        "public health must expose detail_endpoint"
    );
    assert_eq!(
        detail_json["latest_startup_doctor"]["detail_endpoint"], "/api/doctor/startup/latest",
        "detail health must expose detail_endpoint"
    );
    assert_eq!(
        detail_json["latest_startup_doctor"]["followup_context"], latest_json["followup_context"],
        "followup_context must be consistent between detail and latest endpoint"
    );
}

#[tokio::test]
async fn health_api_http_pg_reports_observability_metrics_and_degraded_outbox_backlog() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let harness = crate::services::discord::health::TestHealthHarness::new().await;
    harness.set_recovery_duration_ms(1_250);
    let app = axum::Router::new().nest(
        "/api",
        test_api_router_with_pg(
            db.clone(),
            engine,
            crate::config::Config::default(),
            Some(harness.registry()),
            pool.clone(),
        ),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let url = format!("http://{addr}/api/health/detail");

    let healthy_response = reqwest::get(&url).await.unwrap();
    assert_eq!(healthy_response.status(), reqwest::StatusCode::OK);
    let healthy_json: serde_json::Value = healthy_response.json().await.unwrap();
    assert_eq!(healthy_json["status"], "healthy");
    assert_eq!(healthy_json["server_up"], true);
    assert_eq!(healthy_json["fully_recovered"], true);
    assert_eq!(healthy_json["deferred_hooks"], 0);
    assert_eq!(healthy_json["queue_depth"], 0);
    assert_eq!(healthy_json["watcher_count"], 0);
    assert_eq!(healthy_json["outbox_age"], 0);
    assert!(
        (healthy_json["recovery_duration"].as_f64().unwrap() - 1.25).abs() < f64::EPSILON,
        "expected recovery_duration=1.25, got {}",
        healthy_json["recovery_duration"]
    );

    sqlx::query(
        "INSERT INTO dispatch_outbox (dispatch_id, action, status, created_at) \
         VALUES ($1, 'notify', 'pending', NOW() - INTERVAL '5 minutes')",
    )
    .bind("dispatch-1")
    .execute(&pool)
    .await
    .unwrap();

    let degraded_response = reqwest::get(&url).await.unwrap();
    assert_eq!(degraded_response.status(), reqwest::StatusCode::OK);
    let degraded_json: serde_json::Value = degraded_response.json().await.unwrap();
    assert_eq!(degraded_json["status"], "degraded");
    assert_eq!(degraded_json["server_up"], true);
    assert_eq!(degraded_json["fully_recovered"], true);
    assert!(
        degraded_json["outbox_age"].as_i64().unwrap() >= 299,
        "expected an outbox age close to 300s, got {}",
        degraded_json["outbox_age"]
    );
    assert!(
        degraded_json["degraded_reasons"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(serde_json::Value::as_str)
            .any(|reason| reason.starts_with("dispatch_outbox_oldest_pending_age:")),
        "expected dispatch_outbox_oldest_pending_age reason, got {:?}",
        degraded_json["degraded_reasons"]
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn health_api_reports_server_up_before_full_recovery_on_postgres() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());
    let harness = crate::services::discord::health::TestHealthHarness::new().await;
    harness.set_reconcile_done(false);
    let app = axum::Router::new().nest(
        "/api",
        test_api_router_with_pg(
            db,
            engine,
            crate::config::Config::default(),
            Some(harness.registry()),
            pg_pool.clone(),
        ),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let response = reqwest::get(format!("http://{addr}/api/health/detail"))
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let json: serde_json::Value = response.json().await.unwrap();

    assert_eq!(json["status"], "degraded");
    assert_eq!(json["server_up"], true);
    assert_eq!(json["fully_recovered"], false);
    assert!(
        json["degraded_reasons"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(serde_json::Value::as_str)
            .any(|reason| reason == "provider:claude:reconcile_in_progress"),
        "expected reconcile_in_progress degraded reason, got {:?}",
        json["degraded_reasons"]
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn health_api_pg_standalone_mode_reports_status_field() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let app = axum::Router::new().nest(
        "/api",
        test_api_router_with_pg(
            db,
            engine,
            crate::config::Config::default(),
            None,
            pool.clone(),
        ),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let response = reqwest::get(format!("http://{addr}/api/health"))
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let json: serde_json::Value = response.json().await.unwrap();

    assert_eq!(json["status"], "healthy");
    assert_eq!(json["ok"], true);
    assert_eq!(json["db"], true);
    assert_eq!(json["server_up"], true);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn health_wait_script_passes_when_server_is_up_before_full_recovery_pg() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());
    let harness = crate::services::discord::health::TestHealthHarness::new().await;
    harness.set_reconcile_done(false);
    let app = axum::Router::new().nest(
        "/api",
        test_api_router_with_pg(
            db,
            engine,
            crate::config::Config::default(),
            Some(harness.registry()),
            pg_pool.clone(),
        ),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let warmup_json: serde_json::Value = reqwest::get(format!("http://{addr}/api/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(warmup_json["server_up"], true);
    assert_eq!(warmup_json["fully_recovered"], false);

    let defaults_path = format!("{}/scripts/_defaults.sh", env!("CARGO_MANIFEST_DIR"));
    let port = addr.port();
    let output = tokio::task::spawn_blocking(move || {
        Command::new("bash")
            .arg("-lc")
            .arg(format!(
                ". \"{defaults_path}\"; wait_for_http_service_health test-health {port} 2 0 0 1",
            ))
            .output()
            .unwrap()
    })
    .await
    .unwrap();

    assert!(
        output.status.success(),
        "expected wait_for_http_service_health to pass on server_up=true before full recovery; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn health_wait_script_pg_rejects_non_reconcile_degraded_server_up_response() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let harness = crate::services::discord::health::TestHealthHarness::new().await;
    let app = axum::Router::new().nest(
        "/api",
        test_api_router_with_pg(
            db.clone(),
            engine,
            crate::config::Config::default(),
            Some(harness.registry()),
            pool.clone(),
        ),
    );

    sqlx::query(
        "INSERT INTO dispatch_outbox (dispatch_id, action, status, created_at) \
         VALUES ($1, 'notify', 'pending', NOW() - INTERVAL '5 minutes')",
    )
    .bind("dispatch-degraded")
    .execute(&pool)
    .await
    .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let public_json: serde_json::Value = reqwest::get(format!("http://{addr}/api/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(public_json["status"], "degraded");
    assert_eq!(public_json["server_up"], true);
    assert_eq!(public_json["fully_recovered"], true);

    let defaults_path = format!("{}/scripts/_defaults.sh", env!("CARGO_MANIFEST_DIR"));
    let port = addr.port();
    let output = tokio::task::spawn_blocking(move || {
        Command::new("bash")
            .arg("-lc")
            .arg(format!(
                ". \"{defaults_path}\"; wait_for_http_service_health test-health {port} 1 0 0 1",
            ))
            .output()
            .unwrap()
    })
    .await
    .unwrap();

    assert!(
        !output.status.success(),
        "expected wait_for_http_service_health to reject non-reconcile degraded server_up=true; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn health_wait_script_rejects_unhealthy_server_up_response_pg() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());
    let harness = crate::services::discord::health::TestHealthHarness::new().await;
    harness.set_connected(false);
    let app = axum::Router::new().nest(
        "/api",
        test_api_router_with_pg(
            db,
            engine,
            crate::config::Config::default(),
            Some(harness.registry()),
            pg_pool.clone(),
        ),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let unhealthy_json: serde_json::Value = reqwest::get(format!("http://{addr}/api/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(unhealthy_json["status"], "unhealthy");
    assert_eq!(unhealthy_json["server_up"], true);
    assert_eq!(unhealthy_json["fully_recovered"], true);

    let defaults_path = format!("{}/scripts/_defaults.sh", env!("CARGO_MANIFEST_DIR"));
    let port = addr.port();
    let output = tokio::task::spawn_blocking(move || {
        Command::new("bash")
            .arg("-lc")
            .arg(format!(
                ". \"{defaults_path}\"; wait_for_http_service_health test-health {port} 2 0 0 1",
            ))
            .output()
            .unwrap()
    })
    .await
    .unwrap();

    assert!(
        !output.status.success(),
        "expected wait_for_http_service_health to reject unhealthy server_up=true; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn stats_memento_endpoint_reports_hourly_counts_and_dedup_hits() {
    crate::services::memory::reset_memento_throttle_for_tests();
    crate::services::memory::note_memento_tool_request("recall");
    crate::services::memory::note_memento_remote_call("recall");
    crate::services::memory::note_memento_tool_request("recall");
    crate::services::memory::note_memento_dedup_hit("recall");
    crate::services::memory::note_memento_tool_request("remember");
    crate::services::memory::note_memento_remote_call("remember");
    crate::services::memory::note_memento_tool_feedback_trigger("automatic");
    crate::services::memory::note_memento_tool_feedback_trigger("manual");

    let db = test_db();
    crate::db::memento_feedback_stats::upsert_turn_stat(
        &db,
        &crate::db::memento_feedback_stats::MementoFeedbackTurnStat {
            turn_id: "turn-memento-stats".to_string(),
            stat_date: "2026-04-29".to_string(),
            agent_id: "project-agentdesk".to_string(),
            provider: "codex".to_string(),
            recall_count: 2,
            manual_tool_feedback_count: 1,
            manual_covered_recall_count: 1,
            auto_tool_feedback_count: 1,
            covered_recall_count: 2,
        },
    )
    .unwrap();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/stats/memento?hours=1")
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
    assert_eq!(json["timezone"], "Asia/Seoul");
    assert_eq!(json["window_hours"], 1);
    assert_eq!(json["summary"]["request_count"], 3);
    assert_eq!(json["summary"]["remote_call_count"], 2);
    assert_eq!(json["summary"]["dedup_hit_count"], 1);
    assert_eq!(json["tools"]["recall"]["request_count"], 2);
    assert_eq!(json["tools"]["recall"]["dedup_hit_count"], 1);
    assert_eq!(json["tools"]["remember"]["remote_call_count"], 1);
    assert_eq!(json["hours"][0]["counts"]["request_count"], 3);
    assert_eq!(
        json["searchObservability"]["feedback_counts_by_trigger_type"]["automatic"],
        1
    );
    assert_eq!(
        json["searchObservability"]["feedback_counts_by_trigger_type"]["voluntary"],
        1
    );
    assert_eq!(
        json["searchObservability"]["persisted_feedback_counts_by_trigger_type"]["automatic"],
        1
    );
    assert_eq!(
        json["searchObservability"]["persisted_feedback_counts_by_trigger_type"]["voluntary"],
        1
    );

    crate::services::memory::reset_memento_throttle_for_tests();
}

#[tokio::test]
async fn health_api_pg_includes_latest_config_audit_report() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let harness = crate::services::discord::health::TestHealthHarness::new().await;
    let app = axum::Router::new().nest(
        "/api",
        test_api_router_with_pg(
            db.clone(),
            engine,
            crate::config::Config::default(),
            Some(harness.registry()),
            pool.clone(),
        ),
    );

    {
        let report = crate::services::discord_config_audit::ConfigAuditReport {
            generated_at: "2026-04-11T01:23:45Z".to_string(),
            status: "warn".to_string(),
            dry_run: false,
            warnings_count: 1,
            warnings: vec!["DB agent 'alpha' differs from agentdesk.yaml on provider".to_string()],
            actions: vec![
                "synced 1 agent definitions from agentdesk.yaml into the agents table".to_string(),
            ],
            sources: crate::services::discord_config_audit::ConfigAuditSources {
                yaml_path: "/tmp/agentdesk.yaml".to_string(),
                yaml_present: true,
                role_map_path: Some("/tmp/role_map.json".to_string()),
                role_map_present: true,
                bot_settings_path: Some("/tmp/bot_settings.json".to_string()),
                bot_settings_present: false,
            },
            storage: crate::services::discord_config_audit::ConfigAuditDbSummary {
                missing_agents: Vec::new(),
                extra_agents: Vec::new(),
                mismatched_agents: vec!["alpha".to_string()],
                synced_agents: Some(1),
            },
        };
        sqlx::query(
            "INSERT INTO kv_meta (key, value) VALUES ('config_audit_report', $1)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(serde_json::to_string(&report).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let json: serde_json::Value = reqwest::get(format!("http://{addr}/api/health/detail"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(json["config_audit"]["status"], "warn");
    assert_eq!(json["config_audit"]["warnings_count"], 1);
    assert_eq!(json["config_audit"]["db"]["mismatched_agents"][0], "alpha");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn health_api_pg_includes_pipeline_override_report_and_degraded_reason() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let harness = crate::services::discord::health::TestHealthHarness::new().await;
    let app = axum::Router::new().nest(
        "/api",
        test_api_router_with_pg(
            db.clone(),
            engine,
            crate::config::Config::default(),
            Some(harness.registry()),
            pool.clone(),
        ),
    );

    {
        let report = crate::pipeline::PipelineOverrideHealthReport {
            generated_at: "2026-04-20T00:00:00Z".to_string(),
            status: "warn".to_string(),
            warnings_count: 1,
            warnings: vec![
                "repo override alpha replaces transitions and drops 2 inherited entries"
                    .to_string(),
            ],
            parse_failures: Vec::new(),
            replace_warnings: vec![crate::pipeline::PipelineOverrideReplaceWarning {
                layer: "repo".to_string(),
                target_id: "alpha".to_string(),
                section: "transitions".to_string(),
                dropped_count: 2,
                dropped_items: vec![
                    "backlog->in_progress".to_string(),
                    "in_progress->done".to_string(),
                ],
            }],
        };
        sqlx::query(
            "INSERT INTO kv_meta (key, value) VALUES ('pipeline_override_health_report', $1)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(serde_json::to_string(&report).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let json: serde_json::Value = reqwest::get(format!("http://{addr}/api/health/detail"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(json["status"], "degraded");
    assert_eq!(json["pipeline_overrides"]["status"], "warn");
    assert_eq!(json["pipeline_overrides"]["warnings_count"], 1);
    assert_eq!(
        json["pipeline_overrides"]["replace_warnings"][0]["target_id"],
        "alpha"
    );
    assert!(
        json["degraded_reasons"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(serde_json::Value::as_str)
            .any(|reason| reason == "pipeline_override_warnings:1"),
        "expected pipeline_override_warnings degraded reason, got {:?}",
        json["degraded_reasons"]
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn health_pg_returns_ok_with_db_status() {
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
                .uri("/health")
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
    assert_eq!(json["ok"], true);
    assert_eq!(json["db"], true);
    assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn health_docs_describe_server_up_and_fully_recovered() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/ops")
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
    let endpoints = json["endpoints"]
        .as_array()
        .expect("docs/ops must return endpoint array");
    let health = endpoints
        .iter()
        .find(|endpoint| endpoint["method"] == "GET" && endpoint["path"] == "/api/health")
        .expect("health docs must include GET /api/health");

    let description = health["description"]
        .as_str()
        .expect("health endpoint description must be present");
    assert!(description.contains("server_up"));
    assert!(description.contains("fully_recovered"));
    assert_eq!(health["example"]["response"]["server_up"], true);
    assert_eq!(health["example"]["response"]["fully_recovered"], true);
    assert_eq!(
        health["example"]["response"]["latest_startup_doctor"]["detail_endpoint"],
        "/api/doctor/startup/latest"
    );
}

#[tokio::test]
async fn health_docs_list_doctor_discovery_endpoints() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/observability/health")
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
    let endpoints = json["endpoints"]
        .as_array()
        .expect("docs/observability/health must return endpoint array");
    for (method, path) in [
        ("GET", "/api/health"),
        ("GET", "/api/health/detail"),
        ("GET", "/api/doctor/startup/latest"),
        ("POST", "/api/doctor/stale-mailbox/repair"),
        ("POST", "/api/channels/{id}/relay-recovery"),
    ] {
        assert!(
            endpoints
                .iter()
                .any(|ep| ep["method"] == method && ep["path"] == path),
            "health docs must include {method} {path}"
        );
    }
}

#[tokio::test]
async fn health_api_includes_pipeline_override_report_from_postgres_without_sqlite_mirror() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());
    let harness = crate::services::discord::health::TestHealthHarness::new().await;
    let app = axum::Router::new().nest(
        "/api",
        test_api_router_with_pg(
            db.clone(),
            engine,
            crate::config::Config::default(),
            Some(harness.registry()),
            pg_pool.clone(),
        ),
    );

    let report = crate::pipeline::PipelineOverrideHealthReport {
        generated_at: "2026-04-22T00:00:00Z".to_string(),
        status: "warn".to_string(),
        warnings_count: 1,
        warnings: vec!["repo override pg warns".to_string()],
        parse_failures: Vec::new(),
        replace_warnings: vec![crate::pipeline::PipelineOverrideReplaceWarning {
            layer: "repo".to_string(),
            target_id: "owner/pg-pipeline-config".to_string(),
            section: "hooks".to_string(),
            dropped_count: 1,
            dropped_items: vec!["review".to_string()],
        }],
    };

    sqlx::query(
        "INSERT INTO kv_meta (key, value)
         VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE
         SET value = EXCLUDED.value",
    )
    .bind("pipeline_override_health_report")
    .bind(serde_json::to_string(&report).unwrap())
    .execute(&pg_pool)
    .await
    .unwrap();

    let sqlite_report_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM kv_meta WHERE key = 'pipeline_override_health_report'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sqlite_report_count, 0, "sqlite mirror should stay empty");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let json: serde_json::Value = reqwest::get(format!("http://{addr}/api/health/detail"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(json["status"], "degraded");
    assert_eq!(json["pipeline_overrides"]["status"], "warn");
    assert_eq!(json["pipeline_overrides"]["warnings_count"], 1);
    assert_eq!(
        json["pipeline_overrides"]["replace_warnings"][0]["target_id"],
        "owner/pg-pipeline-config"
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn stats_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    sqlx::query(
        "INSERT INTO offices (id, name, sort_order, created_at)
         VALUES ($1, $2, 0, NOW())",
    )
    .bind("office-pg-stats")
    .bind("PG Stats Office")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO departments (id, name, office_id, sort_order, created_at)
         VALUES ($1, $2, $3, 0, NOW())",
    )
    .bind("dept-pg-stats")
    .bind("PG Stats Department")
    .bind("office-pg-stats")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO agents (
            id, name, name_ko, department, avatar_emoji, status, xp, sprite_number, created_at, updated_at
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW(), NOW())",
    )
    .bind("agent-pg-stats")
    .bind("PG Stats Agent")
    .bind("피지 통계 에이전트")
    .bind("dept-pg-stats")
    .bind("🤖")
    .bind("idle")
    .bind(42_i32)
    .bind(7_i32)
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO office_agents (office_id, agent_id, department_id)
         VALUES ($1, $2, $3)",
    )
    .bind("office-pg-stats")
    .bind("agent-pg-stats")
    .bind("dept-pg-stats")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO sessions (
            session_key, agent_id, status, active_dispatch_id, tokens, last_heartbeat
         ) VALUES ($1, $2, $3, $4, $5, NOW())",
    )
    .bind("session-pg-stats")
    .bind("agent-pg-stats")
    .bind("turn_active")
    .bind("dispatch-working-pg-stats")
    .bind(123_i32)
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, assigned_agent_id, github_issue_url, created_at, updated_at, completed_at
         ) VALUES ($1, $2, $3, $4, $5, $6, NOW(), NOW(), NOW())",
    )
    .bind("card-pg-done")
    .bind("owner/pg-stats-repo")
    .bind("Done Card")
    .bind("done")
    .bind("agent-pg-stats")
    .bind("https://github.com/owner/pg-stats-repo/issues/1")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, assigned_agent_id, started_at, created_at, updated_at
         ) VALUES (
            $1, $2, $3, 'in_progress', $4,
            NOW() - INTERVAL '3 hours',
            NOW() - INTERVAL '3 hours',
            NOW() - INTERVAL '3 hours'
         )",
    )
    .bind("card-pg-stale")
    .bind("owner/pg-stats-repo")
    .bind("Stale Card")
    .bind("agent-pg-stats")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
         ) VALUES (
            $1, $2, $3, 'work', 'pending', 'Dispatch',
            NOW() - INTERVAL '3 hours',
            NOW() - INTERVAL '3 hours'
         )",
    )
    .bind("dispatch-pg-stale")
    .bind("card-pg-stale")
    .bind("agent-pg-stats")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query("UPDATE kanban_cards SET latest_dispatch_id = $1 WHERE id = $2")
        .bind("dispatch-pg-stale")
        .bind("card-pg-stale")
        .execute(&pg_pool)
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, assigned_agent_id, created_at, updated_at
         ) VALUES ($1, $2, $3, 'review', $4, NOW(), NOW())",
    )
    .bind("card-pg-review")
    .bind("owner/pg-stats-repo")
    .bind("Review Card")
    .bind("agent-pg-stats")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, assigned_agent_id, created_at, updated_at
         ) VALUES ($1, $2, $3, 'requested', $4, NOW(), NOW())",
    )
    .bind("card-pg-requested")
    .bind("owner/pg-stats-repo")
    .bind("Requested Card")
    .bind("agent-pg-stats")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, assigned_agent_id, review_status, blocked_reason, created_at, updated_at
         ) VALUES ($1, $2, $3, 'failed', $4, $5, $6, NOW(), NOW())",
    )
    .bind("card-pg-failed")
    .bind("owner/pg-stats-repo")
    .bind("Failed Card")
    .bind("agent-pg-stats")
    .bind("changes_requested")
    .bind("manual-intervention-required")
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
                .uri("/stats?officeId=office-pg-stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "stats_pg_only_without_sqlite_mirror status={} body={}",
        status,
        String::from_utf8_lossy(&body)
    );

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["agents"]["total"], json!(1));
    assert_eq!(json["agents"]["working"], json!(1));
    assert_eq!(json["dispatched_count"], json!(1));
    assert_eq!(json["top_agents"][0]["id"], json!("agent-pg-stats"));
    assert_eq!(json["top_agents"][0]["stats_tasks_done"], json!(1));
    assert_eq!(json["top_agents"][0]["stats_tokens"], json!(123));
    assert_eq!(json["departments"][0]["id"], json!("dept-pg-stats"));
    assert_eq!(json["departments"][0]["working_agents"], json!(1));
    assert_eq!(json["kanban"]["review_queue"], json!(1));
    assert_eq!(json["kanban"]["waiting_acceptance"], json!(1));
    assert_eq!(json["kanban"]["failed"], json!(1));
    assert_eq!(json["kanban"]["blocked"], json!(1));
    assert_eq!(json["kanban"]["stale_in_progress"], json!(1));
    assert_eq!(
        json["kanban"]["top_repos"][0]["github_repo"],
        json!("owner/pg-stats-repo")
    );
    assert_eq!(json["github_closed_today"], json!(1));

    let sqlite_agent_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM agents WHERE id = 'agent-pg-stats'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sqlite_agent_count, 0, "sqlite mirror should stay empty");

    pg_pool.close().await;
    pg_db.drop().await;
}
