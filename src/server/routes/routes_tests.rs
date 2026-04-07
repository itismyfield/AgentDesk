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

fn test_engine_with_policy_dir(db: &Db, dir: &std::path::Path) -> PolicyEngine {
    let mut config = crate::config::Config::default();
    config.policies.dir = dir.to_path_buf();
    config.policies.hot_reload = false;
    PolicyEngine::new(&config, db.clone()).unwrap()
}

fn test_api_router(
    db: Db,
    engine: PolicyEngine,
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
) -> axum::Router {
    let tx = crate::server::ws::new_broadcast();
    let buf = crate::server::ws::spawn_batch_flusher(tx.clone());
    api_router(db, engine, tx, buf, health_registry)
}

fn env_lock() -> MutexGuard<'static, ()> {
    crate::services::discord::runtime_store::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
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
            "tmux_session_name": tmux_name,
            "started_at": "2026-04-06 10:11:12",
            "current_tool_line": "⚙ Bash: rg -n turn src",
            "full_response": "partial output\nOPENAI_API_KEY=sk-secret",
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
    assert_eq!(json["recent_output_source"], "inflight");
    assert_eq!(json["active_dispatch_id"], "dispatch-turn");
    let recent_output = json["recent_output"].as_str().unwrap();
    assert!(recent_output.contains("⚙ Bash: rg -n turn src"));
    assert!(recent_output.contains("OPENAI_API_KEY=[REDACTED]"));
    assert!(!recent_output.contains("sk-secret"));
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
            "tmux_session_name": tmux_name,
            "started_at": "2026-04-06 10:20:00",
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
                .body(Body::from(r#"{"status":"completed","result":{"ok":true}}"#))
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

// ── Pipeline Stages API tests ─────────────────────────────────

#[tokio::test]
async fn pipeline_stages_empty_list() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/pipeline-stages")
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
    assert!(json["stages"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn pipeline_stages_create_and_list() {
    let db = test_db();
    let engine = test_engine(&db);

    // Create
    let app = test_api_router(db.clone(), engine.clone(), None);
    let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/pipeline-stages")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"repo_id":"owner/repo","stage_name":"qa-test","stage_order":1,"trigger_after":"review_pass","entry_skill":"test","timeout_minutes":60}"#,
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
    assert_eq!(json["stage"]["stage_name"], "qa-test");
    assert_eq!(json["stage"]["trigger_after"], "review_pass");
    assert_eq!(json["stage"]["timeout_minutes"], 60);
    let stage_id = json["stage"]["id"].as_i64().unwrap();

    // List with filter
    let app2 = test_api_router(db.clone(), engine.clone(), None);
    let response2 = app2
        .oneshot(
            Request::builder()
                .uri("/pipeline-stages?repo_id=owner/repo")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body2 = axum::body::to_bytes(response2.into_body(), usize::MAX)
        .await
        .unwrap();
    let json2: serde_json::Value = serde_json::from_slice(&body2).unwrap();
    assert_eq!(json2["stages"].as_array().unwrap().len(), 1);

    // Delete
    let app3 = test_api_router(db, engine, None);
    let response3 = app3
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(&format!("/pipeline-stages/{stage_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response3.status(), StatusCode::OK);
    let body3 = axum::body::to_bytes(response3.into_body(), usize::MAX)
        .await
        .unwrap();
    let json3: serde_json::Value = serde_json::from_slice(&body3).unwrap();
    assert_eq!(json3["deleted"], true);
}

#[tokio::test]
async fn pipeline_stages_delete_not_found() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/pipeline-stages/9999")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn pipeline_stages_list_filtered_by_repo() {
    let db = test_db();
    let engine = test_engine(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO pipeline_stages (repo_id, stage_name, stage_order, trigger_after, timeout_minutes) VALUES ('repo-a', 'test', 1, 'review_pass', 30)",
                [],
            ).unwrap();
        conn.execute(
                "INSERT INTO pipeline_stages (repo_id, stage_name, stage_order, trigger_after, timeout_minutes) VALUES ('repo-b', 'deploy', 1, 'review_pass', 60)",
                [],
            ).unwrap();
    }

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/pipeline-stages?repo_id=repo-a")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let stages = json["stages"].as_array().unwrap();
    assert_eq!(stages.len(), 1);
    assert_eq!(stages[0]["stage_name"], "test");
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
            max_concurrent_per_agent INTEGER DEFAULT 1,
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
            created_at      DATETIME DEFAULT CURRENT_TIMESTAMP,
            dispatched_at   DATETIME,
            completed_at    DATETIME,
            thread_group    INTEGER DEFAULT 0
        );",
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

fn seed_live_auto_queue_run(db: &Db, run_id: &str, agent_id: &str, existing_card_id: &str) {
    ensure_auto_queue_tables(db);
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES (?1, 'test-repo', ?2, 'active')",
        rusqlite::params![run_id, agent_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank)
         VALUES (?1, ?2, ?3, ?4, 'pending', 0)",
        rusqlite::params![format!("entry-{run_id}"), run_id, existing_card_id, agent_id],
    )
    .unwrap();
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

    assert_eq!(
        card_status, "review",
        "true orphan implementation dispatch must still promote the card into review"
    );
    assert_eq!(
        dispatch_status, "completed",
        "true orphan implementation dispatch must be marked completed"
    );
    assert!(
        dispatch_result
            .as_deref()
            .unwrap_or("")
            .contains("orphan_recovery"),
        "true orphan recovery must keep the orphan_recovery completion marker"
    );
}

#[test]
fn on_tick30s_orphan_dispatch_skips_card_that_moved_to_backlog_mid_recovery() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    seed_agent(&db, "agent-orphan-race");
    seed_repo(&db, "test-repo");

    let temp_dir = tempfile::tempdir().unwrap();
    let policy_dir = temp_dir.path();
    std::fs::copy(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies/timeouts.js"),
        policy_dir.join("timeouts.js"),
    )
    .unwrap();
    std::fs::write(
        policy_dir.join("zzz_orphan_race.js"),
        r#"
        (function() {
          var raw = agentdesk.dispatch.markCompleted;
          agentdesk.dispatch.markCompleted = function(dispatchId, resultJson) {
            var result = raw(dispatchId, resultJson);
            if (dispatchId === "dispatch-race-330") {
              JSON.parse(agentdesk.db.__execute_raw(
                "UPDATE kanban_cards SET status = 'backlog', updated_at = datetime('now') WHERE id = ?1",
                JSON.stringify(["card-race-330"])
              ));
            }
            return result;
          };
          agentdesk.registerPolicy({ name: "orphan-race-test" });
        })();
        "#,
    )
    .unwrap();

    let engine = test_engine_with_policy_dir(&db, policy_dir);

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

    assert_eq!(
        card_status, "backlog",
        "post-complete race guard must keep a backlogged card from reviving into review"
    );
    assert_eq!(
        dispatch_status, "completed",
        "the orphan implementation dispatch may still complete, but must not resurrect the card"
    );
    assert_eq!(
        review_dispatch_count, 0,
        "skipped orphan recovery must not create a follow-up review dispatch"
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
                approach_change_round, review_entered_at, updated_at
            ) VALUES (
                'card-ft-clean', 'suggestion_pending', 'old-review-dispatch', 4, 'pass', 'approved',
                3, datetime('now', '-11 minutes'), datetime('now')
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
        review_state_entered_at,
    ): (
        i64,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT review_round, state, pending_dispatch_id, last_verdict, last_decision,
                    approach_change_round, review_entered_at
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
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-rereview");
    set_pmd_channel(&db, "pmd-chan-123");
    ensure_auto_queue_tables(&db);

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
    assert_eq!(dispatch_status, "pending");

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
async fn auto_queue_enqueue_rejects_backlog_card() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-eq-backlog");
    seed_auto_queue_card(&db, "card-eq-backlog", 1621, "backlog", "agent-eq-backlog");

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/enqueue")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "issue_number": 1621,
                        "agent_id": "agent-eq-backlog",
                    }))
                    .unwrap(),
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
    assert_eq!(json["status"], "backlog");
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("ready/requested/dispatchable"),
        "error should explain that only prepared work can be enqueued"
    );
    let allowed_states = json["allowed_states"]
        .as_array()
        .expect("allowed_states should be an array");
    assert!(
        !allowed_states
            .iter()
            .any(|state| state.as_str() == Some("backlog")),
        "backlog must not appear in allowed enqueue states"
    );

    let conn = db.lock().unwrap();
    let dispatch_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-eq-backlog'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        dispatch_count, 0,
        "rejected backlog enqueue must not create a side dispatch"
    );
    let queued_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_entries WHERE kanban_card_id = 'card-eq-backlog'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        queued_count, 0,
        "rejected backlog enqueue must not create queue entries"
    );
}

