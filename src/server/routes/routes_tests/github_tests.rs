//! Domain-split routes tests — `github` group.
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
async fn create_issue_route_pg_builds_pmd_body_and_agent_label() {
    let _env_lock = env_lock();
    let gh = install_mock_gh_issue_create("itismyfield/AgentDesk", 819);
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    sqlx::query("INSERT INTO agents (id, name) VALUES ($1, $2)")
        .bind("adk-backend")
        .bind("ADK Backend")
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
                .uri("/issues")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "repo": "ADK",
                        "title": "create-issue 스킬을 ADK API로 승격",
                        "background": "AgentDesk 내부에서 PMD 포맷 이슈를 서버 API로 직접 생성해야 한다.",
                        "content": [
                            "POST /api/github/issues/create 엔드포인트를 추가한다.",
                            "서버에서 PMD 마크다운 포맷을 강제한다."
                        ],
                        "dod": [
                            "성공 시 issue URL과 번호를 반환한다",
                            "DoD 항목은 체크리스트로 렌더링된다"
                        ],
                        "agent_id": "adk-backend"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["issue"]["number"], 819);
    assert_eq!(
        json["issue"]["url"],
        "https://github.com/itismyfield/AgentDesk/issues/819"
    );
    assert_eq!(json["issue"]["repo"], "itismyfield/AgentDesk");
    let card_id = json["kanban_card_id"]
        .as_str()
        .expect("sqlite issue route must create a linked kanban card")
        .to_string();
    assert!(json["kanban_card_sync_error"].is_null());
    assert_eq!(json["applied_labels"], json!(["agent:adk-backend"]));
    assert_eq!(json["issue_format_version"], 1);
    // deprecated alias kept for transition; verify both fields are emitted
    assert_eq!(json["pmd_format_version"], 1);

    let row: (
        Option<String>,
        String,
        Option<i64>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT repo_id, status, github_issue_number, assigned_agent_id, metadata::text
             FROM kanban_cards
             WHERE id = $1",
    )
    .bind(card_id.as_str())
    .fetch_one(&pool)
    .await
    .unwrap();
    let (repo_id, status, issue_number, assigned_agent_id, metadata_raw) = row;
    assert_eq!(repo_id.as_deref(), Some("itismyfield/AgentDesk"));
    assert_eq!(status, "backlog");
    assert_eq!(issue_number, Some(819));
    assert_eq!(assigned_agent_id.as_deref(), Some("adk-backend"));
    let metadata_json: serde_json::Value =
        serde_json::from_str(metadata_raw.as_deref().expect("metadata must exist")).unwrap();
    assert_eq!(metadata_json["labels"], "agent:adk-backend");

    let args = fs::read_to_string(gh.path().join("issue-create-args.txt")).unwrap();
    let args: Vec<&str> = args.lines().collect();
    assert!(
        args.windows(2)
            .any(|pair| pair == ["--repo", "itismyfield/AgentDesk"])
    );
    assert!(
        args.windows(2)
            .any(|pair| pair == ["--label", "agent:adk-backend"])
    );
    assert!(
        args.windows(2)
            .any(|pair| pair == ["--title", "create-issue 스킬을 ADK API로 승격"])
    );

    let issue_body = fs::read_to_string(gh.path().join("issue-create-body.md")).unwrap();
    assert!(
        issue_body
            .contains("## 배경\nAgentDesk 내부에서 PMD 포맷 이슈를 서버 API로 직접 생성해야 한다.")
    );
    assert!(issue_body.contains("## 내용\n- POST /api/github/issues/create 엔드포인트를 추가한다.\n- 서버에서 PMD 마크다운 포맷을 강제한다."));
    assert!(issue_body.contains("## DoD\n- [ ] 성공 시 issue URL과 번호를 반환한다\n- [ ] DoD 항목은 체크리스트로 렌더링된다"));
    assert!(!issue_body.contains("## 의존성"));
    assert!(!issue_body.contains("## 리스크"));

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn create_issue_route_dry_run_renders_without_side_effects() {
    let _env_lock = env_lock();
    let gh = install_mock_gh_issue_create("itismyfield/AgentDesk", 821);
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    sqlx::query("INSERT INTO agents (id, name) VALUES ($1, $2)")
        .bind("adk-backend")
        .bind("ADK Backend")
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
                .uri("/issues")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "repo": "ADK",
                        "title": "dry-run preview",
                        "background": "Preview the issue body without writing external systems.",
                        "content": ["render the body"],
                        "dod": ["no side effects"],
                        "agent_id": "adk-backend",
                        "announcement_channel_id": "1490000000000000001",
                        "dry_run": true
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
    assert_eq!(json["dry_run"], true);
    assert!(json["issue"]["number"].is_null());
    assert!(json["issue"]["url"].is_null());
    assert_eq!(json["issue"]["repo"], "itismyfield/AgentDesk");
    assert!(json["kanban_card_id"].is_null());
    assert_eq!(json["announcement_channel_id"], "1490000000000000001");
    assert!(json["announcement_message_id"].is_null());
    assert!(json["announcement_sync_error"].is_null());
    assert!(json["kanban_card_sync_error"].is_null());
    assert_eq!(json["applied_labels"], json!(["agent:adk-backend"]));
    assert_eq!(json["validation_warnings"], json!([]));
    assert!(json["rendered_body"].as_str().unwrap().contains("## 배경"));
    assert!(
        json["rendered_body"]
            .as_str()
            .unwrap()
            .contains("## 내용\n- render the body")
    );
    assert!(
        json["rendered_body"]
            .as_str()
            .unwrap()
            .contains("## DoD\n- [ ] no side effects")
    );

    assert!(
        !gh.path().join("issue-create-args.txt").exists(),
        "dry_run must not call gh issue create"
    );
    let card_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kanban_cards")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(card_count, 0, "dry_run must not create a kanban card");
    let announcement_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM issue_announcements")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        announcement_count, 0,
        "dry_run must not create an announcement record"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn create_issue_route_dry_run_validation_errors_include_warnings() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/issues")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "repo": "ADK",
                        "title": "",
                        "background": "",
                        "content": [],
                        "dod": ["dod"],
                        "dry_run": true
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["error"], "title is required");
    let warnings = json["validation_warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 3);
    assert_eq!(warnings.first().unwrap(), "title is required");
    for expected in [
        "title is required",
        "background is required",
        "content must contain at least one item",
    ] {
        assert!(
            warnings.iter().any(|warning| warning == expected),
            "missing validation warning {expected}"
        );
    }
}

