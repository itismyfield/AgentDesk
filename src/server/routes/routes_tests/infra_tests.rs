//! Domain-split routes tests — `infra` group.
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
async fn protected_domain_router_only_keeps_expected_auth_exemptions() {
    let db = test_db();
    let engine = test_engine(&db);
    let mut config = crate::config::Config::default();
    config.server.auth_token = Some("secret-token".to_string());
    let state = AppState::test_state_with_config(db, engine, config);
    let app = protected_api_domain(
        axum::Router::new()
            .route(
                "/internal/ping",
                axum::routing::get(|| async { StatusCode::OK }),
            )
            .route(
                "/dispatched-sessions/webhook",
                axum::routing::post(|| async { StatusCode::CREATED }),
            )
            .route(
                "/discord/send",
                axum::routing::post(|| async { StatusCode::OK }),
            )
            .route(
                "/discord/send-to-agent",
                axum::routing::post(|| async { StatusCode::OK }),
            )
            .route(
                "/discord/send-dm",
                axum::routing::post(|| async { StatusCode::OK }),
            )
            .route("/settings", axum::routing::get(|| async { StatusCode::OK })),
        state.clone(),
    )
    .with_state(state);

    let internal_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/internal/ping")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(internal_response.status(), StatusCode::OK);

    let hook_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatched-sessions/webhook")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(hook_response.status(), StatusCode::CREATED);

    let protected_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/settings")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(protected_response.status(), StatusCode::UNAUTHORIZED);

    let send_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/discord/send")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(send_response.status(), StatusCode::UNAUTHORIZED);

    let send_to_agent_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/discord/send-to-agent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(send_to_agent_response.status(), StatusCode::UNAUTHORIZED);

    let senddm_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/discord/send-dm")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(senddm_response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn auth_middleware_same_origin_uses_parsed_loopback_host_and_port() {
    let db = test_db();
    let engine = test_engine(&db);
    let mut config = crate::config::Config::default();
    config.server.port = 8791;
    config.server.auth_token = Some("secret-token".to_string());
    let state = AppState::test_state_with_config(db, engine, config);
    let app = protected_api_domain(
        axum::Router::new().route("/settings", axum::routing::get(|| async { StatusCode::OK })),
        state.clone(),
    )
    .with_state(state);

    for origin in [
        "http://localhost:8791",
        "http://127.0.0.1:8791",
        "http://[::1]:8791",
        "https://localhost:8791",
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/settings")
                    .header("origin", origin)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "origin {origin}");
    }

    let referer_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/settings")
                .header("referer", "http://localhost:8791/settings")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(referer_response.status(), StatusCode::OK);

    for origin in [
        "http://localhost.evil.example:8791",
        "http://127.0.0.1.evil.example:8791",
        "http://localhost:8792",
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/settings")
                    .header("origin", origin)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "origin {origin}"
        );
    }
}

#[tokio::test]
async fn discord_control_endpoints_require_auth_token_on_non_loopback_host() {
    let db = test_db();
    let engine = test_engine(&db);
    let mut config = crate::config::Config::default();
    config.server.host = "0.0.0.0".to_string();
    let app = test_api_router_with_config(db, engine, config, None);

    let mut request = Request::builder()
        .method("POST")
        .uri("/discord/send")
        .body(Body::from("{}"))
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "10.0.0.5:8791".parse::<std::net::SocketAddr>().unwrap(),
    ));

    let response = app.clone().oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let mut send_to_agent_request = Request::builder()
        .method("POST")
        .uri("/discord/send-to-agent")
        .body(Body::from(r#"{"role_id":"ch-pd","message":"hello"}"#))
        .unwrap();
    send_to_agent_request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(
            "10.0.0.5:8791".parse::<std::net::SocketAddr>().unwrap(),
        ));

    let send_to_agent_response = app.oneshot(send_to_agent_request).await.unwrap();

    assert_eq!(send_to_agent_response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn discord_control_endpoints_allow_loopback_without_auth_token() {
    let db = test_db();
    let engine = test_engine(&db);
    let mut config = crate::config::Config::default();
    config.server.host = "127.0.0.1".to_string();
    let app = test_api_router_with_config(db, engine, config, None);

    let mut request = Request::builder()
        .method("POST")
        .uri("/discord/send")
        .body(Body::from("{}"))
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.1:8791".parse::<std::net::SocketAddr>().unwrap(),
    ));

    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn send_to_agent_returns_not_found_for_unknown_role_id() {
    let db = test_db();
    let engine = test_engine(&db);
    let health_registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
    let app = test_api_router(db, engine, Some(health_registry));

    let mut request = Request::builder()
        .method("POST")
        .uri("/discord/send-to-agent")
        .body(Body::from(r#"{"role_id":"missing","message":"hello"}"#))
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.1:8791".parse::<std::net::SocketAddr>().unwrap(),
    ));

    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], false);
    assert_eq!(json["error"], "unknown agent target: missing");
}