#[tokio::test]
async fn auto_queue_enqueue_accepts_requested_without_active_dispatch() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-eq-requested");
    seed_auto_queue_card(
        &db,
        "card-live-requested",
        9101,
        "ready",
        "agent-eq-requested",
    );
    seed_live_auto_queue_run(
        &db,
        "run-live-requested",
        "agent-eq-requested",
        "card-live-requested",
    );
    seed_auto_queue_card(
        &db,
        "card-eq-requested",
        1622,
        "requested",
        "agent-eq-requested",
    );

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/enqueue")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "issue_number": 1622,
                        "agent_id": "agent-eq-requested",
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

    let conn = db.lock().unwrap();
    let dispatch_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-eq-requested'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        dispatch_count, 0,
        "enqueue must not create a side dispatch for requested cards"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_queue_activate_active_only_does_not_promote_generated_runs() {
    crate::pipeline::ensure_loaded();
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

/// #162: A card in 'backlog' (non-dispatchable) state must be silently walked
/// to the dispatchable state via free transitions before dispatch creation.
/// The walk must use the canonical reducer path (ApplyClock, AuditLog, etc.)
/// and NOT raw SQL.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_queue_activate_walks_backlog_card_to_dispatchable_state() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-walk");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-walk-bl", 1631, "backlog", "agent-walk");

    {
        let conn = db.lock().unwrap();
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
}

/// #162 DoD: ready-state backward compatibility — enqueue accepts ready cards
/// without creating side dispatches.
#[tokio::test]
async fn auto_queue_enqueue_accepts_ready_cards_unchanged() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-eq-ready");
    seed_auto_queue_card(&db, "card-live-ready", 9102, "ready", "agent-eq-ready");
    seed_live_auto_queue_run(&db, "run-live-ready", "agent-eq-ready", "card-live-ready");
    seed_auto_queue_card(&db, "card-eq-ready", 1623, "ready", "agent-eq-ready");

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/enqueue")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "issue_number": 1623,
                        "agent_id": "agent-eq-ready",
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

    let conn = db.lock().unwrap();
    let dispatch_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-eq-ready'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        dispatch_count, 0,
        "enqueue must not create a side dispatch — dispatch happens only at activate"
    );
    let entry_status: String = conn
        .query_row(
            "SELECT e.status FROM auto_queue_entries e WHERE e.kanban_card_id = 'card-eq-ready'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(entry_status, "pending");
}

