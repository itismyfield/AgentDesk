use super::*;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use std::ffi::OsString;
use std::process::Command;
use std::sync::Arc;
use std::sync::MutexGuard;
use tower::ServiceExt;

fn test_db() -> Db {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    crate::db::schema::migrate(&conn).unwrap();
    crate::db::wrap_conn(conn)
}

/// Seed test agents for dispatch-related tests (#245 agent-exists guard).
fn seed_test_agents(db: &Db) {
    let c = db.separate_conn().unwrap();
    c.execute_batch(
        "INSERT OR IGNORE INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('ch-td', 'TD', '111', '222');
         INSERT OR IGNORE INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('ag1', 'Agent1', '333', '444');
         INSERT OR IGNORE INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '555', '666');"
    ).unwrap();
}

fn test_engine(db: &Db) -> PolicyEngine {
    let config = crate::config::Config::default();
    PolicyEngine::new(&config, db.clone()).unwrap()
}

fn test_api_router(
    db: Db,
    engine: PolicyEngine,
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
) -> axum::Router {
    test_api_router_with_config(
        db,
        engine,
        crate::config::Config::default(),
        health_registry,
    )
}

fn test_api_router_with_config(
    db: Db,
    engine: PolicyEngine,
    config: crate::config::Config,
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
) -> axum::Router {
    let tx = crate::server::ws::new_broadcast();
    let buf = crate::server::ws::spawn_batch_flusher(tx.clone());
    api_router(db, engine, config, tx, buf, health_registry)
}

fn env_lock() -> MutexGuard<'static, ()> {
    crate::services::discord::runtime_store::lock_test_env()
}

struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set_path(key: &'static str, value: &std::path::Path) -> Self {
        let previous = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

fn run_git(repo_dir: &std::path::Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

struct RepoDirOverride {
    _lock: MutexGuard<'static, ()>,
    _env: EnvVarGuard,
    _config_dir: tempfile::TempDir,
    _config: EnvVarGuard,
}

fn setup_test_repo() -> (tempfile::TempDir, RepoDirOverride) {
    let lock = env_lock();
    let repo = tempfile::tempdir().unwrap();
    run_git(repo.path(), &["init", "-b", "main"]);
    run_git(repo.path(), &["config", "user.email", "test@test.com"]);
    run_git(repo.path(), &["config", "user.name", "Test"]);
    run_git(repo.path(), &["commit", "--allow-empty", "-m", "initial"]);
    let env = EnvVarGuard::set_path("AGENTDESK_REPO_DIR", repo.path());
    let config_dir = write_repo_mapping_config(&[("test-repo", repo.path())]);
    let config_path = config_dir.path().join("agentdesk.yaml");
    let config = EnvVarGuard::set_path("AGENTDESK_CONFIG", &config_path);
    (
        repo,
        RepoDirOverride {
            _lock: lock,
            _env: env,
            _config_dir: config_dir,
            _config: config,
        },
    )
}

fn write_repo_mapping_config(entries: &[(&str, &std::path::Path)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    for (repo_id, repo_dir) in entries {
        config.github.repo_dirs.insert(
            (*repo_id).to_string(),
            repo_dir.to_string_lossy().to_string(),
        );
    }
    crate::config::save_to_path(&dir.path().join("agentdesk.yaml"), &config).unwrap();
    dir
}

fn git_commit(repo_dir: &std::path::Path, message: &str) -> String {
    let filename = format!(
        "commit-{}.txt",
        message
            .chars()
            .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
            .collect::<String>()
    );
    std::fs::write(repo_dir.join(filename), format!("{message}\n")).unwrap();
    run_git(repo_dir, &["add", "."]);
    run_git(repo_dir, &["commit", "-m", message]);
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_dir)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[tokio::test]
async fn protected_domain_router_keeps_internal_and_hook_auth_exemptions() {
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
                "/hook/session",
                axum::routing::post(|| async { StatusCode::CREATED }),
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
                .uri("/hook/session")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(hook_response.status(), StatusCode::CREATED);

    let protected_response = app
        .oneshot(
            Request::builder()
                .uri("/settings")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(protected_response.status(), StatusCode::UNAUTHORIZED);
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
async fn health_api_http_reports_observability_metrics_and_degraded_outbox_backlog() {
    let db = test_db();
    let engine = test_engine(&db);
    let harness = crate::services::discord::health::TestHealthHarness::new().await;
    harness.set_recovery_duration_ms(1_250);
    let app = axum::Router::new().nest(
        "/api",
        test_api_router(db.clone(), engine, Some(harness.registry())),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let url = format!("http://{addr}/api/health");

    let healthy_response = reqwest::get(&url).await.unwrap();
    assert_eq!(healthy_response.status(), reqwest::StatusCode::OK);
    let healthy_json: serde_json::Value = healthy_response.json().await.unwrap();
    assert_eq!(healthy_json["status"], "healthy");
    assert_eq!(healthy_json["deferred_hooks"], 0);
    assert_eq!(healthy_json["queue_depth"], 0);
    assert_eq!(healthy_json["watcher_count"], 0);
    assert_eq!(healthy_json["outbox_age"], 0);
    assert!(
        (healthy_json["recovery_duration"].as_f64().unwrap() - 1.25).abs() < f64::EPSILON,
        "expected recovery_duration=1.25, got {}",
        healthy_json["recovery_duration"]
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO dispatch_outbox (dispatch_id, action, status, created_at) \
             VALUES (?1, 'notify', 'pending', datetime('now', '-5 minutes'))",
            ["dispatch-1"],
        )
        .unwrap();
    }

    let degraded_response = reqwest::get(&url).await.unwrap();
    assert_eq!(degraded_response.status(), reqwest::StatusCode::OK);
    let degraded_json: serde_json::Value = degraded_response.json().await.unwrap();
    assert_eq!(degraded_json["status"], "degraded");
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
}

#[tokio::test]
async fn health_api_includes_latest_config_audit_report() {
    let db = test_db();
    let engine = test_engine(&db);
    let harness = crate::services::discord::health::TestHealthHarness::new().await;
    let app = axum::Router::new().nest(
        "/api",
        test_api_router(db.clone(), engine, Some(harness.registry())),
    );

    {
        let conn = db.lock().unwrap();
        let report = crate::services::discord::config_audit::ConfigAuditReport {
            generated_at: "2026-04-11T01:23:45Z".to_string(),
            status: "warn".to_string(),
            dry_run: false,
            warnings_count: 1,
            warnings: vec!["DB agent 'alpha' differs from agentdesk.yaml on provider".to_string()],
            actions: vec![
                "synced 1 agent definitions from agentdesk.yaml into the agents table".to_string(),
            ],
            sources: crate::services::discord::config_audit::ConfigAuditSources {
                yaml_path: "/tmp/agentdesk.yaml".to_string(),
                yaml_present: true,
                role_map_path: Some("/tmp/role_map.json".to_string()),
                role_map_present: true,
                bot_settings_path: Some("/tmp/bot_settings.json".to_string()),
                bot_settings_present: false,
            },
            db: crate::services::discord::config_audit::ConfigAuditDbSummary {
                missing_agents: Vec::new(),
                extra_agents: Vec::new(),
                mismatched_agents: vec!["alpha".to_string()],
                synced_agents: Some(1),
            },
        };
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('config_audit_report', ?1)",
            [serde_json::to_string(&report).unwrap()],
        )
        .unwrap();
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let json: serde_json::Value = reqwest::get(format!("http://{addr}/api/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(json["config_audit"]["status"], "warn");
    assert_eq!(json["config_audit"]["warnings_count"], 1);
    assert_eq!(json["config_audit"]["db"]["mismatched_agents"][0], "alpha");
}

#[tokio::test]
async fn offices_reorder_accepts_bare_array_and_updates_listing_order() {
    let db = test_db();
    let engine = test_engine(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO offices (id, name, sort_order) VALUES ('office-a', 'Alpha', 2)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO offices (id, name, sort_order) VALUES ('office-b', 'Beta', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO offices (id, name, sort_order) VALUES ('office-c', 'Gamma', 1)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
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
}

#[tokio::test]
async fn offices_reorder_rejects_wrapped_order_body() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
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
async fn agent_turn_returns_recent_output_from_inflight_snapshot() {
    let _env_lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    let inflight_dir = temp
        .path()
        .join("runtime")
        .join("discord_inflight")
        .join("codex");
    std::fs::create_dir_all(&inflight_dir).unwrap();

    let tmux_name = "AgentDesk-codex-adk-cdx";
    std::fs::write(
        inflight_dir.join("1485506232256168011.json"),
        serde_json::to_string(&json!({
            "version": 1,
            "provider": "codex",
            "channel_id": 1485506232256168011u64,
            "channel_name": "adk-cdx",
            "request_owner_user_id": 1u64,
            "user_msg_id": 2u64,
            "current_msg_id": 3u64,
            "current_msg_len": 0,
            "user_text": "show me output",
            "session_id": null,
            "tmux_session_name": tmux_name,
            "output_path": null,
            "input_fifo_path": null,
            "last_offset": 0u64,
            "started_at": "2026-04-06 10:11:12",
            "updated_at": "2026-04-06 10:11:13",
            "prev_tool_status": "✓ Read: src/config.rs",
            "current_tool_line": "⚙ Bash: rg -n turn src",
            "full_response": "partial output\nOPENAI_API_KEY=sk-secret",
            "response_sent_offset": 0,
        }))
        .unwrap(),
    )
    .unwrap();

    let db = test_db();
    let engine = test_engine(&db);
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, provider, discord_channel_id, created_at, updated_at)
             VALUES ('agent-turn', 'Agent Turn', 'codex', '1485506232256168011', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions
             (session_key, agent_id, provider, status, active_dispatch_id, last_heartbeat, created_at)
             VALUES (?1, 'agent-turn', 'codex', 'working', 'dispatch-turn', datetime('now'), '2026-04-06 10:00:00')",
            [format!("mac-mini:{tmux_name}")],
        )
        .unwrap();
    }

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/agents/agent-turn/turn")
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
    assert_eq!(json["status"], "working");
    assert_eq!(json["started_at"], "2026-04-06 10:11:12");
    assert_eq!(json["updated_at"], "2026-04-06 10:11:13");
    assert_eq!(json["recent_output_source"], "inflight");
    assert_eq!(json["active_dispatch_id"], "dispatch-turn");
    assert_eq!(json["prev_tool_status"], "✓ Read: src/config.rs");
    assert_eq!(json["current_tool_line"], "⚙ Bash: rg -n turn src");
    assert_eq!(json["tool_count"], 2);
    let recent_output = json["recent_output"].as_str().unwrap();
    assert!(recent_output.contains("⚙ Bash: rg -n turn src"));
    assert!(recent_output.contains("✓ Read: src/config.rs"));
    assert!(recent_output.contains("OPENAI_API_KEY=[REDACTED]"));
    assert!(!recent_output.contains("sk-secret"));
    let tool_events = json["tool_events"].as_array().unwrap();
    assert_eq!(tool_events.len(), 2);
    assert_eq!(tool_events[0]["tool_name"], "Read");
    assert_eq!(tool_events[0]["status"], "success");
    assert_eq!(tool_events[1]["tool_name"], "Bash");
    assert_eq!(tool_events[1]["status"], "running");
}

#[tokio::test]
async fn agent_turn_reports_idle_when_agent_has_no_active_session() {
    let db = test_db();
    let engine = test_engine(&db);
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, provider, created_at, updated_at)
             VALUES ('agent-idle', 'Agent Idle', 'codex', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/agents/agent-idle/turn")
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
    assert_eq!(json["status"], "idle");
    assert!(json["recent_output"].is_null());
    assert!(json["started_at"].is_null());
    assert!(json["updated_at"].is_null());
    assert_eq!(json["recent_output_source"], "none");
    assert!(json["current_tool_line"].is_null());
    assert!(json["prev_tool_status"].is_null());
    assert_eq!(json["tool_count"], 0);
    assert!(json["tool_events"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn stop_agent_turn_force_kills_matching_tmux_session() {
    let _env_lock = env_lock();
    if Command::new("tmux").arg("-V").output().is_err() {
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    let inflight_dir = temp
        .path()
        .join("runtime")
        .join("discord_inflight")
        .join("codex");
    std::fs::create_dir_all(&inflight_dir).unwrap();

    let tmux_name = format!("AgentDesk-codex-agent-turn-stop-{}", std::process::id());
    let session_key = format!("mac-mini:{tmux_name}");
    let inflight_path = inflight_dir.join("agent-stop.json");
    std::fs::write(
        &inflight_path,
        serde_json::to_string(&json!({
            "version": 1,
            "provider": "codex",
            "channel_id": 1485506232256168011u64,
            "channel_name": "agent-stop",
            "request_owner_user_id": 1u64,
            "user_msg_id": 2u64,
            "current_msg_id": 3u64,
            "current_msg_len": 0,
            "user_text": "stop now",
            "session_id": null,
            "tmux_session_name": tmux_name,
            "output_path": null,
            "input_fifo_path": null,
            "last_offset": 0u64,
            "full_response": "",
            "response_sent_offset": 0,
            "started_at": "2026-04-06 10:20:00",
            "updated_at": "2026-04-06 10:20:01",
        }))
        .unwrap(),
    )
    .unwrap();

    let tmux_started = Command::new("tmux")
        .args(["new-session", "-d", "-s", &tmux_name, "sleep 30"])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !tmux_started {
        return;
    }

    let db = test_db();
    let engine = test_engine(&db);
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, provider, discord_channel_id, created_at, updated_at)
             VALUES ('agent-stop', 'Agent Stop', 'codex', '1485506232256168011', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions
             (session_key, agent_id, provider, status, last_heartbeat, created_at)
             VALUES (?1, 'agent-stop', 'codex', 'working', datetime('now'), datetime('now'))",
            [session_key.clone()],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents/agent-stop/turn/stop")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let tmux_still_alive = Command::new("tmux")
        .args(["has-session", "-t", &tmux_name])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if tmux_still_alive {
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", &tmux_name])
            .status();
    }

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "stopped");
    assert_eq!(json["tmux_killed"], true);
    assert_eq!(json["lifecycle_path"], "direct-fallback");
    assert!(
        !tmux_still_alive,
        "tmux session should be gone after /turn/stop"
    );
    assert!(
        !inflight_path.exists(),
        "matching inflight state should be removed by /turn/stop"
    );

    let conn = db.lock().unwrap();
    let session_status: String = conn
        .query_row(
            "SELECT status FROM sessions WHERE session_key = ?1",
            [session_key],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(session_status, "disconnected");
}

#[tokio::test]
async fn stop_agent_turn_preserves_pending_queue_via_mailbox_fallback_cleanup() {
    let db = test_db();
    let engine = test_engine(&db);
    let harness = crate::services::discord::health::TestHealthHarness::new_with_provider(
        crate::services::provider::ProviderKind::Codex,
    )
    .await;
    let channel_id = "1485506232256168012";
    let channel_num = channel_id.parse::<u64>().unwrap();
    let tmux_name = "AgentDesk-codex-stop-canonical";
    let session_key = format!("mac-mini:{tmux_name}");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, provider, discord_channel_id, created_at, updated_at)
             VALUES ('agent-stop-canonical', 'Agent Stop Canonical', 'codex', ?1, datetime('now'), datetime('now'))",
            [channel_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions
             (session_key, agent_id, provider, status, last_heartbeat, created_at)
             VALUES (?1, 'agent-stop-canonical', 'codex', 'working', datetime('now'), datetime('now'))",
            [session_key.as_str()],
        )
        .unwrap();
    }

    harness
        .seed_channel_session(
            channel_num,
            "stop-canonical",
            Some("session-stop-canonical"),
        )
        .await;
    harness.seed_active_turn(channel_num, 9, 91).await;
    harness
        .seed_queue(channel_num, &[(1_001, "preserve stop queue")])
        .await;
    harness.insert_dispatch_role_override(channel_num, 1485506232256168999);

    let app = test_api_router(db.clone(), engine, Some(harness.registry()));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents/agent-stop-canonical/turn/stop")
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
    assert_eq!(json["status"], "stopped");
    assert_eq!(json["lifecycle_path"], "runtime-fallback");
    assert_eq!(json["queue_preserved"], true);

    let (has_active_turn, queue_depth, session_id) = harness.mailbox_state(channel_num).await;
    assert!(!has_active_turn);
    assert_eq!(queue_depth, 1);
    assert_eq!(session_id, None);
    assert!(harness.has_dispatch_role_override(channel_num));

    let conn = db.lock().unwrap();
    let session_status: String = conn
        .query_row(
            "SELECT status FROM sessions WHERE session_key = ?1",
            [session_key],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(session_status, "disconnected");
}

#[tokio::test]
async fn cancel_turn_kills_tmux_and_cancels_active_dispatch() {
    let _env_lock = env_lock();
    if Command::new("tmux").arg("-V").output().is_err() {
        return;
    }

    let tmux_name = format!("AgentDesk-codex-turn-cancel-{}", std::process::id());
    let session_key = format!("mac-mini:{tmux_name}");
    let channel_id = "1485506232256168011";

    let tmux_started = Command::new("tmux")
        .args(["new-session", "-d", "-s", &tmux_name, "sleep 30"])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !tmux_started {
        return;
    }

    let db = test_db();
    let engine = test_engine(&db);
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, provider, discord_channel_id, created_at, updated_at)
             VALUES ('agent-queue-stop', 'Agent Queue Stop', 'codex', ?1, datetime('now'), datetime('now'))",
            [channel_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at)
             VALUES ('card-turn-cancel', 'Turn Cancel', 'in_progress', 'agent-queue-stop', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches
             (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
             VALUES ('dispatch-turn-cancel', 'card-turn-cancel', 'agent-queue-stop', 'implementation', 'dispatched', 'Cancel me', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions
             (session_key, agent_id, provider, status, active_dispatch_id, last_heartbeat, created_at)
             VALUES (?1, 'agent-queue-stop', 'codex', 'working', 'dispatch-turn-cancel', datetime('now'), datetime('now'))",
            [session_key.clone()],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/turns/{channel_id}/cancel"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let tmux_still_alive = Command::new("tmux")
        .args(["has-session", "-t", &tmux_name])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if tmux_still_alive {
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", &tmux_name])
            .status();
    }

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["session_key"], session_key);
    assert_eq!(json["tmux_session"], tmux_name);
    assert_eq!(json["tmux_killed"], true);
    assert_eq!(json["lifecycle_path"], "direct-fallback");
    assert_eq!(json["dispatch_cancelled"], "dispatch-turn-cancel");
    assert!(
        !tmux_still_alive,
        "tmux session should be gone after /turns/{{channel_id}}/cancel"
    );

    let conn = db.lock().unwrap();
    let session_row: (String, Option<String>) = conn
        .query_row(
            "SELECT status, active_dispatch_id FROM sessions WHERE session_key = ?1",
            [session_key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(session_row.0, "disconnected");
    assert_eq!(session_row.1, None);

    let dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-turn-cancel'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dispatch_status, "cancelled");
}

#[tokio::test]
async fn cancel_turn_preserves_pending_queue_via_mailbox_fallback_cleanup() {
    let db = test_db();
    let engine = test_engine(&db);
    let harness = crate::services::discord::health::TestHealthHarness::new().await;
    let channel_id = "1485506232256168013";
    let channel_num = channel_id.parse::<u64>().unwrap();
    let session_key = "mac-mini:AgentDesk-claude-cancel-canonical";

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, provider, discord_channel_id, created_at, updated_at)
             VALUES ('agent-cancel-canonical', 'Agent Cancel Canonical', 'claude', ?1, datetime('now'), datetime('now'))",
            [channel_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions
             (session_key, agent_id, provider, status, last_heartbeat, created_at)
             VALUES (?1, 'agent-cancel-canonical', 'claude', 'working', datetime('now'), datetime('now'))",
            [session_key],
        )
        .unwrap();
    }

    harness
        .seed_channel_session(
            channel_num,
            "cancel-canonical",
            Some("session-cancel-canonical"),
        )
        .await;
    harness.seed_active_turn(channel_num, 11, 111).await;
    harness
        .seed_queue(channel_num, &[(2_001, "preserve cancel queue")])
        .await;
    harness.insert_dispatch_role_override(channel_num, 1485506232256168998);

    let app = test_api_router(db.clone(), engine, Some(harness.registry()));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/turns/{channel_id}/cancel"))
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
    assert_eq!(json["session_key"], session_key);
    assert_eq!(json["lifecycle_path"], "runtime-fallback");
    assert_eq!(json["queue_preserved"], true);
    assert!(json["dispatch_cancelled"].is_null());

    let (has_active_turn, queue_depth, session_id) = harness.mailbox_state(channel_num).await;
    assert!(!has_active_turn);
    assert_eq!(queue_depth, 1);
    assert_eq!(session_id, None);
    assert!(harness.has_dispatch_role_override(channel_num));

    let conn = db.lock().unwrap();
    let session_status: String = conn
        .query_row(
            "SELECT status FROM sessions WHERE session_key = ?1",
            [session_key],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(session_status, "disconnected");
}

#[tokio::test]
async fn health_returns_ok_with_db_status() {
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

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["db"], true);
    assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn agents_empty_list() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/agents")
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
    assert!(json["agents"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn agents_returns_synced_agents() {
    let db = test_db();
    let engine = test_engine(&db);

    // Insert an agent
    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO agents (id, name, provider, status, xp) VALUES ('a1', 'Agent1', 'claude', 'idle', 0)",
                [],
            )
            .unwrap();
    }

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/agents")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let agents = json["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["id"], "a1");
    assert_eq!(agents[0]["name"], "Agent1");
}

#[tokio::test]
async fn agents_include_current_thread_channel_id_from_working_session() {
    let db = test_db();
    let engine = test_engine(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO agents (id, name, provider, status, xp) VALUES ('a1', 'Agent1', 'codex', 'idle', 0)",
                [],
            )
            .unwrap();
        conn.execute(
                "INSERT INTO sessions (session_key, agent_id, provider, status, thread_channel_id, last_heartbeat)
                 VALUES (?1, 'a1', 'codex', 'working', '1485506232256168011', datetime('now'))",
                ["mac-mini:AgentDesk-codex-adk-cdx-t1485506232256168011"],
            )
            .unwrap();
    }

    let app = test_api_router(db, engine, None);

    let list_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/agents")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let list_body = axum::body::to_bytes(list_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let list_json: serde_json::Value = serde_json::from_slice(&list_body).unwrap();
    assert_eq!(
        list_json["agents"][0]["current_thread_channel_id"],
        serde_json::Value::String("1485506232256168011".to_string())
    );

    let get_response = app
        .oneshot(
            Request::builder()
                .uri("/agents/a1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let get_body = axum::body::to_bytes(get_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let get_json: serde_json::Value = serde_json::from_slice(&get_body).unwrap();
    assert_eq!(
        get_json["agent"]["current_thread_channel_id"],
        serde_json::Value::String("1485506232256168011".to_string())
    );
}

#[tokio::test]
async fn claude_session_id_get_clears_stale_fixed_working_session() {
    let db = test_db();
    let engine = test_engine(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, provider, status, active_dispatch_id, claude_session_id, last_heartbeat, created_at
             ) VALUES (
                'test:stale-working', 'claude', 'working', 'dispatch-123', 'stale-sid',
                datetime('now', '-7 hours'), datetime('now', '-7 hours')
             )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/dispatched-sessions/claude-session-id?session_key=test:stale-working")
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
    assert!(json["claude_session_id"].is_null());
    assert!(json["session_id"].is_null());

    let conn = db.lock().unwrap();
    let (status, dispatch_id, session_id): (String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT status, active_dispatch_id, claude_session_id
             FROM sessions
             WHERE session_key = 'test:stale-working'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(status, "disconnected");
    assert!(dispatch_id.is_none());
    assert!(session_id.is_none());
}

#[tokio::test]
async fn claude_session_id_get_keeps_old_idle_fixed_session() {
    let db = test_db();
    let engine = test_engine(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, provider, status, claude_session_id, last_heartbeat, created_at
             ) VALUES (
                'test:old-idle', 'claude', 'idle', 'idle-sid',
                datetime('now', '-7 hours'), datetime('now', '-7 hours')
             )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db, engine, None);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/dispatched-sessions/claude-session-id?session_key=test:old-idle")
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
    assert_eq!(json["claude_session_id"], "idle-sid");
    assert_eq!(json["session_id"], "idle-sid");
}

#[tokio::test]
async fn claude_session_id_get_returns_null_on_provider_mismatch() {
    let db = test_db();
    let engine = test_engine(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, provider, status, claude_session_id, last_heartbeat, created_at
             ) VALUES (
                'host:AgentDesk-codex-adk-cdx', 'claude', 'idle', 'claude-sid',
                datetime('now', '-1 minutes'), datetime('now', '-1 minutes')
             )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db, engine, None);
    let resp = app
        .oneshot(
            Request::builder()
                .uri(
                    "/dispatched-sessions/claude-session-id?session_key=host:AgentDesk-codex-adk-cdx&provider=codex",
                )
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
    assert!(json["claude_session_id"].is_null());
    assert!(json["session_id"].is_null());
}

#[tokio::test]
async fn claude_session_id_get_keeps_value_on_provider_match() {
    let db = test_db();
    let engine = test_engine(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, provider, status, claude_session_id, last_heartbeat, created_at
             ) VALUES (
                'host:AgentDesk-codex-adk-cdx', 'codex', 'idle', 'codex-sid',
                datetime('now', '-1 minutes'), datetime('now', '-1 minutes')
             )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db, engine, None);
    let resp = app
        .oneshot(
            Request::builder()
                .uri(
                    "/dispatched-sessions/claude-session-id?session_key=host:AgentDesk-codex-adk-cdx&provider=codex",
                )
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
    assert_eq!(json["claude_session_id"], "codex-sid");
    assert_eq!(json["session_id"], "codex-sid");
}

#[tokio::test]
async fn get_agent_found() {
    let db = test_db();
    let engine = test_engine(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO agents (id, name, provider, status, xp) VALUES ('a1', 'Agent1', 'claude', 'idle', 0)",
                [],
            )
            .unwrap();
    }

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/agents/a1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["agent"]["id"], "a1");
}

#[tokio::test]
async fn get_agent_not_found() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/agents/nonexistent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"], "agent not found");
}

#[tokio::test]
async fn sessions_empty_list() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/sessions")
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
    assert!(json["sessions"].as_array().unwrap().is_empty());
}

// ── Kanban CRUD tests ──────────────────────────────────────────

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
async fn kanban_update_card_rejects_manual_non_backlog_transition() {
    let db = test_db();
    let engine = test_engine(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at) VALUES ('c1', 'Card1', 'ready', 'medium', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
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

    let conn = db.lock().unwrap();
    let status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'c1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status, "ready");
}

#[tokio::test]
async fn kanban_update_card_to_backlog_cleans_up_dispatches_auto_queue_and_turns() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-manual-backlog");
    seed_repo(&db, "test-repo");
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
                ?1, 'agent-manual-backlog', 'codex', 'working', 'dispatch-manual-backlog',
                datetime('now', '-9 minutes'), datetime('now', '-9 minutes')
            )",
            rusqlite::params![session_key],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-manual-backlog', 'test-repo', 'agent-manual-backlog', 'active')",
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
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status
            ) VALUES (
                'entry-manual-backlog-pending', 'run-manual-backlog', 'card-manual-backlog', 'agent-manual-backlog', 'pending'
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
        i64,
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

    let entry_rows: Vec<(String, Option<String>)> = conn
        .prepare(
            "SELECT status, dispatch_id FROM auto_queue_entries
             WHERE kanban_card_id = 'card-manual-backlog'
             ORDER BY id ASC",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(
        entry_rows,
        vec![("skipped".to_string(), None), ("skipped".to_string(), None),],
    );

    let (session_status, active_dispatch_id): (String, Option<String>) = conn
        .query_row(
            "SELECT status, active_dispatch_id
             FROM sessions
             WHERE session_key = ?1",
            rusqlite::params![session_key],
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

// ── Dispatch API tests ─────────────────────────────────────────

#[tokio::test]
async fn dispatch_list_empty() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

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
}

#[tokio::test]
async fn dispatch_create_and_get() {
    let db = test_db();
    seed_test_agents(&db);
    let engine = test_engine(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at) VALUES ('c1', 'Card1', 'ready', 'medium', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
    }

    let app = test_api_router(db.clone(), engine.clone(), None);
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

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let dispatch_id = json["dispatch"]["id"].as_str().unwrap().to_string();
    assert_eq!(json["dispatch"]["status"], "pending");
    assert_eq!(json["dispatch"]["kanban_card_id"], "c1");

    // #255: ready→requested is free, so dispatch from ready kicks off to "in_progress"
    let conn = db.lock().unwrap();
    let card_status: String = conn
        .query_row("SELECT status FROM kanban_cards WHERE id = 'c1'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(card_status, "in_progress");
    let notify_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM dispatch_outbox WHERE dispatch_id = ?1 AND action = 'notify'",
            [&dispatch_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(notify_count, 1, "API create must persist notify outbox");
    drop(conn);

    // GET single dispatch
    let app2 = test_api_router(db, engine, None);
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
}

#[tokio::test]
async fn dispatch_create_for_terminal_card_returns_conflict_with_reason() {
    let db = test_db();
    seed_test_agents(&db);
    let engine = test_engine(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, priority, assigned_agent_id, created_at, updated_at)
             VALUES ('c-terminal', 'Terminal Card', 'done', 'medium', 'agent-1', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db, engine, None);
    let response = app
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
}

#[tokio::test]
async fn dispatch_create_with_skip_outbox_omits_notify_row() {
    let db = test_db();
    seed_test_agents(&db);
    let engine = test_engine(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at) VALUES ('c1-skip', 'Card1 Skip', 'ready', 'medium', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
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

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let dispatch_id = json["dispatch"]["id"].as_str().unwrap().to_string();

    let conn = db.lock().unwrap();
    let notify_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM dispatch_outbox WHERE dispatch_id = ?1 AND action = 'notify'",
            [&dispatch_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        notify_count, 0,
        "skip_outbox=true must suppress notify outbox persistence"
    );
}

#[tokio::test]
async fn api_docs_returns_category_summaries_by_default() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(Request::builder().uri("/docs").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let categories = json["categories"]
        .as_array()
        .expect("docs must return category array");

    let dispatches = categories
        .iter()
        .find(|category| category["name"] == "dispatches")
        .expect("dispatches category must be present");
    assert!(
        dispatches["count"].as_u64().unwrap_or(0) >= 4,
        "dispatches category count must reflect documented endpoints"
    );
    assert!(
        dispatches["description"]
            .as_str()
            .unwrap_or_default()
            .contains("Dispatch"),
        "dispatches category description must be informative: {dispatches}"
    );
    assert!(
        json.get("endpoints").is_none(),
        "default docs response must return grouped categories, not flat endpoints"
    );
}

#[tokio::test]
async fn api_docs_flat_format_mentions_skip_outbox_for_dispatch_create() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs?format=flat")
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
        .expect("docs?format=flat must return endpoint array");
    let dispatch_post = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/dispatches")
        .expect("dispatch create endpoint must be documented");

    let description = dispatch_post["description"]
        .as_str()
        .expect("dispatch docs description must be string");
    assert!(
        description.contains("skip_outbox"),
        "dispatch create docs must mention skip_outbox option: {description}"
    );
    assert_eq!(dispatch_post["params"]["skip_outbox"]["type"], "boolean");
}

#[tokio::test]
async fn api_docs_category_exposes_dispatch_params_and_examples() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/dispatches")
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
    assert_eq!(json["category"], "dispatches");
    assert!(
        json["count"].as_u64().unwrap_or(0) >= 4,
        "dispatches detail should include documented endpoints"
    );

    let endpoints = json["endpoints"]
        .as_array()
        .expect("category detail must include endpoint array");
    let dispatch_post = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/dispatches")
        .expect("dispatch create endpoint must be present in detail view");
    assert_eq!(
        dispatch_post["params"]["kanban_card_id"]["location"],
        "body"
    );
    assert_eq!(dispatch_post["params"]["skip_outbox"]["type"], "boolean");
    assert_eq!(
        dispatch_post["example"]["request"]["body"]["skip_outbox"],
        serde_json::json!(true)
    );
    assert_eq!(
        dispatch_post["example"]["response"]["dispatch"]["status"],
        "pending"
    );
}

#[tokio::test]
async fn api_docs_category_exposes_auto_queue_params_and_examples() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/auto-queue")
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
    assert_eq!(json["category"], "auto-queue");

    let endpoints = json["endpoints"]
        .as_array()
        .expect("category detail must include endpoint array");
    let generate = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/auto-queue/generate")
        .expect("auto-queue generate endpoint must be present");
    assert!(
        generate["params"].get("mode").is_none(),
        "generate docs should not expose legacy mode selection"
    );
    assert!(
        generate["params"].get("parallel").is_none(),
        "generate docs should not expose legacy parallel toggle"
    );
    assert_eq!(
        generate["example"]["response"]["run"]["unified_thread"],
        serde_json::json!(false)
    );

    let dispatch = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/auto-queue/dispatch")
        .expect("auto-queue dispatch endpoint must be present");
    assert_eq!(dispatch["params"]["groups"]["required"], true);
    assert_eq!(dispatch["params"]["activate"]["default"], true);
    assert_eq!(dispatch["params"]["auto_assign_agent"]["default"], true);
    assert_eq!(dispatch["example"]["response"]["dispatch"]["count"], 1);
}

#[tokio::test]
async fn api_docs_category_exposes_kanban_params_and_examples() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/kanban")
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
    assert_eq!(json["category"], "kanban");

    let endpoints = json["endpoints"]
        .as_array()
        .expect("category detail must include endpoint array");
    let create = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/kanban-cards")
        .expect("kanban create endpoint must be present");
    assert_eq!(create["params"]["title"]["type"], "string");
    assert_eq!(create["example"]["response"]["card"]["status"], "backlog");

    let resume = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/kanban-cards/{id}/resume")
        .expect("kanban resume endpoint must be present");
    assert_eq!(resume["params"]["force"]["type"], "boolean");
    assert_eq!(
        resume["example"]["response"]["action"]["type"],
        "new_implementation_dispatch"
    );
}

#[tokio::test]
async fn api_docs_flat_format_lists_routes_missing_from_legacy_docs() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs?format=flat")
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
        .expect("docs?format=flat must return endpoint array");

    for path in [
        "/api/kanban-cards/{id}/reopen",
        "/api/review-decision",
        "/api/auto-queue/dispatch",
        "/api/auto-queue/entries/{id}",
        "/api/auto-queue/slots/{agent_id}/{slot_index}/reset-thread",
        "/api/help",
        "/api/docs/{category}",
    ] {
        assert!(
            endpoints.iter().any(|ep| ep["path"] == path),
            "flat docs must include {path}"
        );
    }
}

#[tokio::test]
async fn api_docs_flat_format_omits_removed_legacy_routes() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs?format=flat")
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
        .expect("docs?format=flat must return endpoint array");

    for path in [
        "/api/agent-channels",
        "/api/dispatch-cancel/{id}",
        "/api/pipeline-stages",
        "/api/pipeline-stages/{id}",
        "/api/session/start",
        "/api/sessions/search",
        "/api/sessions/force-kill",
        "/api/auto-queue/enqueue",
        "/api/api-friction/events",
        "/api/api-friction/patterns",
        "/api/api-friction/process",
    ] {
        assert!(
            endpoints.iter().all(|ep| ep["path"] != path),
            "flat docs must omit removed route {path}"
        );
    }

    assert!(
        endpoints
            .iter()
            .any(|ep| ep["method"] == "POST" && ep["path"] == "/api/auto-queue/runs/{id}/order"),
        "flat docs must keep the PM-assisted submit_order callback route"
    );
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

        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{method} {uri} should return 404 after route cleanup"
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
            .any(|category| category["name"] == "auto-queue"),
        "/help must expose category summaries"
    );
    let dispatch = json["endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|ep| ep["path"] == "/api/auto-queue/dispatch")
        .expect("/help must include the declarative dispatch endpoint");
    assert_eq!(dispatch["params"]["groups"]["required"], true);
}

#[tokio::test]
async fn api_docs_unknown_category_returns_not_found() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/not-a-real-category")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn resume_requested_creates_single_notify_backed_dispatch() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-resume");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, created_at, updated_at
            ) VALUES (
                'card-resume', 'Resume Card', 'requested', 'medium', 'agent-resume',
                datetime('now'), datetime('now')
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
                .uri("/kanban-cards/card-resume/resume")
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
    let dispatch_id = json["action"]["dispatch_id"].as_str().unwrap().to_string();
    assert_eq!(json["action"]["type"], "new_implementation_dispatch");

    let conn = db.lock().unwrap();
    let (dispatch_type, dispatch_status, context, latest_dispatch_id): (
        String,
        String,
        String,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT td.dispatch_type, td.status, td.context, kc.latest_dispatch_id
             FROM task_dispatches td
             JOIN kanban_cards kc ON kc.id = td.kanban_card_id
             WHERE td.id = ?1",
            [&dispatch_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(dispatch_type, "implementation");
    assert_eq!(dispatch_status, "pending");
    assert_eq!(latest_dispatch_id.as_deref(), Some(dispatch_id.as_str()));
    let context_json: serde_json::Value = serde_json::from_str(&context).unwrap();
    assert_eq!(context_json["resume"], true);
    assert_eq!(context_json["resumed_from"], "requested");

    let notify_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM dispatch_outbox WHERE dispatch_id = ?1 AND action = 'notify'",
            [&dispatch_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        notify_count, 1,
        "resume(requested) must create exactly one notify outbox row via canonical core"
    );
}

#[tokio::test]
async fn dispatch_create_card_not_found() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

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
}

#[tokio::test]
async fn dispatch_complete() {
    let db = test_db();
    seed_test_agents(&db);
    let engine = test_engine(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at) VALUES ('c1', 'Card1', 'ready', 'medium', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
    }

    // Create dispatch
    let app = test_api_router(db.clone(), engine.clone(), None);
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
    let app2 = test_api_router(db, engine, None);
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
}

#[tokio::test]
async fn dispatch_get_not_found() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

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
}

// ── Policy hook firing tests ───────────────────────────────────

#[tokio::test]
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

    let db = test_db();
    let config = crate::config::Config {
        policies: crate::config::PoliciesConfig {
            dir: dir.path().to_path_buf(),
            hot_reload: false,
        },
        ..crate::config::Config::default()
    };
    let engine = PolicyEngine::new(&config, db.clone()).unwrap();

    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at) VALUES ('c1', 'Card1', 'pending_decision', 'medium', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
    }

    // Use force transition: pending_decision → done (force_only in YAML pipeline)
    let result =
        crate::kanban::transition_status_with_opts(&db, &engine, "c1", "done", "pmd", true);
    assert!(
        result.is_ok(),
        "force transition should succeed: {:?}",
        result
    );

    let conn = db.lock().unwrap();
    let transition: String = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = 'transition'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(transition, "pending_decision->done");

    let terminal: String = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = 'terminal'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(terminal, "c1:done");
}

#[tokio::test]
async fn dispatch_list_with_filter() {
    let db = test_db();
    let engine = test_engine(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at) VALUES ('c1', 'Card1', 'ready', 'medium', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
        conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, status, title, created_at, updated_at) VALUES ('d1', 'c1', 'ag1', 'pending', 'T1', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
        conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, status, title, created_at, updated_at) VALUES ('d2', 'c1', 'ag1', 'completed', 'T2', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
    }

    let app = test_api_router(db, engine, None);
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
}

// ── GitHub Repos API tests ────────────────────────────────────

#[tokio::test]
async fn github_repos_empty_list() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

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
}