#[tokio::test]
async fn public_domain_router_wraps_plain_server_errors_in_app_error_json() {
    let db = test_db();
    let engine = test_engine(&db);
    let state = AppState::test_state(db, engine);
    let app = public_api_domain(axum::Router::new().route(
        "/boom",
        axum::routing::get(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
    ))
    .with_state(state);

    let response = app
        .oneshot(Request::builder().uri("/boom").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"], "internal server error");
    assert_eq!(json["code"], "internal");
}

#[tokio::test]
async fn offices_reorder_pg_accepts_bare_array_and_updates_listing_order() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query("INSERT INTO offices (id, name, sort_order) VALUES ('office-a', 'Alpha', 2)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO offices (id, name, sort_order) VALUES ('office-b', 'Beta', 0)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO offices (id, name, sort_order) VALUES ('office-c', 'Gamma', 1)")
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
    let reorder_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/offices/reorder")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"[{"id":"office-a","sort_order":1},{"id":"office-b","sort_order":2},{"id":"office-c","sort_order":0}]"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(reorder_response.status(), StatusCode::OK);
    let reorder_body = axum::body::to_bytes(reorder_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let reorder_json: serde_json::Value = serde_json::from_slice(&reorder_body).unwrap();
    assert_eq!(reorder_json["ok"], true);
    assert_eq!(reorder_json["updated"], 3);

    let list_response = app
        .oneshot(
            Request::builder()
                .uri("/offices")
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
    let offices = list_json["offices"].as_array().unwrap();

    assert_eq!(offices.len(), 3);
    assert_eq!(offices[0]["id"], "office-c");
    assert_eq!(offices[0]["sort_order"], 0);
    assert_eq!(offices[1]["id"], "office-a");
    assert_eq!(offices[1]["sort_order"], 1);
    assert_eq!(offices[2]["id"], "office-b");
    assert_eq!(offices[2]["sort_order"], 2);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn offices_reorder_rejects_wrapped_order_body() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/offices/reorder")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"order":[{"id":"office-a","sort_order":0}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn round_table_meeting_channels_endpoint_does_not_fall_through_to_meeting_lookup() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = axum::Router::new().nest("/api", test_api_router(db, engine, None));

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/round-table-meetings/channels")
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

    assert!(
        json["channels"].is_array(),
        "expected channels array, got {json}"
    );
    assert_ne!(json["error"], json!("meeting not found"));
}

#[tokio::test]
async fn round_table_meeting_channels_endpoint_returns_configured_experts_and_fallback_name() {
    let _lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());

    let mut config = crate::config::Config::default();
    config.agents = vec![
        crate::config::AgentDef {
            id: "meeting-host".to_string(),
            name: "Meeting Host".to_string(),
            name_ko: None,
            aliases: Vec::new(),
            wake_word: None,
            voice_enabled: true,
            sensitivity_mode: None,
            provider: "codex".to_string(),
            channels: crate::config::AgentChannels {
                codex: Some(crate::config::AgentChannel::Detailed(
                    crate::config::AgentChannelConfig {
                        id: Some("123456789".to_string()),
                        name: Some("meeting-room".to_string()),
                        provider: Some("codex".to_string()),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            },
            keywords: vec!["facilitation".to_string()],
            department: None,
            avatar_emoji: None,
        },
        crate::config::AgentDef {
            id: "qwen".to_string(),
            name: "QWEN".to_string(),
            name_ko: None,
            aliases: Vec::new(),
            wake_word: None,
            voice_enabled: true,
            sensitivity_mode: None,
            provider: "qwen".to_string(),
            channels: crate::config::AgentChannels::default(),
            keywords: vec!["planning".to_string()],
            department: None,
            avatar_emoji: None,
        },
        crate::config::AgentDef {
            id: "gemini".to_string(),
            name: "GEMINI".to_string(),
            name_ko: None,
            aliases: Vec::new(),
            wake_word: None,
            voice_enabled: true,
            sensitivity_mode: None,
            provider: "gemini".to_string(),
            channels: crate::config::AgentChannels::default(),
            keywords: vec!["analysis".to_string()],
            department: None,
            avatar_emoji: None,
        },
    ];
    config.meeting = Some(crate::config::MeetingSettings {
        channel_name: "meeting-room".to_string(),
        max_rounds: Some(3),
        max_participants: Some(4),
        summary_agent: Some(crate::config::MeetingSummaryAgentDef::Static(
            "meeting-host".to_string(),
        )),
        available_agents: vec![
            crate::config::MeetingAgentEntry::RoleId("qwen".to_string()),
            crate::config::MeetingAgentEntry::RoleId("gemini".to_string()),
        ],
    });

    let config_path = crate::runtime_layout::config_file_path(runtime_root.path());
    crate::config::save_to_path(&config_path, &config).unwrap();

    let db = test_db();
    let engine = test_engine(&db);
    let health_registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
    let app = axum::Router::new().nest("/api", test_api_router(db, engine, Some(health_registry)));

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/round-table-meetings/channels")
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

    let channels = json["channels"].as_array().unwrap();
    assert_eq!(channels.len(), 1, "expected one registered meeting channel");
    assert_eq!(channels[0]["channel_id"], json!("123456789"));
    assert_eq!(channels[0]["channel_name"], json!("meeting-room"));

    let experts = channels[0]["available_experts"].as_array().unwrap();
    assert_eq!(experts.len(), 2, "expected configured meeting experts");
    assert!(experts.iter().any(|expert| {
        expert["role_id"] == json!("qwen") && expert["provider_hint"] == json!("qwen")
    }));
    assert!(experts.iter().any(|expert| {
        expert["role_id"] == json!("gemini") && expert["provider_hint"] == json!("gemini")
    }));
}

#[tokio::test]
async fn hooks_skill_usage_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id)
         VALUES ($1, $2, '111')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind("agent-pg-hook-skill")
    .bind("Hook Skill Agent")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO skills (id, name, description, source_path, updated_at)
         VALUES ($1, $2, $3, $4, NOW())",
    )
    .bind("skill-pg-hook")
    .bind("PG Hook Skill")
    .bind("PG hook skill")
    .bind("/tmp/skill-pg-hook")
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
                .uri("/hook/skill-usage")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{
                        "skill_id":"skill-pg-hook",
                        "role_id":"agent-pg-hook-skill",
                        "session_key":"session-pg-hook"
                    }"#,
                ))
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
        "hooks_skill_usage_pg_only_without_sqlite_mirror status={} body={}",
        status,
        body_text
    );

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], json!(true));

    let skill_usage_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT FROM skill_usage WHERE skill_id = $1 AND agent_id = $2 AND session_key = $3",
    )
    .bind("skill-pg-hook")
    .bind("agent-pg-hook-skill")
    .bind("session-pg-hook")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(skill_usage_count, 1);

    let sqlite_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM skill_usage", [], |row| row.get(0))
        .unwrap();
    assert_eq!(sqlite_count, 0, "sqlite mirror should stay empty");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn hooks_disconnect_session_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id)
         VALUES ($1, $2, '111')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind("agent-pg-hook-session")
    .bind("Hook Session Agent")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions (
            session_key, agent_id, provider, status, created_at
         ) VALUES (
            $1, $2, $3, $4, NOW()
         )",
    )
    .bind("session-pg-hook-disconnect")
    .bind("agent-pg-hook-session")
    .bind("claude")
    .bind("turn_active")
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
                .uri("/hook/session/session-pg-hook-disconnect")
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
        "hooks_disconnect_session_pg_only_without_sqlite_mirror status={} body={}",
        status,
        body_text
    );

    let session_status: String =
        sqlx::query_scalar("SELECT status FROM sessions WHERE session_key = $1")
            .bind("session-pg-hook-disconnect")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(session_status, "disconnected");

    let sqlite_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE session_key = 'session-pg-hook-disconnect'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sqlite_count, 0, "sqlite mirror should stay empty");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn departments_roundtrip_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    sqlx::query(
        "INSERT INTO offices (id, name, sort_order, created_at)
         VALUES ($1, $2, 0, NOW())",
    )
    .bind("office-pg-dept")
    .bind("PG Office")
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

    let create_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/departments")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"name":"PG Department","office_id":"office-pg-dept"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_response.status(), StatusCode::CREATED);
    let create_body = axum::body::to_bytes(create_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let create_json: serde_json::Value = serde_json::from_slice(&create_body).unwrap();
    let department_id = create_json["department"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let pg_created_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM departments WHERE id = $1")
            .bind(&department_id)
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(pg_created_count, 1);

    let sqlite_created_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM departments WHERE id = ?1",
            [&department_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sqlite_created_count, 0, "sqlite mirror should stay empty");

    let update_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/departments/{department_id}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"PG Department Updated"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(update_response.status(), StatusCode::OK);
    let update_body = axum::body::to_bytes(update_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let update_json: serde_json::Value = serde_json::from_slice(&update_body).unwrap();
    assert_eq!(
        update_json["department"]["name"],
        json!("PG Department Updated")
    );

    let list_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/departments?officeId=office-pg-dept")
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
    let departments = list_json["departments"].as_array().unwrap();
    assert_eq!(departments.len(), 1);
    assert_eq!(departments[0]["id"], json!(department_id.clone()));
    assert_eq!(departments[0]["name"], json!("PG Department Updated"));
    assert_eq!(departments[0]["office_id"], json!("office-pg-dept"));

    let reorder_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/departments/reorder")
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"order":[{{"id":"{department_id}","sort_order":7}}]}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reorder_response.status(), StatusCode::OK);

    let pg_sort_order: i64 =
        sqlx::query_scalar("SELECT sort_order::BIGINT FROM departments WHERE id = $1")
            .bind(&department_id)
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(pg_sort_order, 7);

    let delete_response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/departments/{department_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(delete_response.status(), StatusCode::OK);

    let pg_remaining_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM departments WHERE id = $1")
            .bind(&department_id)
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(pg_remaining_count, 0);

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn offices_roundtrip_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    let app = test_api_router_with_pg(
        db.clone(),
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
                .uri("/offices")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"PG Office","layout":"grid"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_response.status(), StatusCode::CREATED);
    let create_body = axum::body::to_bytes(create_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let create_json: serde_json::Value = serde_json::from_slice(&create_body).unwrap();
    let office_id = create_json["office"]["id"].as_str().unwrap().to_string();

    let sqlite_office_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM offices WHERE id = ?1",
            [&office_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sqlite_office_count, 0, "sqlite mirror should stay empty");

    let update_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/offices/{office_id}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"name":"PG Office Updated","layout":"stack"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(update_response.status(), StatusCode::OK);

    let add_agent_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/offices/{office_id}/agents"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"agent_id":"pg-office-agent-1","department_id":"dept-alpha"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(add_agent_response.status(), StatusCode::OK);

    let update_agent_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/offices/{office_id}/agents/pg-office-agent-1"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"department_id":"dept-beta"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(update_agent_response.status(), StatusCode::OK);

    let batch_add_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/offices/{office_id}/agents/batch"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"agent_ids":["pg-office-agent-2","pg-office-agent-3"]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(batch_add_response.status(), StatusCode::OK);

    let pg_agent_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM office_agents WHERE office_id = $1")
            .bind(&office_id)
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(pg_agent_count, 3);

    let pg_department_id: Option<String> = sqlx::query_scalar(
        "SELECT department_id FROM office_agents WHERE office_id = $1 AND agent_id = $2",
    )
    .bind(&office_id)
    .bind("pg-office-agent-1")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(pg_department_id.as_deref(), Some("dept-beta"));

    let sqlite_office_agent_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM office_agents WHERE office_id = ?1",
            [&office_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        sqlite_office_agent_count, 0,
        "sqlite office_agents mirror should stay empty"
    );

    let reorder_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/offices/reorder")
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"[{{"id":"{office_id}","sort_order":4}}]"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reorder_response.status(), StatusCode::OK);

    let list_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/offices")
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
    let offices = list_json["offices"].as_array().unwrap();
    assert_eq!(offices.len(), 1);
    assert_eq!(offices[0]["id"], json!(office_id.clone()));
    assert_eq!(offices[0]["name"], json!("PG Office Updated"));
    assert_eq!(offices[0]["agent_count"], json!(3));
    assert_eq!(offices[0]["sort_order"], json!(4));

    let remove_agent_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/offices/{office_id}/agents/pg-office-agent-2"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(remove_agent_response.status(), StatusCode::OK);

    let pg_agent_count_after_remove: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM office_agents WHERE office_id = $1")
            .bind(&office_id)
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(pg_agent_count_after_remove, 2);

    let delete_response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/offices/{office_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(delete_response.status(), StatusCode::OK);

    let pg_remaining_offices: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM offices WHERE id = $1")
            .bind(&office_id)
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(pg_remaining_offices, 0);

    let pg_remaining_links: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM office_agents WHERE office_id = $1")
            .bind(&office_id)
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(pg_remaining_links, 0);

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn removed_legacy_routes_return_not_found() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    for (method, uri) in [
        ("GET", "/agent-channels"),
        ("POST", "/dispatch-cancel/dispatch-123"),
        ("GET", "/pipeline-stages"),
        ("POST", "/pipeline-stages"),
        ("DELETE", "/pipeline-stages/legacy-stage"),
        ("POST", "/session/start"),
        ("GET", "/sessions/search"),
        ("POST", "/sessions/force-kill"),
        ("POST", "/auto-queue/enqueue"),
        ("GET", "/api-friction/events"),
        ("GET", "/api-friction/patterns"),
        ("POST", "/api-friction/process"),
        // #1064 removals
        ("POST", "/re-review"),
        ("POST", "/hook/session"),
        ("DELETE", "/hook/session"),
        ("POST", "/auto-queue/activate"),
        ("POST", "/auto-queue/dispatch"),
        ("POST", "/queue/activate"),
        ("POST", "/queue/dispatch"),
        ("POST", "/kanban-cards/bulk-action"),
        ("POST", "/kanban-cards/batch-transition"),
        ("POST", "/kanban-cards/card-x/force-transition"),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // 404 when the path is fully removed; 405 when the removed endpoint
        // collided with a remaining `{id}`-style wildcard route that now
        // rejects the method (e.g. /kanban-cards/bulk-action matching the
        // /kanban-cards/{id} GET/PATCH/DELETE route).
        assert!(
            matches!(
                response.status(),
                StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
            ),
            "{method} {uri} should return 404/405 after route cleanup, got {}",
            response.status()
        );
    }
}

#[tokio::test]
async fn api_help_exposes_detailed_endpoint_inventory() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(Request::builder().uri("/help").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["categories"]
            .as_array()
            .unwrap()
            .iter()
            .any(|category| category["name"] == "queue"),
        "/help must expose category summaries"
    );
    let generate = json["endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|ep| ep["path"] == "/api/queue/generate")
        .expect("/help must include the queue generate endpoint");
    assert_eq!(generate["params"]["entries"]["required"], false);
}

// #1064: /api/kanban-cards/batch-transition and bulk-action were removed in
// favour of per-card POST /api/kanban-cards/{id}/transition. The paths now
// collide with the /kanban-cards/{id} wildcard (GET/PATCH/DELETE), so POST
// against them returns 405 Method Not Allowed — still unambiguously "not
// served" from the caller's perspective.
#[tokio::test]
async fn removed_batch_transition_route_is_unserved() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_card_with_status(&db, "card-bt-1", "backlog");
    set_pmd_channel(&db, "pmd-chan-123");

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/batch-transition")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(r#"{"card_ids":["card-bt-1"],"status":"ready"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(matches!(
        response.status(),
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
    ));
}

#[tokio::test]
async fn removed_bulk_action_route_is_unserved() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_card_with_status(&db, "card-ba-1", "backlog");

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/bulk-action")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"action":"pass","card_ids":["card-ba-1"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(matches!(
        response.status(),
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
    ));
}