#[tokio::test]
async fn create_issue_route_dry_run_warns_for_unknown_agent() {
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
                .uri("/issues")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "repo": "ADK",
                        "title": "dry-run unknown agent",
                        "background": "background",
                        "content": ["content"],
                        "dod": ["dod"],
                        "agent_id": "missing-agent",
                        "dry_run": true
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
    assert_eq!(json["applied_labels"], json!(["agent:missing-agent"]));
    assert_eq!(
        json["validation_warnings"],
        json!(["unknown agent_id: missing-agent"])
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn create_issue_route_pg_returns_kanban_card_id() {
    let _env_lock = env_lock();
    let _gh = install_mock_gh_issue_create("itismyfield/AgentDesk", 820);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine(&db);

    sqlx::query("INSERT INTO agents (id, name) VALUES ($1, $2)")
        .bind("adk-backend")
        .bind("ADK Backend")
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
                .uri("/issues")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "repo": "ADK",
                        "title": "PG issue sync path",
                        "background": "Postgres-backed issue creation must return the linked card id.",
                        "content": [
                            "GitHub issue create success should upsert a kanban backlog card.",
                            "Response payload should expose the linked card id."
                        ],
                        "dod": [
                            "kanban_card_id is returned",
                            "card metadata keeps the applied agent label"
                        ],
                        "agent_id": "adk-backend"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let card_id = json["kanban_card_id"]
        .as_str()
        .expect("postgres issue route must return kanban_card_id")
        .to_string();
    assert!(json["kanban_card_sync_error"].is_null());

    let row = sqlx::query(
        "SELECT repo_id, status, github_issue_number, assigned_agent_id, description, metadata::text AS metadata
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(&card_id)
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(
        row.try_get::<Option<String>, _>("repo_id").unwrap(),
        Some("itismyfield/AgentDesk".to_string())
    );
    assert_eq!(row.try_get::<String, _>("status").unwrap(), "backlog");
    assert_eq!(
        row.try_get::<Option<i64>, _>("github_issue_number")
            .unwrap(),
        Some(820)
    );
    assert_eq!(
        row.try_get::<Option<String>, _>("assigned_agent_id")
            .unwrap(),
        Some("adk-backend".to_string())
    );
    let description = row
        .try_get::<Option<String>, _>("description")
        .unwrap()
        .expect("description must contain issue body");
    assert!(description.contains("## 배경"));
    let metadata_json: serde_json::Value = serde_json::from_str(
        row.try_get::<Option<String>, _>("metadata")
            .unwrap()
            .as_deref()
            .expect("metadata must exist"),
    )
    .unwrap();
    assert_eq!(metadata_json["labels"], "agent:adk-backend");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn create_issue_route_pg_posts_and_persists_issue_announcement() {
    let _env_lock = env_lock();
    let _gh = install_mock_gh_issue_create("itismyfield/AgentDesk", 1331);
    let (discord_base, discord_state, discord_handle) =
        spawn_mock_issue_announcement_discord_server().await;
    let runtime_root = tempfile::tempdir().unwrap();
    let _root = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
    let _api_base = EnvVarGuard::set("AGENTDESK_DISCORD_API_BASE_URL", &discord_base);
    write_announce_token(runtime_root.path());

    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pg_pool.clone());

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
                .uri("/github/issues/create")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "repo": "ADK",
                        "title": "Issue announcement lifecycle",
                        "background": "A created issue should post a Discord card.",
                        "content": ["post an announcement"],
                        "dod": ["announcement is persisted"],
                        "agent_id": "project-agentdesk",
                        "announcement_channel_id": "1490000000000000001"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["announcement_channel_id"], "1490000000000000001");
    assert_eq!(
        json["announcement_message_id"],
        "issue-announcement-1490000000000000001"
    );
    assert!(json["announcement_sync_error"].is_null());

    let posts = {
        let state = discord_state.lock().unwrap();
        state.posts.clone()
    };
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].0, "1490000000000000001");
    assert!(posts[0].1.contains("📋 **새 이슈 #1331**"));
    assert!(posts[0].1.contains("> 상태: 🟡 open"));
    assert!(posts[0].1.contains("> 담당: agent:project-agentdesk"));

    let row = sqlx::query(
        "SELECT repo, issue_number, channel_id, message_id, completed_at
         FROM issue_announcements
         WHERE repo = 'itismyfield/AgentDesk' AND issue_number = 1331",
    )
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(
        row.try_get::<String, _>("channel_id").unwrap(),
        "1490000000000000001"
    );
    assert_eq!(
        row.try_get::<String, _>("message_id").unwrap(),
        "issue-announcement-1490000000000000001"
    );
    assert!(
        row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("completed_at")
            .unwrap()
            .is_none()
    );

    discord_handle.abort();
    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn github_sync_pg_edits_issue_announcement_on_pr_merge_completion() {
    let _env_lock = env_lock();
    let (discord_base, discord_state, discord_handle) =
        spawn_mock_issue_announcement_discord_server().await;
    let runtime_root = tempfile::tempdir().unwrap();
    let _root = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
    let _api_base = EnvVarGuard::set("AGENTDESK_DISCORD_API_BASE_URL", &discord_base);
    write_announce_token(runtime_root.path());

    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    sqlx::query(
        "INSERT INTO issue_announcements (
            repo, issue_number, issue_url, title, agent_id,
            channel_id, message_id, created_at, updated_at
         )
         VALUES (
            'itismyfield/AgentDesk', 1332,
            'https://github.com/itismyfield/AgentDesk/issues/1332',
            'PR-backed completion', 'project-agentdesk',
            '1490000000000000002', 'message-1332',
            NOW() - INTERVAL '2 hours 13 minutes', NOW()
         )",
    )
    .execute(&pg_pool)
    .await
    .unwrap();

    crate::github::sync::sync_github_issues_for_repo_pg(
        &pg_pool,
        "itismyfield/AgentDesk",
        &[crate::github::sync::GhIssue {
            number: 1332,
            state: "CLOSED".to_string(),
            title: "PR-backed completion".to_string(),
            labels: Vec::new(),
            body: None,
            url: Some("https://github.com/itismyfield/AgentDesk/issues/1332".to_string()),
            closed_at: Some("2026-04-29T00:00:00Z".to_string()),
            closed_by_pull_requests_references: vec![crate::github::sync::GhPullRequestReference {
                number: Some(1410),
                url: Some("https://github.com/itismyfield/AgentDesk/pull/1410".to_string()),
            }],
        }],
    )
    .await
    .unwrap();

    let edits = {
        let state = discord_state.lock().unwrap();
        state.edits.clone()
    };
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].0, "1490000000000000002");
    assert_eq!(edits[0].1, "message-1332");
    assert!(edits[0].2.contains("✅ **#1332 완료**"));
    assert!(
        edits[0]
            .2
            .contains("> 머지: PR #1410 https://github.com/itismyfield/AgentDesk/pull/1410")
    );

    let row = sqlx::query(
        "SELECT completion_kind, completion_pr_number, completed_at, last_edit_error
         FROM issue_announcements
         WHERE repo = 'itismyfield/AgentDesk' AND issue_number = 1332",
    )
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(
        row.try_get::<Option<String>, _>("completion_kind").unwrap(),
        Some("merged".to_string())
    );
    assert_eq!(
        row.try_get::<Option<i64>, _>("completion_pr_number")
            .unwrap(),
        Some(1410)
    );
    assert!(
        row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("completed_at")
            .unwrap()
            .is_some()
    );
    assert!(
        row.try_get::<Option<String>, _>("last_edit_error")
            .unwrap()
            .is_none()
    );

    discord_handle.abort();
    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn create_issue_route_rejects_more_than_ten_dod_items() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);
    let dod: Vec<String> = (0..11).map(|index| format!("item-{index}")).collect();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/issues")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "repo": "ADK",
                        "title": "invalid dod",
                        "background": "background",
                        "content": ["content"],
                        "dod": dod,
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"], "dod items must be 10 or fewer");
}