/// #259 regression: enqueue must reject when there is no live active/pending run.
/// A stale finished run left as `active` should be auto-completed first instead of
/// silently absorbing new entries that will never dispatch.
#[tokio::test]
async fn auto_queue_enqueue_rejects_when_only_stale_finished_run_exists() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-eq-stale");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-stale-finished", 9103, "done", "agent-eq-stale");
    seed_auto_queue_card(
        &db,
        "card-eq-stale-target",
        16235,
        "ready",
        "agent-eq-stale",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-stale-finished', 'test-repo', 'agent-eq-stale', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, completed_at)
             VALUES ('entry-stale-finished', 'run-stale-finished', 'card-stale-finished', 'agent-eq-stale', 'done', 0, datetime('now'))",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/enqueue")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "issue_number": 16235,
                        "agent_id": "agent-eq-stale",
                    }))
                    .unwrap(),
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
            .contains("completed runs cannot accept enqueue"),
        "error should explain that enqueue requires a live run"
    );
    assert_eq!(json["last_run_id"], "run-stale-finished");
    assert_eq!(json["last_run_status"], "completed");

    let conn = db.lock().unwrap();
    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-stale-finished'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        run_status, "completed",
        "stale active run must be auto-completed before rejecting enqueue"
    );
    let queued_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_entries WHERE kanban_card_id = 'card-eq-stale-target'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        queued_count, 0,
        "rejected enqueue must not create queue entries"
    );
}