#[tokio::test]
async fn postgres_force_transition_to_ready_cleans_up_live_state() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());
    seed_agent(&db, "agent-ft-clean-pg");
    seed_repo(&db, "test-repo");
    set_pmd_channel(&db, "pmd-chan-123");
    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO agents (id, name) VALUES ($1, $2)")
        .bind("agent-ft-clean-pg")
        .bind("Agent Force Transition Cleanup PG")
        .execute(&pg_pool)
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, repo_id,
            latest_dispatch_id, review_status, review_round, review_notes,
            suggestion_pending_at, review_entered_at, awaiting_dod_at,
            created_at, updated_at, started_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6,
            $7, $8, $9, $10,
            NOW() - INTERVAL '12 minutes', NOW() - INTERVAL '11 minutes', NOW() - INTERVAL '10 minutes',
            NOW() - INTERVAL '20 minutes', NOW() - INTERVAL '20 minutes', NOW() - INTERVAL '20 minutes'
         )",
    )
    .bind("card-ft-clean-pg")
    .bind("Force Transition Cleanup PG")
    .bind("in_progress")
    .bind("medium")
    .bind("agent-ft-clean-pg")
    .bind("test-repo")
    .bind("dispatch-ft-clean-pg")
    .bind("reviewing")
    .bind(4_i64)
    .bind("stale review notes")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, NOW() - INTERVAL '10 minutes', NOW() - INTERVAL '10 minutes'
         )",
    )
    .bind("dispatch-ft-clean-pg")
    .bind("card-ft-clean-pg")
    .bind("agent-ft-clean-pg")
    .bind("implementation")
    .bind("pending")
    .bind("live impl")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions (
            session_key, agent_id, provider, status, active_dispatch_id, last_heartbeat, created_at
         ) VALUES (
            $1, $2, $3, $4, $5, NOW() - INTERVAL '9 minutes', NOW() - INTERVAL '9 minutes'
         )",
    )
    .bind("session-ft-clean-pg")
    .bind("agent-ft-clean-pg")
    .bind("codex")
    .bind("turn_active")
    .bind("dispatch-ft-clean-pg")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES
            ($1, $2, $3, 'active'),
            ($4, $2, $3, 'active')",
    )
    .bind("run-ft-clean-pg")
    .bind("test-repo")
    .bind("agent-ft-clean-pg")
    .bind("run-ft-clean-pg-pending")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, NOW() - INTERVAL '10 minutes'
         )",
    )
    .bind("entry-ft-clean-pg-dispatched")
    .bind("run-ft-clean-pg")
    .bind("card-ft-clean-pg")
    .bind("agent-ft-clean-pg")
    .bind("dispatched")
    .bind("dispatch-ft-clean-pg")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status
         ) VALUES (
            $1, $2, $3, $4, $5
         )",
    )
    .bind("entry-ft-clean-pg-pending")
    .bind("run-ft-clean-pg-pending")
    .bind("card-ft-clean-pg")
    .bind("agent-ft-clean-pg")
    .bind("pending")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO card_review_state (
            card_id, state, pending_dispatch_id, review_round, last_verdict, last_decision,
            approach_change_round, session_reset_round, review_entered_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, NOW() - INTERVAL '11 minutes', NOW()
         )",
    )
    .bind("card-ft-clean-pg")
    .bind("suggestion_pending")
    .bind("old-review-dispatch")
    .bind(4_i64)
    .bind("pass")
    .bind("approved")
    .bind(3_i64)
    .bind(4_i64)
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
                .uri("/kanban-cards/card-ft-clean-pg/transition")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                // #1444: card has a live dispatch — force=true is now required
                // for the ready transition to bypass the idempotency guard.
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
        "unexpected force-transition response: {body_text}"
    );
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["forced"], true);
    // #1235: lower-bound counts; backend-specific cleanup paths can fold in
    // additional rows whose count is not a stable contract.
    let cancelled_dispatches_reported = json["cancelled_dispatches"].as_i64().unwrap_or_default();
    let skipped_entries_reported = json["skipped_auto_queue_entries"]
        .as_i64()
        .unwrap_or_default();
    assert!(
        cancelled_dispatches_reported >= 1,
        "expected at least the live impl dispatch to be cancelled, got {cancelled_dispatches_reported}"
    );
    assert!(
        skipped_entries_reported >= 2,
        "expected at least the 2 seeded auto_queue entries to be skipped, got {skipped_entries_reported}"
    );
    assert_eq!(json["card"]["status"], "ready");

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
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT status, latest_dispatch_id, review_status, review_round, review_notes,
                suggestion_pending_at::text, review_entered_at::text, awaiting_dod_at::text
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind("card-ft-clean-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    let (review_state_round, review_state_status, review_state_pending_dispatch): (
        i64,
        String,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT review_round, state, pending_dispatch_id
         FROM card_review_state
         WHERE card_id = $1",
    )
    .bind("card-ft-clean-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    let dispatch_status: String =
        sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
            .bind("dispatch-ft-clean-pg")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    // #1235: stable-key lookups (id → row) replace Vec equality. Order-only
    // comparisons assumed deterministic IDs and exact row counts; both vary
    // when sibling cleanup paths fan out.
    let entry_rows_vec: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT id, status, dispatch_id
         FROM auto_queue_entries
         WHERE kanban_card_id = $1",
    )
    .bind("card-ft-clean-pg")
    .fetch_all(&pg_pool)
    .await
    .unwrap();
    let entry_rows: std::collections::BTreeMap<String, (String, Option<String>)> = entry_rows_vec
        .into_iter()
        .map(|(id, status, dispatch_id)| (id, (status, dispatch_id)))
        .collect();
    let run_rows_vec: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, status
         FROM auto_queue_runs
         WHERE id IN ($1, $2)",
    )
    .bind("run-ft-clean-pg")
    .bind("run-ft-clean-pg-pending")
    .fetch_all(&pg_pool)
    .await
    .unwrap();
    let run_rows: std::collections::BTreeMap<String, String> = run_rows_vec.into_iter().collect();
    let (session_status, active_dispatch_id): (String, Option<String>) = sqlx::query_as(
        "SELECT status, active_dispatch_id
         FROM sessions
         WHERE session_key = $1",
    )
    .bind("session-ft-clean-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();

    assert_eq!(card_status, "ready");
    assert!(latest_dispatch_id.is_none());
    assert!(review_status.is_none());
    assert_eq!(review_round, 0);
    assert!(review_notes.is_none());
    assert!(suggestion_pending_at.is_none());
    assert!(review_entered_at.is_none());
    assert!(awaiting_dod_at.is_none());
    assert_eq!(review_state_round, 0);
    assert_eq!(review_state_status, "idle");
    assert!(review_state_pending_dispatch.is_none());
    assert_eq!(dispatch_status, "cancelled");
    // #1235: per-id contract assertions. The PG cleanup path keeps the
    // dispatch_id link on the originally-dispatched entry but skips the
    // status; only the status field is the stable contract here.
    let pg_entry_dispatched = entry_rows
        .get("entry-ft-clean-pg-dispatched")
        .expect("seeded dispatched entry must still exist");
    let pg_entry_pending = entry_rows
        .get("entry-ft-clean-pg-pending")
        .expect("seeded pending entry must still exist");
    assert_eq!(
        pg_entry_dispatched.0, "skipped",
        "force-transition cleanup must skip the live (dispatched) auto-queue entry on PG"
    );
    assert_eq!(
        pg_entry_pending.0, "skipped",
        "force-transition cleanup must skip the pending auto-queue entry on PG"
    );
    assert!(
        pg_entry_pending.1.is_none(),
        "pending entry never had a dispatch_id; cleanup must keep it unset on PG, got {:?}",
        pg_entry_pending.1
    );
    assert_eq!(
        run_rows.get("run-ft-clean-pg").map(String::as_str),
        Some("completed"),
        "force-transition cleanup must complete the live run on PG"
    );
    assert_eq!(
        run_rows.get("run-ft-clean-pg-pending").map(String::as_str),
        Some("completed"),
        "force-transition cleanup must complete the pending-only run on PG"
    );
    assert_eq!(session_status, "idle");
    assert!(active_dispatch_id.is_none());

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn postgres_reopen_updates_review_tuning_outcome_in_postgres() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    seed_agent(&db, "agent-reopen-pg");
    seed_repo(&db, "test-repo");
    set_pmd_channel(&db, "pmd-chan-123");
    ensure_auto_queue_tables(&db);
    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO agents (id, name) VALUES ($1, $2)")
        .bind("agent-reopen-pg")
        .bind("Agent Reopen PG")
        .execute(&pg_pool)
        .await
        .unwrap();
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
            $1, $2, $3, $4, $5, $6, $7, NOW(), NOW(), NOW()
         )",
    )
    .bind("card-reopen-pg")
    .bind("Issue #270 PG")
    .bind("done")
    .bind("medium")
    .bind("agent-reopen-pg")
    .bind("test-repo")
    .bind("pass")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ($1, $2, $3, $4)",
    )
    .bind("run-reopen-pg")
    .bind("test-repo")
    .bind("agent-reopen-pg")
    .bind("active")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, completed_at
         ) VALUES (
            $1, $2, $3, $4, $5, NOW()
         )",
    )
    .bind("entry-reopen-pg")
    .bind("run-reopen-pg")
    .bind("card-reopen-pg")
    .bind("agent-reopen-pg")
    .bind("done")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO review_tuning_outcomes (
            card_id, dispatch_id, review_round, verdict, decision, outcome
         ) VALUES (
            $1, $2, $3, $4, $5, $6
         )",
    )
    .bind("card-reopen-pg")
    .bind("review-pass-pg")
    .bind(1_i32)
    .bind("pass")
    .bind("approved")
    .bind("true_negative")
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
                .uri("/kanban-cards/card-reopen-pg/reopen")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(
                    r#"{"reason":"retry after incorrect pass","review_status":"queued"}"#,
                ))
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
        "unexpected reopen response for card-reopen-pg: {body_text}"
    );
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["reopened"], true);
    assert_eq!(json["to"], reopen_target);
    assert_eq!(json["card"]["status"], reopen_target);

    let outcome: String = sqlx::query_scalar(
        "SELECT outcome
         FROM review_tuning_outcomes
         WHERE card_id = $1
         ORDER BY review_round DESC, id DESC
         LIMIT 1",
    )
    .bind("card-reopen-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(outcome, "false_negative");

    pg_pool.close().await;
    pg_db.drop().await;
}

