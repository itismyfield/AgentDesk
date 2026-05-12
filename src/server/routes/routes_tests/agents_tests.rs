//! Domain-split routes tests — `agents` group.
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
async fn agent_turn_pg_returns_recent_output_from_inflight_snapshot() {
    let _env_lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    let inflight_dir = temp
        .path()
        .join("runtime")
        .join("discord_inflight")
        .join("codex");
    std::fs::create_dir_all(&inflight_dir).unwrap();

    let tmux_name = format!(
        "AgentDesk-codex-adk-cdx-inflight-test-{}",
        std::process::id()
    );
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
            "tmux_session_name": tmux_name.clone(),
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

    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id, created_at, updated_at)
         VALUES ('agent-turn', 'Agent Turn', 'codex', '1485506232256168011', NOW(), NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions
         (session_key, agent_id, provider, status, active_dispatch_id, last_heartbeat, created_at)
         VALUES ($1, 'agent-turn', 'codex', 'turn_active', 'dispatch-turn', NOW(), '2026-04-06 10:00:00')",
    )
    .bind(format!("mac-mini:{tmux_name}"))
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
                .uri("/agents/agent-turn/turn")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "turn_active");
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

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn agent_turn_pg_reports_idle_when_agent_has_no_active_session() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, provider, created_at, updated_at)
         VALUES ('agent-idle', 'Agent Idle', 'codex', NOW(), NOW())",
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
                .uri("/agents/agent-idle/turn")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["status"], "idle");
    assert!(json["recent_output"].is_null());
    assert!(json["started_at"].is_null());
    assert!(json["updated_at"].is_null());
    assert_eq!(json["recent_output_source"], "none");
    assert!(json["current_tool_line"].is_null());
    assert!(json["prev_tool_status"].is_null());
    assert_eq!(json["tool_count"], 0);
    assert!(json["tool_events"].as_array().unwrap().is_empty());

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn agent_transcripts_pg_returns_structured_events() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query("INSERT INTO agents (id, name, provider, status, xp) VALUES ($1, $2, $3, $4, $5)")
        .bind("agent-transcript")
        .bind("Transcript Agent")
        .bind("codex")
        .bind("idle")
        .bind(0_i32)
        .execute(&pool)
        .await
        .unwrap();

    let events_json = serde_json::to_string(&json!([{
        "kind": "tool_use",
        "tool_name": "Bash",
        "summary": "cargo test",
        "content": "cargo test --no-run",
        "status": "success",
        "is_error": false,
    }]))
    .unwrap();
    sqlx::query(
        "INSERT INTO session_transcripts (
            turn_id, session_key, channel_id, agent_id, provider, dispatch_id,
            user_message, assistant_message, events_json, duration_ms
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, CAST($9 AS jsonb), $10)",
    )
    .bind("discord:agent-transcript:1")
    .bind("host:agent-transcript")
    .bind("chan-1")
    .bind("agent-transcript")
    .bind("codex")
    .bind(Option::<String>::None)
    .bind("verify build")
    .bind("build verified")
    .bind(events_json)
    .bind(4200_i32)
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
                .uri("/agents/agent-transcript/transcripts?limit=5")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, StatusCode::OK, "unexpected body: {json}");
    assert_eq!(json["agent_id"], "agent-transcript");
    assert_eq!(
        json["transcripts"][0]["turn_id"],
        "discord:agent-transcript:1"
    );
    assert!(json["transcripts"][0]["card_title"].is_null());
    assert!(json["transcripts"][0]["github_issue_number"].is_null());
    assert_eq!(json["transcripts"][0]["duration_ms"], 4200);
    assert_eq!(json["transcripts"][0]["events"][0]["kind"], "tool_use");
    assert_eq!(json["transcripts"][0]["events"][0]["tool_name"], "Bash");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn agent_transcripts_pg_falls_back_to_session_agent_for_legacy_rows() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query("INSERT INTO agents (id, name, provider, status, xp) VALUES ($1, $2, $3, $4, $5)")
        .bind("agent-transcript-fallback")
        .bind("Transcript Fallback")
        .bind("codex")
        .bind("idle")
        .bind(0_i32)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO sessions (session_key, agent_id, provider, status, last_heartbeat)
         VALUES ($1, $2, $3, $4, NOW())",
    )
    .bind("host:agent-transcript-fallback")
    .bind("agent-transcript-fallback")
    .bind("codex")
    .bind("idle")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO session_transcripts (
            turn_id, session_key, channel_id, agent_id, provider, dispatch_id,
            user_message, assistant_message, events_json
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, CAST($9 AS jsonb))",
    )
    .bind("discord:agent-transcript-fallback:1")
    .bind("host:agent-transcript-fallback")
    .bind("chan-fallback")
    .bind(Option::<String>::None)
    .bind("codex")
    .bind(Option::<String>::None)
    .bind("legacy question")
    .bind("legacy answer")
    .bind("[]")
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
                .uri("/agents/agent-transcript-fallback/transcripts?limit=5")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, StatusCode::OK, "unexpected body: {json}");
    assert_eq!(json["transcripts"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        json["transcripts"][0]["turn_id"],
        "discord:agent-transcript-fallback:1"
    );
    assert_eq!(json["transcripts"][0]["agent_id"], serde_json::Value::Null);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
#[ignore = "requires tmux"]
async fn stop_agent_turn_preserves_matching_tmux_session() {
    let _env_lock = env_lock();
    Command::new("tmux")
        .arg("-V")
        .output()
        .expect("tmux must be installed for this test");

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
        .expect("tmux session should start for this test");
    assert!(
        tmux_started.success(),
        "tmux session should start for this test"
    );

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
             VALUES (?1, 'agent-stop', 'codex', 'turn_active', datetime('now'), datetime('now'))",
            [session_key.clone()],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .clone()
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

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    if status != StatusCode::OK {
        panic!(
            "auto_queue_activate_dispatches_pg_only_run_without_sqlite_mirror status={} body={}",
            status,
            String::from_utf8_lossy(&body)
        );
    }
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "stopped");
    assert_eq!(json["tmux_killed"], false);
    assert_eq!(json["lifecycle_path"], "direct-fallback");
    assert!(
        tmux_still_alive,
        "tmux session should stay alive after /turn/stop"
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
async fn stop_agent_turn_pg_preserves_pending_queue_via_mailbox_fallback_cleanup() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let harness = crate::services::discord::health::TestHealthHarness::new_with_provider(
        crate::services::provider::ProviderKind::Codex,
    )
    .await;
    let channel_id = "1485506232256168012";
    let channel_num = channel_id.parse::<u64>().unwrap();
    let tmux_name = "AgentDesk-codex-stop-canonical";
    let session_key = format!("mac-mini:{tmux_name}");

    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id, created_at, updated_at)
         VALUES ('agent-stop-canonical', 'Agent Stop Canonical', 'codex', $1, NOW(), NOW())",
    )
    .bind(channel_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions
         (session_key, agent_id, provider, status, last_heartbeat, created_at)
         VALUES ($1, 'agent-stop-canonical', 'codex', 'turn_active', NOW(), NOW())",
    )
    .bind(session_key.as_str())
    .execute(&pool)
    .await
    .unwrap();

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

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        Some(harness.registry()),
        pool.clone(),
    );
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

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    if status != StatusCode::OK {
        panic!(
            "stop_agent_turn_pg_preserves_pending_queue status={} body={}",
            status,
            String::from_utf8_lossy(&body)
        );
    }
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "stopped");
    assert_eq!(json["lifecycle_path"], "runtime-fallback");
    assert_eq!(json["queue_preserved"], true);

    let (has_active_turn, queue_depth, session_id) = harness.mailbox_state(channel_num).await;
    assert!(!has_active_turn);
    assert_eq!(queue_depth, 1);
    assert_eq!(session_id, None);
    assert!(harness.has_dispatch_role_override(channel_num));

    let session_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM sessions WHERE session_key = $1")
            .bind(&session_key)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(session_status, "disconnected");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn stop_agent_turn_tmux_only_pg_fallback_clears_mailbox_without_detaching_watcher() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let provider = crate::services::provider::ProviderKind::Codex;
    let harness =
        crate::services::discord::health::TestHealthHarness::new_with_provider(provider.clone())
            .await;
    let channel_num = 1485506232256168013u64;
    let channel_name = "operator-stop-tmux-only";
    let tmux_name = provider.build_tmux_session_name(channel_name);
    let session_key = format!("mac-mini:{tmux_name}");

    sqlx::query(
        "INSERT INTO agents (id, name, provider, created_at, updated_at)
         VALUES ('agent-stop-tmux-only', 'Agent Stop Tmux Only', 'codex', NOW(), NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions
         (session_key, agent_id, provider, status, last_heartbeat, created_at)
         VALUES ($1, 'agent-stop-tmux-only', 'codex', 'turn_active', NOW(), NOW())",
    )
    .bind(session_key.as_str())
    .execute(&pool)
    .await
    .unwrap();

    harness
        .seed_channel_session(channel_num, channel_name, Some("session-stop-tmux-only"))
        .await;
    harness.seed_active_turn(channel_num, 9, 91).await;
    let watcher_cancel = harness.seed_watcher(channel_num);

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        Some(harness.registry()),
        pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents/agent-stop-tmux-only/turn/stop")
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
            "stop_agent_turn_tmux_only_fallback status={} body={}",
            status,
            String::from_utf8_lossy(&body)
        );
    }
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "stopped");
    assert_eq!(json["lifecycle_path"], "mailbox_canonical");
    assert_eq!(json["tmux_killed"], false);

    let (has_active_turn, _, _) = harness.mailbox_state(channel_num).await;
    assert!(
        !has_active_turn,
        "tmux-only operator stop fallback must clear active mailbox state",
    );
    assert!(
        harness.has_watcher(channel_num),
        "tmux-only operator stop fallback must preserve live watcher ownership",
    );
    assert!(
        !watcher_cancel.load(std::sync::atomic::Ordering::Relaxed),
        "tmux-only operator stop fallback must not cancel the watcher",
    );

    let session_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM sessions WHERE session_key = $1")
            .bind(&session_key)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(session_status, "disconnected");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn start_agent_turn_pg_returns_conflict_when_mailbox_is_busy() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let harness = crate::services::discord::health::TestHealthHarness::new_with_provider(
        crate::services::provider::ProviderKind::Codex,
    )
    .await;
    let channel_id = "1485506232256168123";
    let channel_num = channel_id.parse::<u64>().unwrap();

    sqlx::query(
        "INSERT INTO agents
         (id, name, provider, discord_channel_id, discord_channel_alt, created_at, updated_at)
         VALUES ('agent-turn-start-busy', 'Agent Turn Start Busy', 'codex', 'legacy-busy', $1, NOW(), NOW())",
    )
    .bind(channel_id)
    .execute(&pool)
    .await
    .unwrap();

    harness.seed_active_turn(channel_num, 7, 77).await;

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        Some(harness.registry()),
        pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents/agent-turn-start-busy/turn/start")
                .header("content-type", "application/json")
                .body(Body::from(
                    format!(
                        r#"{{"prompt":"run headless probe","source":"system","metadata":{{"trigger_source":"test"}},"channel_id":"{}"}}"#,
                        channel_id
                    ),
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
    assert_eq!(json["ok"], false);
    assert_eq!(json["status"], "conflict");
    assert!(
        json["error"]
            .as_str()
            .is_some_and(|value| value.contains("mailbox is busy")),
        "unexpected error body: {json}"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn start_agent_turn_pg_rejects_channel_override_outside_agent_bindings() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let harness = crate::services::discord::health::TestHealthHarness::new_with_provider(
        crate::services::provider::ProviderKind::Codex,
    )
    .await;
    let bound_channel_id = "1485506232256168124";
    let forbidden_channel_id = "1485506232256168125";

    sqlx::query(
        "INSERT INTO agents
         (id, name, provider, discord_channel_id, discord_channel_alt, created_at, updated_at)
         VALUES ('agent-turn-start-forbidden', 'Agent Turn Start Forbidden', 'codex', 'legacy-forbidden', $1, NOW(), NOW())",
    )
    .bind(bound_channel_id)
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        Some(harness.registry()),
        pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents/agent-turn-start-forbidden/turn/start")
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"prompt":"run headless probe","source":"system","channel_id":"{}"}}"#,
                    forbidden_channel_id
                )))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], false);
    assert!(
        json["error"]
            .as_str()
            .is_some_and(|value| value.contains("not allowed")),
        "unexpected error body: {json}"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
#[ignore = "requires tmux"]
async fn cancel_turn_preserves_tmux_and_cancels_active_dispatch() {
    let _env_lock = env_lock();
    Command::new("tmux")
        .arg("-V")
        .output()
        .expect("tmux must be installed for this test");

    let tmux_name = format!("AgentDesk-codex-turn-cancel-{}", std::process::id());
    let session_key = format!("mac-mini:{tmux_name}");
    let channel_id = "1485506232256168011";

    let tmux_started = Command::new("tmux")
        .args(["new-session", "-d", "-s", &tmux_name, "sleep 30"])
        .status()
        .expect("tmux session should start for this test");
    assert!(
        tmux_started.success(),
        "tmux session should start for this test"
    );

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
             VALUES (?1, 'agent-queue-stop', 'codex', 'turn_active', 'dispatch-turn-cancel', datetime('now'), datetime('now'))",
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

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body).to_string();
    assert_eq!(
        status,
        StatusCode::OK,
        "unexpected reopen response: {body_text}"
    );
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["session_key"], session_key);
    assert_eq!(json["tmux_session"], tmux_name);
    assert_eq!(json["tmux_killed"], true);
    assert_eq!(json["lifecycle_path"], "direct-fallback");
    assert_eq!(json["dispatch_cancelled"], "dispatch-turn-cancel");
    assert_eq!(json["exact_channel_match"], true);
    assert!(
        !tmux_still_alive,
        "tmux session should be killed after /turns/{{channel_id}}/cancel"
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
async fn cancel_turn_preserves_pending_queue_via_mailbox_fallback_cleanup_pg() {
    let _env_lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());

    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let harness = crate::services::discord::health::TestHealthHarness::new().await;
    let channel_id = "1485506232256168013";
    let channel_num = channel_id.parse::<u64>().unwrap();
    let session_key = "mac-mini:AgentDesk-claude-cancel-canonical";
    let inflight_path = runtime_root
        .path()
        .join("runtime")
        .join("discord_inflight")
        .join("claude")
        .join(format!("{channel_num}.json"));
    fs::create_dir_all(inflight_path.parent().unwrap()).unwrap();
    fs::write(&inflight_path, "{}").unwrap();

    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id, created_at, updated_at)
         VALUES ('agent-cancel-canonical', 'Agent Cancel Canonical', 'claude', $1, NOW(), NOW())",
    )
    .bind(channel_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions
         (session_key, agent_id, provider, status, last_heartbeat, created_at)
         VALUES ($1, 'agent-cancel-canonical', 'claude', 'turn_active', NOW(), NOW())",
    )
    .bind(session_key)
    .execute(&pool)
    .await
    .unwrap();

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
    let watcher_cancel = harness.seed_watcher(channel_num);

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        Some(harness.registry()),
        pool.clone(),
    );
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

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["session_key"], session_key);
    assert_eq!(json["lifecycle_path"], "runtime-fallback");
    assert_eq!(json["tmux_killed"], false);
    assert_eq!(json["queue_preserved"], true);
    assert_eq!(json["inflight_cleared"], false);
    assert_eq!(json["exact_channel_match"], true);
    assert!(json["dispatch_cancelled"].is_null());
    assert!(
        inflight_path.exists(),
        "default killed=false cancel must preserve persistent inflight for live-session handoff"
    );

    let (has_active_turn, queue_depth, session_id) = harness.mailbox_state(channel_num).await;
    assert!(!has_active_turn);
    assert_eq!(queue_depth, 1);
    assert_eq!(session_id, None);
    assert!(harness.has_dispatch_role_override(channel_num));
    assert!(
        harness.has_watcher(channel_num),
        "killed=false cancel must preserve watcher ownership"
    );
    assert!(
        !watcher_cancel.load(std::sync::atomic::Ordering::Relaxed),
        "killed=false cancel must not signal watcher cancellation"
    );

    let session_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM sessions WHERE session_key = $1")
            .bind(session_key)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(session_status, "disconnected");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn cancel_turn_targets_requested_provider_for_paired_agent_pg() {
    let _obs_guard = crate::services::observability::test_runtime_lock();
    crate::services::observability::reset_for_tests();
    crate::services::observability::init_observability(None);

    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let harness = crate::services::discord::health::TestHealthHarness::new_with_provider(
        crate::services::provider::ProviderKind::Claude,
    )
    .await;
    let cc_channel_id = "1479671298497183835";
    let cc_channel_num = cc_channel_id.parse::<u64>().unwrap();
    let cdx_channel_id = "1479671301387059200";
    let cc_session_key = "mac-mini:AgentDesk-claude-adk-cc";
    let cdx_session_key = "mac-mini:AgentDesk-codex-adk-cdx";

    sqlx::query(
        "INSERT INTO agents (
            id, name, provider, discord_channel_id, discord_channel_alt,
            discord_channel_cc, discord_channel_cdx, created_at, updated_at
         )
         VALUES (
            'project-agentdesk', 'AgentDesk', 'codex', $1, $2, $1, $2,
            NOW(), NOW()
         )",
    )
    .bind(cc_channel_id)
    .bind(cdx_channel_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions
         (session_key, agent_id, provider, status, last_heartbeat, created_at)
         VALUES ($1, 'project-agentdesk', 'claude', 'turn_active', NOW() - INTERVAL '1 minute', NOW())",
    )
    .bind(cc_session_key)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions
         (session_key, agent_id, provider, status, last_heartbeat, created_at)
         VALUES ($1, 'project-agentdesk', 'codex', 'turn_active', NOW(), NOW())",
    )
    .bind(cdx_session_key)
    .execute(&pool)
    .await
    .unwrap();
    harness
        .seed_channel_session(cc_channel_num, "adk-cc", Some("session-1636-turn-cancel"))
        .await;
    let token = harness
        .start_active_turn(cc_channel_num, 16, 36, Some("AgentDesk-claude-adk-cc"))
        .await;
    harness
        .seed_queue(cc_channel_num, &[(2_636, "preserve turn cancel follow-up")])
        .await;

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        Some(harness.registry()),
        pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/turns/{cc_channel_id}/cancel"))
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
    assert_eq!(status, StatusCode::OK, "unexpected body: {body_text}");
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["channel_id"], cc_channel_id);
    assert_eq!(json["agent_id"], "project-agentdesk");
    assert_eq!(json["requested_provider"], "claude");
    assert_eq!(json["exact_channel_match"], true);
    assert_eq!(json["session_key"], cc_session_key);
    assert_eq!(json["tmux_session"], "AgentDesk-claude-adk-cc");
    assert_eq!(json["lifecycle_path"], "mailbox_canonical");
    assert_eq!(json["queued_remaining"], 1);
    assert_eq!(json["queue_preserved"], true);
    assert_eq!(json["inflight_cleared"], false);
    assert_eq!(json["turn_status"], "cancelled");
    assert!(json["turn_completed_at"].as_str().is_some());
    // #1672: response must surface the *observed* pre/post queue depth
    // and the disk-presence transition so operators can tell the
    // difference between "queue preserved" and "queue silently
    // dropped". The legacy contract reported `queue_preserved=true`
    // unconditionally; this test pins the observability fields the
    // queue-api cancel response now carries.
    assert_eq!(json["queued_before"], 1);
    assert_eq!(
        json["queued_remaining"], 1,
        "1 queued intervention must survive cancel — issue #1672"
    );
    assert!(
        json["queue_disk_present_before"].is_boolean(),
        "queue_disk_present_before must be reported"
    );
    assert!(
        json["queue_disk_present_after"].is_boolean(),
        "queue_disk_present_after must be reported"
    );
    assert!(
        token.cancelled.load(std::sync::atomic::Ordering::Relaxed),
        "turn cancel must signal the active turn token"
    );
    let (has_active_turn, queue_depth, session_id) = harness.mailbox_state(cc_channel_num).await;
    assert!(!has_active_turn);
    assert_eq!(queue_depth, 1);
    assert_eq!(session_id, None);

    // #1672 P2: turn cancel must schedule the deferred idle-queue
    // drain so preserved pending_queue items resume without waiting
    // for the next user message. The dispatch-cancel sibling test
    // pins the same invariant for `/dispatches/{id}/cancel`.
    let backlog_after_cancel = harness.deferred_hook_backlog();
    assert!(
        backlog_after_cancel >= 1,
        "turn cancel must schedule the post-cancel queue drain (backlog={backlog_after_cancel}) — issue #1672 P2"
    );

    let event = crate::services::observability::events::recent(10)
        .into_iter()
        .find(|event| event.event_type == "turn_cancelled")
        .expect("turn_cancelled event should be recorded");
    assert_eq!(
        event.channel_id,
        Some(cc_channel_id.parse::<u64>().unwrap())
    );
    assert_eq!(event.provider.as_deref(), Some("claude"));
    assert_eq!(event.payload["reason"], "queue-api cancel_turn (preserve)");
    assert_eq!(event.payload["surface"], "queue_cancel_preserve");
    assert_eq!(event.payload["lifecyclePath"], "mailbox_canonical");
    assert_eq!(event.payload["queueDepth"], 1);
    assert_eq!(event.payload["queuePreserved"], true);
    assert_eq!(event.payload["inflightCleared"], false);
    assert_eq!(event.payload["terminationRecorded"], true);
    assert_eq!(event.payload["session_key"], cc_session_key);
    assert!(event.payload["dispatch_id"].is_null());

    let cc_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM sessions WHERE session_key = $1")
            .bind(cc_session_key)
            .fetch_one(&pool)
            .await
            .unwrap();
    let cdx_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM sessions WHERE session_key = $1")
            .bind(cdx_session_key)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(cc_status, "disconnected");
    assert_eq!(cdx_status, "turn_active");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn agents_pg_empty_list() {
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

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn agents_pg_returns_synced_agents() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, provider, status, xp) VALUES ('a1', 'Agent1', 'claude', 'idle', 0)",
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

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn agents_pg_include_current_thread_channel_id_from_working_session() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, provider, status, xp) VALUES ('a1', 'Agent1', 'codex', 'idle', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions (session_key, agent_id, provider, status, thread_channel_id, last_heartbeat)
         VALUES ($1, 'a1', 'codex', 'turn_active', '1485506232256168011', NOW())",
    )
    .bind("mac-mini:AgentDesk-codex-adk-cdx-t1485506232256168011")
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

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn agents_pg_crud_round_trip() {
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
                .uri("/agents")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"id":"pg-agent","name":"PG Agent","provider":"codex","office_id":"hq","discord_channel_cdx":"1479671301387059200"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_response.status(), StatusCode::CREATED);

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
    assert_eq!(list_json["agents"].as_array().unwrap().len(), 1);
    assert_eq!(
        list_json["agents"][0]["discord_channel_cdx"],
        "1479671301387059200"
    );

    let update_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/agents/pg-agent")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"name_ko":"피지 에이전트","pipeline_config":{"hooks":{"review":{"on_enter":["MyHook"],"on_exit":[]}}}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let update_status = update_response.status();
    let update_body = axum::body::to_bytes(update_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let update_json: serde_json::Value = serde_json::from_slice(&update_body).unwrap();
    assert_eq!(
        update_status,
        StatusCode::OK,
        "unexpected update body: {update_json}"
    );
    assert_eq!(update_json["agent"]["name_ko"], "피지 에이전트");
    assert!(update_json["agent"]["pipeline_config"].is_object());

    let delete_response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/agents/pg-agent")
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
async fn claude_session_id_pg_get_clears_stale_fixed_working_session() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO sessions (
            session_key, provider, status, active_dispatch_id, claude_session_id, last_heartbeat, created_at
         ) VALUES (
            'test:stale-working', 'claude', 'turn_active', 'dispatch-123', 'stale-sid',
            NOW() - INTERVAL '7 hours', NOW() - INTERVAL '7 hours'
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

    let row: (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT status, active_dispatch_id, claude_session_id
         FROM sessions
         WHERE session_key = 'test:stale-working'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.0, "disconnected");
    assert!(row.1.is_none());
    assert!(row.2.is_none());

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn claude_session_id_pg_get_keeps_old_idle_fixed_session() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO sessions (
            session_key, provider, status, claude_session_id, last_heartbeat, created_at
         ) VALUES (
            'test:old-idle', 'claude', 'idle', 'idle-sid',
            NOW() - INTERVAL '7 hours', NOW() - INTERVAL '7 hours'
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

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn claude_session_id_pg_get_returns_null_on_provider_mismatch() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO sessions (
            session_key, provider, status, claude_session_id, last_heartbeat, created_at
         ) VALUES (
            'host:AgentDesk-codex-adk-cdx', 'claude', 'idle', 'claude-sid',
            NOW() - INTERVAL '1 minutes', NOW() - INTERVAL '1 minutes'
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

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn claude_session_id_pg_get_keeps_value_on_provider_match() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO sessions (
            session_key, provider, status, claude_session_id, last_heartbeat, created_at
         ) VALUES (
            'host:AgentDesk-codex-adk-cdx', 'codex', 'idle', 'codex-sid',
            NOW() - INTERVAL '1 minutes', NOW() - INTERVAL '1 minutes'
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

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn agent_detail_pg_http_regression_returns_agent_payload() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO agents (
            id, name, name_ko, provider, status, xp, department,
            discord_channel_id, discord_channel_cdx, created_at, updated_at
         ) VALUES (
            'agent-detail', 'Agent Detail', '상세 에이전트', 'codex', 'idle', 7, 'platform',
            '1485506232256168011', '1485506232256168012',
            TIMESTAMPTZ '2026-05-06 00:00:00+00', TIMESTAMPTZ '2026-05-06 00:00:00+00'
         )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions (
            session_key, agent_id, provider, status, tokens, thread_channel_id, last_heartbeat, created_at
         ) VALUES (
            'host:agent-detail', 'agent-detail', 'codex', 'turn_active', 123,
            '1485506232256168999',
            TIMESTAMPTZ '2026-05-06 00:02:00+00',
            TIMESTAMPTZ '2026-05-06 00:01:00+00'
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
                .uri("/agents/agent-detail")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, StatusCode::OK, "unexpected body: {json}");
    assert_eq!(json["agent"]["id"], "agent-detail");
    assert_eq!(json["agent"]["name"], "Agent Detail");
    assert_eq!(json["agent"]["provider"], "codex");
    assert_eq!(json["agent"]["discord_channel_id"], "1485506232256168011");
    assert_eq!(
        json["agent"]["current_thread_channel_id"],
        "1485506232256168999"
    );
    assert_eq!(json["agent"]["stats_tokens"], 123);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn agent_timeline_pg_http_regression_returns_recent_events() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, provider, status, xp)
         VALUES ('agent-timeline', 'Agent Timeline', 'codex', 'idle', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions (
            session_key, agent_id, provider, status, last_heartbeat, created_at
         ) VALUES (
            'host:agent-timeline', 'agent-timeline', 'codex', 'idle',
            TIMESTAMPTZ '2026-05-06 00:01:30+00',
            TIMESTAMPTZ '2026-05-06 00:01:00+00'
         )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, created_at, updated_at
         ) VALUES (
            'card-agent-timeline', 'Timeline Card', 'in_progress', 'medium', 'agent-timeline',
            TIMESTAMPTZ '2026-05-06 00:02:00+00',
            TIMESTAMPTZ '2026-05-06 00:02:30+00'
         )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
         ) VALUES (
            'dispatch-agent-timeline', 'card-agent-timeline', 'agent-timeline',
            'implementation', 'completed', 'Timeline Dispatch',
            TIMESTAMPTZ '2026-05-06 00:03:00+00',
            TIMESTAMPTZ '2026-05-06 00:04:00+00'
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
                .uri("/agents/agent-timeline/timeline?limit=2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, StatusCode::OK, "unexpected body: {json}");
    let events = json["events"].as_array().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["id"], "dispatch-agent-timeline");
    assert_eq!(events[0]["source"], "dispatch");
    assert_eq!(events[0]["type"], "implementation");
    assert_eq!(events[0]["title"], "Timeline Dispatch");
    assert_eq!(events[0]["status"], "completed");
    assert_eq!(events[0]["duration_ms"], 60000);
    assert_eq!(events[1]["id"], "card-agent-timeline");
    assert_eq!(events[1]["source"], "kanban");
    assert_eq!(events[1]["type"], "card");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn get_agent_pg_not_found() {
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

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn sessions_pg_empty_list() {
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

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn messages_list_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, name_ko, avatar_emoji, discord_channel_id)
         VALUES ($1, $2, $3, $4, '111')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind("agent-pg-message-sender")
    .bind("Sender PG")
    .bind("보내는 에이전트")
    .bind("🤖")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, name_ko, avatar_emoji, discord_channel_id)
         VALUES ($1, $2, $3, $4, '222')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind("agent-pg-message-receiver")
    .bind("Receiver PG")
    .bind("받는 에이전트")
    .bind("🛰️")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO messages (
            sender_type, sender_id, receiver_type, receiver_id, content, message_type, created_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, NOW()
         )",
    )
    .bind("agent")
    .bind("agent-pg-message-sender")
    .bind("agent")
    .bind("agent-pg-message-receiver")
    .bind("PG only message")
    .bind("chat")
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
                .uri("/messages?receiverId=agent-pg-message-receiver")
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
        "messages_list_pg_only_without_sqlite_mirror status={} body={}",
        status,
        body_text
    );

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["messages"].as_array().unwrap().len(), 1);
    assert_eq!(json["messages"][0]["content"], json!("PG only message"));
    assert_eq!(
        json["messages"][0]["sender_name_ko"],
        json!("보내는 에이전트")
    );

    let sqlite_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(sqlite_count, 0, "sqlite mirror should stay empty");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn messages_create_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, name_ko, avatar_emoji, discord_channel_id)
         VALUES ($1, $2, $3, $4, '111')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind("agent-pg-message-create-sender")
    .bind("Sender Create PG")
    .bind("생성 발신자")
    .bind("🤖")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, name_ko, avatar_emoji, discord_channel_id)
         VALUES ($1, $2, $3, $4, '222')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind("agent-pg-message-create-receiver")
    .bind("Receiver Create PG")
    .bind("생성 수신자")
    .bind("🛰️")
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
                .uri("/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{
                        "sender_type":"agent",
                        "sender_id":"agent-pg-message-create-sender",
                        "receiver_type":"agent",
                        "receiver_id":"agent-pg-message-create-receiver",
                        "content":"created through pg path",
                        "message_type":"chat"
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
        StatusCode::CREATED,
        "messages_create_pg_only_without_sqlite_mirror status={} body={}",
        status,
        body_text
    );

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["content"], json!("created through pg path"));
    assert_eq!(json["receiver_name_ko"], json!("생성 수신자"));

    let pg_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM messages WHERE content = $1")
            .bind("created through pg path")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(pg_count, 1);

    let sqlite_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE content = 'created through pg path'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sqlite_count, 0, "sqlite mirror should stay empty");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn agent_setup_pg_dry_run_reports_plan_without_mutation() {
    let _env_lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
    let config_path = crate::runtime_layout::config_file_path(runtime_root.path());
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    crate::config::save_to_path(&config_path, &crate::config::Config::default()).unwrap();
    let prompt_template = crate::runtime_layout::shared_prompt_path(runtime_root.path());
    fs::create_dir_all(prompt_template.parent().unwrap()).unwrap();
    fs::write(&prompt_template, "shared prompt\n").unwrap();
    write_test_skill(runtime_root.path(), "memory-read", "Memory read");

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
                .uri("/agents/setup")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "agent_id": "setup-agent",
                        "channel_id": "1473922824350601297",
                        "provider": "codex",
                        "prompt_template_path": "config/agents/_shared.prompt.md",
                        "skills": ["memory-read"],
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
    assert_eq!(json["ok"], true);
    assert_eq!(json["dry_run"], true);
    assert!(json["created"].as_array().unwrap().is_empty());
    assert!(json["errors"].as_array().unwrap().is_empty());
    assert!(
        json["planned"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["step"] == "agentdesk_yaml" && entry["status"] == "planned")
    );
    assert!(
        json["planned"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["step"] == "skill_mapping" && entry["status"] == "planned")
    );

    let config = crate::config::load_from_path(&config_path).unwrap();
    assert!(config.agents.iter().all(|agent| agent.id != "setup-agent"));
    assert!(
        !runtime_root
            .path()
            .join("config/agents/setup-agent/IDENTITY.md")
            .exists()
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agents WHERE id = 'setup-agent'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert!(
        !crate::runtime_layout::managed_skills_manifest_path(runtime_root.path()).exists(),
        "dry_run must not create skills manifest"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn agent_setup_pg_creates_resources_and_retry_is_idempotent() {
    let _env_lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
    let config_path = crate::runtime_layout::config_file_path(runtime_root.path());
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    crate::config::save_to_path(&config_path, &crate::config::Config::default()).unwrap();
    let prompt_template = crate::runtime_layout::shared_prompt_path(runtime_root.path());
    fs::create_dir_all(prompt_template.parent().unwrap()).unwrap();
    fs::write(&prompt_template, "shared prompt\n").unwrap();
    write_test_skill(runtime_root.path(), "memory-read", "Memory read");

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
    let request_body = json!({
        "agent_id": "setup-agent",
        "channel_id": "1473922824350601297",
        "provider": "codex",
        "prompt_template_path": "config/agents/_shared.prompt.md",
        "skills": ["memory-read"]
    });

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents/setup")
                .header("content-type", "application/json")
                .body(Body::from(request_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert!(
        json["created"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["step"] == "agentdesk_yaml")
    );
    assert!(json["transaction"]["audit_log"].as_str().is_some());

    let config = crate::config::load_from_path(&config_path).unwrap();
    let agent = config
        .agents
        .iter()
        .find(|agent| agent.id == "setup-agent")
        .expect("setup agent in config");
    assert_eq!(agent.provider, "codex");
    let codex_channel = agent.channels.codex.as_ref().expect("codex channel");
    assert_eq!(
        codex_channel.channel_id().as_deref(),
        Some("1473922824350601297")
    );
    assert_eq!(
        fs::read_to_string(
            runtime_root
                .path()
                .join("config/agents/setup-agent/IDENTITY.md")
        )
        .unwrap(),
        "shared prompt\n"
    );
    assert!(runtime_root.path().join("workspaces/setup-agent").is_dir());
    let db_channel: Option<String> =
        sqlx::query_scalar("SELECT discord_channel_cdx FROM agents WHERE id = 'setup-agent'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(db_channel.as_deref(), Some("1473922824350601297"));
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(crate::runtime_layout::managed_skills_manifest_path(
            runtime_root.path(),
        ))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["skills"]["memory-read"]["providers"][0], "codex");
    assert_eq!(
        manifest["skills"]["memory-read"]["workspaces"][0],
        "setup-agent"
    );

    let retry = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents/setup")
                .header("content-type", "application/json")
                .body(Body::from(request_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(retry.status(), StatusCode::OK);
    let body = axum::body::to_bytes(retry.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["created"].as_array().unwrap().is_empty());
    assert!(
        json["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["step"] == "db_seed")
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn agent_setup_pg_rolls_back_when_mid_step_fails() {
    let _env_lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
    let _fail = EnvVarGuard::set("AGENTDESK_TEST_AGENT_SETUP_FAIL_AFTER", "prompt_file");
    let config_path = crate::runtime_layout::config_file_path(runtime_root.path());
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    crate::config::save_to_path(&config_path, &crate::config::Config::default()).unwrap();
    let prompt_template = crate::runtime_layout::shared_prompt_path(runtime_root.path());
    fs::create_dir_all(prompt_template.parent().unwrap()).unwrap();
    fs::write(&prompt_template, "shared prompt\n").unwrap();

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
                .uri("/agents/setup")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "agent_id": "setup-agent",
                        "channel_id": "1473922824350601297",
                        "provider": "codex",
                        "prompt_template_path": "config/agents/_shared.prompt.md"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], false);
    assert!(
        json["rolled_back"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["step"] == "prompt_file")
    );
    assert!(
        json["rolled_back"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["step"] == "agentdesk_yaml")
    );

    let config = crate::config::load_from_path(&config_path).unwrap();
    assert!(config.agents.iter().all(|agent| agent.id != "setup-agent"));
    assert!(
        !runtime_root
            .path()
            .join("config/agents/setup-agent/IDENTITY.md")
            .exists()
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agents WHERE id = 'setup-agent'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert!(runtime_root.path().join("config/.audit").is_dir());

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn agent_pg_patch_updates_metadata_and_prompt_content() {
    let _env_lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
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
    seed_setup_agent_for_management_test_pg(
        app.clone(),
        runtime_root.path(),
        "managed-agent",
        "1473922824350601297",
    )
    .await;

    let response = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/agents/managed-agent")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "name": "Managed Agent",
                        "cli_provider": "codex",
                        "sprite_number": 42,
                        "personality": "operational prompt summary",
                        "prompt_content": "updated prompt\n",
                        "auto_commit": false
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
    assert_eq!(json["prompt"]["changed"], true);
    assert_eq!(
        fs::read_to_string(
            runtime_root
                .path()
                .join("config/agents/managed-agent/IDENTITY.md")
        )
        .unwrap(),
        "updated prompt\n"
    );
    let row: (String, Option<i64>, Option<String>) = sqlx::query_as(
        "SELECT name, sprite_number, system_prompt FROM agents WHERE id = 'managed-agent'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.0, "Managed Agent");
    assert_eq!(row.1, Some(42));
    assert_eq!(row.2.as_deref(), Some("operational prompt summary"));

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn agent_pg_archive_and_unarchive_record_state_and_restore_config() {
    let _env_lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
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
    seed_setup_agent_for_management_test_pg(
        app.clone(),
        runtime_root.path(),
        "managed-agent",
        "1473922824350601297",
    )
    .await;

    let archived = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents/managed-agent/archive")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "reason": "test archive",
                        "discord_action": "none"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(archived.status(), StatusCode::OK);
    let body = axum::body::to_bytes(archived.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["archive_state"], "archived");
    let config = crate::config::load_from_path(&crate::runtime_layout::config_file_path(
        runtime_root.path(),
    ))
    .unwrap();
    assert!(
        config
            .agents
            .iter()
            .all(|agent| agent.id != "managed-agent")
    );
    let archive_state: String =
        sqlx::query_scalar("SELECT state FROM agent_archive WHERE agent_id = 'managed-agent'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(archive_state, "archived");

    let unarchived = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents/managed-agent/unarchive")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unarchived.status(), StatusCode::OK);
    let config = crate::config::load_from_path(&crate::runtime_layout::config_file_path(
        runtime_root.path(),
    ))
    .unwrap();
    assert!(
        config
            .agents
            .iter()
            .any(|agent| agent.id == "managed-agent")
    );
    let archive_state: String =
        sqlx::query_scalar("SELECT state FROM agent_archive WHERE agent_id = 'managed-agent'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(archive_state, "unarchived");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn agent_pg_duplicate_reuses_setup_and_copies_prompt() {
    let _env_lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
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
    seed_setup_agent_for_management_test_pg(
        app.clone(),
        runtime_root.path(),
        "managed-agent",
        "1473922824350601297",
    )
    .await;
    fs::write(
        runtime_root
            .path()
            .join("config/agents/managed-agent/IDENTITY.md"),
        "source identity prompt\n",
    )
    .unwrap();

    let duplicated = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents/managed-agent/duplicate")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "new_agent_id": "managed-copy",
                        "channel_id": "1473922824350601298",
                        "name": "Managed Copy"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(duplicated.status(), StatusCode::CREATED);
    assert_eq!(
        fs::read_to_string(
            runtime_root
                .path()
                .join("config/agents/managed-copy/IDENTITY.md")
        )
        .unwrap(),
        "source identity prompt\n"
    );
    let copied_name: String =
        sqlx::query_scalar("SELECT name FROM agents WHERE id = 'managed-copy'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(copied_name, "Managed Copy");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn agent_pg_archive_rejects_when_active_turn_present() {
    let _env_lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
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
    seed_setup_agent_for_management_test_pg(
        app.clone(),
        runtime_root.path(),
        "managed-agent",
        "1473922824350601297",
    )
    .await;

    // Seed an active turn for the managed-agent (status='working').
    sqlx::query(
        "INSERT INTO sessions (session_key, agent_id, provider, status, active_dispatch_id, last_heartbeat)
         VALUES ('sess-active', 'managed-agent', 'codex', 'turn_active', 'dispatch-1', NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();

    let archived = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents/managed-agent/archive")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"reason": "blocked", "discord_action": "none"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(archived.status(), StatusCode::CONFLICT);
    let body = axum::body::to_bytes(archived.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("active turn"),
        "expected 'active turn' error, got: {json:?}"
    );

    // agent_archive row should NOT be written when rejected.
    let archive_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_archive WHERE agent_id = 'managed-agent'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(archive_count, 0);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn agent_pg_duplicate_ignores_sensitive_fields_from_body() {
    let _env_lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
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
    seed_setup_agent_for_management_test_pg(
        app.clone(),
        runtime_root.path(),
        "managed-agent",
        "1473922824350601297",
    )
    .await;
    fs::write(
        runtime_root
            .path()
            .join("config/agents/managed-agent/IDENTITY.md"),
        "source identity prompt\n",
    )
    .unwrap();

    let source_channel = "1473922824350601297";
    let new_channel = "1473922824350601299";

    // Send sensitive fields that must be ignored (not in the allowlist struct):
    // - `id` / `agent_id`: must not override new_agent_id
    // - `discord_channel_id` (raw DB col): must not leak source channel
    // - `token`, `api_key`, `system_prompt`: must not be carried over
    let duplicated = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents/managed-agent/duplicate")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "new_agent_id": "managed-copy-2",
                        "channel_id": new_channel,
                        "name": "Managed Copy 2",
                        "id": "attacker-override",
                        "agent_id": "attacker-override",
                        "discord_channel_id": source_channel,
                        "token": "secret-token",
                        "api_key": "secret-key",
                        "system_prompt": "leaked personality"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(duplicated.status(), StatusCode::CREATED);

    // Resulting agent row must use new_agent_id + new channel (via setup's provider→column mapping),
    // NOT the source channel, and NOT any body-supplied sensitive fields.
    let (copied_id, channel_primary, channel_alt, channel_cc, channel_cdx, system_prompt): (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT id, discord_channel_id, discord_channel_alt, discord_channel_cc,
                discord_channel_cdx, system_prompt
         FROM agents WHERE id = 'managed-copy-2'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(copied_id, "managed-copy-2");
    let all_channels = [&channel_primary, &channel_alt, &channel_cc, &channel_cdx];
    assert!(
        all_channels
            .iter()
            .any(|c| c.as_deref() == Some(new_channel)),
        "at least one channel column must be the new_channel (got {all_channels:?})"
    );
    assert!(
        all_channels
            .iter()
            .all(|c| c.as_deref() != Some(source_channel)),
        "source channel must not be reused in any column (got {all_channels:?})"
    );
    assert!(
        system_prompt.as_deref() != Some("leaked personality"),
        "system_prompt from body must NOT be written during duplicate (got {system_prompt:?})"
    );

    // Attacker-override id must not exist as an agent row.
    let attacker_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agents WHERE id = 'attacker-override'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(attacker_rows, 0);

    pool.close().await;
    pg_db.drop().await;
}

// #1067: skill promotion integration test — watch-agent-turn.
#[tokio::test]
async fn sessions_tmux_output_pg_http_route_returns_shape_for_seeded_session() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let tmux_name = format!("AgentDesk-codex-1067-http-{}", std::process::id());
    let session_key = format!("mac-mini:{tmux_name}");

    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id, created_at, updated_at)
         VALUES ('agent-1067-http', 'Agent 1067', 'codex', '123456789012345678', NOW(), NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions
         (session_key, agent_id, provider, status, last_heartbeat, created_at)
         VALUES ($1, 'agent-1067-http', 'codex', 'turn_active', NOW(), NOW())",
    )
    .bind(&session_key)
    .execute(&pool)
    .await
    .unwrap();
    let session_id: i64 = sqlx::query_scalar("SELECT id FROM sessions WHERE session_key = $1")
        .bind(&session_key)
        .fetch_one(&pool)
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
                .method("GET")
                .uri(format!("/sessions/{session_id}/tmux-output?lines=25"))
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
    assert_eq!(json["session_id"], session_id);
    assert_eq!(json["session_key"], session_key);
    assert_eq!(json["tmux_name"], tmux_name);
    assert_eq!(json["agent_id"], "agent-1067-http");
    assert_eq!(json["provider"], "codex");
    assert_eq!(json["status"], "turn_active");
    assert_eq!(json["lines_requested"], 25);
    assert_eq!(json["lines_effective"], 25);
    // tmux session was never created, so capture returns empty and tmux_alive=false.
    assert_eq!(json["tmux_alive"], false);
    assert_eq!(json["recent_output"], "");
    assert!(json["captured_at_ms"].as_i64().unwrap() > 0);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn sessions_tmux_output_http_route_returns_404_for_unknown_session() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/sessions/987654321/tmux-output")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["session_id"], 987654321);
    assert!(
        json["error"]
            .as_str()
            .map(|s| s.contains("not found"))
            .unwrap_or(false)
    );
}

#[tokio::test]
async fn skills_catalog_pg_filters_stale_entries_and_exposes_disk_presence() {
    let _env_lock = env_lock();
    let home = tempfile::tempdir().unwrap();
    let runtime_root = home.path().join(".adk").join("release");
    write_test_skill(&runtime_root, "live-skill", "Live skill description");
    let _home_env = EnvVarGuard::set_path("HOME", home.path());
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", &runtime_root);

    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let stale_path = home
        .path()
        .join("missing")
        .join("stale-skill")
        .join("SKILL.md")
        .display()
        .to_string();
    sqlx::query(
        "INSERT INTO skills (id, name, description, source_path, updated_at)
         VALUES ($1, $2, $3, $4, NOW())",
    )
    .bind("stale-skill")
    .bind("stale-skill")
    .bind("Stale skill description")
    .bind(&stale_path)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO skill_usage (skill_id, agent_id, session_key, used_at)
         VALUES ($1, $2, $3, NOW())",
    )
    .bind("stale-skill")
    .bind("agent-stale")
    .bind("session-stale")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO skill_usage (skill_id, agent_id, session_key, used_at)
         VALUES ($1, $2, $3, NOW())",
    )
    .bind("live-skill")
    .bind("agent-live")
    .bind("session-live")
    .execute(&pool)
    .await
    .unwrap();

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
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/skills/catalog?include_stale=true")
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
    let catalog = json["catalog"]
        .as_array()
        .expect("catalog response must include a catalog array");
    let live = catalog
        .iter()
        .find(|entry| entry["name"] == "live-skill")
        .expect("live skill must be present when include_stale=true");
    assert_eq!(live["disk_present"], true);
    let stale = catalog
        .iter()
        .find(|entry| entry["name"] == "stale-skill")
        .expect("stale skill must be present when include_stale=true");
    assert_eq!(stale["disk_present"], false);

    let filtered = app
        .oneshot(
            Request::builder()
                .uri("/api/skills/catalog")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(filtered.status(), StatusCode::OK);
    let filtered_body = axum::body::to_bytes(filtered.into_body(), usize::MAX)
        .await
        .unwrap();
    let filtered_json: serde_json::Value = serde_json::from_slice(&filtered_body).unwrap();
    let filtered_catalog = filtered_json["catalog"]
        .as_array()
        .expect("filtered catalog response must include a catalog array");
    assert!(
        filtered_catalog
            .iter()
            .any(|entry| entry["name"] == "live-skill"),
        "default catalog response must keep live skills"
    );
    assert!(
        filtered_catalog
            .iter()
            .all(|entry| entry["name"] != "stale-skill"),
        "default catalog response must filter stale skills"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn skills_prune_dry_run_pg_previews_and_delete_preserves_usage() {
    let _env_lock = env_lock();
    let home = tempfile::tempdir().unwrap();
    let runtime_root = home.path().join(".adk").join("release");
    write_test_skill(&runtime_root, "live-skill", "Live skill description");
    let _home_env = EnvVarGuard::set_path("HOME", home.path());
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", &runtime_root);

    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let stale_path = home
        .path()
        .join("missing")
        .join("stale-skill")
        .join("SKILL.md")
        .display()
        .to_string();
    sqlx::query(
        "INSERT INTO skills (id, name, description, source_path, updated_at)
         VALUES ($1, $2, $3, $4, NOW())",
    )
    .bind("stale-skill")
    .bind("stale-skill")
    .bind("Stale skill description")
    .bind(&stale_path)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO skill_usage (skill_id, agent_id, session_key, used_at)
         VALUES ($1, $2, $3, NOW())",
    )
    .bind("stale-skill")
    .bind("agent-stale")
    .bind("session-stale")
    .execute(&pool)
    .await
    .unwrap();

    let app = axum::Router::new().nest(
        "/api",
        test_api_router_with_pg(
            db.clone(),
            engine,
            crate::config::Config::default(),
            None,
            pool.clone(),
        ),
    );
    let dry_run = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/skills/prune?dry_run=true")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(dry_run.status(), StatusCode::OK);
    let dry_run_body = axum::body::to_bytes(dry_run.into_body(), usize::MAX)
        .await
        .unwrap();
    let dry_run_json: serde_json::Value = serde_json::from_slice(&dry_run_body).unwrap();
    assert_eq!(dry_run_json["dry_run"], true);
    assert_eq!(dry_run_json["soft_deleted_from_skills"], 0);
    assert!(
        dry_run_json["stale_skill_ids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry == "stale-skill"),
        "dry-run must preview stale skill ids"
    );

    let stale_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM skills WHERE id = 'stale-skill'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stale_count, 1, "dry-run must not delete skills rows");

    let prune = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/skills/prune")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(prune.status(), StatusCode::OK);
    let prune_body = axum::body::to_bytes(prune.into_body(), usize::MAX)
        .await
        .unwrap();
    let prune_json: serde_json::Value = serde_json::from_slice(&prune_body).unwrap();
    assert_eq!(prune_json["soft_deleted_from_skills"], 1);
    assert_eq!(prune_json["skill_usage_policy"], "preserved");

    let stale_live_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM skills WHERE id = 'stale-skill' AND deleted_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        stale_live_count, 0,
        "prune must soft-delete stale skill metadata"
    );

    let usage_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM skill_usage WHERE skill_id = 'stale-skill'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(usage_count, 1, "prune must preserve historical skill usage");

    let filtered = app
        .oneshot(
            Request::builder()
                .uri("/api/skills/catalog")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(filtered.status(), StatusCode::OK);
    let filtered_body = axum::body::to_bytes(filtered.into_body(), usize::MAX)
        .await
        .unwrap();
    let filtered_json: serde_json::Value = serde_json::from_slice(&filtered_body).unwrap();
    let filtered_catalog = filtered_json["catalog"]
        .as_array()
        .expect("catalog response must include a catalog array");
    assert!(
        filtered_catalog
            .iter()
            .all(|entry| entry["name"] != "stale-skill"),
        "default catalog response must hide pruned stale skills"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn cron_jobs_include_github_issue_card_sync_job_pg() {
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

    let response = app
        .oneshot(
            Request::builder()
                .uri("/cron-jobs")
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
    let jobs = json["jobs"]
        .as_array()
        .expect("cron jobs response must include jobs array");
    let github_sync_job = jobs
        .iter()
        .find(|job| job["id"] == "github_issue_card_sync")
        .expect("cron jobs must expose github issue card sync");
    assert_eq!(github_sync_job["schedule"]["kind"], "every");
    assert_eq!(github_sync_job["schedule"]["everyMs"], 300000);
    assert_eq!(github_sync_job["enabled"], true);

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn maintenance_jobs_pg_endpoint_lists_seed_job() -> Result<(), Box<dyn std::error::Error>> {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    sqlx::query(
        "INSERT INTO kv_meta (key, value) VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind("maintenance_job:maintenance.noop_heartbeat:next_run_ms")
    .bind("1700000000000")
    .execute(&pool)
    .await?;
    let engine = test_engine_with_pg(&db, pool.clone());
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
                .uri("/maintenance/jobs")
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    let jobs = json["jobs"].as_array().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::Other,
            "maintenance response must include jobs array",
        )
    })?;
    let noop_job = jobs
        .iter()
        .find(|job| job["id"] == "maintenance.noop_heartbeat")
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                "maintenance response must include noop heartbeat job",
            )
        })?;

    assert_eq!(noop_job["schedule"]["kind"], "every");
    assert_eq!(noop_job["schedule"]["everyMs"], 900000);
    assert_eq!(
        noop_job["state"]["nextRunAtMs"],
        json!(1_700_000_000_000i64)
    );
    let quality_job = jobs
        .iter()
        .find(|job| job["id"] == "agent_quality_rollup")
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                "maintenance response must include agent quality rollup job",
            )
        })?;
    assert_eq!(quality_job["schedule"]["kind"], "every");
    assert_eq!(quality_job["schedule"]["everyMs"], 3_600_000);
    assert_eq!(quality_job["enabled"], true);

    let cron_response = app
        .oneshot(Request::builder().uri("/cron-jobs").body(Body::empty())?)
        .await?;
    assert_eq!(cron_response.status(), StatusCode::OK);
    let cron_body = axum::body::to_bytes(cron_response.into_body(), usize::MAX).await?;
    let cron_json: serde_json::Value = serde_json::from_slice(&cron_body)?;
    let cron_jobs = cron_json["jobs"].as_array().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::Other,
            "cron response must include jobs array",
        )
    })?;
    let cron_maintenance_job = cron_jobs
        .iter()
        .find(|job| job["id"] == "maintenance.noop_heartbeat")
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                "cron response must include noop maintenance job",
            )
        })?;
    assert_eq!(cron_maintenance_job["state"]["status"], "active");

    pool.close().await;
    pg_db.drop().await;
    Ok(())
}

#[tokio::test]
async fn cron_api_response_includes_maintenance_section() -> Result<(), Box<dyn std::error::Error>>
{
    // #1091: /api/cron-jobs must include dynamically-registered maintenance
    // jobs, tagged `source: "maintenance"` alongside the existing cron tiers
    // which are tagged `source: "cron"`.
    use crate::services::maintenance::{register_maintenance_job, test_serialization_lock};
    use std::time::Duration;

    // Serialize with any parallel services::maintenance::tests::* test that
    // clears the process-global registry mid-run.
    let _maintenance_lock = test_serialization_lock();

    register_maintenance_job("test.cron_api_section", Duration::from_secs(300), || {
        Box::pin(async { Ok(()) })
    });

    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(Request::builder().uri("/cron-jobs").body(Body::empty())?)
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    let jobs = json["jobs"].as_array().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::Other,
            "cron response must include jobs array",
        )
    })?;

    // Every job must have a `source` tag, and the set must contain both
    // cron tiers and our registered maintenance job.
    let mut saw_cron = false;
    let mut saw_maintenance = false;
    let mut saw_target = false;
    for job in jobs {
        let source = job["source"].as_str().unwrap_or("");
        assert!(
            !source.is_empty(),
            "every cron-jobs entry must carry a non-empty `source` tag; got {job:?}"
        );
        match source {
            "cron" => saw_cron = true,
            "maintenance" => saw_maintenance = true,
            other => panic!("unexpected source {other:?}"),
        }
        if job["id"] == "maintenance:test.cron_api_section" {
            saw_target = true;
            assert_eq!(job["source"], "maintenance");
            assert_eq!(job["schedule"]["everyMs"], 300_000);
            assert_eq!(job["enabled"], true);
        }
    }
    assert!(
        saw_cron,
        "response must include at least one `cron` source job"
    );
    assert!(
        saw_maintenance,
        "response must include at least one `maintenance` source job"
    );
    assert!(
        saw_target,
        "response must include the registered test.cron_api_section maintenance job"
    );

    Ok(())
}

#[tokio::test]
async fn agent_quality_api_returns_daily_rollup() -> Result<(), Box<dyn std::error::Error>> {
    let db = test_db();
    seed_test_agents(&db);
    {
        let conn = db.lock()?;
        conn.execute(
            "INSERT INTO agent_quality_daily (
                agent_id,
                day,
                provider,
                channel_id,
                turn_success_count,
                turn_error_count,
                review_pass_count,
                review_fail_count,
                turn_sample_size,
                review_sample_size,
                sample_size,
                turn_success_rate,
                review_pass_rate,
                turn_success_count_7d,
                turn_error_count_7d,
                review_pass_count_7d,
                review_fail_count_7d,
                turn_sample_size_7d,
                review_sample_size_7d,
                sample_size_7d,
                turn_success_rate_7d,
                review_pass_rate_7d,
                measurement_unavailable_7d,
                turn_success_count_30d,
                turn_error_count_30d,
                review_pass_count_30d,
                review_fail_count_30d,
                turn_sample_size_30d,
                review_sample_size_30d,
                sample_size_30d,
                turn_success_rate_30d,
                review_pass_rate_30d,
                measurement_unavailable_30d
             ) VALUES (
                'agent-1',
                date('now'),
                'codex',
                '555',
                4,
                1,
                3,
                1,
                5,
                4,
                9,
                0.8,
                0.75,
                4,
                1,
                3,
                1,
                5,
                4,
                9,
                0.8,
                0.75,
                0,
                20,
                5,
                12,
                4,
                25,
                16,
                41,
                0.8,
                0.75,
                0
             )",
            [],
        )?;
    }
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/agents/agent-1/quality")
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(json["agentId"], "agent-1");
    assert_eq!(json["latest"]["rolling7d"]["sampleSize"], 9);
    assert_eq!(json["latest"]["rolling7d"]["measurementUnavailable"], false);
    assert_eq!(json["latest"]["rolling7d"]["turnSuccessRate"], json!(0.8));
    assert_eq!(json["daily"].as_array().map(Vec::len), Some(1));
    // #1102: DoD-mandated current / trend_7d / trend_30d fields.
    assert_eq!(json["current"]["agentId"], "agent-1");
    assert_eq!(json["trend7d"].as_array().map(Vec::len), Some(1));
    assert_eq!(json["trend30d"].as_array().map(Vec::len), Some(1));
    assert_eq!(json["fallbackFromEvents"], false);

    let ranking_response = app
        .oneshot(
            Request::builder()
                .uri("/agents/quality/ranking")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(ranking_response.status(), StatusCode::OK);
    let ranking_body = axum::body::to_bytes(ranking_response.into_body(), usize::MAX).await?;
    let ranking_json: serde_json::Value = serde_json::from_slice(&ranking_body)?;
    assert_eq!(ranking_json["agents"][0]["agentId"], "agent-1");
    assert_eq!(ranking_json["metric"], "turn_success_rate");
    assert_eq!(ranking_json["window"], "7d");
    assert_eq!(ranking_json["minSampleSize"], 5);
    // metric_value for rolling_7d turn_success_rate on the seeded row is 0.8.
    assert_eq!(ranking_json["agents"][0]["metricValue"], json!(0.8));

    Ok(())
}

/// #1102 DoD: ranking excludes agents whose rolling_7d sample_size < 5 so
/// the client doesn't have to filter client-side.
#[tokio::test]
async fn agent_quality_api_ranking_excludes_low_sample_size()
-> Result<(), Box<dyn std::error::Error>> {
    let db = test_db();
    seed_test_agents(&db);
    {
        let conn = db.lock()?;
        // agent-1: sample_size_7d = 2 (below threshold, measurement_unavailable=1)
        conn.execute(
            "INSERT INTO agent_quality_daily (
                agent_id, day, provider, channel_id,
                turn_success_count, turn_error_count, review_pass_count, review_fail_count,
                turn_sample_size, review_sample_size, sample_size,
                turn_success_rate, review_pass_rate,
                turn_success_count_7d, turn_error_count_7d, review_pass_count_7d, review_fail_count_7d,
                turn_sample_size_7d, review_sample_size_7d, sample_size_7d,
                turn_success_rate_7d, review_pass_rate_7d, measurement_unavailable_7d,
                turn_success_count_30d, turn_error_count_30d, review_pass_count_30d, review_fail_count_30d,
                turn_sample_size_30d, review_sample_size_30d, sample_size_30d,
                turn_success_rate_30d, review_pass_rate_30d, measurement_unavailable_30d
             ) VALUES (
                'agent-1', date('now'), 'codex', '555',
                1, 0, 1, 0,
                1, 1, 2,
                1.0, 1.0,
                1, 0, 1, 0,
                1, 1, 2,
                1.0, 1.0, 1,
                1, 0, 1, 0,
                1, 1, 2,
                1.0, 1.0, 1
             )",
            [],
        )?;
        // ag1: sample_size_7d = 10 (well above threshold)
        conn.execute(
            "INSERT INTO agent_quality_daily (
                agent_id, day, provider, channel_id,
                turn_success_count, turn_error_count, review_pass_count, review_fail_count,
                turn_sample_size, review_sample_size, sample_size,
                turn_success_rate, review_pass_rate,
                turn_success_count_7d, turn_error_count_7d, review_pass_count_7d, review_fail_count_7d,
                turn_sample_size_7d, review_sample_size_7d, sample_size_7d,
                turn_success_rate_7d, review_pass_rate_7d, measurement_unavailable_7d,
                turn_success_count_30d, turn_error_count_30d, review_pass_count_30d, review_fail_count_30d,
                turn_sample_size_30d, review_sample_size_30d, sample_size_30d,
                turn_success_rate_30d, review_pass_rate_30d, measurement_unavailable_30d
             ) VALUES (
                'ag1', date('now'), 'codex', '333',
                6, 1, 3, 0,
                7, 3, 10,
                0.857, 1.0,
                6, 1, 3, 0,
                7, 3, 10,
                0.857, 1.0, 0,
                6, 1, 3, 0,
                7, 3, 10,
                0.857, 1.0, 0
             )",
            [],
        )?;
    }
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/agents/quality/ranking?metric=turn_success_rate&window=7d")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    let agents = json["agents"].as_array().expect("agents array");
    assert_eq!(agents.len(), 1, "only ag1 (sample_size_7d=10) should pass");
    assert_eq!(agents[0]["agentId"], "ag1");
    assert_eq!(agents[0]["rank"], 1);
    Ok(())
}

/// #1102 DoD: when `agent_quality_daily` has no rows, the per-agent summary
/// falls back to an on-the-fly mini-rollup over `agent_quality_event`.
#[tokio::test]
async fn agent_quality_api_event_fallback_mini_rollup() -> Result<(), Box<dyn std::error::Error>> {
    let db = test_db();
    seed_test_agents(&db);
    {
        let conn = db.lock()?;
        // Seed 6 events (enough to exceed QUALITY_SAMPLE_GUARD=5 → window
        // should be measurable).
        for (i, etype) in [
            "turn_complete",
            "turn_complete",
            "turn_complete",
            "turn_complete",
            "turn_error",
            "review_pass",
        ]
        .iter()
        .enumerate()
        {
            conn.execute(
                "INSERT INTO agent_quality_event (
                    source_event_id, correlation_id, agent_id, provider, channel_id,
                    card_id, dispatch_id, event_type, payload_json, created_at
                 ) VALUES (?1, NULL, 'agent-1', 'codex', '555', NULL, NULL, ?2, '{}', datetime('now'))",
                sqlite_params![format!("evt-{i}"), etype],
            )?;
        }
    }
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/agents/agent-1/quality")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(json["agentId"], "agent-1");
    assert_eq!(
        json["fallbackFromEvents"], true,
        "fallbackFromEvents must be true when daily is empty"
    );
    let daily_len = json["daily"].as_array().map(Vec::len).unwrap_or(0);
    assert!(daily_len >= 1, "expected synthesized daily rows");
    let current_sample = json["current"]["sampleSize"].as_i64().unwrap_or(-1);
    assert_eq!(current_sample, 6, "6 events synthesized for today");
    Ok(())
}

/// #1102 DoD: docs catalog exposes both new quality endpoints.
#[tokio::test]
async fn agent_quality_api_docs_catalog_includes_endpoints()
-> Result<(), Box<dyn std::error::Error>> {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/agents?format=flat")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("/api/agents/{id}/quality"),
        "docs must list /api/agents/{{id}}/quality, got: {text}"
    );
    assert!(
        text.contains("/api/agents/quality/ranking"),
        "docs must list /api/agents/quality/ranking, got: {text}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn smart_activate_pg_dispatches_multiple_groups() {
    crate::pipeline::ensure_loaded();

    let (repo, _repo_guard) = setup_test_repo();
    let config_dir = write_repo_mapping_config(&[("test-repo", repo.path())]);
    let _config_guard = EnvVarGuard::set_path(
        "AGENTDESK_CONFIG",
        &config_dir.path().join("agentdesk.yaml"),
    );

    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let _card_ids = seed_parallel_test_cards_pg(&pool).await;

    let app = test_api_router_with_pg(
        db.clone(),
        engine.clone(),
        crate::config::Config::default(),
        None,
        pool.clone(),
    );

    // Step 1: Generate with the smart planner (no agent_id filter — cards have mixed agents)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/generate")
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
                .uri("/queue/dispatch-next")
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
                .uri("/queue/status?repo=test-repo")
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

    pool.close().await;
    pg_db.drop().await;
}