#[tokio::test]
async fn github_repos_register_and_list() {
    let db = test_db();
    let engine = test_engine(&db);

    // Register
    let app = test_api_router(db.clone(), engine.clone(), None);
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
    let app2 = test_api_router(db, engine, None);
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
async fn github_repos_sync_not_registered() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

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
}
// ── Pipeline config hierarchy tests (#135) ──

fn seed_repo(db: &Db, repo_id: &str) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT OR IGNORE INTO github_repos (id, display_name) VALUES (?1, ?1)",
        [repo_id],
    )
    .unwrap();
}

fn seed_agent(db: &Db, agent_id: &str) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT OR IGNORE INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES (?1, ?1, '111', '222')",
        [agent_id],
    )
    .unwrap();
}

#[tokio::test]
async fn create_repo_seeds_builtin_agentdesk_pipeline_stages_for_new_db() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db.clone(), engine, None);

    let resp = app
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
    assert_eq!(resp.status(), StatusCode::CREATED);

    let conn = db.lock().unwrap();
    let rows: Vec<(String, i64, Option<String>, Option<String>, Option<String>)> = conn
        .prepare(
            "SELECT stage_name, stage_order, trigger_after, provider, skip_condition
             FROM pipeline_stages
             WHERE repo_id = 'itismyfield/AgentDesk'
             ORDER BY stage_order ASC",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(
        rows,
        vec![
            (
                "dev-deploy".to_string(),
                100,
                Some("review_pass".to_string()),
                Some("self".to_string()),
                Some("no_rs_changes".to_string()),
            ),
            (
                "e2e-test".to_string(),
                200,
                None,
                Some("counter".to_string()),
                Some("no_rs_changes".to_string()),
            ),
        ]
    );
}

#[tokio::test]
async fn create_repo_does_not_duplicate_builtin_agentdesk_pipeline_stages() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db.clone(), engine, None);

    for _ in 0..2 {
        let resp = app
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
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    let conn = db.lock().unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pipeline_stages WHERE repo_id = 'itismyfield/AgentDesk'",
            [],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(count, 2);
}

#[tokio::test]
async fn pipeline_config_repo_get_set_override() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_repo(&db, "owner/repo-a");

    // GET — initially null
    let app = test_api_router(db.clone(), engine.clone(), None);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/pipeline/config/repo/owner/repo-a")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(body["pipeline_config"].is_null());

    // PUT — set override
    let app2 = test_api_router(db.clone(), engine.clone(), None);
    let resp2 = app2
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/pipeline/config/repo/owner/repo-a")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"config":{"hooks":{"review":{"on_enter":["CustomReviewHook"],"on_exit":[]}}}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);

    // GET — now has override
    let app3 = test_api_router(db, engine, None);
    let resp3 = app3
        .oneshot(
            Request::builder()
                .uri("/pipeline/config/repo/owner/repo-a")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body3: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp3.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(
        body3["pipeline_config"]["hooks"]["review"]["on_enter"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "CustomReviewHook")
    );
}