/// #107 regression: empty claude_session_id must be normalized to NULL at the API
/// boundary so that stale clear paths don't poison the DB with "".
#[tokio::test]
async fn hook_session_pg_normalizes_empty_claude_session_id_to_null() {
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

    // 1. Save a valid claude_session_id
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatched-sessions/webhook")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"session_key":"test:sess1","status":"working","claude_session_id":"valid-id-123"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Verify it was stored
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT claude_session_id FROM sessions WHERE session_key = 'test:sess1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored.as_deref(), Some("valid-id-123"));

    // 2. Send empty string — should be normalized to NULL (not stored as "")
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatched-sessions/webhook")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"session_key":"test:sess1","status":"working","claude_session_id":""}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The COALESCE in the upsert preserves the old value when the new one is NULL,
    // so the valid-id-123 should still be there (empty was normalized to NULL → COALESCE keeps old).
    // This is correct: to actually clear, use the dedicated clear-session-id endpoint.
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT claude_session_id FROM sessions WHERE session_key = 'test:sess1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        stored.as_deref(),
        Some("valid-id-123"),
        "Empty string should be normalized to NULL, and COALESCE keeps the old value"
    );

    // 3. Use the dedicated clear endpoint to actually clear
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatched-sessions/clear-session-id")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"session_key":"test:sess1"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Verify it's actually cleared (NULL)
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT claude_session_id FROM sessions WHERE session_key = 'test:sess1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        stored.is_none(),
        "After clear-session-id, value should be NULL"
    );

    // 4. Verify GET returns null after clear
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/dispatched-sessions/claude-session-id?session_key=test:sess1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["claude_session_id"].is_null(),
        "GET should return null after clear"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn hook_session_pg_persists_raw_provider_session_id_separately() {
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

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatched-sessions/webhook")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"session_key":"test:gemini-raw","status":"working","provider":"gemini","claude_session_id":"latest","session_id":"aa678e6b-c6d3-4dd2-9197-58580c00cc6c"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let stored: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT claude_session_id, raw_provider_session_id
         FROM sessions
         WHERE session_key = 'test:gemini-raw'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored.0.as_deref(), Some("latest"));
    assert_eq!(
        stored.1.as_deref(),
        Some("aa678e6b-c6d3-4dd2-9197-58580c00cc6c")
    );

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/dispatched-sessions/claude-session-id?session_key=test:gemini-raw&provider=gemini")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["claude_session_id"], "latest");
    assert_eq!(json["session_id"], "latest");
    assert_eq!(
        json["raw_provider_session_id"],
        "aa678e6b-c6d3-4dd2-9197-58580c00cc6c"
    );

    pool.close().await;
    pg_db.drop().await;
}

