use super::*;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::sync::Arc;
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
    let tx = crate::server::ws::new_broadcast();
    let buf = crate::server::ws::spawn_batch_flusher(tx.clone());
    api_router(db, engine, tx, buf, health_registry)
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
    assert_eq!(json["card"]["status"], "ready");
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

    // Card should be "requested"
    let conn = db.lock().unwrap();
    let card_status: String = conn
        .query_row("SELECT status FROM kanban_cards WHERE id = 'c1'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(card_status, "requested");
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
async fn create_repo_does_not_seed_disabled_agentdesk_pipeline_stages() {
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

    assert!(rows.is_empty());
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
async fn auto_queue_enqueue_accepts_backlog_without_creating_dispatch() {
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

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);

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
        "enqueue must not create a side dispatch for backlog cards"
    );
    let (run_status, entry_status): (String, String) = conn
        .query_row(
            "SELECT r.status, e.status FROM auto_queue_runs r \
             JOIN auto_queue_entries e ON e.run_id = r.id \
             WHERE e.kanban_card_id = 'card-eq-backlog'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        run_status, "pending",
        "enqueue creates run in pending state — requires activate to dispatch"
    );
    assert_eq!(entry_status, "pending");
}

#[tokio::test]
async fn auto_queue_enqueue_accepts_requested_without_active_dispatch() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-eq-requested");
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