/// #162 DoD: active dispatch guard — rejects enqueue for cards with pending/dispatched dispatch.
#[tokio::test]
async fn auto_queue_enqueue_rejects_card_with_active_dispatch() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-eq-dup");
    seed_auto_queue_card(&db, "card-eq-dup", 1624, "ready", "agent-eq-dup");

    // Pre-create an active dispatch for this card
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, title, status, created_at) \
             VALUES ('disp-dup', 'card-eq-dup', 'agent-eq-dup', 'implementation', 'test', 'pending', datetime('now'))",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/enqueue")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "issue_number": 1624,
                        "agent_id": "agent-eq-dup",
                    }))
                    .unwrap(),
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
    assert!(
        json["error"].as_str().unwrap().contains("active dispatch"),
        "must reject with active-dispatch error"
    );
}

/// #162 DoD: unified_thread continuity — dispatches entry correctly within unified run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_queue_activate_unified_thread_run_dispatches_to_same_run() {
    crate::pipeline::ensure_loaded();
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
async fn parallel_generate_creates_correct_thread_groups() {
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
                        "parallel": true,
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

    // Verify run has correct parallel config
    let run = &json["run"];
    assert_eq!(run["max_concurrent_threads"], 3);
    assert_eq!(run["max_concurrent_per_agent"], 3);

    // Collect thread_group assignments per issue number
    let mut groups: std::collections::HashMap<i64, Vec<(i64, i64)>> =
        std::collections::HashMap::new();
    for entry in entries {
        let issue_num = entry["github_issue_number"].as_i64().unwrap();
        let thread_group = entry["thread_group"].as_i64().unwrap();
        let priority_rank = entry["priority_rank"].as_i64().unwrap();
        groups
            .entry(thread_group)
            .or_default()
            .push((issue_num, priority_rank));
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
    chain_members.sort_by_key(|(_, rank)| *rank);
    let chain_issues: Vec<i64> = chain_members.iter().map(|(num, _)| *num).collect();
    // Issue #4 must come first (rank 0), #5 second, then #6 and #7 (order between 6,7 may vary
    // since #7 depends on both #5 and #6, making #6 and #7 at different levels)
    assert_eq!(chain_issues[0], 4, "chain start (#4) must have lowest rank");
    assert_eq!(chain_issues[1], 5, "#5 depends on #4, must be second");
    // #6 depends on #5, #7 depends on #5 and #6 — so #6 before #7
    assert_eq!(chain_issues[2], 6, "#6 depends on #5, must be third");
    assert_eq!(chain_issues[3], 7, "#7 depends on #5 and #6, must be last");
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
async fn generate_similarity_aware_groups_by_file_paths_and_recommends_threads() {
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
                        "mode": "similarity-aware",
                        "parallel": true,
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
    assert_eq!(
        run["ai_model"].as_str().unwrap(),
        "similarity-aware-thread-group"
    );

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
async fn generate_similarity_aware_without_file_paths_falls_back_to_dependency_only_groups() {
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
                        "mode": "similarity-aware",
                        "parallel": true,
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
        "without file paths, similarity-aware must fall back to dependency-only grouping"
    );
    assert_eq!(
        run["ai_model"].as_str().unwrap(),
        "similarity-aware-thread-group"
    );
    assert!(
        run["ai_rationale"]
            .as_str()
            .map(|text| text.contains("fallback"))
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
async fn priority_sort_default_keeps_similarity_candidates_in_single_group() {
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
    assert_eq!(run["thread_group_count"], 1);
    assert_eq!(run["max_concurrent_threads"], 1);
    assert_eq!(run["ai_model"], "priority-sort");
    assert!(
        entries
            .iter()
            .all(|entry| entry["thread_group"].as_i64().unwrap() == 0),
        "default priority-sort should keep a single sequential group"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_activate_dispatches_multiple_groups() {
    crate::pipeline::ensure_loaded();

    let db = test_db();
    let engine = test_engine(&db);
    let _card_ids = seed_parallel_test_cards(&db);

    let app = test_api_router(db.clone(), engine.clone(), None);

    // Step 1: Generate with parallel mode (no agent_id filter — cards have mixed agents)
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
                        "parallel": true,
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
async fn parallel_false_keeps_single_group_sequential() {
    let db = test_db();
    let engine = test_engine(&db);
    let _card_ids = seed_parallel_test_cards(&db);

    let app = test_api_router(db, engine, None);

    // Generate WITHOUT parallel — should put all entries in group 0
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

    // All entries should be in thread_group 0
    for entry in entries {
        assert_eq!(
            entry["thread_group"].as_i64().unwrap(),
            0,
            "non-parallel mode: all entries must be in group 0"
        );
    }
    assert_eq!(run["thread_group_count"], 1);
    assert_eq!(run["max_concurrent_threads"], 1);
}

/// Regression test for #191: onTick1min recovery must reset stuck auto-queue
/// entries that are 'dispatched' but have orphan (NULL), phantom (missing row),
/// or cancelled/failed dispatch_ids — while leaving valid dispatches untouched.
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
async fn rereview_resets_approach_change_round() {
    crate::pipeline::ensure_loaded();
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
        // Seed card_review_state with a non-null approach_change_round from a previous cycle
        conn.execute(
            "INSERT INTO card_review_state (card_id, state, review_round, approach_change_round, updated_at)
             VALUES ('card-acr', 'reviewing', 3, 2, datetime('now'))",
            [],
        )
        .unwrap();
    }

    // Verify approach_change_round is set before rereview
    {
        let conn = db.lock().unwrap();
        let acr: Option<i64> = conn
            .query_row(
                "SELECT approach_change_round FROM card_review_state WHERE card_id = 'card-acr'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            acr,
            Some(2),
            "approach_change_round should be 2 before rereview"
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
                    r#"{"reason":"approach_change_round reset test"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    // approach_change_round should be NULL after rereview
    let conn = db.lock().unwrap();
    let acr: Option<i64> = conn
        .query_row(
            "SELECT approach_change_round FROM card_review_state WHERE card_id = 'card-acr'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        acr.is_none(),
        "approach_change_round should be NULL after rereview, got {:?}",
        acr
    );
}

#[tokio::test]
async fn idle_sync_preserves_approach_change_round() {
    // Regression test for #272: generic idle sync (timeout, gate-failure, pass)
    // must NOT clear approach_change_round — only the explicit rereview path does.
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
            "INSERT INTO card_review_state (card_id, state, review_round, approach_change_round, updated_at)
             VALUES ('card-preserve', 'reviewing', 3, 2, datetime('now'))",
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

        let acr: Option<i64> = conn
            .query_row(
                "SELECT approach_change_round FROM card_review_state WHERE card_id = 'card-preserve'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            acr,
            Some(2),
            "approach_change_round must be preserved on generic idle sync, got {:?}",
            acr
        );
    }
}

#[tokio::test]
async fn rereview_backlog_card_transitions_to_review_with_dispatch() {
    crate::pipeline::ensure_loaded();
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
async fn batch_rereview_processes_multiple_issues() {
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