/// #1069 / 904-7 — callsite migration smoke test.
///
/// Scans the dashboard frontend, policies (JS), shell scripts, skills (Markdown),
/// and the example config for references to API paths that were renamed in
/// #1064 / #1065. Server-side route handler files and auto-generated docs are
/// excluded — they legitimately reference the old paths as deprecated aliases
/// or history. The test fails when a new callsite re-introduces a legacy path.
#[test]
fn callsites_migrated_off_legacy_api_paths_1069() {
    use std::path::Path;

    // Banned substrings — these are the paths fully removed in #1064/#1065.
    // /api/hook/session is not banned: the parameterized DELETE
    // (`/api/hook/session/{sessionKey}`) and auth.rs prefix bypass legitimately
    // keep the prefix alive. Callsites should still hit
    // /api/dispatched-sessions/webhook for the unparameterized POST/DELETE.
    let banned: &[&str] = &[
        "/api/re-review",
        "/api/review-verdict",
        "/api/review-decision",
        "/api/review-tuning/",
        "/api/send",
        "/api/send_to_agent",
        "/api/senddm",
        "/api/issues",
        "/api/discord-bindings",
        "/api/auto-queue/",
        "/api/queue/activate",
    ];

    // Roots that frontend / policy / script / skill / config callsites live in.
    let roots = [
        "dashboard/src",
        "policies",
        "scripts",
        "skills",
        "agentdesk.example.yaml",
        "FEATURES.md",
        "README.md",
        "CLAUDE.md",
    ];

    fn walk(p: &Path, out: &mut Vec<std::path::PathBuf>) {
        if p.is_file() {
            out.push(p.to_path_buf());
            return;
        }
        let Ok(entries) = std::fs::read_dir(p) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if matches!(
                name,
                "node_modules" | "dist" | "build" | ".git" | "generated" | "target"
            ) {
                continue;
            }
            if path.is_dir() {
                walk(&path, out);
            } else if path.is_file() {
                out.push(path);
            }
        }
    }

    let repo_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    for r in roots {
        walk(&repo_root.join(r), &mut files);
    }

    let mut hits: Vec<String> = Vec::new();
    for file in &files {
        let ext = file
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !matches!(
            ext.as_str(),
            "ts" | "tsx" | "js" | "jsx" | "mjs" | "json" | "yaml" | "yml" | "sh" | "md"
        ) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(file) else {
            continue;
        };
        let path_str = file.to_string_lossy();
        for needle in banned {
            if !content.contains(needle) {
                continue;
            }
            for line in content.lines() {
                if !line.contains(needle) {
                    continue;
                }
                let trimmed = line.trim_start();
                // Allow comment / doc lines that explicitly call out the
                // historical migration. A live callsite (fetch URL string,
                // bash curl, etc.) will not match these markers.
                let is_comment_like = trimmed.starts_with("//")
                    || trimmed.starts_with("#")
                    || trimmed.starts_with("*")
                    || trimmed.starts_with("/*")
                    || trimmed.starts_with("|") // markdown table row
                    || trimmed.starts_with(">"); // markdown blockquote
                let mentions_history = trimmed.contains("#1064")
                    || trimmed.contains("#1065")
                    || trimmed.contains("#1069")
                    || trimmed.contains("removed")
                    || trimmed.contains("legacy")
                    || trimmed.contains("formerly")
                    || trimmed.contains("deprecated")
                    || trimmed.contains("→")
                    || trimmed.contains("->");
                if is_comment_like && mentions_history {
                    continue;
                }
                hits.push(format!("{path_str}: {trimmed}"));
            }
        }
    }

    assert!(
        hits.is_empty(),
        "#1069 callsite audit: legacy API paths still referenced outside server route handlers / generated docs:\n  {}",
        hits.join("\n  ")
    );
}