#[tokio::test]
async fn pipeline_config_agent_get_set_override() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-x");

    // PUT
    let app = test_api_router(db.clone(), engine.clone(), None);
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/pipeline/config/agent/agent-x")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"config":{"timeouts":{"in_progress":{"duration":"4h","clock":"started_at"}}}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // GET
    let app2 = test_api_router(db, engine, None);
    let resp2 = app2
        .oneshot(
            Request::builder()
                .uri("/pipeline/config/agent/agent-x")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp2.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        body["pipeline_config"]["timeouts"]["in_progress"]["duration"],
        "4h"
    );
}

#[tokio::test]
async fn pipeline_config_effective_merges_layers() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_repo(&db, "owner/repo-e");
    seed_agent(&db, "agent-e");

    // Set repo override (hooks)
    let app = test_api_router(db.clone(), engine.clone(), None);
    app.oneshot(
        Request::builder()
            .method("PUT")
            .uri("/pipeline/config/repo/owner/repo-e")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"config":{"hooks":{"in_progress":{"on_enter":["RepoHook"],"on_exit":[]}}}}"#,
            ))
            .unwrap(),
    )
    .await
    .unwrap();

    // Get effective — should include repo hook
    let app2 = test_api_router(db.clone(), engine.clone(), None);
    let resp = app2
        .oneshot(
            Request::builder()
                .uri("/pipeline/config/effective?repo=owner/repo-e&agent_id=agent-e")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["layers"]["repo"], true);
    assert_eq!(body["layers"]["agent"], false);
    // Hooks from repo override should be in effective pipeline
    let hooks = &body["pipeline"]["hooks"]["in_progress"]["on_enter"];
    assert!(hooks.as_array().unwrap().iter().any(|v| v == "RepoHook"));
}

#[tokio::test]
async fn pipeline_config_graph_returns_nodes_and_edges() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);

    let app = test_api_router(db, engine, None);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/pipeline/config/graph")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    let nodes = body["nodes"].as_array().unwrap();
    let edges = body["edges"].as_array().unwrap();
    assert!(!nodes.is_empty());
    assert!(!edges.is_empty());
    // Each node has expected fields
    assert!(nodes[0]["id"].is_string());
    assert!(nodes[0]["label"].is_string());
    // Each edge has from/to/type
    assert!(edges[0]["from"].is_string());
    assert!(edges[0]["to"].is_string());
    assert!(edges[0]["type"].is_string());
}

#[tokio::test]
async fn pipeline_config_repo_invalid_override_rejected() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_repo(&db, "owner/repo-bad");

    let app = test_api_router(db, engine, None);
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/pipeline/config/repo/owner/repo-bad")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"config":{"states":"not-an-array"}}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn pipeline_config_repo_broken_merge_rejected() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_repo(&db, "owner/repo-merge");

    // Override that adds a timeout referencing an unknown clock and a non-existent state.
    // This parses as valid JSON but the merged effective pipeline should fail validate().
    let body = r#"{"config":{"timeouts":{"nonexistent_state":{"duration":"1h","clock":"no_such_clock"}}}}"#;

    let app = test_api_router(db, engine, None);
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/pipeline/config/repo/owner/repo-merge")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("validation failed"),
        "expected merged validation error, got: {}",
        body
    );
}

// ── force-transition auth tests ──

fn seed_card_with_status(db: &Db, card_id: &str, status: &str) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO kanban_cards (id, title, status, priority, created_at, updated_at) \
             VALUES (?1, 'test', ?2, 'medium', datetime('now'), datetime('now'))",
        rusqlite::params![card_id, status],
    )
    .unwrap();
}

fn set_pmd_channel(db: &Db, channel_id: &str) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('kanban_manager_channel_id', ?1)",
        [channel_id],
    )
    .unwrap();
}

fn ensure_auto_queue_tables(db: &Db) {
    let conn = db.lock().unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS auto_queue_runs (
            id          TEXT PRIMARY KEY,
            repo        TEXT,
            agent_id    TEXT,
            status      TEXT DEFAULT 'active',
            ai_model    TEXT,
            ai_rationale TEXT,
            timeout_minutes INTEGER DEFAULT 120,
            unified_thread  INTEGER DEFAULT 0,
            unified_thread_id TEXT,
            unified_thread_channel_id TEXT,
            created_at  DATETIME DEFAULT CURRENT_TIMESTAMP,
            completed_at DATETIME,
            max_concurrent_threads INTEGER DEFAULT 1,
            thread_group_count INTEGER DEFAULT 1
        );
        CREATE TABLE IF NOT EXISTS auto_queue_entries (
            id              TEXT PRIMARY KEY,
            run_id          TEXT REFERENCES auto_queue_runs(id),
            kanban_card_id  TEXT REFERENCES kanban_cards(id),
            agent_id        TEXT,
            priority_rank   INTEGER DEFAULT 0,
            reason          TEXT,
            status          TEXT DEFAULT 'pending',
            dispatch_id     TEXT,
            slot_index      INTEGER,
            created_at      DATETIME DEFAULT CURRENT_TIMESTAMP,
            dispatched_at   DATETIME,
            completed_at    DATETIME,
            thread_group    INTEGER DEFAULT 0,
            batch_phase     INTEGER DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS auto_queue_entry_dispatch_history (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            entry_id        TEXT NOT NULL,
            dispatch_id     TEXT NOT NULL,
            trigger_source  TEXT,
            created_at      DATETIME DEFAULT CURRENT_TIMESTAMP,
            UNIQUE(entry_id, dispatch_id)
        );
        CREATE TABLE IF NOT EXISTS auto_queue_slots (
            agent_id              TEXT NOT NULL,
            slot_index            INTEGER NOT NULL,
            assigned_run_id       TEXT,
            assigned_thread_group INTEGER,
            thread_id_map         TEXT,
            created_at            DATETIME DEFAULT CURRENT_TIMESTAMP,
            updated_at            DATETIME DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (agent_id, slot_index)
        );
        CREATE TABLE IF NOT EXISTS auto_queue_phase_gates (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            run_id          TEXT NOT NULL REFERENCES auto_queue_runs(id) ON DELETE CASCADE,
            phase           INTEGER NOT NULL,
            status          TEXT NOT NULL DEFAULT 'pending',
            verdict         TEXT,
            dispatch_id     TEXT REFERENCES task_dispatches(id) ON DELETE CASCADE
                                CHECK(dispatch_id IS NULL OR TRIM(dispatch_id) <> ''),
            pass_verdict    TEXT NOT NULL DEFAULT 'phase_gate_passed',
            next_phase      INTEGER,
            final_phase     INTEGER NOT NULL DEFAULT 0,
            anchor_card_id  TEXT REFERENCES kanban_cards(id) ON DELETE SET NULL,
            failure_reason  TEXT,
            created_at      DATETIME DEFAULT CURRENT_TIMESTAMP,
            updated_at      DATETIME DEFAULT CURRENT_TIMESTAMP
        );
        CREATE UNIQUE INDEX IF NOT EXISTS uq_aq_phase_gates_run_phase_dispatch_key
            ON auto_queue_phase_gates(run_id, phase, COALESCE(dispatch_id, ''));
        CREATE UNIQUE INDEX IF NOT EXISTS uq_aq_phase_gates_dispatch_id
            ON auto_queue_phase_gates(dispatch_id);",
    )
    .unwrap();
}

fn seed_auto_queue_card(db: &Db, card_id: &str, issue_number: i64, status: &str, agent_id: &str) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, repo_id,
            github_issue_number, created_at, updated_at
        ) VALUES (
            ?1, ?2, ?3, 'medium', ?4, 'test-repo', ?5, datetime('now'), datetime('now')
        )",
        rusqlite::params![
            card_id,
            format!("Issue #{issue_number}"),
            status,
            agent_id,
            issue_number
        ],
    )
    .unwrap();
}

#[test]
fn auto_queue_schema_migration_drops_legacy_max_concurrent_per_agent_column() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "PRAGMA foreign_keys=ON;
         CREATE TABLE kanban_cards (id TEXT PRIMARY KEY);
         CREATE TABLE task_dispatches (
            id TEXT PRIMARY KEY,
            kanban_card_id TEXT,
            to_agent_id TEXT,
            dispatch_type TEXT,
            created_at DATETIME
         );",
    )
    .unwrap();
    conn.execute_batch(
        "CREATE TABLE auto_queue_runs (
            id          TEXT PRIMARY KEY,
            repo        TEXT,
            agent_id    TEXT,
            status      TEXT DEFAULT 'active',
            ai_model    TEXT,
            ai_rationale TEXT,
            timeout_minutes INTEGER DEFAULT 120,
            unified_thread INTEGER DEFAULT 0,
            unified_thread_id TEXT,
            unified_thread_channel_id TEXT,
            max_concurrent_threads INTEGER DEFAULT 1,
            max_concurrent_per_agent INTEGER DEFAULT 1,
            created_at  DATETIME DEFAULT CURRENT_TIMESTAMP,
            completed_at DATETIME
        );
        CREATE TABLE auto_queue_entries (
            id              TEXT PRIMARY KEY,
            run_id          TEXT REFERENCES auto_queue_runs(id),
            kanban_card_id  TEXT REFERENCES kanban_cards(id),
            agent_id        TEXT,
            priority_rank   INTEGER DEFAULT 0,
            reason          TEXT,
            status          TEXT DEFAULT 'pending',
            created_at      DATETIME DEFAULT CURRENT_TIMESTAMP,
            dispatched_at   DATETIME,
            completed_at    DATETIME
        );",
    )
    .unwrap();

    crate::db::schema::ensure_auto_queue_schema(&conn).unwrap();

    let has_legacy_column: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('auto_queue_runs') WHERE name = 'max_concurrent_per_agent'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let has_max_threads: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('auto_queue_runs') WHERE name = 'max_concurrent_threads'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let has_thread_group_count: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('auto_queue_runs') WHERE name = 'thread_group_count'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let has_batch_phase: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('auto_queue_entries') WHERE name = 'batch_phase'",
            [],
            |row| row.get(0),
        )
        .unwrap();

    assert!(!has_legacy_column);
    assert!(has_max_threads);
    assert!(has_thread_group_count);
    assert!(has_batch_phase);
}

fn seed_in_progress_stall_case(
    db: &Db,
    card_id: &str,
    title: &str,
    agent_id: &str,
    started_offset: &str,
    updated_offset: &str,
    latest_dispatch: Option<(&str, &str)>,
) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, repo_id,
            started_at, created_at, updated_at
        ) VALUES (
            ?1, ?2, 'in_progress', 'medium', ?3, 'test-repo',
            datetime('now', ?4), datetime('now', ?4), datetime('now', ?5)
        )",
        rusqlite::params![card_id, title, agent_id, started_offset, updated_offset,],
    )
    .unwrap();

    if let Some((dispatch_id, dispatch_offset)) = latest_dispatch {
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                ?1, ?2, ?3, 'implementation', 'dispatched', ?4, datetime('now', ?5), datetime('now', ?5)
            )",
            rusqlite::params![dispatch_id, card_id, agent_id, format!("{title} Dispatch"), dispatch_offset],
        )
        .unwrap();
        conn.execute(
            "UPDATE kanban_cards SET latest_dispatch_id = ?1 WHERE id = ?2",
            rusqlite::params![dispatch_id, card_id],
        )
        .unwrap();
    }
}

fn seed_review_e2e_case(
    db: &Db,
    card_id: &str,
    title: &str,
    agent_id: &str,
    review_offset: &str,
    dispatch_id: &str,
    dispatch_status: &str,
    dispatch_offset: &str,
) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, repo_id,
            review_entered_at, created_at, updated_at
        ) VALUES (
            ?1, ?2, 'review', 'medium', ?3, 'test-repo',
            datetime('now', ?4), datetime('now', ?4), datetime('now', ?4)
        )",
        rusqlite::params![card_id, title, agent_id, review_offset],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
        ) VALUES (
            ?1, ?2, ?3, 'e2e-test', ?4, ?5, datetime('now', ?6), datetime('now', ?6)
        )",
        rusqlite::params![
            dispatch_id,
            card_id,
            agent_id,
            dispatch_status,
            format!("{title} E2E"),
            dispatch_offset
        ],
    )
    .unwrap();
    conn.execute(
        "UPDATE kanban_cards SET latest_dispatch_id = ?1 WHERE id = ?2",
        rusqlite::params![dispatch_id, card_id],
    )
    .unwrap();
}

fn drain_pending_transitions(db: &Db, engine: &PolicyEngine) {
    loop {
        let transitions = engine.drain_pending_transitions();
        if transitions.is_empty() {
            break;
        }
        for (card_id, old_s, new_s) in &transitions {
            crate::kanban::fire_transition_hooks(db, engine, card_id, old_s, new_s);
        }
    }
}

#[tokio::test]
async fn force_transition_rejects_without_channel_header() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_card_with_status(&db, "card-ft1", "backlog");
    set_pmd_channel(&db, "pmd-chan-123");

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-ft1/force-transition")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"status":"ready"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[test]
fn on_tick5min_stalled_timeout_uses_latest_activity_timestamp() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-stalled");

    seed_in_progress_stall_case(
        &db,
        "card-fresh-dispatch",
        "Fresh Dispatch",
        "agent-stalled",
        "-3 hours",
        "-3 hours",
        Some(("dispatch-fresh", "-10 minutes")),
    );
    seed_in_progress_stall_case(
        &db,
        "card-reentered",
        "Re-entered",
        "agent-stalled",
        "-3 hours",
        "-10 minutes",
        Some(("dispatch-old", "-3 hours")),
    );
    seed_in_progress_stall_case(
        &db,
        "card-truly-stalled",
        "Truly Stalled",
        "agent-stalled",
        "-3 hours",
        "-3 hours",
        Some(("dispatch-stale", "-3 hours")),
    );

    let _ = engine.try_fire_hook_by_name("OnTick5min", serde_json::json!({}));
    drain_pending_transitions(&db, &engine);

    let conn = db.lock().unwrap();
    let rows: std::collections::HashMap<String, (String, Option<String>)> = conn
        .prepare("SELECT id, status, blocked_reason FROM kanban_cards ORDER BY id ASC")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, (row.get(1)?, row.get(2)?)))
        })
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();

    assert_eq!(
        rows.get("card-fresh-dispatch").map(|row| row.0.as_str()),
        Some("in_progress"),
        "fresh dispatch must reset the stalled timer"
    );
    assert_eq!(
        rows.get("card-reentered").map(|row| row.0.as_str()),
        Some("in_progress"),
        "in_progress re-entry must reset the stalled timer even if latest dispatch is older"
    );
    assert_ne!(
        rows.get("card-truly-stalled").map(|row| row.0.as_str()),
        Some("in_progress"),
        "truly stale card must still be detected by timeout policy"
    );
    assert!(
        rows.get("card-truly-stalled")
            .and_then(|row| row.1.as_deref())
            .map(|reason| reason.contains("Stalled: no activity"))
            .unwrap_or(false),
        "truly stale card must carry the stalled blocked_reason"
    );
}

#[test]
fn on_tick1min_orphan_review_treats_e2e_dispatch_as_active() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-e2e");

    seed_review_e2e_case(
        &db,
        "card-e2e-review",
        "E2E Review",
        "agent-e2e",
        "-10 minutes",
        "dispatch-e2e",
        "dispatched",
        "-10 minutes",
    );

    let _ = engine.try_fire_hook_by_name("OnTick1min", serde_json::json!({}));
    drain_pending_transitions(&db, &engine);

    let conn = db.lock().unwrap();
    let (status, blocked_reason): (String, Option<String>) = conn
        .query_row(
            "SELECT status, blocked_reason FROM kanban_cards WHERE id = 'card-e2e-review'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-e2e'",
            [],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(
        status, "review",
        "active e2e-test dispatch must keep the card out of orphan review recovery"
    );
    assert!(
        blocked_reason.is_none(),
        "protected review card must not gain an orphan-review blocked_reason"
    );
    assert_eq!(
        dispatch_status, "dispatched",
        "e2e-test dispatch should stay active after onTick1min orphan review sweep"
    );
}