// #1067: skill promotion integration test — exercise the canonical
// `/api/github/issues/create` path end-to-end via the mounted Axum router to
// confirm the create-issue skill body is absorbed by the server endpoint.
#[cfg(unix)]
#[tokio::test]
async fn github_issues_create_canonical_path_returns_created_issue() {
    let _env_lock = env_lock();
    let _gh = install_mock_gh_issue_create("itismyfield/AgentDesk", 1067);
    let db = test_db();
    let engine = test_engine(&db);
    db.lock()
        .unwrap()
        .execute(
            "INSERT INTO agents (id, name) VALUES (?1, ?2)",
            sqlite_params!["adk-backend", "ADK Backend"],
        )
        .unwrap();
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/github/issues/create")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "repo": "ADK",
                        "title": "#1067 skill promotion",
                        "background": "create-issue 스킬을 서버 API로 흡수한다.",
                        "content": [
                            "POST /api/github/issues/create 엔드포인트 사용.",
                            "skill body는 서버에서 PMD 포맷으로 변환된다."
                        ],
                        "dod": [
                            "canonical path (/api/github/issues/create)를 통해 이슈가 생성된다",
                            "응답에 issue_number와 url이 포함된다"
                        ],
                        "agent_id": "adk-backend"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["issue"]["number"], 1067);
    assert_eq!(
        json["issue"]["url"],
        "https://github.com/itismyfield/AgentDesk/issues/1067"
    );
    assert_eq!(json["issue"]["repo"], "itismyfield/AgentDesk");
    assert_eq!(
        json["applied_labels"]
            .as_array()
            .and_then(|v| v.first())
            .and_then(|v| v.as_str()),
        Some("agent:adk-backend")
    );
}