#[test]
fn on_tick30s_orphan_dispatch_recovers_true_orphan_without_regression() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-orphan-330");
    seed_repo(&db, "test-repo");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, latest_dispatch_id, started_at, created_at, updated_at
            ) VALUES (
                'card-orphan-330', 'True Orphan #330', 'in_progress', 'medium', 'agent-orphan-330', 'test-repo',
                330, 'dispatch-orphan-330', datetime('now', '-20 minutes'), datetime('now', '-20 minutes'), datetime('now', '-20 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-orphan-330', 'card-orphan-330', 'agent-orphan-330', 'implementation', 'pending',
                'orphan impl', datetime('now', '-10 minutes'), datetime('now', '-10 minutes')
            )",
            [],
        )
        .unwrap();
    }

    let _ = engine.try_fire_hook_by_name("OnTick30s", serde_json::json!({}));
    drain_pending_transitions(&db, &engine);

    let conn = db.lock().unwrap();
    let card_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-orphan-330'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let (dispatch_status, dispatch_result): (String, Option<String>) = conn
        .query_row(
            "SELECT status, result FROM task_dispatches WHERE id = 'dispatch-orphan-330'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let (decision_signal, chosen_action, audit_dispatch_id): (String, String, Option<String>) =
        conn.query_row(
            "SELECT signal, chosen_action, dispatch_id
             FROM runtime_decisions
             WHERE dispatch_id = 'dispatch-orphan-330'
             ORDER BY id DESC
             LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    let review_dispatch_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches
             WHERE kanban_card_id = 'card-orphan-330' AND dispatch_type = 'review'",
            [],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(
        dispatch_status, "failed",
        "true orphan implementation dispatch must be failed when no agent work was observed"
    );
    assert!(
        dispatch_result
            .as_deref()
            .unwrap_or("")
            .contains("orphan_recovery_rollback"),
        "true orphan recovery must keep the rollback marker"
    );
    assert_ne!(
        card_status, "review",
        "true orphan implementation dispatch must not auto-promote the card into review"
    );
    assert_eq!(
        review_dispatch_count, 0,
        "true orphan recovery must not create a follow-up review dispatch"
    );
    assert_eq!(decision_signal, "OrphanCandidate");
    assert_eq!(chosen_action, "Resume");
    assert_eq!(audit_dispatch_id.as_deref(), Some("dispatch-orphan-330"));
}

#[test]
fn on_tick30s_orphan_dispatch_skips_card_that_moved_to_backlog_mid_recovery() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-orphan-race");
    seed_repo(&db, "test-repo");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, latest_dispatch_id, started_at, created_at, updated_at
            ) VALUES (
                'card-race-330', 'Orphan Race #330', 'in_progress', 'medium', 'agent-orphan-race', 'test-repo',
                330, 'dispatch-race-330', datetime('now', '-20 minutes'), datetime('now', '-20 minutes'), datetime('now', '-20 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-race-330', 'card-race-330', 'agent-orphan-race', 'implementation', 'pending',
                'race impl', datetime('now', '-10 minutes'), datetime('now', '-10 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, 'backlog')",
            [format!(
                "test:runtime_supervisor:orphan_post_complete_override:{}",
                "dispatch-race-330"
            )],
        )
        .unwrap();
    }

    let _ = engine.try_fire_hook_by_name("OnTick30s", serde_json::json!({}));
    drain_pending_transitions(&db, &engine);

    let conn = db.lock().unwrap();
    let card_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-race-330'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-race-330'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let review_dispatch_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches
             WHERE kanban_card_id = 'card-race-330' AND dispatch_type = 'review'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let (chosen_action, decision_note): (String, Option<String>) = conn
        .query_row(
            "SELECT chosen_action, json_extract(evidence_json, '$.supervisor_note')
             FROM runtime_decisions
             WHERE dispatch_id = 'dispatch-race-330'
             ORDER BY id DESC
             LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();

    assert_eq!(
        card_status, "backlog",
        "post-complete race guard must keep a backlogged card from reviving into review"
    );
    assert_eq!(
        dispatch_status, "failed",
        "the orphan implementation dispatch must fail instead of completing without work evidence"
    );
    assert_eq!(
        review_dispatch_count, 0,
        "skipped orphan recovery must not create a follow-up review dispatch"
    );
    assert_eq!(
        chosen_action, "Resume",
        "supervisor should still choose resume before the post-complete race guard trips"
    );
    assert!(
        decision_note
            .as_deref()
            .unwrap_or("")
            .contains("card moved to status=backlog"),
        "runtime_decisions audit must explain why the resume transition was skipped"
    );
}

#[tokio::test]
async fn stalled_cards_and_stats_use_latest_activity_timestamp() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-stalled");
    seed_repo(&db, "test-repo");

    seed_in_progress_stall_case(
        &db,
        "card-fresh-dispatch",
        "Fresh Dispatch",
        "agent-stalled",
        "-3 hours",
        "-3 hours",
        Some(("dispatch-fresh", "-10 minutes")),
    );
    seed_in_progress_stall_case(
        &db,
        "card-reentered",
        "Re-entered",
        "agent-stalled",
        "-3 hours",
        "-10 minutes",
        Some(("dispatch-old", "-3 hours")),
    );
    seed_in_progress_stall_case(
        &db,
        "card-truly-stalled",
        "Truly Stalled",
        "agent-stalled",
        "-3 hours",
        "-3 hours",
        Some(("dispatch-stale", "-3 hours")),
    );

    let app = test_api_router(db, engine, None);

    let stalled_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/kanban-cards/stalled")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stalled_resp.status(), StatusCode::OK);
    let stalled_body = axum::body::to_bytes(stalled_resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let stalled_json: serde_json::Value = serde_json::from_slice(&stalled_body).unwrap();
    let stalled_ids: Vec<String> = stalled_json
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|card| card["id"].as_str().map(ToString::to_string))
        .collect();
    assert_eq!(
        stalled_ids,
        vec!["card-truly-stalled".to_string()],
        "stalled endpoint must ignore fresh-dispatch and re-entered cards"
    );

    let stats_resp = app
        .oneshot(
            Request::builder()
                .uri("/stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stats_resp.status(), StatusCode::OK);
    let stats_body = axum::body::to_bytes(stats_resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let stats_json: serde_json::Value = serde_json::from_slice(&stats_body).unwrap();
    assert_eq!(
        stats_json["kanban"]["stale_in_progress"],
        serde_json::json!(1),
        "stats stale_in_progress count must match latest-activity stalled detection"
    );
}

#[tokio::test]
async fn force_transition_rejects_wrong_channel() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_card_with_status(&db, "card-ft2", "backlog");
    set_pmd_channel(&db, "pmd-chan-123");

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-ft2/force-transition")
                .header("content-type", "application/json")
                .header("x-channel-id", "wrong-channel")
                .body(Body::from(r#"{"status":"ready"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn batch_transition_rejects_wrong_channel() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_card_with_status(&db, "card-bt-auth", "backlog");
    set_pmd_channel(&db, "pmd-chan-123");

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/batch-transition")
                .header("content-type", "application/json")
                .header("x-channel-id", "wrong-channel")
                .body(Body::from(
                    r#"{"card_ids":["card-bt-auth"],"status":"ready"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn force_transition_succeeds_with_correct_channel() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_card_with_status(&db, "card-ft3", "requested");
    set_pmd_channel(&db, "pmd-chan-123");

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-ft3/force-transition")
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
}

#[tokio::test]
async fn batch_transition_returns_per_card_results_and_transitions_targets() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_card_with_status(&db, "card-bt-1", "backlog");
    set_pmd_channel(&db, "pmd-chan-123");

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/batch-transition")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(
                    r#"{"card_ids":["card-bt-1","missing-card"],"status":"ready"}"#,
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
    let results = json["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["card_id"], "card-bt-1");
    assert_eq!(results[0]["ok"], true);
    assert_eq!(results[0]["to"], "ready");
    assert_eq!(results[1]["card_id"], "missing-card");
    assert_eq!(results[1]["ok"], false);

    let conn = db.lock().unwrap();
    let status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-bt-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status, "ready");
}

#[tokio::test]
async fn batch_transition_resolves_issue_numbers_to_cards() {
    let db = test_db();
    let engine = test_engine(&db);
    set_pmd_channel(&db, "pmd-chan-123");
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, github_issue_number, created_at, updated_at
            ) VALUES (
                'card-bt-issue', 'Batch Transition Issue', 'backlog', 'medium', 3277, datetime('now'), datetime('now')
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
                .uri("/kanban-cards/batch-transition")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(
                    r#"{"issue_numbers":[3277,3999],"status":"ready"}"#,
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
    let results = json["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["issue_number"], 3999);
    assert_eq!(results[0]["ok"], false);
    assert_eq!(results[1]["card_id"], "card-bt-issue");
    assert_eq!(results[1]["issue_number"], 3277);
    assert_eq!(results[1]["ok"], true);

    let conn = db.lock().unwrap();
    let status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-bt-issue'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status, "ready");
}

#[tokio::test]
async fn force_transition_to_ready_cancels_live_dispatches_and_skips_auto_queue_entries() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-ft-clean");
    seed_repo(&db, "test-repo");
    set_pmd_channel(&db, "pmd-chan-123");
    ensure_auto_queue_tables(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, latest_dispatch_id, review_status, review_round, review_notes,
                suggestion_pending_at, review_entered_at, awaiting_dod_at,
                created_at, updated_at, started_at
            ) VALUES (
                'card-ft-clean', 'Force Transition Cleanup', 'in_progress', 'medium', 'agent-ft-clean', 'test-repo',
                330, 'dispatch-ft-clean', 'reviewing', 4, 'stale review notes',
                datetime('now', '-12 minutes'), datetime('now', '-11 minutes'), datetime('now', '-10 minutes'),
                datetime('now', '-20 minutes'), datetime('now', '-20 minutes'), datetime('now', '-20 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, result,
                created_at, updated_at, completed_at
            ) VALUES (
                'review-ft-stale', 'card-ft-clean', 'agent-ft-clean', 'review', 'completed',
                'old pass review', '{\"verdict\":\"pass\"}',
                datetime('now', '-2 hours'), datetime('now', '-2 hours'), datetime('now', '-2 hours')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-ft-clean', 'card-ft-clean', 'agent-ft-clean', 'implementation', 'pending',
                'live impl', datetime('now', '-10 minutes'), datetime('now', '-10 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, agent_id, provider, status, active_dispatch_id, last_heartbeat, created_at
            ) VALUES (
                'session-ft-clean', 'agent-ft-clean', 'codex', 'working', 'dispatch-ft-clean',
                datetime('now', '-9 minutes'), datetime('now', '-9 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-ft-clean', 'test-repo', 'agent-ft-clean', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at
            ) VALUES (
                'entry-ft-dispatched', 'run-ft-clean', 'card-ft-clean', 'agent-ft-clean',
                'dispatched', 'dispatch-ft-clean', datetime('now', '-10 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status
            ) VALUES (
                'entry-ft-pending', 'run-ft-clean', 'card-ft-clean', 'agent-ft-clean', 'pending'
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO card_review_state (
                card_id, state, pending_dispatch_id, review_round, last_verdict, last_decision,
                approach_change_round, session_reset_round, review_entered_at, updated_at
            ) VALUES (
                'card-ft-clean', 'suggestion_pending', 'old-review-dispatch', 4, 'pass', 'approved',
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
                .uri("/kanban-cards/card-ft-clean/force-transition")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
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
    assert_eq!(json["forced"], true);
    assert_eq!(json["cancelled_dispatches"], serde_json::json!(1));
    assert_eq!(json["skipped_auto_queue_entries"], serde_json::json!(2));

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
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT status, latest_dispatch_id, review_status, review_round, review_notes,
                    suggestion_pending_at, review_entered_at, awaiting_dod_at
             FROM kanban_cards WHERE id = 'card-ft-clean'",
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
             FROM card_review_state WHERE card_id = 'card-ft-clean'",
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
    let dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-ft-clean'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let entry_rows: Vec<(String, Option<String>)> = conn
        .prepare(
            "SELECT status, dispatch_id FROM auto_queue_entries
             WHERE kanban_card_id = 'card-ft-clean'
             ORDER BY id ASC",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    let (session_status, active_dispatch_id): (String, Option<String>) = conn
        .query_row(
            "SELECT status, active_dispatch_id
             FROM sessions
             WHERE session_key = 'session-ft-clean'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();

    assert_eq!(card_status, "ready");
    assert!(
        latest_dispatch_id.is_none(),
        "force-transition cleanup must clear latest_dispatch_id for backed-out cards"
    );
    assert!(
        review_status.is_none(),
        "force-transition cleanup must clear stale review_status"
    );
    assert_eq!(
        review_round, 0,
        "force-transition cleanup must reset kanban_cards.review_round"
    );
    assert!(
        review_notes.is_none(),
        "force-transition cleanup must clear kanban_cards.review_notes"
    );
    assert!(suggestion_pending_at.is_none());
    assert!(review_entered_at.is_none());
    assert!(awaiting_dod_at.is_none());
    assert_eq!(
        review_state_round, 0,
        "force-transition cleanup must reset card_review_state.review_round"
    );
    assert_eq!(
        review_state_status, "idle",
        "force-transition cleanup must reset card_review_state.state to idle"
    );
    assert!(
        review_state_pending_dispatch.is_none(),
        "force-transition cleanup must clear stale pending review dispatch"
    );
    assert!(
        review_state_verdict.is_none(),
        "force-transition cleanup must clear card_review_state.last_verdict"
    );
    assert!(
        review_state_decision.is_none(),
        "force-transition cleanup must clear card_review_state.last_decision"
    );
    assert!(
        review_state_approach_change_round.is_none(),
        "force-transition cleanup must clear card_review_state.approach_change_round"
    );
    assert!(
        review_state_session_reset_round.is_none(),
        "force-transition cleanup must clear card_review_state.session_reset_round"
    );
    assert!(
        review_state_entered_at.is_none(),
        "force-transition cleanup must clear card_review_state.review_entered_at"
    );
    assert_eq!(
        dispatch_status, "cancelled",
        "force-transition to ready must cancel the live dispatch"
    );
    assert_eq!(
        entry_rows,
        vec![("skipped".to_string(), None), ("skipped".to_string(), None),],
        "force-transition cleanup must skip live auto-queue entries and clear dispatch links"
    );
    assert_eq!(
        session_status, "idle",
        "force-transition cleanup must demote working sessions off the cancelled dispatch"
    );
    assert!(
        active_dispatch_id.is_none(),
        "force-transition cleanup must clear stale session active_dispatch_id"
    );

    drop(conn);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-ft-clean-retry', 'card-ft-clean', 'agent-ft-clean', 'implementation', 'pending',
                'retry impl', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let verify_engine = test_engine(&db);
    crate::kanban::transition_status(&db, &verify_engine, "card-ft-clean", "requested").unwrap();
    crate::kanban::transition_status(&db, &verify_engine, "card-ft-clean", "in_progress").unwrap();
    crate::kanban::transition_status(&db, &verify_engine, "card-ft-clean", "review").unwrap();

    let conn = db.lock().unwrap();
    let (reentered_round, reentered_at): (i64, Option<String>) = conn
        .query_row(
            "SELECT review_round, review_entered_at FROM kanban_cards WHERE id = 'card-ft-clean'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let (reentered_review_state_round, reentered_review_state_status): (i64, String) = conn
        .query_row(
            "SELECT review_round, state FROM card_review_state WHERE card_id = 'card-ft-clean'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();

    assert_eq!(
        reentered_round, 1,
        "force-transitioned card must restart review_round at R1 on next review entry"
    );
    assert!(
        reentered_at.is_some(),
        "re-entering review must stamp a fresh review_entered_at"
    );
    assert_eq!(
        reentered_review_state_round, 1,
        "card_review_state.review_round must also restart from 1 after force-transition"
    );
    assert_eq!(
        reentered_review_state_status, "reviewing",
        "card_review_state.state must reflect the new review round"
    );
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
async fn dispute_repeat_does_not_reuse_poisoned_review_target() {
    crate::pipeline::ensure_loaded();
    let _env_lock = env_lock();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-dispute-repeat");
    seed_repo(&db, "test-repo");
    ensure_auto_queue_tables(&db);

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

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, latest_dispatch_id, review_status, created_at, updated_at
            ) VALUES (
                'card-dispute-repeat', 'Issue #472', 'review', 'medium', 'agent-dispute-repeat', 'test-repo',
                472, 'rd-dispute-1', 'suggestion_pending', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result,
                created_at, updated_at, completed_at
            ) VALUES (
                'impl-dispute-repeat', 'card-dispute-repeat', 'agent-dispute-repeat', 'implementation',
                'completed', 'impl', ?1, ?2, datetime('now', '-10 minutes'), datetime('now', '-10 minutes'),
                datetime('now', '-10 minutes')
            )",
            rusqlite::params![
                serde_json::json!({
                    "worktree_path": worktree_path,
                    "branch": "wt/472-poison"
                })
                .to_string(),
                serde_json::json!({
                    "completed_worktree_path": worktree_path,
                    "completed_branch": "wt/472-poison",
                    "completed_commit": poisoned_commit.clone(),
                })
                .to_string(),
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                created_at, updated_at
            ) VALUES (
                'rd-dispute-1', 'card-dispute-repeat', 'agent-dispute-repeat', 'review-decision',
                'pending', '[Review Decision]', datetime('now', '-1 minutes'), datetime('now', '-1 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO card_review_state (
                card_id, state, pending_dispatch_id, review_round, updated_at
            ) VALUES (
                'card-dispute-repeat', 'suggestion_pending', 'rd-dispute-1', 1, datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let dispute_request = |dispatch_id: &str| {
        Request::builder()
            .method("POST")
            .uri("/review-decision")
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

    {
        let conn = db.lock().unwrap();
        let first_rd_status: String = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = 'rd-dispute-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(first_rd_status, "completed");

        conn.execute(
            "UPDATE task_dispatches
             SET status = 'completed', completed_at = datetime('now'), updated_at = datetime('now')
             WHERE id = ?1",
            [&first_review_dispatch_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                created_at, updated_at
            ) VALUES (
                'rd-dispute-2', 'card-dispute-repeat', 'agent-dispute-repeat', 'review-decision',
                'pending', '[Review Decision 2]', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE kanban_cards
             SET latest_dispatch_id = 'rd-dispute-2', review_status = 'suggestion_pending', updated_at = datetime('now')
             WHERE id = 'card-dispute-repeat'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE card_review_state
             SET state = 'suggestion_pending', pending_dispatch_id = 'rd-dispute-2', updated_at = datetime('now')
             WHERE card_id = 'card-dispute-repeat'",
            [],
        )
        .unwrap();
    }

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

    let conn = db.lock().unwrap();
    let second_context = conn
        .query_row(
            "SELECT context FROM task_dispatches WHERE id = ?1",
            [&second_review_dispatch_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .unwrap()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
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
}

#[tokio::test]
async fn reopen_reactivates_done_card_without_deadlocking_review_tuning_fixup() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-reopen");
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
                review_status, created_at, updated_at, completed_at
            ) VALUES (
                'card-reopen', 'Issue #270', 'done', 'medium', 'agent-reopen', 'test-repo',
                'pass', datetime('now'), datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-reopen', 'test-repo', 'agent-reopen', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, completed_at
            ) VALUES (
                'entry-reopen', 'run-reopen', 'card-reopen', 'agent-reopen',
                'done', datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO review_tuning_outcomes (
                card_id, dispatch_id, review_round, verdict, decision, outcome
            ) VALUES (
                'card-reopen', 'review-pass', 1, 'pass', 'approved', 'true_negative'
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

    let conn = db.lock().unwrap();
    let (status, review_status, completed_at): (String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT status, review_status, completed_at
             FROM kanban_cards WHERE id = 'card-reopen'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(status, reopen_target);
    assert_eq!(review_status.as_deref(), Some("queued"));
    assert!(completed_at.is_none());

    let entry_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-reopen'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(entry_status, "dispatched");

    let outcome: String = conn
        .query_row(
            "SELECT outcome FROM review_tuning_outcomes
             WHERE card_id = 'card-reopen'
             ORDER BY review_round DESC, id DESC
             LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(outcome, "false_negative");
}

#[tokio::test]
async fn reopen_skips_preflight_already_applied_for_pmd_reopen() {
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
                .body(Body::from(r#"{"reason":"skip preflight on PMD reopen"}"#))
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
        "PMD reopen must skip already_applied preflight and keep card reopened"
    );
    let metadata: serde_json::Value =
        serde_json::from_str(metadata_raw.as_deref().unwrap_or("{}")).unwrap();
    assert_eq!(metadata["preflight_status"], "skipped");
    assert_eq!(metadata["preflight_summary"], "Skipped for PMD reopen");
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
                'session-reopen-reset', 'agent-reopen-reset', 'codex', 'working', 'dispatch-reopen-reset',
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
                'entry-reopen-done', 'run-reopen-reset', 'card-reopen-reset', 'agent-reopen-reset',
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
async fn auto_queue_dispatch_prepares_backlog_cards_and_auto_assigns_agent() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "project-agentdesk");
    ensure_auto_queue_tables(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, created_at, updated_at
            ) VALUES (
                'card-dq-423', 'Issue #423', 'backlog', 'high', NULL, 'test-repo', 423,
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, created_at, updated_at
            ) VALUES (
                'card-dq-405', 'Issue #405', 'ready', 'medium', NULL, 'test-repo', 405,
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, created_at, updated_at
            ) VALUES (
                'card-dq-407', 'Issue #407', 'requested', 'medium', NULL, 'test-repo', 407,
                datetime('now'), datetime('now')
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
                .uri("/auto-queue/dispatch")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "project-agentdesk",
                        "groups": [
                            {"issues": [423, 405], "sequential": true},
                            {"issues": [407]}
                        ],
                        "activate": false
                    }))
                    .unwrap(),
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
    assert_eq!(json["activated"], false);
    assert_eq!(json["requested"]["auto_assign_agent"], true);
    assert_eq!(json["run"]["status"], "generated");

    let run_id = json["run"]["id"]
        .as_str()
        .expect("dispatch run id must be present");
    let entries = json["entries"]
        .as_array()
        .expect("dispatch snapshot must include entries");
    assert_eq!(entries.len(), 3);

    let conn = db.lock().unwrap();
    let assigned_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM kanban_cards
             WHERE id IN ('card-dq-423', 'card-dq-405', 'card-dq-407')
               AND assigned_agent_id = 'project-agentdesk'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(assigned_count, 3);

    let backlog_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-dq-423'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(backlog_status, "ready");

    let entry_layout: Vec<(i64, i64, i64)> = {
        let mut stmt = conn
            .prepare(
                "SELECT kc.github_issue_number, e.thread_group, e.priority_rank
                 FROM auto_queue_entries e
                 JOIN kanban_cards kc ON kc.id = e.kanban_card_id
                 WHERE e.run_id = ?1
                 ORDER BY kc.github_issue_number ASC",
            )
            .unwrap();
        stmt.query_map([run_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .collect()
    };
    assert_eq!(entry_layout, vec![(405, 0, 1), (407, 1, 0), (423, 0, 0)]);
}

#[tokio::test]
async fn auto_queue_dispatch_reuses_existing_active_run_and_enqueues_entries() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_repo(&db, "test-repo");
    seed_agent(&db, "project-agentdesk");
    seed_auto_queue_card(
        &db,
        "card-dispatch-existing",
        4901,
        "ready",
        "project-agentdesk",
    );
    seed_auto_queue_card(
        &db,
        "card-dispatch-backlog",
        4902,
        "backlog",
        "project-agentdesk",
    );
    seed_auto_queue_card(
        &db,
        "card-dispatch-ready",
        4903,
        "ready",
        "project-agentdesk",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count, created_at
            ) VALUES (
                'run-dispatch-active', 'test-repo', 'project-agentdesk', 'active', 1, 1, datetime('now', '-1 minute')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-dispatch-existing', 'run-dispatch-active', 'card-dispatch-existing', 'project-agentdesk', 'pending', 0, 0
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
                .uri("/auto-queue/dispatch")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "project-agentdesk",
                        "groups": [
                            {"issues": [4903], "thread_group": 0, "batch_phase": 1},
                            {"issues": [4902], "thread_group": 7, "batch_phase": 3}
                        ],
                        "activate": false
                    }))
                    .unwrap(),
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
    assert_eq!(json["run"]["id"], "run-dispatch-active");
    assert_eq!(json["run"]["status"], "active");
    assert_eq!(json["activated"], false);

    let conn = db.lock().unwrap();
    let total_runs: i64 = conn
        .query_row("SELECT COUNT(*) FROM auto_queue_runs", [], |row| row.get(0))
        .unwrap();
    let active_runs: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_runs WHERE status = 'active'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(total_runs, 1, "dispatch must reuse the existing active run");
    assert_eq!(
        active_runs, 1,
        "dispatch must not create a second active run"
    );

    let backlog_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-dispatch-backlog'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(backlog_status, "ready");

    let entry_layout: Vec<(i64, i64, i64, i64)> = {
        let mut stmt = conn
            .prepare(
                "SELECT kc.github_issue_number, e.thread_group, e.priority_rank, COALESCE(e.batch_phase, 0)
                 FROM auto_queue_entries e
                 JOIN kanban_cards kc ON kc.id = e.kanban_card_id
                 WHERE e.run_id = 'run-dispatch-active'
                 ORDER BY kc.github_issue_number ASC",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .unwrap()
        .map(|row| row.unwrap())
        .collect()
    };
    assert_eq!(
        entry_layout,
        vec![(4901, 0, 0, 0), (4902, 7, 0, 3), (4903, 0, 1, 1)]
    );

    let run_meta: (i64, i64) = conn
        .query_row(
            "SELECT max_concurrent_threads, thread_group_count
             FROM auto_queue_runs
             WHERE id = 'run-dispatch-active'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(run_meta, (2, 2));
}

#[tokio::test]
async fn auto_queue_update_entry_moves_pending_entry_and_syncs_run_groups() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_agent(&db, "agent-update");
    seed_auto_queue_card(&db, "card-update-1", 1801, "ready", "agent-update");
    seed_auto_queue_card(&db, "card-update-2", 1802, "ready", "agent-update");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-update-entry', 'test-repo', 'agent-update', 'generated', 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-update-1', 'run-update-entry', 'card-update-1', 'agent-update', 'pending', 0, 0
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-update-2', 'run-update-entry', 'card-update-2', 'agent-update', 'pending', 1, 0
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
                .uri("/auto-queue/entries/entry-update-2")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "thread_group": 3,
                        "priority_rank": 0
                    }))
                    .unwrap(),
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
    assert_eq!(json["ok"], true);
    assert_eq!(json["entry"]["thread_group"], 3);
    assert_eq!(json["entry"]["priority_rank"], 0);

    let conn = db.lock().unwrap();
    let run_meta: (i64, i64) = conn
        .query_row(
            "SELECT max_concurrent_threads, thread_group_count
             FROM auto_queue_runs
             WHERE id = 'run-update-entry'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(run_meta, (2, 2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_queue_activate_active_only_does_not_promote_generated_runs() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-active-only");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-active-run", 1701, "ready", "agent-active-only");
    seed_auto_queue_card(
        &db,
        "card-generated-run",
        1702,
        "ready",
        "agent-active-only",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-active', 'test-repo', 'agent-active-only', 'active', datetime('now', '-2 minutes'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-generated', 'test-repo', 'agent-active-only', 'generated', datetime('now', '-1 minutes'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-active', 'run-active', 'card-active-run', 'agent-active-only', 'pending', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-generated', 'run-generated', 'card-generated-run', 'agent-active-only', 'pending', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/activate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "agent_id": "agent-active-only",
                        "active_only": true,
                    }))
                    .unwrap(),
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
    assert_eq!(json["count"], 1);
    assert_eq!(json["dispatched"][0]["card_id"], "card-active-run");

    let conn = db.lock().unwrap();
    let generated_run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-generated'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let generated_entry_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-generated'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let active_dispatch_card: String = conn
        .query_row(
            "SELECT kanban_card_id FROM task_dispatches ORDER BY rowid DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(generated_run_status, "generated");
    assert_eq!(generated_entry_status, "pending");
    assert_eq!(active_dispatch_card, "card-active-run");
}

/// #162: A card in 'requested' state, assigned to the same agent, must not
/// be blocked by the busy-agent guard when that card itself is the dispatch target.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_queue_activate_requested_card_not_blocked_by_own_status() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-req-self");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-req-self", 1630, "requested", "agent-req-self");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
             VALUES ('run-req-self', 'test-repo', 'agent-req-self', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-req-self', 'run-req-self', 'card-req-self', 'agent-req-self', 'pending', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/activate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "agent_id": "agent-req-self",
                    }))
                    .unwrap(),
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
    assert_eq!(
        json["count"], 1,
        "activate must succeed — busy guard must exclude the card being dispatched"
    );

    let conn = db.lock().unwrap();
    let entry_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-req-self'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(entry_status, "dispatched");
}

/// #162/#500: A card in 'backlog' (non-dispatchable) state must be walked
/// to the dispatchable state via canonical transitions before dispatch creation.
/// The walk must preserve the same requested-state hook side-effects as a
/// manual transition.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_queue_activate_walks_backlog_card_to_dispatchable_state() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-walk");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-walk-bl", 1631, "backlog", "agent-walk");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "UPDATE kanban_cards
             SET description = ?1
             WHERE id = 'card-walk-bl'",
            ["DoD: keep auto-queue walk hook parity and preserve activation behavior."],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
             VALUES ('run-walk', 'test-repo', 'agent-walk', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-walk', 'run-walk', 'card-walk-bl', 'agent-walk', 'pending', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/activate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "agent_id": "agent-walk",
                    }))
                    .unwrap(),
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
    assert_eq!(
        json["count"], 1,
        "activate must succeed for backlog card via silent walk"
    );

    // Verify the card was walked through free transitions and dispatch was created
    let conn = db.lock().unwrap();
    let entry_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-walk'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(entry_status, "dispatched");

    // Card should have been dispatched (moved past backlog via silent walk)
    let dispatch_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-walk-bl'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        dispatch_count, 1,
        "exactly one dispatch must be created for the walked card"
    );

    let metadata: Option<String> = conn
        .query_row(
            "SELECT metadata FROM kanban_cards WHERE id = 'card-walk-bl'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let metadata_json: serde_json::Value =
        serde_json::from_str(metadata.as_deref().expect("walk must persist metadata")).unwrap();
    assert_eq!(
        metadata_json["preflight_status"], "clear",
        "requested-state preflight hook must run during auto-queue walk"
    );
}

/// #500: If the requested-state hook decides the card is already applied,
/// activate() must respect that side-effect instead of creating a new dispatch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_queue_activate_walk_respects_requested_hook_skip() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-walk-skip");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-walk-skip", 1632, "backlog", "agent-walk-skip");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at, completed_at
            ) VALUES (
                'dispatch-walk-skip', 'card-walk-skip', 'agent-walk-skip', 'implementation', 'completed',
                'Existing implementation', datetime('now'), datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
             VALUES ('run-walk-skip', 'test-repo', 'agent-walk-skip', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-walk-skip', 'run-walk-skip', 'card-walk-skip', 'agent-walk-skip', 'pending', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/activate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "agent_id": "agent-walk-skip",
                    }))
                    .unwrap(),
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
    assert_eq!(
        json["count"], 0,
        "activate must not create a new dispatch when requested-state preflight skips the card"
    );

    let conn = db.lock().unwrap();
    let (card_status, entry_status): (String, String) = conn
        .query_row(
            "SELECT
                (SELECT status FROM kanban_cards WHERE id = 'card-walk-skip'),
                (SELECT status FROM auto_queue_entries WHERE id = 'entry-walk-skip')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(card_status, "done");
    assert_eq!(entry_status, "skipped");

    let dispatch_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-walk-skip'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        dispatch_count, 1,
        "hook-driven skip must not create an additional dispatch"
    );
}

/// #430: legacy unified_thread runs still dispatch, but via slot pooling.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_queue_activate_legacy_unified_thread_run_dispatches_via_slot_pool() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-unified");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-unified-1", 1625, "ready", "agent-unified");
    seed_auto_queue_card(&db, "card-unified-2", 1626, "ready", "agent-unified");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, unified_thread) \
             VALUES ('run-unified', 'test-repo', 'agent-unified', 'active', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-u1', 'run-unified', 'card-unified-1', 'agent-unified', 'pending', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-u2', 'run-unified', 'card-unified-2', 'agent-unified', 'pending', 1)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/activate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "agent_id": "agent-unified",
                    }))
                    .unwrap(),
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
    assert_eq!(json["count"], 1, "first activate dispatches one entry");
    assert_eq!(json["dispatched"][0]["card_id"], "card-unified-1");

    // Verify dispatch was created and entry was linked
    let conn = db.lock().unwrap();
    let (entry_status, dispatch_id): (String, Option<String>) = conn
        .query_row(
            "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-u1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(entry_status, "dispatched");
    let dispatch_id = dispatch_id.expect("entry must have linked dispatch_id");

    // Verify the dispatch references the correct card
    let dispatch_card: String = conn
        .query_row(
            "SELECT kanban_card_id FROM task_dispatches WHERE id = ?1",
            [&dispatch_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dispatch_card, "card-unified-1");
    let notify_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM dispatch_outbox WHERE dispatch_id = ?1 AND action = 'notify'",
            [&dispatch_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        notify_count, 1,
        "auto-queue activation must use canonical notify persistence"
    );

    // Second entry stays pending (sequential within group)
    let entry2_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-u2'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(entry2_status, "pending");

    // Run stays active (not prematurely completed)
    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-unified'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(run_status, "active");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_queue_activate_consult_required_creates_consultation_dispatch() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-consult");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-consult", 1720, "ready", "agent-consult");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "UPDATE kanban_cards SET metadata = ?1 WHERE id = 'card-consult'",
            [serde_json::json!({
                "preflight_status": "consult_required",
                "preflight_summary": "need counter review"
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
             VALUES ('run-consult', 'test-repo', 'agent-consult', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-consult', 'run-consult', 'card-consult', 'agent-consult', 'pending', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/activate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-consult",
                        "active_only": true
                    }))
                    .unwrap(),
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
    assert_eq!(
        json["count"], 1,
        "consultation dispatch should count as dispatched"
    );

    let conn = db.lock().unwrap();
    let (entry_status, dispatch_id): (String, Option<String>) = conn
        .query_row(
            "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-consult'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(entry_status, "dispatched");
    let dispatch_id = dispatch_id.expect("consultation dispatch id must be stored");

    let (dispatch_type, to_agent_id): (String, String) = conn
        .query_row(
            "SELECT dispatch_type, to_agent_id FROM task_dispatches WHERE id = ?1",
            [&dispatch_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(dispatch_type, "consultation");
    assert_eq!(to_agent_id, "agent-consult");

    let metadata_raw: String = conn
        .query_row(
            "SELECT metadata FROM kanban_cards WHERE id = 'card-consult'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let metadata: serde_json::Value = serde_json::from_str(&metadata_raw).unwrap();
    assert_eq!(metadata["consultation_status"], "pending");
    assert_eq!(metadata["consultation_dispatch_id"], dispatch_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_queue_activate_consult_required_prefers_registry_counterpart_provider() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-qwen");
    seed_agent(&db, "agent-codex");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-consult-qwen", 1721, "ready", "agent-qwen");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "UPDATE agents SET provider = 'qwen' WHERE id = 'agent-qwen'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE agents SET provider = 'codex' WHERE id = 'agent-codex'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE kanban_cards SET metadata = ?1 WHERE id = 'card-consult-qwen'",
            [serde_json::json!({
                "preflight_status": "consult_required",
                "preflight_summary": "need external consultation"
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
             VALUES ('run-consult-qwen', 'test-repo', 'agent-qwen', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-consult-qwen', 'run-consult-qwen', 'card-consult-qwen', 'agent-qwen', 'pending', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/activate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-consult-qwen",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let conn = db.lock().unwrap();
    let to_agent_id: String = conn
        .query_row(
            "SELECT to_agent_id
             FROM task_dispatches
             WHERE kanban_card_id = 'card-consult-qwen'
             ORDER BY created_at DESC
             LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(to_agent_id, "agent-codex");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_queue_activate_already_applied_skips_entry_and_completes_run() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-skip");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-skip", 1721, "ready", "agent-skip");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "UPDATE kanban_cards SET metadata = ?1 WHERE id = 'card-skip'",
            [serde_json::json!({
                "preflight_status": "already_applied",
                "preflight_summary": "nothing to do"
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
             VALUES ('run-skip', 'test-repo', 'agent-skip', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-skip', 'run-skip', 'card-skip', 'agent-skip', 'pending', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/activate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-skip",
                        "active_only": true
                    }))
                    .unwrap(),
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
    assert_eq!(
        json["count"], 0,
        "already_applied entry should be skipped, not dispatched"
    );

    let conn = db.lock().unwrap();
    let (entry_status, dispatch_id): (String, Option<String>) = conn
        .query_row(
            "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-skip'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(entry_status, "skipped");
    assert!(
        dispatch_id.is_none(),
        "skipped entry must not create a dispatch"
    );

    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-skip'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(run_status, "completed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_queue_activate_reuses_released_slot_for_next_group() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-slot");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-slot-0", 1722, "ready", "agent-slot");
    seed_auto_queue_card(&db, "card-slot-1", 1723, "ready", "agent-slot");
    seed_auto_queue_card(&db, "card-slot-2", 1724, "ready", "agent-slot");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
             VALUES ('agent-slot', 0, ?1)",
            [json!({"111": "222000000000000001"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
             VALUES ('agent-slot', 1, ?1)",
            [json!({"111": "222000000000000002"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, agent_id, provider, status, session_info, tokens,
                active_dispatch_id, thread_channel_id, claude_session_id,
                last_heartbeat, created_at
            ) VALUES (
                'host:AgentDesk-claude-slot-thread-0', 'agent-slot', 'claude', 'working',
                'slot 0 seed', 41, 'dispatch-slot-old-0', '222000000000000001', 'claude-slot-0',
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, agent_id, provider, status, session_info, tokens,
                active_dispatch_id, thread_channel_id, claude_session_id,
                last_heartbeat, created_at
            ) VALUES (
                'host:AgentDesk-claude-slot-thread-1', 'agent-slot', 'claude', 'working',
                'slot 1 seed', 73, 'dispatch-slot-old-1', '222000000000000002', 'claude-slot-1',
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, unified_thread,
                max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-slot', 'test-repo', 'agent-slot', 'active', 1, 2, 3
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group) \
             VALUES ('entry-slot-0', 'run-slot', 'card-slot-0', 'agent-slot', 'pending', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group) \
             VALUES ('entry-slot-1', 'run-slot', 'card-slot-1', 'agent-slot', 'pending', 1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group) \
             VALUES ('entry-slot-2', 'run-slot', 'card-slot-2', 'agent-slot', 'pending', 2, 2)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let first_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/activate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-slot",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(first_response.status(), StatusCode::OK);
    let first_body = axum::body::to_bytes(first_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let first_json: serde_json::Value = serde_json::from_slice(&first_body).unwrap();
    assert_eq!(
        first_json["count"], 1,
        "first activation must dispatch only one group for the same agent"
    );

    {
        let conn = db.lock().unwrap();
        let first_slot_session: (String, Option<String>, i64, Option<String>) = conn
            .query_row(
                "SELECT status, active_dispatch_id, COALESCE(tokens, 0), claude_session_id
                 FROM sessions WHERE thread_channel_id = '222000000000000001'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        let second_slot_session: (String, Option<String>, i64, Option<String>) = conn
            .query_row(
                "SELECT status, active_dispatch_id, COALESCE(tokens, 0), claude_session_id
                 FROM sessions WHERE thread_channel_id = '222000000000000002'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            first_slot_session.0, "idle",
            "slot session status must be idle after release"
        );
        assert_eq!(
            first_slot_session.1, None,
            "active_dispatch_id must be cleared on slot release"
        );
        assert_eq!(
            first_slot_session.2, 0,
            "tokens must be cleared so the slot starts from a fresh session"
        );
        assert!(
            first_slot_session.3.is_none(),
            "claude_session_id must be cleared on slot release"
        );
        assert_eq!(
            second_slot_session.0, "working",
            "unused sibling slot must stay untouched until it is actually reused"
        );
        assert_eq!(
            second_slot_session.1.as_deref(),
            Some("dispatch-slot-old-1"),
            "unused sibling slot must keep its original dispatch context"
        );
        assert_eq!(
            second_slot_session.2, 73,
            "unused sibling slot must preserve existing token state"
        );
        assert!(
            second_slot_session.3.as_deref() == Some("claude-slot-1"),
            "unused sibling slot must preserve claude_session_id"
        );
        let first_slot: Option<i64> = conn
            .query_row(
                "SELECT slot_index FROM auto_queue_entries WHERE id = 'entry-slot-0'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let second_slot: Option<i64> = conn
            .query_row(
                "SELECT slot_index FROM auto_queue_entries WHERE id = 'entry-slot-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(first_slot, Some(0));
        assert_eq!(second_slot, None);
        conn.execute(
            "UPDATE sessions
             SET status = 'working',
                 session_info = 'slot 0 in-progress context',
                 tokens = 99,
                 active_dispatch_id = 'dispatch-slot-in-progress',
                 claude_session_id = 'claude-slot-0-rehydrated'
             WHERE thread_channel_id = '222000000000000001'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE sessions
             SET status = 'working',
                 session_info = 'slot 1 should stay hot',
                 tokens = 123,
                 active_dispatch_id = 'dispatch-slot-1-hot',
                 claude_session_id = 'claude-slot-1-hot'
             WHERE thread_channel_id = '222000000000000002'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE auto_queue_entries SET status = 'done', completed_at = datetime('now') WHERE id = 'entry-slot-0'",
            [],
        )
        .unwrap();
    }

    let second_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/activate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-slot",
                        "thread_group": 2,
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(second_response.status(), StatusCode::OK);
    let second_body = axum::body::to_bytes(second_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let second_json: serde_json::Value = serde_json::from_slice(&second_body).unwrap();
    assert_eq!(
        second_json["count"], 1,
        "released slot should allow next group dispatch"
    );

    let conn = db.lock().unwrap();
    let recycled_slot: Option<i64> = conn
        .query_row(
            "SELECT slot_index FROM auto_queue_entries WHERE id = 'entry-slot-2'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        recycled_slot,
        Some(0),
        "completed group slot must be reused for the next group"
    );

    let slot_zero_group: Option<i64> = conn
        .query_row(
            "SELECT assigned_thread_group FROM auto_queue_slots WHERE agent_id = 'agent-slot' AND slot_index = 0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let slot_one_group: Option<i64> = conn
        .query_row(
            "SELECT assigned_thread_group FROM auto_queue_slots WHERE agent_id = 'agent-slot' AND slot_index = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(slot_zero_group, Some(2));
    assert_eq!(slot_one_group, None);

    let recycled_slot_session: (String, Option<String>, i64, Option<String>) = conn
        .query_row(
            "SELECT status, active_dispatch_id, COALESCE(tokens, 0), claude_session_id
             FROM sessions WHERE thread_channel_id = '222000000000000001'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    let untouched_slot_session: (String, Option<String>, i64, Option<String>) = conn
        .query_row(
            "SELECT status, active_dispatch_id, COALESCE(tokens, 0), claude_session_id
             FROM sessions WHERE thread_channel_id = '222000000000000002'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        recycled_slot_session.0, "idle",
        "status must be idle after slot release"
    );
    assert_eq!(
        recycled_slot_session.1, None,
        "active_dispatch_id must be cleared"
    );
    assert_eq!(
        recycled_slot_session.2, 0,
        "recycled slot must clear prior token counts before the next dispatch"
    );
    assert!(
        recycled_slot_session.3.is_none(),
        "recycled slot must clear claude_session_id before the next dispatch"
    );
    assert_eq!(
        untouched_slot_session,
        (
            "working".to_string(),
            Some("dispatch-slot-1-hot".to_string()),
            123,
            Some("claude-slot-1-hot".to_string())
        ),
        "active sibling group must not be cleared while it is still reusing its own context"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_queue_activate_reuses_same_group_slot_with_fresh_session_each_time() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-same-group");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-same-group-0", 1822, "ready", "agent-same-group");
    seed_auto_queue_card(&db, "card-same-group-1", 1823, "ready", "agent-same-group");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
             VALUES ('agent-same-group', 0, ?1)",
            [json!({"111": "222000000000000101"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, agent_id, provider, status, session_info, tokens,
                active_dispatch_id, thread_channel_id, claude_session_id,
                last_heartbeat, created_at
            ) VALUES (
                'host:AgentDesk-claude-same-group-thread', 'agent-same-group', 'claude', 'working',
                'slot seed', 17, 'dispatch-same-group-seed', '222000000000000101', 'claude-same-group-seed',
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, unified_thread,
                max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-same-group', 'test-repo', 'agent-same-group', 'active', 1, 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group)
             VALUES ('entry-same-group-0', 'run-same-group', 'card-same-group-0', 'agent-same-group', 'pending', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group)
             VALUES ('entry-same-group-1', 'run-same-group', 'card-same-group-1', 'agent-same-group', 'pending', 1, 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let first_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/activate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-same-group",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(first_response.status(), StatusCode::OK);

    {
        let conn = db.lock().unwrap();
        let cleared_session: (String, Option<String>, i64, Option<String>) = conn
            .query_row(
                "SELECT status, active_dispatch_id, COALESCE(tokens, 0), claude_session_id
                 FROM sessions WHERE thread_channel_id = '222000000000000101'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            cleared_session,
            ("idle".to_string(), None, 0, None),
            "slot release must clear provider session continuity before dispatch"
        );
        conn.execute(
            "UPDATE sessions
             SET status = 'working',
                 session_info = 'group context retained',
                 tokens = 77,
                 active_dispatch_id = 'dispatch-same-group-hot',
                 claude_session_id = 'claude-same-group-hot'
             WHERE thread_channel_id = '222000000000000101'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE auto_queue_entries
             SET status = 'done', completed_at = datetime('now')
             WHERE id = 'entry-same-group-0'",
            [],
        )
        .unwrap();
    }

    let second_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/activate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-same-group",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(second_response.status(), StatusCode::OK);

    let conn = db.lock().unwrap();
    let continued_session: (String, Option<String>, i64, Option<String>) = conn
        .query_row(
            "SELECT status, active_dispatch_id, COALESCE(tokens, 0), claude_session_id
             FROM sessions WHERE thread_channel_id = '222000000000000101'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    let slot_group: Option<i64> = conn
        .query_row(
            "SELECT assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = 'agent-same-group' AND slot_index = 0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        continued_session,
        ("idle".to_string(), None, 0, None),
        "same-group continuation must keep the slot assignment but start from a fresh session"
    );
    assert_eq!(
        slot_group,
        Some(0),
        "same-group continuation must keep the original slot assignment"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_queue_activate_does_not_dispatch_same_group_follow_up_while_prior_is_active() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-same-group-guard");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(
        &db,
        "card-same-group-guard-0",
        4160,
        "ready",
        "agent-same-group-guard",
    );
    seed_auto_queue_card(
        &db,
        "card-same-group-guard-1",
        4161,
        "ready",
        "agent-same-group-guard",
    );
    seed_auto_queue_card(
        &db,
        "card-same-group-guard-2",
        4162,
        "ready",
        "agent-same-group-guard",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
             VALUES ('agent-same-group-guard', 0, ?1)",
            [json!({"111": "222000000000000201"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
             VALUES ('agent-same-group-guard', 1, ?1)",
            [json!({"111": "222000000000000202"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, unified_thread,
                max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-same-group-guard', 'test-repo', 'agent-same-group-guard', 'active', 1, 2, 2
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group)
             VALUES ('entry-same-group-guard-0', 'run-same-group-guard', 'card-same-group-guard-0', 'agent-same-group-guard', 'pending', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group)
             VALUES ('entry-same-group-guard-1', 'run-same-group-guard', 'card-same-group-guard-1', 'agent-same-group-guard', 'pending', 1, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group)
             VALUES ('entry-same-group-guard-2', 'run-same-group-guard', 'card-same-group-guard-2', 'agent-same-group-guard', 'pending', 0, 1)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let first_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/activate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-same-group-guard",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(first_response.status(), StatusCode::OK);
    let first_body = axum::body::to_bytes(first_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let first_json: serde_json::Value = serde_json::from_slice(&first_body).unwrap();
    assert_eq!(
        first_json["count"], 1,
        "first activate must dispatch only one group for the same agent"
    );

    let second_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/activate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-same-group-guard",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(second_response.status(), StatusCode::OK);
    let second_body = axum::body::to_bytes(second_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let second_json: serde_json::Value = serde_json::from_slice(&second_body).unwrap();
    assert_eq!(
        second_json["count"], 0,
        "same-group follow-up must stay pending while the prior entry is still dispatched"
    );

    let conn = db.lock().unwrap();
    let guard_entry_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-same-group-guard-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        guard_entry_status, "pending",
        "follow-up entry must not be marked dispatched before prior same-group work completes"
    );
    let sibling_group_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-same-group-guard-2'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        sibling_group_status, "pending",
        "different thread_group for the same agent must also stay pending until the next activate"
    );

    let guard_dispatch_count: i64 = conn
        .query_row(
            "SELECT COUNT(*)
             FROM task_dispatches
             WHERE kanban_card_id = 'card-same-group-guard-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        guard_dispatch_count, 0,
        "no dispatch row should be created for the blocked same-group follow-up"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_queue_activate_expands_slot_pool_to_run_max_concurrency() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-slot-expand");
    ensure_auto_queue_tables(&db);

    for issue_number in 0..4 {
        seed_auto_queue_card(
            &db,
            &format!("card-slot-expand-{issue_number}"),
            1900 + issue_number,
            "ready",
            "agent-slot-expand",
        );
    }

    {
        let conn = db.lock().unwrap();
        for slot_index in 0..3 {
            conn.execute(
                "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
                 VALUES (?1, ?2, '{}')",
                rusqlite::params!["agent-slot-expand", slot_index],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, unified_thread,
                max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-slot-expand', 'test-repo', 'agent-slot-expand', 'active', 1, 4, 4
            )",
            [],
        )
        .unwrap();
        for (priority_rank, thread_group) in (0..4).enumerate() {
            conn.execute(
                "INSERT INTO auto_queue_entries (
                    id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
                ) VALUES (?1, 'run-slot-expand', ?2, 'agent-slot-expand', 'pending', ?3, ?4)",
                rusqlite::params![
                    format!("entry-slot-expand-{thread_group}"),
                    format!("card-slot-expand-{thread_group}"),
                    priority_rank as i64,
                    thread_group as i64,
                ],
            )
            .unwrap();
        }
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/activate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-slot-expand",
                        "active_only": true
                    }))
                    .unwrap(),
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
    assert_eq!(
        json["count"], 1,
        "activate must still dispatch only one group when all pending work targets the same agent"
    );
    assert_eq!(json["active_groups"], 1);

    let conn = db.lock().unwrap();
    let slot_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_slots WHERE agent_id = 'agent-slot-expand'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        slot_count, 4,
        "slot pool should expand from 3 seeded rows to match run max_concurrent_threads"
    );

    let mut stmt = conn
        .prepare(
            "SELECT slot_index
             FROM auto_queue_entries
             WHERE run_id = 'run-slot-expand'
             ORDER BY priority_rank ASC",
        )
        .unwrap();
    let mut assigned_slots = stmt
        .query_map([], |row| row.get::<_, Option<i64>>(0))
        .unwrap()
        .filter_map(|row| row.ok().flatten())
        .collect::<Vec<_>>();
    assigned_slots.sort_unstable();
    assert_eq!(assigned_slots, vec![0]);

    let fourth_slot_group: Option<i64> = conn
        .query_row(
            "SELECT assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = 'agent-slot-expand' AND slot_index = 3",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        fourth_slot_group, None,
        "newly created slot must stay unassigned until a later activate consumes it"
    );
}

// NOTE: auto_queue_activate_requested_card_not_blocked_by_own_status and
// auto_queue_activate_walks_backlog_card_to_dispatchable_state tests already
// defined above (from main branch merge). Duplicate definitions removed.

/// #107 regression: empty claude_session_id must be normalized to NULL at the API
/// boundary so that stale clear paths don't poison the DB with "".
#[tokio::test]
async fn hook_session_normalizes_empty_claude_session_id_to_null() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db.clone(), engine, None);

    // 1. Save a valid claude_session_id
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/hook/session")
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
    {
        let conn = db.lock().unwrap();
        let stored: Option<String> = conn
            .query_row(
                "SELECT claude_session_id FROM sessions WHERE session_key = 'test:sess1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored.as_deref(), Some("valid-id-123"));
    }

    // 2. Send empty string — should be normalized to NULL (not stored as "")
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/hook/session")
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
    {
        let conn = db.lock().unwrap();
        let stored: Option<String> = conn
            .query_row(
                "SELECT claude_session_id FROM sessions WHERE session_key = 'test:sess1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stored.as_deref(),
            Some("valid-id-123"),
            "Empty string should be normalized to NULL, and COALESCE keeps the old value"
        );
    }

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
    {
        let conn = db.lock().unwrap();
        let stored: Option<String> = conn
            .query_row(
                "SELECT claude_session_id FROM sessions WHERE session_key = 'test:sess1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            stored.is_none(),
            "After clear-session-id, value should be NULL"
        );
    }

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
}

// ── #140: Parallel thread group auto-queue tests ──────────────────

/// Helper: seed kanban cards for the parallel dispatch test scenario.
/// Creates 7 cards:
///   - 3 independent (issue #1, #2, #3)
///   - 4 in a dependency chain: #4 → #5 → #6 → #7
/// Returns card IDs in order [A, B, C, D, E, F, G].
fn seed_parallel_test_cards(db: &Db) -> Vec<String> {
    let conn = db.lock().unwrap();
    // Create separate agents so busy-agent guard doesn't block parallel dispatch
    for i in 1..=4 {
        conn.execute(
            &format!(
                "INSERT INTO agents (id, name, provider, status, discord_channel_id, discord_channel_alt)
                 VALUES ('agent-{i}', 'Agent{i}', 'claude', 'idle', '{}', '{}')",
                1000 + i,
                2000 + i,
            ),
            [],
        )
        .unwrap();
    }

    let mut card_ids = Vec::new();
    let labels = ["A", "B", "C", "D", "E", "F", "G"];
    let issue_nums = [1, 2, 3, 4, 5, 6, 7];
    // Each independent card gets its own agent; chain cards share agent-4
    let agents = [
        "agent-1", // A: independent
        "agent-2", // B: independent
        "agent-3", // C: independent
        "agent-4", // D: chain start
        "agent-4", // E: depends on D
        "agent-4", // F: depends on E
        "agent-4", // G: depends on E and F
    ];
    // Dependency metadata: cards E(#5), F(#6), G(#7) reference their predecessor
    let metadata = [
        None,          // A: independent
        None,          // B: independent
        None,          // C: independent
        None,          // D: chain start
        Some("#4"),    // E: depends on D
        Some("#5"),    // F: depends on E
        Some("#5 #6"), // G: depends on E and F (still same component)
    ];

    for i in 0..7 {
        let card_id = format!("card-{}", labels[i]);
        let meta_val = metadata[i]
            .map(|m| format!("'{}'", m))
            .unwrap_or("NULL".to_string());
        conn.execute(
            &format!(
                "INSERT INTO kanban_cards (id, repo_id, title, status, priority, assigned_agent_id, github_issue_number, metadata)
                 VALUES ('{}', 'test-repo', 'Task {}', 'ready', 'medium', '{}', {}, {})",
                card_id, labels[i], agents[i], issue_nums[i], meta_val
            ),
            [],
        )
        .unwrap();
        card_ids.push(card_id);
    }

    card_ids
}

fn seed_similarity_group_cards(db: &Db) -> Vec<String> {
    let conn = db.lock().unwrap();
    for i in 1..=3 {
        conn.execute(
            &format!(
                "INSERT OR IGNORE INTO agents (id, name, provider, status, discord_channel_id, discord_channel_alt)
                 VALUES ('sim-agent-{i}', 'SimAgent{i}', 'claude', 'idle', '{}', '{}')",
                3000 + i,
                4000 + i,
            ),
            [],
        )
        .unwrap();
    }

    let rows = [
        (
            "sim-card-auth-1",
            "sim-agent-1",
            101,
            "Auto-queue route generate update",
            "Touches src/server/routes/auto_queue.rs and dashboard/src/components/agent-manager/AutoQueuePanel.tsx",
        ),
        (
            "sim-card-auth-2",
            "sim-agent-1",
            102,
            "Auto-queue panel reason rendering",
            "Updates src/server/routes/auto_queue.rs plus dashboard/src/api/client.ts for generated reason text",
        ),
        (
            "sim-card-billing-1",
            "sim-agent-2",
            201,
            "Unified thread nested map cleanup",
            "Files: src/server/routes/dispatches/discord_delivery.rs and policies/auto-queue.js",
        ),
        (
            "sim-card-billing-2",
            "sim-agent-2",
            202,
            "Auto queue follow-up dispatch policy",
            "Relevant files: policies/auto-queue.js and src/server/routes/routes_tests.rs",
        ),
        (
            "sim-card-ops-1",
            "sim-agent-3",
            301,
            "Release health probe logs",
            "Only docs/operations/release-health.md changes are needed here",
        ),
    ];

    let mut ids = Vec::new();
    for (card_id, agent_id, issue_num, title, description) in rows {
        conn.execute(
            "INSERT INTO kanban_cards (
                id, repo_id, title, description, status, priority, assigned_agent_id, github_issue_number
             ) VALUES (?1, 'test-repo', ?2, ?3, 'ready', 'medium', ?4, ?5)",
            rusqlite::params![card_id, title, description, agent_id, issue_num],
        )
        .unwrap();
        ids.push(card_id.to_string());
    }

    ids
}

#[tokio::test]
async fn smart_generate_creates_correct_thread_groups_and_batch_phases() {
    let db = test_db();
    let engine = test_engine(&db);
    let _card_ids = seed_parallel_test_cards(&db);

    let app = test_api_router(db, engine, None);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "max_concurrent_threads": 3,
                        "max_concurrent_per_agent": 3,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let entries = json["entries"].as_array().expect("entries must be array");
    assert_eq!(entries.len(), 7, "all 7 cards should be queued");

    // Verify run keeps the requested concurrency cap while ignoring the
    // legacy max_concurrent_per_agent input.
    let run = &json["run"];
    assert_eq!(run["max_concurrent_threads"], 3);
    assert!(run.get("max_concurrent_per_agent").is_none());
    assert_eq!(run["ai_model"], "smart-planner");

    // Collect thread_group assignments per issue number.
    let mut groups: std::collections::HashMap<i64, Vec<(i64, i64, i64)>> =
        std::collections::HashMap::new();
    for entry in entries {
        let issue_num = entry["github_issue_number"].as_i64().unwrap();
        let thread_group = entry["thread_group"].as_i64().unwrap();
        let priority_rank = entry["priority_rank"].as_i64().unwrap();
        let batch_phase = entry["batch_phase"].as_i64().unwrap();
        groups
            .entry(thread_group)
            .or_default()
            .push((issue_num, priority_rank, batch_phase));
    }

    let group_count = run["thread_group_count"].as_i64().unwrap();
    assert_eq!(
        group_count,
        groups.len() as i64,
        "thread_group_count must match actual distinct groups"
    );

    // Independent cards (issues 1, 2, 3) should each be in their own group (size 1)
    let mut independent_groups = 0;
    let mut chain_group = None;
    for (group_num, members) in &groups {
        if members.len() == 1 {
            let issue = members[0].0;
            assert!(
                [1, 2, 3].contains(&issue),
                "single-member group should be an independent card, got issue #{issue}"
            );
            assert_eq!(
                members[0].2, 0,
                "independent cards should start in batch phase 0"
            );
            independent_groups += 1;
        } else {
            // This must be the dependency chain group
            assert!(
                chain_group.is_none(),
                "only one multi-member group expected"
            );
            chain_group = Some(*group_num);
        }
    }
    assert_eq!(independent_groups, 3, "3 independent cards → 3 groups");

    // Verify the chain group: issues 4,5,6,7 in topological order
    let chain = chain_group.expect("dependency chain group must exist");
    let mut chain_members = groups[&chain].clone();
    chain_members.sort_by_key(|(_, rank, _)| *rank);
    let chain_issues: Vec<i64> = chain_members.iter().map(|(num, _, _)| *num).collect();
    let chain_phases: Vec<i64> = chain_members.iter().map(|(_, _, phase)| *phase).collect();
    // Issue #4 must come first (rank 0), #5 second, then #6 and #7 (order between 6,7 may vary
    // since #7 depends on both #5 and #6, making #6 and #7 at different levels)
    assert_eq!(chain_issues[0], 4, "chain start (#4) must have lowest rank");
    assert_eq!(chain_issues[1], 5, "#5 depends on #4, must be second");
    // #6 depends on #5, #7 depends on #5 and #6 — so #6 before #7
    assert_eq!(chain_issues[2], 6, "#6 depends on #5, must be third");
    assert_eq!(chain_issues[3], 7, "#7 depends on #5 and #6, must be last");
    assert_eq!(
        chain_phases,
        vec![0, 1, 2, 3],
        "dependency chain should advance one batch phase at a time"
    );
}

#[tokio::test]
async fn auto_queue_status_exposes_explicit_thread_links_from_configured_channels() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_repo(&db, "test-repo");
    seed_agent(&db, "agent-thread-links");
    seed_auto_queue_card(
        &db,
        "card-thread-links",
        4131,
        "review",
        "agent-thread-links",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "UPDATE kanban_cards
             SET channel_thread_map = ?1
             WHERE id = 'card-thread-links'",
            [json!({
                "111": "222000000000000001",
                "222": "222000000000000002"
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, created_at
            ) VALUES (
                'run-thread-links', 'test-repo', 'agent-thread-links', 'active', datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank
            ) VALUES (
                'entry-thread-links', 'run-thread-links', 'card-thread-links',
                'agent-thread-links', 'dispatched', 0
            )",
            [],
        )
        .unwrap();
    }

    let mut config = crate::config::Config::default();
    config.discord.guild_id = Some("guild-123".to_string());
    let app = test_api_router_with_config(db.clone(), engine, config, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/auto-queue/status?agent_id=agent-thread-links")
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
    let thread_links = json["entries"][0]["thread_links"]
        .as_array()
        .expect("thread_links must be an array");

    assert_eq!(thread_links.len(), 2);
    assert_eq!(thread_links[0]["label"], "work");
    assert_eq!(thread_links[0]["channel_id"], "111");
    assert_eq!(thread_links[0]["thread_id"], "222000000000000001");
    assert_eq!(
        thread_links[0]["url"],
        "https://discord.com/channels/guild-123/222000000000000001"
    );
    assert_eq!(thread_links[1]["label"], "review");
    assert_eq!(thread_links[1]["channel_id"], "222");
    assert_eq!(thread_links[1]["thread_id"], "222000000000000002");
    assert_eq!(
        thread_links[1]["url"],
        "https://discord.com/channels/guild-123/222000000000000002"
    );
}

#[tokio::test]
async fn auto_queue_status_legacy_thread_falls_back_to_active_label_without_url_guessing() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_repo(&db, "test-repo");
    seed_agent(&db, "agent-thread-links-legacy");
    seed_auto_queue_card(
        &db,
        "card-thread-links-legacy",
        4132,
        "in_progress",
        "agent-thread-links-legacy",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "UPDATE kanban_cards
             SET active_thread_id = '333000000000000009'
             WHERE id = 'card-thread-links-legacy'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, created_at
            ) VALUES (
                'run-thread-links-legacy', 'test-repo', 'agent-thread-links-legacy',
                'active', datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank
            ) VALUES (
                'entry-thread-links-legacy', 'run-thread-links-legacy',
                'card-thread-links-legacy', 'agent-thread-links-legacy', 'pending', 0
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/auto-queue/status?agent_id=agent-thread-links-legacy")
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
    let thread_links = json["entries"][0]["thread_links"]
        .as_array()
        .expect("thread_links must be an array");

    assert_eq!(thread_links.len(), 1);
    assert_eq!(thread_links[0]["label"], "active");
    assert_eq!(thread_links[0]["role"], "active");
    assert_eq!(thread_links[0]["thread_id"], "333000000000000009");
    assert_eq!(thread_links[0]["url"], serde_json::Value::Null);
}

#[tokio::test]
async fn auto_queue_status_scopes_global_run_entries_by_repo_and_agent() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_repo(&db, "test-repo");
    seed_repo(&db, "other-repo");
    seed_agent(&db, "agent-scope-a");
    seed_agent(&db, "agent-scope-b");
    seed_auto_queue_card(&db, "card-scope-a", 4201, "ready", "agent-scope-a");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, created_at, updated_at
             ) VALUES (
                'card-scope-b', 'Issue #4202', 'ready', 'medium', 'agent-scope-b', 'other-repo',
                4202, datetime('now'), datetime('now')
             )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, status, created_at)
             VALUES ('run-scope-global', 'active', datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, reason
             ) VALUES (
                'entry-scope-a', 'run-scope-global', 'card-scope-a', 'agent-scope-a',
                'pending', 0, 0, 'scope group a'
             )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, reason
             ) VALUES (
                'entry-scope-b', 'run-scope-global', 'card-scope-b', 'agent-scope-b',
                'dispatched', 1, 1, 'scope group b'
             )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/auto-queue/status?repo=test-repo&agent_id=agent-scope-a")
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

    assert_eq!(json["run"]["id"], "run-scope-global");
    let entries = json["entries"]
        .as_array()
        .expect("entries must be an array");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["id"], "entry-scope-a");

    let agents = json["agents"]
        .as_object()
        .expect("agents must be an object");
    assert_eq!(agents.len(), 1);
    assert_eq!(agents["agent-scope-a"]["pending"], 1);
    assert!(agents.get("agent-scope-b").is_none());

    let thread_groups = json["thread_groups"]
        .as_object()
        .expect("thread_groups must be an object");
    assert_eq!(thread_groups.len(), 1);
    assert_eq!(thread_groups["0"]["pending"], 1);
    assert_eq!(thread_groups["0"]["status"], "pending");
    assert_eq!(thread_groups["0"]["reason"], "scope group a");
}

#[tokio::test]
async fn auto_queue_generate_issue_numbers_filters_cards_and_promotes_backlog() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-generate-327");
    seed_repo(&db, "test-repo");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id, github_issue_number, created_at, updated_at
            ) VALUES (
                'card-gen-327-ready', 'Generate Ready #327', 'ready', 'high', 'agent-generate-327', 'test-repo', 3271, datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id, github_issue_number, created_at, updated_at
            ) VALUES (
                'card-gen-327-backlog', 'Generate Backlog #327', 'backlog', 'medium', 'agent-generate-327', 'test-repo', 3272, datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id, github_issue_number, created_at, updated_at
            ) VALUES (
                'card-gen-327-extra', 'Generate Extra', 'ready', 'urgent', 'agent-generate-327', 'test-repo', 3999, datetime('now'), datetime('now')
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
                .uri("/auto-queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "agent-generate-327",
                        "issue_numbers": [3271, 3272],
                    }))
                    .unwrap(),
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
    let entries = json["entries"].as_array().unwrap();
    let queued_issues: Vec<i64> = entries
        .iter()
        .filter_map(|entry| entry["github_issue_number"].as_i64())
        .collect();
    assert_eq!(entries.len(), 2);
    assert!(queued_issues.contains(&3271));
    assert!(queued_issues.contains(&3272));
    assert!(!queued_issues.contains(&3999));

    let conn = db.lock().unwrap();
    let backlog_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-gen-327-backlog'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        backlog_status, "ready",
        "selected backlog card must be promoted before queue generation"
    );
}

#[tokio::test]
async fn auto_queue_generate_empty_state_reports_filtered_counts() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-generate-counts");
    seed_agent(&db, "other-agent");
    seed_repo(&db, "test-repo");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id, github_issue_number,
                created_at, updated_at
            ) VALUES (
                'card-generate-counts-backlog', 'Generate Counts Backlog', 'backlog', 'medium',
                'agent-generate-counts', 'test-repo', 5410, datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id, github_issue_number,
                created_at, updated_at
            ) VALUES (
                'card-generate-counts-other-agent', 'Other Agent Ready', 'ready', 'high',
                'other-agent', 'test-repo', 5411, datetime('now'), datetime('now')
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
                .uri("/auto-queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "agent-generate-counts",
                    }))
                    .unwrap(),
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
    assert_eq!(json["message"], "No dispatchable cards found");
    assert_eq!(json["counts"]["backlog"], 1);
    assert_eq!(json["counts"]["ready"], 0);
}

#[tokio::test]
async fn auto_queue_generate_accepts_but_ignores_unified_thread_flag() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-generate-unified");
    seed_repo(&db, "test-repo");
    seed_auto_queue_card(
        &db,
        "card-generate-unified",
        3881,
        "ready",
        "agent-generate-unified",
    );

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "agent-generate-unified",
                        "unified_thread": true,
                    }))
                    .unwrap(),
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
    assert_eq!(json["run"]["unified_thread"], serde_json::json!(false));

    let run_id = json["run"]["id"]
        .as_str()
        .expect("generated run id must be present");
    let conn = db.lock().unwrap();
    let stored_unified_thread: i64 = conn
        .query_row(
            "SELECT unified_thread FROM auto_queue_runs WHERE id = ?1",
            [run_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        stored_unified_thread, 0,
        "generate must ignore unified_thread and keep slot pooling enabled"
    );
}

#[tokio::test]
async fn auto_queue_generate_entries_payload_persists_batch_phases() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-generate-phase");
    seed_repo(&db, "test-repo");
    seed_auto_queue_card(
        &db,
        "card-generate-phase-1",
        4231,
        "ready",
        "agent-generate-phase",
    );
    seed_auto_queue_card(
        &db,
        "card-generate-phase-2",
        4232,
        "ready",
        "agent-generate-phase",
    );

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "agent-generate-phase",
                        "entries": [
                            { "issue_number": 4232, "batch_phase": 2 },
                            { "issue_number": 4231, "batch_phase": 1 }
                        ],
                    }))
                    .unwrap(),
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
    let entries = json["entries"].as_array().expect("entries must be array");

    let phases_by_issue: std::collections::HashMap<i64, i64> = entries
        .iter()
        .filter_map(|entry| {
            Some((
                entry["github_issue_number"].as_i64()?,
                entry["batch_phase"].as_i64()?,
            ))
        })
        .collect();

    assert_eq!(phases_by_issue.get(&4231), Some(&1));
    assert_eq!(phases_by_issue.get(&4232), Some(&2));

    let conn = db.lock().unwrap();
    let stored_phases: std::collections::HashMap<i64, i64> = {
        let mut stmt = conn
            .prepare(
                "SELECT kc.github_issue_number, COALESCE(e.batch_phase, 0)
                 FROM auto_queue_entries e
                 JOIN kanban_cards kc ON kc.id = e.kanban_card_id
                 ORDER BY kc.github_issue_number ASC",
            )
            .unwrap();
        stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
            .unwrap()
            .filter_map(|row| row.ok())
            .collect()
    };
    assert_eq!(stored_phases.get(&4231), Some(&1));
    assert_eq!(stored_phases.get(&4232), Some(&2));
}

#[tokio::test]
async fn generate_smart_planner_groups_by_file_paths_and_recommends_threads() {
    let db = test_db();
    let engine = test_engine(&db);
    let _card_ids = seed_similarity_group_cards(&db);

    let app = test_api_router(db, engine, None);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let run = &json["run"];
    let entries = json["entries"].as_array().expect("entries must be array");
    assert_eq!(
        entries.len(),
        5,
        "all similarity test cards should be queued"
    );
    assert_eq!(
        run["thread_group_count"].as_i64().unwrap(),
        3,
        "two similarity pairs plus one independent task should yield three groups"
    );
    assert_eq!(
        run["max_concurrent_threads"].as_i64().unwrap(),
        3,
        "recommended concurrency should match the number of distinct runnable groups"
    );
    assert_eq!(run["ai_model"].as_str().unwrap(), "smart-planner");

    let similarity_reason_count = entries
        .iter()
        .filter(|entry| {
            entry["reason"]
                .as_str()
                .map(|reason| reason.contains("유사도 그룹"))
                .unwrap_or(false)
        })
        .count();
    assert!(
        similarity_reason_count >= 4,
        "two similarity groups should stamp group reasons on their entries"
    );

    let status_resp = app
        .oneshot(
            Request::builder()
                .uri("/auto-queue/status?repo=test-repo")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(status_resp.status(), StatusCode::OK);
    let status_body = axum::body::to_bytes(status_resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let status_json: serde_json::Value = serde_json::from_slice(&status_body).unwrap();
    let thread_groups = status_json["thread_groups"]
        .as_object()
        .expect("thread_groups must be present");
    assert!(
        thread_groups.values().any(|group| {
            group["reason"]
                .as_str()
                .map(|reason| reason.contains("유사도 그룹"))
                .unwrap_or(false)
        }),
        "status should expose group-level reasons for similarity-based lanes"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generate_smart_planner_without_file_paths_uses_dependency_only_groups() {
    let db = test_db();
    let engine = test_engine(&db);
    let _card_ids = seed_parallel_test_cards(&db);

    let app = test_api_router(db, engine, None);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let run = &json["run"];
    let entries = json["entries"].as_array().expect("entries must be array");
    assert_eq!(
        entries.len(),
        7,
        "all dependency-seed cards should be queued"
    );
    assert_eq!(
        run["thread_group_count"].as_i64().unwrap(),
        4,
        "without file paths, smart planner must fall back to dependency-only grouping"
    );
    assert_eq!(run["ai_model"].as_str().unwrap(), "smart-planner");
    assert!(
        run["ai_rationale"]
            .as_str()
            .map(|text| text.contains("파일 경로 신호 없이"))
            .unwrap_or(false),
        "rationale should explain the dependency-only fallback"
    );
    assert!(
        entries.iter().all(|entry| {
            entry["reason"]
                .as_str()
                .map(|reason| !reason.contains("유사도 그룹"))
                .unwrap_or(true)
        }),
        "fallback path should not stamp similarity reasons"
    );
}

#[tokio::test]
async fn generate_ignores_legacy_mode_and_still_uses_smart_planner() {
    let db = test_db();
    let engine = test_engine(&db);
    let _card_ids = seed_similarity_group_cards(&db);

    let app = test_api_router(db, engine, None);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "mode": "pm-assisted",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let run = &json["run"];
    let entries = json["entries"].as_array().expect("entries must be array");
    assert_eq!(run["thread_group_count"], 3);
    assert_eq!(run["max_concurrent_threads"], 3);
    assert_eq!(run["ai_model"], "smart-planner");
    assert!(
        !entries.is_empty(),
        "legacy mode input should be ignored rather than triggering PM-assisted flow"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn smart_activate_dispatches_multiple_groups() {
    crate::pipeline::ensure_loaded();

    let (repo, _repo_guard) = setup_test_repo();
    let config_dir = write_repo_mapping_config(&[("test-repo", repo.path())]);
    let _config_guard = EnvVarGuard::set_path(
        "AGENTDESK_CONFIG",
        &config_dir.path().join("agentdesk.yaml"),
    );

    let db = test_db();
    let engine = test_engine(&db);
    let _card_ids = seed_parallel_test_cards(&db);

    let app = test_api_router(db.clone(), engine.clone(), None);

    // Step 1: Generate with the smart planner (no agent_id filter — cards have mixed agents)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "max_concurrent_threads": 3,
                        "max_concurrent_per_agent": 3,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Step 2: Activate without agent_id — allows dispatching across different agents
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/activate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "unified_thread": false,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let activate_json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    // Should dispatch 3 entries (one per group, up to max_concurrent_threads=3)
    let dispatched_count = activate_json["count"].as_i64().unwrap();
    assert_eq!(
        dispatched_count, 3,
        "activate should dispatch 3 groups (max_concurrent_threads=3)"
    );
    assert_eq!(activate_json["active_groups"], 3);

    // Step 3: Verify status API shows group-level info
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/auto-queue/status?repo=test-repo")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let status_json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    // thread_groups should be present with group-level statuses
    let thread_groups = status_json["thread_groups"]
        .as_object()
        .expect("thread_groups must be an object");
    assert!(
        thread_groups.len() >= 2,
        "status should have multiple thread groups"
    );

    // At least some groups should be "active" (dispatched) and some "pending"
    let active_count = thread_groups
        .values()
        .filter(|g| g["status"] == "active")
        .count();
    let pending_count = thread_groups
        .values()
        .filter(|g| g["status"] == "pending")
        .count();
    assert!(active_count > 0, "should have active groups");
    assert!(
        pending_count > 0,
        "should have pending groups (4th group not yet started)"
    );
}

#[tokio::test]
async fn generate_ignores_legacy_parallel_toggle_and_keeps_smart_groups() {
    let db = test_db();
    let engine = test_engine(&db);
    let _card_ids = seed_parallel_test_cards(&db);

    let app = test_api_router(db, engine, None);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "parallel": false,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let entries = json["entries"].as_array().unwrap();
    let run = &json["run"];

    let distinct_groups = entries
        .iter()
        .map(|entry| entry["thread_group"].as_i64().unwrap())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(run["thread_group_count"], 4);
    assert_eq!(run["max_concurrent_threads"], 4);
    assert_eq!(run["ai_model"], "smart-planner");
    assert_eq!(
        distinct_groups.len(),
        4,
        "legacy parallel=false should be ignored in favor of smart grouping"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activate_waits_for_current_batch_phase_before_dispatching_next_phase() {
    crate::pipeline::ensure_loaded();

    let (repo, _repo_guard) = setup_test_repo();
    let config_dir = write_repo_mapping_config(&[("test-repo", repo.path())]);
    let _config_guard = EnvVarGuard::set_path(
        "AGENTDESK_CONFIG",
        &config_dir.path().join("agentdesk.yaml"),
    );

    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_repo(&db, "test-repo");
    seed_agent(&db, "agent-phase-a");
    seed_agent(&db, "agent-phase-b");
    seed_auto_queue_card(&db, "card-phase-1-a", 4241, "ready", "agent-phase-a");
    seed_auto_queue_card(&db, "card-phase-1-b", 4242, "ready", "agent-phase-b");
    seed_auto_queue_card(&db, "card-phase-2-a", 4243, "ready", "agent-phase-a");
    seed_auto_queue_card(&db, "card-phase-2-b", 4244, "ready", "agent-phase-b");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-batch-phase', 'test-repo', 'active', 2, 2
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, batch_phase
            ) VALUES (
                'entry-phase-1-a', 'run-batch-phase', 'card-phase-1-a', 'agent-phase-a', 'pending', 0, 0, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, batch_phase
            ) VALUES (
                'entry-phase-1-b', 'run-batch-phase', 'card-phase-1-b', 'agent-phase-b', 'pending', 1, 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, batch_phase
            ) VALUES (
                'entry-phase-2-a', 'run-batch-phase', 'card-phase-2-a', 'agent-phase-a', 'pending', 2, 0, 2
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, batch_phase
            ) VALUES (
                'entry-phase-2-b', 'run-batch-phase', 'card-phase-2-b', 'agent-phase-b', 'pending', 3, 1, 2
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);

    let first_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/activate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-batch-phase",
                        "active_only": true,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first_response.status(), StatusCode::OK);
    let first_body = axum::body::to_bytes(first_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let first_json: serde_json::Value = serde_json::from_slice(&first_body).unwrap();
    assert_eq!(first_json["count"], 2);

    {
        let conn = db.lock().unwrap();
        let dispatched_phases: std::collections::HashMap<String, i64> = {
            let mut stmt = conn
                .prepare(
                    "SELECT id, COALESCE(batch_phase, 0)
                     FROM auto_queue_entries
                     WHERE status = 'dispatched'
                     ORDER BY id ASC",
                )
                .unwrap();
            stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .unwrap()
            .filter_map(|row| row.ok())
            .collect()
        };
        assert_eq!(dispatched_phases.len(), 2);
        assert_eq!(dispatched_phases.get("entry-phase-1-a"), Some(&1));
        assert_eq!(dispatched_phases.get("entry-phase-1-b"), Some(&1));
    }

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "UPDATE auto_queue_entries
             SET status = 'done', dispatch_id = NULL, completed_at = datetime('now')
             WHERE id = 'entry-phase-1-a'",
            [],
        )
        .unwrap();
    }

    let second_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/activate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-batch-phase",
                        "active_only": true,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second_response.status(), StatusCode::OK);
    let second_body = axum::body::to_bytes(second_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let second_json: serde_json::Value = serde_json::from_slice(&second_body).unwrap();
    assert_eq!(
        second_json["count"], 0,
        "phase 2 must stay blocked while phase 1 still has an in-flight entry"
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "UPDATE auto_queue_entries
             SET status = 'done', dispatch_id = NULL, completed_at = datetime('now')
             WHERE id = 'entry-phase-1-b'",
            [],
        )
        .unwrap();
    }

    let third_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/activate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-batch-phase",
                        "active_only": true,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(third_response.status(), StatusCode::OK);
    let third_body = axum::body::to_bytes(third_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let third_json: serde_json::Value = serde_json::from_slice(&third_body).unwrap();
    assert_eq!(
        third_json["count"], 2,
        "next batch phase should become dispatchable once phase 1 is complete"
    );

    let conn = db.lock().unwrap();
    let phase_two_dispatched: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_entries
             WHERE status = 'dispatched' AND COALESCE(batch_phase, 0) = 2",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(phase_two_dispatched, 2);
}

#[tokio::test]
async fn auto_queue_pause_cancels_live_dispatches_and_releases_slots() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_agent(&db, "agent-pause-slot");
    seed_auto_queue_card(
        &db,
        "card-pause-dispatched",
        4496,
        "in_progress",
        "agent-pause-slot",
    );
    seed_auto_queue_card(&db, "card-pause-pending", 4497, "ready", "agent-pause-slot");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-pause-slot', 'test-repo', 'agent-pause-slot', 'active', 1, 2
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (
                agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
            ) VALUES (
                'agent-pause-slot', 0, 'run-pause-slot', 0, ?1
            )",
            [json!({"111": "222000000000004496"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-pause-slot', 'card-pause-dispatched', 'agent-pause-slot',
                'implementation', 'dispatched', 'Pause slot dispatch', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, priority_rank, thread_group, dispatched_at
            ) VALUES (
                'entry-pause-dispatched', 'run-pause-slot', 'card-pause-dispatched',
                'agent-pause-slot', 'dispatched', 'dispatch-pause-slot', 0, 0, 0, datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-pause-pending', 'run-pause-slot', 'card-pause-pending',
                'agent-pause-slot', 'pending', 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, agent_id, provider, status, session_info, tokens,
                active_dispatch_id, thread_channel_id, claude_session_id,
                last_heartbeat, created_at
            ) VALUES (
                'host:AgentDesk-claude-pause-slot', 'agent-pause-slot', 'claude', 'working',
                'pause slot seed', 19, 'dispatch-pause-slot', '222000000000004496', 'claude-pause-slot',
                datetime('now'), datetime('now')
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
                .uri("/auto-queue/pause")
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
    assert_eq!(json["paused_runs"], 1);
    assert_eq!(json["cancelled_dispatches"], 1);
    assert_eq!(json["released_slots"], 1);

    let conn = db.lock().unwrap();
    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-pause-slot'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(run_status, "paused");

    let dispatched_entry: (String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT status, dispatch_id, dispatched_at
             FROM auto_queue_entries
             WHERE id = 'entry-pause-dispatched'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(dispatched_entry.0, "pending");
    assert_eq!(dispatched_entry.1, None);
    assert_eq!(dispatched_entry.2, None);

    let pending_entry: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-pause-pending'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(pending_entry, "pending");

    let dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-pause-slot'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dispatch_status, "cancelled");

    let slot: (Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT assigned_run_id, assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = 'agent-pause-slot' AND slot_index = 0",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(slot, (None, None));

    let session: (String, Option<String>, i64, Option<String>) = conn
        .query_row(
            "SELECT status, active_dispatch_id, tokens, claude_session_id
             FROM sessions
             WHERE session_key = 'host:AgentDesk-claude-pause-slot'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(session.0, "idle");
    assert_eq!(session.1, None);
    assert_eq!(session.2, 0);
    assert_eq!(session.3, None);
}

#[tokio::test]
async fn auto_queue_cancel_cancels_live_dispatches_skips_entries_and_releases_slots() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_agent(&db, "agent-cancel-slot");
    seed_auto_queue_card(
        &db,
        "card-cancel-dispatched",
        4596,
        "in_progress",
        "agent-cancel-slot",
    );
    seed_auto_queue_card(
        &db,
        "card-cancel-pending",
        4597,
        "ready",
        "agent-cancel-slot",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-cancel-slot', 'test-repo', 'agent-cancel-slot', 'paused', 1, 2
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (
                agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
            ) VALUES (
                'agent-cancel-slot', 0, 'run-cancel-slot', 0, ?1
            )",
            [json!({"111": "222000000000004597"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-cancel-slot', 'card-cancel-dispatched', 'agent-cancel-slot',
                'implementation', 'dispatched', 'Cancel slot dispatch', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, priority_rank, thread_group, dispatched_at
            ) VALUES (
                'entry-cancel-dispatched', 'run-cancel-slot', 'card-cancel-dispatched',
                'agent-cancel-slot', 'dispatched', 'dispatch-cancel-slot', 0, 0, 0, datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-cancel-pending', 'run-cancel-slot', 'card-cancel-pending',
                'agent-cancel-slot', 'pending', 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, agent_id, provider, status, session_info, tokens,
                active_dispatch_id, thread_channel_id, claude_session_id,
                last_heartbeat, created_at
            ) VALUES (
                'host:AgentDesk-claude-cancel-slot', 'agent-cancel-slot', 'claude', 'working',
                'cancel slot seed', 23, 'dispatch-cancel-slot', '222000000000004597', 'claude-cancel-slot',
                datetime('now'), datetime('now')
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
                .uri("/auto-queue/cancel")
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
    assert_eq!(json["cancelled_runs"], 1);
    assert_eq!(json["cancelled_entries"], 2);
    assert_eq!(json["cancelled_dispatches"], 1);
    assert_eq!(json["released_slots"], 1);

    let conn = db.lock().unwrap();
    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-cancel-slot'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(run_status, "cancelled");

    let entries: Vec<(String, Option<String>, Option<String>)> = {
        let mut stmt = conn
            .prepare(
                "SELECT status, dispatch_id, completed_at
                 FROM auto_queue_entries
                 WHERE run_id = 'run-cancel-slot'
                 ORDER BY id ASC",
            )
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|(status, dispatch_id, completed_at)| status == "skipped"
                && dispatch_id.is_none()
                && completed_at.is_some()),
        "cancel must skip every active/pending queue entry and stamp completed_at"
    );

    let dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-cancel-slot'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dispatch_status, "cancelled");

    let slot: (Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT assigned_run_id, assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = 'agent-cancel-slot' AND slot_index = 0",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(slot, (None, None));

    let session: (String, Option<String>, i64, Option<String>) = conn
        .query_row(
            "SELECT status, active_dispatch_id, tokens, claude_session_id
             FROM sessions
             WHERE session_key = 'host:AgentDesk-claude-cancel-slot'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(session.0, "idle");
    assert_eq!(session.1, None);
    assert_eq!(session.2, 0);
    assert_eq!(session.3, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activate_run_id_blocks_phase_gate_paused_runs() {
    crate::pipeline::ensure_loaded();

    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_repo(&db, "test-repo");
    seed_agent(&db, "agent-phase-gate");
    seed_auto_queue_card(
        &db,
        "card-phase-gate-paused",
        4381,
        "ready",
        "agent-phase-gate",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-phase-gate-paused', 'test-repo', 'agent-phase-gate', 'paused')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, batch_phase
            ) VALUES (
                'entry-phase-gate-paused', 'run-phase-gate-paused', 'card-phase-gate-paused',
                'agent-phase-gate', 'pending', 0, 2
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_phase_gates (
                run_id, phase, status, dispatch_id, pass_verdict
             ) VALUES (?1, ?2, ?3, NULL, 'phase_gate_passed')",
            rusqlite::params!["run-phase-gate-paused", 1, "pending",],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/activate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-phase-gate-paused",
                        "active_only": true,
                    }))
                    .unwrap(),
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
    assert_eq!(json["count"], 0);
    assert_eq!(json["message"], "Run is waiting on phase gate");

    let conn = db.lock().unwrap();
    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-phase-gate-paused'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(run_status, "paused");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_run_skips_phase_gate_blocked_runs() {
    crate::pipeline::ensure_loaded();

    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_repo(&db, "test-repo");
    seed_agent(&db, "agent-resume-gate");
    seed_agent(&db, "agent-resume-free");
    seed_auto_queue_card(&db, "card-resume-gate", 4382, "ready", "agent-resume-gate");
    seed_auto_queue_card(&db, "card-resume-free", 4383, "ready", "agent-resume-free");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-resume-gate', 'test-repo', 'agent-resume-gate', 'paused')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-resume-free', 'test-repo', 'agent-resume-free', 'paused')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, batch_phase
            ) VALUES (
                'entry-resume-gate', 'run-resume-gate', 'card-resume-gate',
                'agent-resume-gate', 'pending', 0, 2
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank
            ) VALUES (
                'entry-resume-free', 'run-resume-free', 'card-resume-free',
                'agent-resume-free', 'pending', 0
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_phase_gates (
                run_id, phase, status, dispatch_id, pass_verdict
             ) VALUES (?1, ?2, ?3, NULL, 'phase_gate_passed')",
            rusqlite::params!["run-resume-gate", 1, "failed",],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/resume")
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
    assert_eq!(json["resumed_runs"], 1);
    assert_eq!(json["blocked_runs"], 1);

    let conn = db.lock().unwrap();
    let blocked_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-resume-gate'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let resumed_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-resume-free'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(blocked_status, "paused");
    assert_eq!(resumed_status, "active");
}

/// Regression test for #191: onTick1min recovery must reset stuck auto-queue
/// entries that are 'dispatched' but have orphan (NULL), phantom (missing row),
/// or cancelled/failed dispatch_ids — while leaving valid dispatches untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_queue_activate_ignores_legacy_max_concurrent_per_agent() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();

    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_agent(&db, "agent-393");
    seed_auto_queue_card(&db, "card-393-1", 3931, "ready", "agent-393");
    seed_auto_queue_card(&db, "card-393-2", 3932, "ready", "agent-393");

    let app = test_api_router(db.clone(), engine.clone(), None);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "agent-393",
                        "parallel": true,
                        "max_concurrent_threads": 2,
                        "max_concurrent_per_agent": 1,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let generated_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let run = &generated_json["run"];
    assert_eq!(run["max_concurrent_threads"], 2);
    assert!(run.get("max_concurrent_per_agent").is_none());

    {
        let conn = db.lock().unwrap();
        let run_id = run["id"].as_str().unwrap();
        conn.execute(
            "UPDATE auto_queue_entries
             SET thread_group = CASE id
                 WHEN ?1 THEN 0
                 WHEN ?2 THEN 1
                 ELSE thread_group
             END
             WHERE run_id = ?3",
            rusqlite::params![
                generated_json["entries"][0]["id"].as_str().unwrap(),
                generated_json["entries"][1]["id"].as_str().unwrap(),
                run_id
            ],
        )
        .unwrap();
        conn.execute(
            "UPDATE auto_queue_runs SET thread_group_count = 2 WHERE id = ?1",
            [run_id],
        )
        .unwrap();
    }

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/activate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "agent-393",
                        "unified_thread": false,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let activate_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(activate_json["count"], 1);
    assert_eq!(activate_json["active_groups"], 1);
}

#[test]
fn auto_queue_recovery_resets_orphan_phantom_and_cancelled_entries() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);

    seed_agent(&db, "agent-recovery");
    seed_auto_queue_card(&db, "card-orphan", 9001, "in_progress", "agent-recovery");
    seed_auto_queue_card(&db, "card-phantom", 9002, "in_progress", "agent-recovery");
    seed_auto_queue_card(&db, "card-cancelled", 9003, "in_progress", "agent-recovery");
    seed_auto_queue_card(&db, "card-valid", 9004, "in_progress", "agent-recovery");

    {
        let conn = db.lock().unwrap();

        // Active run
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
             VALUES ('run-recovery', 'test-repo', 'agent-recovery', 'active')",
            [],
        )
        .unwrap();

        // Entry A: dispatched + dispatch_id=NULL (orphan — should be reset)
        // #214: dispatched_at must be >2min ago to pass grace period
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at) \
             VALUES ('entry-orphan', 'run-recovery', 'card-orphan', 'agent-recovery', 'dispatched', NULL, datetime('now', '-3 minutes'))",
            [],
        )
        .unwrap();

        // Entry B: dispatched + phantom dispatch_id (not in task_dispatches — should be reset)
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at) \
             VALUES ('entry-phantom', 'run-recovery', 'card-phantom', 'agent-recovery', 'dispatched', 'phantom-id-999', datetime('now', '-3 minutes'))",
            [],
        )
        .unwrap();

        // Entry C: dispatched + cancelled dispatch (should be reset)
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title) \
             VALUES ('dispatch-cancelled', 'card-cancelled', 'agent-recovery', 'implementation', 'cancelled', 'test')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at) \
             VALUES ('entry-cancelled', 'run-recovery', 'card-cancelled', 'agent-recovery', 'dispatched', 'dispatch-cancelled', datetime('now', '-3 minutes'))",
            [],
        )
        .unwrap();

        // Entry D: dispatched + valid active dispatch (must NOT be reset)
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title) \
             VALUES ('dispatch-valid', 'card-valid', 'agent-recovery', 'implementation', 'dispatched', 'test')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at) \
             VALUES ('entry-valid', 'run-recovery', 'card-valid', 'agent-recovery', 'dispatched', 'dispatch-valid', datetime('now'))",
            [],
        )
        .unwrap();
    }

    // Fire onTick1min — triggers recovery path 2
    engine
        .fire_hook(
            crate::engine::hooks::Hook::OnTick1min,
            serde_json::json!({}),
        )
        .unwrap();

    // Verify
    let conn = db.lock().unwrap();

    // A: orphan (NULL dispatch_id) → reset to pending
    let (status_a, did_a): (String, Option<String>) = conn
        .query_row(
            "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-orphan'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(status_a, "pending", "orphan entry must be reset to pending");
    assert!(did_a.is_none(), "orphan entry dispatch_id must stay NULL");

    // B: phantom dispatch_id → reset to pending
    let (status_b, did_b): (String, Option<String>) = conn
        .query_row(
            "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-phantom'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        status_b, "pending",
        "phantom dispatch entry must be reset to pending"
    );
    assert!(
        did_b.is_none(),
        "phantom entry dispatch_id must be cleared to NULL"
    );

    // C: cancelled dispatch → reset to pending
    let (status_c, did_c): (String, Option<String>) = conn
        .query_row(
            "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-cancelled'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        status_c, "pending",
        "cancelled dispatch entry must be reset to pending"
    );
    assert!(
        did_c.is_none(),
        "cancelled entry dispatch_id must be cleared to NULL"
    );

    // D: valid active dispatch → must remain dispatched
    let (status_d, did_d): (String, Option<String>) = conn
        .query_row(
            "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-valid'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        status_d, "dispatched",
        "valid dispatch entry must NOT be reset"
    );
    assert_eq!(
        did_d.as_deref(),
        Some("dispatch-valid"),
        "valid entry dispatch_id must be preserved"
    );
}

/// Regression test for #295: onTick1min must backstop terminal cards that still
/// have pending auto-queue entries in active/paused runs.
#[test]
fn auto_queue_recovery_skips_terminal_pending_entries() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);

    seed_agent(&db, "agent-terminal-recovery");
    seed_auto_queue_card(
        &db,
        "card-terminal-active",
        9011,
        "done",
        "agent-terminal-recovery",
    );
    seed_auto_queue_card(
        &db,
        "card-terminal-paused",
        9012,
        "done",
        "agent-terminal-recovery",
    );
    seed_auto_queue_card(
        &db,
        "card-terminal-generated",
        9013,
        "done",
        "agent-terminal-recovery",
    );
    seed_auto_queue_card(
        &db,
        "card-nonterminal-active",
        9014,
        "requested",
        "agent-terminal-recovery",
    );

    {
        let conn = db.lock().unwrap();
        for (run_id, status) in [
            ("run-terminal-active", "active"),
            ("run-terminal-paused", "paused"),
            ("run-terminal-generated", "generated"),
        ] {
            conn.execute(
                "INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
                 VALUES (?1, 'test-repo', 'agent-terminal-recovery', ?2)",
                rusqlite::params![run_id, status],
            )
            .unwrap();
        }

        for (entry_id, run_id, card_id) in [
            (
                "entry-terminal-active",
                "run-terminal-active",
                "card-terminal-active",
            ),
            (
                "entry-terminal-paused",
                "run-terminal-paused",
                "card-terminal-paused",
            ),
            (
                "entry-terminal-generated",
                "run-terminal-generated",
                "card-terminal-generated",
            ),
            (
                "entry-nonterminal-active",
                "run-terminal-active",
                "card-nonterminal-active",
            ),
        ] {
            conn.execute(
                "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status) \
                 VALUES (?1, ?2, ?3, 'agent-terminal-recovery', 'pending')",
                rusqlite::params![entry_id, run_id, card_id],
            )
            .unwrap();
        }
    }

    engine
        .fire_hook(
            crate::engine::hooks::Hook::OnTick1min,
            serde_json::json!({}),
        )
        .unwrap();

    let conn = db.lock().unwrap();
    let statuses: std::collections::HashMap<String, String> = conn
        .prepare("SELECT id, status FROM auto_queue_entries ORDER BY id ASC")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();

    assert_eq!(
        statuses.get("entry-terminal-active").map(String::as_str),
        Some("skipped")
    );
    assert_eq!(
        statuses.get("entry-terminal-paused").map(String::as_str),
        Some("skipped")
    );
    assert_eq!(
        statuses.get("entry-terminal-generated").map(String::as_str),
        Some("pending"),
        "generated runs are not part of #295 terminal cleanup scope"
    );
    assert_eq!(
        statuses.get("entry-nonterminal-active").map(String::as_str),
        Some("pending")
    );
}

#[test]
fn auto_queue_recovery_completes_finished_non_phase_gate_runs_and_releases_slots() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);

    seed_agent(&db, "agent-finished-recovery");
    seed_auto_queue_card(
        &db,
        "card-finished-done",
        9015,
        "done",
        "agent-finished-recovery",
    );
    seed_auto_queue_card(
        &db,
        "card-finished-skipped",
        9016,
        "done",
        "agent-finished-recovery",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-finished-recovery', 'test-repo', 'agent-finished-recovery', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, completed_at
            ) VALUES (
                'entry-finished-done', 'run-finished-recovery', 'card-finished-done',
                'agent-finished-recovery', 'done', 0, 0, datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, completed_at
            ) VALUES (
                'entry-finished-skipped', 'run-finished-recovery', 'card-finished-skipped',
                'agent-finished-recovery', 'skipped', 1, 1, datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (
                agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
            ) VALUES (
                'agent-finished-recovery', 0, 'run-finished-recovery', 0, '{}'
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (
                agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
            ) VALUES (
                'agent-finished-recovery', 1, 'run-finished-recovery', 1, '{}'
            )",
            [],
        )
        .unwrap();
    }

    engine
        .fire_hook(
            crate::engine::hooks::Hook::OnTick1min,
            serde_json::json!({}),
        )
        .unwrap();

    let conn = db.lock().unwrap();
    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-finished-recovery'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        run_status, "completed",
        "finished non-phase-gate run must be completed by onTick1min backstop"
    );

    for slot_index in [0, 1] {
        let slot: (Option<String>, Option<i64>) = conn
            .query_row(
                "SELECT assigned_run_id, assigned_thread_group
                 FROM auto_queue_slots
                 WHERE agent_id = 'agent-finished-recovery' AND slot_index = ?1",
                [slot_index],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            slot,
            (None, None),
            "completed run must release slot {slot_index}"
        );
    }
}

#[test]
fn auto_queue_recovery_keeps_finished_phase_gate_runs_blocked_until_gate_resolves() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);

    seed_agent(&db, "agent-finished-gate");
    seed_auto_queue_card(
        &db,
        "card-finished-gate",
        9017,
        "done",
        "agent-finished-gate",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-finished-gate', 'test-repo', 'agent-finished-gate', 'paused')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, batch_phase, completed_at
            ) VALUES (
                'entry-finished-gate', 'run-finished-gate', 'card-finished-gate',
                'agent-finished-gate', 'done', 0, 0, 1, datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (
                agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
            ) VALUES (
                'agent-finished-gate', 0, 'run-finished-gate', 0, '{}'
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_phase_gates (
                run_id, phase, status, dispatch_id, pass_verdict
             ) VALUES (?1, ?2, ?3, NULL, 'phase_gate_passed')",
            rusqlite::params!["run-finished-gate", 1, "pending",],
        )
        .unwrap();
    }

    engine
        .fire_hook(
            crate::engine::hooks::Hook::OnTick1min,
            serde_json::json!({}),
        )
        .unwrap();

    let conn = db.lock().unwrap();
    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-finished-gate'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        run_status, "paused",
        "finished phase-gate run must stay paused until the gate resolves"
    );

    let slot: (Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT assigned_run_id, assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = 'agent-finished-gate' AND slot_index = 0",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        slot,
        (Some("run-finished-gate".to_string()), Some(0)),
        "phase-gate blocked run must retain its slot assignment"
    );
}

// ── #265: Dispatch status validation ──────────────────────────

/// #265: PATCH /dispatches/:id with an invalid status like "done" must return
/// 400 and must NOT modify the dispatch or its associated card state.
#[tokio::test]
async fn patch_dispatch_rejects_invalid_status() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_test_agents(&db);

    // Seed a card in in_progress + a rework dispatch
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at)
             VALUES ('card-265', 'Stuck Card', 'in_progress', 'ch-td', 'dispatch-265', datetime('now'), datetime('now'))",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
             VALUES ('dispatch-265', 'card-265', 'ch-td', 'rework', 'dispatched', 'Rework task', datetime('now'), datetime('now'))",
            [],
        ).unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
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
    let conn = db.lock().unwrap();
    let dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-265'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        dispatch_status, "dispatched",
        "dispatch status must be unchanged after rejected update"
    );

    // Verify card state is also unchanged (pipeline invariant)
    let card_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-265'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        card_status, "in_progress",
        "card status must be unchanged after rejected dispatch update"
    );
}

/// #265: Valid statuses like "cancelled" must still work through the generic path.
#[tokio::test]
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
        let payload = serde_json::json!({
            "card_id": "card-preserve",
            "state": "idle",
            "last_verdict": "pass",
        })
        .to_string();
        let result = crate::engine::ops::review_state_sync_on_conn(&conn, &payload);
        assert!(result.contains("\"ok\""), "sync should succeed: {result}");

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
async fn rereview_backlog_card_transitions_to_review_with_dispatch() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-backlog-rr");
    set_pmd_channel(&db, "pmd-chan-123");
    ensure_auto_queue_tables(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, created_at, updated_at
            ) VALUES (
                'card-backlog-rr', 'Issue #301', 'backlog', 'medium', 'agent-backlog-rr', 'test-repo',
                301, datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                created_at, updated_at
            ) VALUES (
                'impl-backlog-rr', 'card-backlog-rr', 'agent-backlog-rr', 'implementation', 'completed',
                'impl', datetime('now', '-30 minutes'), datetime('now', '-30 minutes')
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

    let conn = db.lock().unwrap();
    let card_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-backlog-rr'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(card_status, "review", "card should transition to review");
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
                .uri("/re-review")
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_queue_reset_completes_generated_and_pending_runs() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-reset");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-reset-generated", 1711, "ready", "agent-reset");
    seed_auto_queue_card(&db, "card-reset-pending", 1712, "ready", "agent-reset");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-reset-generated', 'test-repo', 'agent-reset', 'generated', datetime('now', '-2 minutes'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-reset-pending', 'test-repo', 'agent-reset', 'pending', datetime('now', '-1 minutes'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-reset-generated', 'run-reset-generated', 'card-reset-generated', 'agent-reset', 'pending', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-reset-pending', 'run-reset-pending', 'card-reset-pending', 'agent-reset', 'pending', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/reset")
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
    assert_eq!(json["deleted_entries"], 2);
    assert_eq!(json["completed_runs"], 2);

    let status_response = app
        .oneshot(
            Request::builder()
                .uri("/auto-queue/status?agent_id=agent-reset")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(status_response.status(), StatusCode::OK);
    let status_body = axum::body::to_bytes(status_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let status_json: serde_json::Value = serde_json::from_slice(&status_body).unwrap();
    assert_eq!(status_json["run"]["id"], "run-reset-pending");
    assert_eq!(status_json["run"]["status"], "completed");
    assert_eq!(
        status_json["entries"]
            .as_array()
            .map(|entries| entries.len()),
        Some(0)
    );

    let conn = db.lock().unwrap();
    let generated_run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-reset-generated'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let pending_run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-reset-pending'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let remaining_entries: i64 = conn
        .query_row("SELECT COUNT(*) FROM auto_queue_entries", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(generated_run_status, "completed");
    assert_eq!(pending_run_status, "completed");
    assert_eq!(remaining_entries, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_queue_reset_with_agent_id_only_clears_matching_agent_scope() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-reset-a");
    seed_agent(&db, "agent-reset-b");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(
        &db,
        "card-reset-a-generated",
        1713,
        "ready",
        "agent-reset-a",
    );
    seed_auto_queue_card(&db, "card-reset-a-active", 1714, "ready", "agent-reset-a");
    seed_auto_queue_card(&db, "card-reset-b-active", 1715, "ready", "agent-reset-b");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-reset-a-generated', 'test-repo', 'agent-reset-a', 'generated', datetime('now', '-3 minutes'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-reset-a-active', 'test-repo', 'agent-reset-a', 'active', datetime('now', '-2 minutes'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-reset-b-active', 'test-repo', 'agent-reset-b', 'active', datetime('now', '-1 minutes'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-reset-a-generated', 'run-reset-a-generated', 'card-reset-a-generated', 'agent-reset-a', 'pending', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-reset-a-active', 'run-reset-a-active', 'card-reset-a-active', 'agent-reset-a', 'dispatched', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-reset-b-active', 'run-reset-b-active', 'card-reset-b-active', 'agent-reset-b', 'dispatched', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/reset")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"agent_id":"agent-reset-a"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["deleted_entries"], 2);
    assert_eq!(json["completed_runs"], 2);
    assert_eq!(json["protected_active_runs"], 0);

    let conn = db.lock().unwrap();
    let run_a_generated: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-reset-a-generated'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let run_a_active: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-reset-a-active'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let run_b_active: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-reset-b-active'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let remaining_agent_b_entries: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_entries WHERE agent_id = 'agent-reset-b'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let remaining_entries: i64 = conn
        .query_row("SELECT COUNT(*) FROM auto_queue_entries", [], |row| {
            row.get(0)
        })
        .unwrap();
    drop(conn);

    assert_eq!(run_a_generated, "completed");
    assert_eq!(run_a_active, "completed");
    assert_eq!(run_b_active, "active");
    assert_eq!(remaining_agent_b_entries, 1);
    assert_eq!(remaining_entries, 1);

    let status_response = app
        .oneshot(
            Request::builder()
                .uri("/auto-queue/status?agent_id=agent-reset-b")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(status_response.status(), StatusCode::OK);
    let status_body = axum::body::to_bytes(status_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let status_json: serde_json::Value = serde_json::from_slice(&status_body).unwrap();
    assert_eq!(status_json["run"]["id"], "run-reset-b-active");
    assert_eq!(status_json["run"]["status"], "active");
    assert_eq!(
        status_json["entries"]
            .as_array()
            .map(|entries| entries.len()),
        Some(1)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_queue_reset_without_agent_id_preserves_active_runs() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-reset-global-active");
    seed_agent(&db, "agent-reset-global-pending");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(
        &db,
        "card-reset-global-active",
        1716,
        "ready",
        "agent-reset-global-active",
    );
    seed_auto_queue_card(
        &db,
        "card-reset-global-pending",
        1717,
        "ready",
        "agent-reset-global-pending",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-reset-global-active', 'test-repo', 'agent-reset-global-active', 'active', datetime('now', '-2 minutes'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-reset-global-pending', 'test-repo', 'agent-reset-global-pending', 'pending', datetime('now', '-1 minutes'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-reset-global-active', 'run-reset-global-active', 'card-reset-global-active', 'agent-reset-global-active', 'dispatched', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-reset-global-pending', 'run-reset-global-pending', 'card-reset-global-pending', 'agent-reset-global-pending', 'pending', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/reset")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["deleted_entries"], 1);
    assert_eq!(json["completed_runs"], 1);
    assert_eq!(json["protected_active_runs"], 1);
    assert_eq!(
        json["warning"],
        "global reset preserved 1 active run(s); use agent_id to reset a specific queue"
    );

    let conn = db.lock().unwrap();
    let active_run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-reset-global-active'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let pending_run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-reset-global-pending'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let active_entries: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_entries WHERE run_id = 'run-reset-global-active'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let remaining_entries: i64 = conn
        .query_row("SELECT COUNT(*) FROM auto_queue_entries", [], |row| {
            row.get(0)
        })
        .unwrap();
    drop(conn);

    assert_eq!(active_run_status, "active");
    assert_eq!(pending_run_status, "completed");
    assert_eq!(active_entries, 1);
    assert_eq!(remaining_entries, 1);
}