#[tokio::test]
async fn github_docs_include_issue_creation_endpoint() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/integrations/github")
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
        .expect("docs/github must return endpoint array");
    let create_issue = endpoints
        .iter()
        .find(|endpoint| {
            endpoint["method"] == "POST" && endpoint["path"] == "/api/github/issues/create"
        })
        .expect("integration docs must include POST /api/github/issues/create");
    assert_eq!(json["group"], "integrations");
    assert_eq!(json["category"], "github");
    assert_eq!(create_issue["params"]["repo"]["required"], true);
    assert_eq!(create_issue["params"]["dod"]["type"], "array[string]");
    assert_eq!(create_issue["params"]["agent_id"]["required"], false);
    assert_eq!(create_issue["params"]["dry_run"]["type"], "boolean");
    assert_eq!(create_issue["params"]["dry_run"]["default"], false);
    assert_eq!(create_issue["dry_run_example"]["response"]["dry_run"], true);
}

#[tokio::test]
async fn github_repos_pg_empty_list() {
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
                .uri("/github/repos")
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
    assert!(json["repos"].as_array().unwrap().is_empty());

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn github_repos_pg_register_and_list_basic() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    // Register
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
                .uri("/github/repos")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"id":"owner/repo1"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["repo"]["id"], "owner/repo1");

    // List
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
                .uri("/github/repos")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body2 = axum::body::to_bytes(response2.into_body(), usize::MAX)
        .await
        .unwrap();
    let json2: serde_json::Value = serde_json::from_slice(&body2).unwrap();
    assert_eq!(json2["repos"].as_array().unwrap().len(), 1);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn github_repos_register_bad_format() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/github/repos")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"id":"noslash"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn github_repos_pg_sync_not_registered() {
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
                .uri("/github/repos/unknown/repo/sync")
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
async fn github_repos_pg_register_and_list() {
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

    let register_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/github/repos")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"id":"owner/pg-repo"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(register_response.status(), StatusCode::CREATED);

    let list_response = app
        .oneshot(
            Request::builder()
                .uri("/github/repos")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let list_body = axum::body::to_bytes(list_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let list_json: serde_json::Value = serde_json::from_slice(&list_body).unwrap();
    assert_eq!(list_json["repos"].as_array().unwrap().len(), 1);
    assert_eq!(list_json["repos"][0]["id"], "owner/pg-repo");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn github_repos_pg_sync_triages_open_issue() {
    let _env_lock = env_lock();
    let _gh = install_mock_gh_issue_list(
        "owner/pg-repo",
        r#"[{"number":101,"state":"OPEN","title":"PG route open","labels":[{"name":"bug"},{"name":"p1"},{"name":"agent:agent-sync"}],"body":"Investigate route sync"}]"#,
        "[]",
    );
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine(&db);

    sqlx::query("INSERT INTO agents (id, name) VALUES ($1, $2)")
        .bind("agent-sync")
        .bind("Agent Sync")
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

    let register_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/github/repos")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"id":"owner/pg-repo"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(register_response.status(), StatusCode::CREATED);

    let sync_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/github/repos/owner/pg-repo/sync")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sync_response.status(), StatusCode::OK);

    let sync_body = axum::body::to_bytes(sync_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let sync_json: serde_json::Value = serde_json::from_slice(&sync_body).unwrap();
    assert_eq!(sync_json["repo"], "owner/pg-repo");
    assert_eq!(sync_json["issues_fetched"], 1);
    assert_eq!(sync_json["cards_created"], 1);
    assert_eq!(sync_json["cards_closed"], 0);

    let second_sync_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/github/repos/owner/pg-repo/sync")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second_sync_response.status(), StatusCode::OK);
    let second_sync_body = axum::body::to_bytes(second_sync_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let second_sync_json: serde_json::Value = serde_json::from_slice(&second_sync_body).unwrap();
    assert_eq!(second_sync_json["cards_created"], 0);

    let (status, priority, issue_number, description, assigned_agent_id, metadata_text): (
        String,
        String,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT status, priority, github_issue_number, description, assigned_agent_id, metadata::text
         FROM kanban_cards
         WHERE repo_id = $1
         ORDER BY github_issue_number",
    )
    .bind("owner/pg-repo")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(status, "backlog");
    assert_eq!(priority, "high");
    assert_eq!(issue_number, Some(101));
    assert_eq!(description.as_deref(), Some("Investigate route sync"));
    assert_eq!(assigned_agent_id.as_deref(), Some("agent-sync"));
    let metadata_json: serde_json::Value =
        serde_json::from_str(metadata_text.as_deref().expect("metadata must exist")).unwrap();
    assert_eq!(metadata_json["labels"], "bug,p1,agent:agent-sync");

    let last_synced_at: Option<String> =
        sqlx::query_scalar("SELECT last_synced_at::text FROM github_repos WHERE id = $1")
            .bind("owner/pg-repo")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert!(last_synced_at.is_some());

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn github_repos_pg_sync_closes_card_and_cleans_live_state() {
    crate::pipeline::ensure_loaded();
    let terminal = crate::pipeline::try_get()
        .map(|pipeline| {
            pipeline
                .states
                .iter()
                .find(|state| state.terminal)
                .map(|state| state.id.clone())
                .unwrap_or_else(|| "done".to_string())
        })
        .unwrap_or_else(|| "done".to_string());
    let _env_lock = env_lock();
    let _gh = install_mock_gh_issue_list(
        "owner/pg-repo",
        r#"[{"number":404,"state":"CLOSED","title":"PG route closed","labels":[],"body":"Issue is already closed"}]"#,
        "[]",
    );
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

    let register_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/github/repos")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"id":"owner/pg-repo"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(register_response.status(), StatusCode::CREATED);

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, github_issue_number,
            latest_dispatch_id, review_status, review_round, review_entered_at,
            created_at, updated_at
         )
         VALUES (
            $1, $2, $3, 'in_progress', 'medium', $4,
            $5, 'reviewing', 2, NOW(),
            NOW(), NOW()
         )",
    )
    .bind("card-pg-sync")
    .bind("owner/pg-repo")
    .bind("PG sync close")
    .bind(404_i64)
    .bind("dispatch-pg-sync")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
         )
         VALUES (
            $1, $2, $3, 'implementation', 'dispatched', 'Live implementation', NOW(), NOW()
         )",
    )
    .bind("dispatch-pg-sync")
    .bind("card-pg-sync")
    .bind("pg-agent")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ($1, $2, $3, 'active')",
    )
    .bind("run-pg-sync")
    .bind("owner/pg-repo")
    .bind("pg-agent")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, created_at
         )
         VALUES (
            $1, $2, $3, $4, 'pending', NOW()
         )",
    )
    .bind("entry-pg-sync")
    .bind("run-pg-sync")
    .bind("card-pg-sync")
    .bind("pg-agent")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO card_review_state (
            card_id, review_round, state, pending_dispatch_id, review_entered_at, updated_at
         )
         VALUES (
            $1, 2, 'reviewing', $2, NOW(), NOW()
         )",
    )
    .bind("card-pg-sync")
    .bind("dispatch-pg-sync")
    .execute(&pg_pool)
    .await
    .unwrap();

    let sync_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/github/repos/owner/pg-repo/sync")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sync_response.status(), StatusCode::OK);

    let sync_body = axum::body::to_bytes(sync_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let sync_json: serde_json::Value = serde_json::from_slice(&sync_body).unwrap();
    assert_eq!(sync_json["cards_created"], 0);
    assert_eq!(sync_json["cards_closed"], 1);

    let (card_status, latest_dispatch_id, review_status, review_round): (
        String,
        Option<String>,
        Option<String>,
        Option<i64>,
    ) = sqlx::query_as(
        "SELECT status, latest_dispatch_id, review_status, review_round
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind("card-pg-sync")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(card_status, terminal);
    assert!(latest_dispatch_id.is_none());
    assert!(review_status.is_none());
    assert!(review_round.is_none());

    let dispatch_status: String =
        sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
            .bind("dispatch-pg-sync")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(dispatch_status, "cancelled");

    let (entry_status, entry_dispatch_id): (String, Option<String>) =
        sqlx::query_as("SELECT status, dispatch_id FROM auto_queue_entries WHERE id = $1")
            .bind("entry-pg-sync")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(entry_status, "skipped");
    assert!(entry_dispatch_id.is_none());

    let (run_status, run_completed_at): (String, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT status, completed_at FROM auto_queue_runs WHERE id = $1")
            .bind("run-pg-sync")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(run_status, "completed");
    assert!(run_completed_at.is_some());

    let (review_state, pending_dispatch_id): (String, Option<String>) = sqlx::query_as(
        "SELECT state, pending_dispatch_id
         FROM card_review_state
         WHERE card_id = $1",
    )
    .bind("card-pg-sync")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(review_state, "idle");
    assert!(pending_dispatch_id.is_none());

    let audit_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM kanban_audit_logs WHERE card_id = $1 AND source = 'github-sync'",
    )
    .bind("card-pg-sync")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(audit_count, 1);

    let last_synced_at: Option<String> =
        sqlx::query_scalar("SELECT last_synced_at::text FROM github_repos WHERE id = $1")
            .bind("owner/pg-repo")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert!(last_synced_at.is_some());

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn github_repos_pg_sync_marks_in_progress_card_done_from_main_commit() {
    let _env_lock = env_lock();
    let _gh = install_mock_gh_issue_list(
        "owner/pg-repo",
        r#"[{"number":404,"state":"OPEN","title":"PG route mainline","labels":[],"body":"Issue remains open"}]"#,
        "[]",
    );
    let repo = tempfile::tempdir().unwrap();
    run_git(repo.path(), &["init", "-b", "main"]);
    run_git(repo.path(), &["config", "user.email", "test@test.com"]);
    run_git(repo.path(), &["config", "user.name", "Test"]);
    run_git(repo.path(), &["commit", "--allow-empty", "-m", "initial"]);
    git_commit(repo.path(), "fix: mainline merge (#404)");
    let config_dir = write_repo_mapping_config(&[("owner/pg-repo", repo.path())]);
    let config_path = config_dir.path().join("agentdesk.yaml");
    let _config = EnvVarGuard::set_path("AGENTDESK_CONFIG", &config_path);

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

    let register_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/github/repos")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"id":"owner/pg-repo"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(register_response.status(), StatusCode::CREATED);

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, github_issue_number,
            latest_dispatch_id, review_status, review_round, review_entered_at,
            created_at, updated_at
         )
         VALUES (
            $1, $2, $3, 'in_progress', 'medium', $4,
            $5, 'reviewing', 2, NOW(),
            NOW(), NOW()
         )",
    )
    .bind("card-pg-mainline")
    .bind("owner/pg-repo")
    .bind("PG mainline sync")
    .bind(404_i64)
    .bind("dispatch-pg-mainline")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
         )
         VALUES (
            $1, $2, $3, 'implementation', 'dispatched', 'Live implementation', NOW(), NOW()
         )",
    )
    .bind("dispatch-pg-mainline")
    .bind("card-pg-mainline")
    .bind("pg-agent")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ($1, $2, $3, 'active')",
    )
    .bind("run-pg-mainline")
    .bind("owner/pg-repo")
    .bind("pg-agent")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, dispatch_id, status, created_at
         )
         VALUES (
            $1, $2, $3, $4, $5, 'dispatched', NOW()
         )",
    )
    .bind("entry-pg-mainline")
    .bind("run-pg-mainline")
    .bind("card-pg-mainline")
    .bind("pg-agent")
    .bind("dispatch-pg-mainline")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO card_review_state (
            card_id, review_round, state, pending_dispatch_id, review_entered_at, updated_at
         )
         VALUES (
            $1, 2, 'reviewing', $2, NOW(), NOW()
         )",
    )
    .bind("card-pg-mainline")
    .bind("dispatch-pg-mainline")
    .execute(&pg_pool)
    .await
    .unwrap();

    let sync_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/github/repos/owner/pg-repo/sync")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sync_response.status(), StatusCode::OK);

    let sync_body = axum::body::to_bytes(sync_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let sync_json: serde_json::Value = serde_json::from_slice(&sync_body).unwrap();
    assert_eq!(sync_json["cards_created"], 0);

    let (card_status, latest_dispatch_id, review_status, review_round): (
        String,
        Option<String>,
        Option<String>,
        Option<i64>,
    ) = sqlx::query_as(
        "SELECT status, latest_dispatch_id, review_status, review_round
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind("card-pg-mainline")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(card_status, "done");
    assert!(latest_dispatch_id.is_none());
    assert!(review_status.is_none());
    assert!(review_round.is_none());

    let dispatch_status: String =
        sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
            .bind("dispatch-pg-mainline")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(dispatch_status, "cancelled");

    let (entry_status, entry_dispatch_id): (String, Option<String>) =
        sqlx::query_as("SELECT status, dispatch_id FROM auto_queue_entries WHERE id = $1")
            .bind("entry-pg-mainline")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(entry_status, "done");
    assert_eq!(entry_dispatch_id.as_deref(), Some("dispatch-pg-mainline"));

    let run_status: String = sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = $1")
        .bind("run-pg-mainline")
        .fetch_one(&pg_pool)
        .await
        .unwrap();
    assert_eq!(
        run_status, "active",
        "mainline issue sync should not force the queue run to complete"
    );

    let (review_state, pending_dispatch_id): (String, Option<String>) = sqlx::query_as(
        "SELECT state, pending_dispatch_id
         FROM card_review_state
         WHERE card_id = $1",
    )
    .bind("card-pg-mainline")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(review_state, "idle");
    assert!(pending_dispatch_id.is_none());

    let last_synced_at: Option<String> =
        sqlx::query_scalar("SELECT last_synced_at::text FROM github_repos WHERE id = $1")
            .bind("owner/pg-repo")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert!(last_synced_at.is_some());

    pg_pool.close().await;
    pg_db.drop().await;
}
