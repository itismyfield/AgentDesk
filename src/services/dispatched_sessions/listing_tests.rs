#![cfg(unix)]

use axum::extract::{Query, State};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::sync::Arc;

use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::host_defer_gate::tests::ScriptedTmux;

use crate::db::session_agent_resolution::{
    normalize_thread_channel_id, parse_thread_channel_id_from_session_key,
};
use crate::domain::session_identity::tmux_name_from_session_key;
use crate::services::session_activity::SessionActivityResolver;

include!("listing_tests/legacy.rs");

const RECENT: &str = "2100-01-02T03:04:05Z";
const STALE: &str = "2000-01-02T03:04:05Z";

async fn seed_session(
    pool: &PgPool,
    key: &str,
    status: &str,
    heartbeat: Option<&str>,
    dispatch: bool,
) {
    sqlx::query(
        "INSERT INTO sessions
         (session_key, instance_id, agent_id, provider, status, active_dispatch_id,
          model, tokens, cwd, last_heartbeat, session_info, thread_channel_id)
         VALUES ($1, 'listing-node', 'listing-agent', 'claude', $2,
                 CASE WHEN $4 THEN 'listing-dispatch' END,
                 'fixture-model', 321, '/fixture/repo', $3::TEXT::TIMESTAMPTZ,
                 '── phase-gate dispatch ──', '123456789012345')",
    )
    .bind(key)
    .bind(status)
    .bind(heartbeat)
    .bind(dispatch)
    .execute(pool)
    .await
    .expect("seed listing session");
}

async fn seed_joined_rows(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO departments (id, name, name_ko, color)
         VALUES ('listing-department', 'Engineering', '개발', '#123456')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, department, provider, sprite_number, avatar_emoji)
         VALUES ('listing-agent', 'Listing Agent', 'listing-department', 'claude', 17, '🦊')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches
         (id, to_agent_id, dispatch_type, status, title, context, thread_id, created_at)
         VALUES ('listing-dispatch', 'listing-agent', 'implementation', 'dispatched',
                 'Listing fixture', '{}', '234567890123456', '2001-02-03T04:05:06Z')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO dispatch_delivery_events
         (dispatch_id, correlation_id, semantic_event_id, status, created_at)
         VALUES ('listing-dispatch', 'listing-correlation', 'listing-sent',
                 'sent', '2001-02-03T04:06:07Z')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ('listing-run', 'listing-repo', 'listing-agent', 'active')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries
         (id, run_id, agent_id, status, dispatch_id, slot_index, thread_group, created_at)
         VALUES ('listing-entry-old', 'listing-run', 'listing-agent', 'done',
                 'listing-dispatch', 1, 2, '2001-01-01'),
                ('listing-entry-current', 'listing-run', 'listing-agent', 'dispatched',
                 'listing-dispatch', 3, 4, '2001-01-02')",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn session_rows(pool: &PgPool) -> Vec<Value> {
    sqlx::query_scalar("SELECT to_jsonb(s) FROM sessions s ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap()
}

fn assert_same_bytes(actual: &[Value], expected: &[Value]) {
    assert_eq!(
        serde_json::to_vec(actual).unwrap(),
        serde_json::to_vec(expected).unwrap(),
        "the full listing JSON must match the frozen implementation"
    );
}

#[tokio::test]
async fn listing_json_matches_frozen_oracle_and_leaves_rows_unchanged_pg() {
    let runtime_root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ScriptedTmux::install();
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    seed_joined_rows(&pool).await;
    for (key, status, heartbeat, dispatch) in [
        (
            "s410-remote:AgentDesk-claude-recent",
            "turn_active",
            Some(RECENT),
            true,
        ),
        (
            "s410-remote:AgentDesk-claude-stale",
            "turn_active",
            Some(STALE),
            true,
        ),
        ("s410-unknown-key-recent", "turn_active", Some(RECENT), true),
        ("s410-unknown-key-stale", "turn_active", None, true),
        (
            "s410-remote:AgentDesk-claude-terminal",
            "aborted",
            Some(RECENT),
            true,
        ),
        (
            "s410-remote:AgentDesk-claude-background",
            "awaiting_bg",
            Some(STALE),
            true,
        ),
        (
            "s410-remote:AgentDesk-claude-idle",
            "idle",
            Some(STALE),
            false,
        ),
    ] {
        seed_session(&pool, key, status, heartbeat, dispatch).await;
    }
    sqlx::query("INSERT INTO sessions (status, tokens) VALUES ('disconnected', 0)")
        .execute(&pool)
        .await
        .unwrap();
    let before = session_rows(&pool).await;
    for include_all in [false, true] {
        let expected = legacy_list_dispatched_sessions_pg(&pool, include_all)
            .await
            .unwrap();
        let actual = super::list_dispatched_sessions_pg(&pool, include_all)
            .await
            .unwrap();
        assert_same_bytes(&actual, &expected);
        assert_eq!(actual.len(), if include_all { 8 } else { 3 });
        let recent = actual
            .iter()
            .find(|row| row["session_key"] == "s410-remote:AgentDesk-claude-recent")
            .unwrap();
        assert_eq!(recent["session_info"], "── 구현 dispatch ──");
        assert_eq!(recent["session_info_raw"], "── phase-gate dispatch ──");
        assert_eq!(recent["delivery_state"], "codex_active");
        assert_eq!(recent["resolved_thread_channel_id"], "234567890123456");
        assert_eq!(
            recent["recovery_identifiers"]["auto_queue_entry_id"],
            "listing-entry-current"
        );
    }
    assert_eq!(session_rows(&pool).await, before);
    assert!(
        tmux.take_calls().is_empty(),
        "remote and unknown keys never probe tmux"
    );
    pool.close().await;
    database.drop().await;
    drop(tmux);
    drop(runtime_root);
}

#[tokio::test]
async fn listing_marker_refusal_preserves_heartbeat_projection_and_avoids_tmux_pg() {
    let runtime_root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ScriptedTmux::install();
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    seed_joined_rows(&pool).await;
    let host = std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .unwrap();
    assert!(host.status.success());
    let host = String::from_utf8(host.stdout).unwrap();
    let host = host.trim();
    for (suffix, marker_kind, heartbeat) in [
        ("herdr-recent", "herdr", RECENT),
        ("herdr-stale", "herdr", STALE),
        ("unknown-recent", "unrecognized-host", RECENT),
        ("unknown-stale", "unrecognized-host", STALE),
    ] {
        let name = format!("AgentDesk-claude-listing-{suffix}");
        let marker = crate::services::tmux_common::session_temp_path(&name, "host_kind");
        std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(marker, marker_kind).unwrap();
        seed_session(
            &pool,
            &format!("{host}:{name}"),
            "turn_active",
            Some(heartbeat),
            true,
        )
        .await;
    }
    let before = session_rows(&pool).await;
    for include_all in [false, true] {
        let expected = legacy_list_dispatched_sessions_pg(&pool, include_all)
            .await
            .unwrap();
        let actual = super::list_dispatched_sessions_pg(&pool, include_all)
            .await
            .unwrap();
        assert_same_bytes(&actual, &expected);
        assert_eq!(actual.len(), if include_all { 4 } else { 2 });
        if include_all {
            assert_eq!(actual[0]["status"], "turn_active");
            assert_eq!(actual[1]["status"], "idle");
            assert_eq!(actual[2]["status"], "turn_active");
            assert_eq!(actual[3]["active_dispatch_id"], Value::Null);
        }
    }
    assert!(
        tmux.take_calls().is_empty(),
        "refused markers never fall back to local tmux"
    );
    let local_key = format!("{host}:AgentDesk-claude-listing-legacy-absent");
    seed_session(&pool, &local_key, "turn_active", Some(RECENT), true).await;
    let expected = legacy_list_dispatched_sessions_pg(&pool, true)
        .await
        .unwrap();
    let actual = super::list_dispatched_sessions_pg(&pool, true)
        .await
        .unwrap();
    assert_same_bytes(&actual, &expected);
    assert!(
        !tmux.take_calls().is_empty(),
        "the unmarked local key still probes tmux"
    );
    assert_eq!(session_rows(&pool).await[..4], before);
    pool.close().await;
    database.drop().await;
    drop(tmux);
    drop(runtime_root);
}

fn app_state(pool: &PgPool, runtime_root: &std::path::Path) -> crate::app_state::AppState {
    let mut config = crate::config::Config::default();
    config.policies.hot_reload = false;
    config.policies.dir = runtime_root.join("empty-policies");
    std::fs::create_dir_all(&config.policies.dir).unwrap();
    let broadcast_tx = crate::server::ws::new_broadcast();
    crate::app_state::AppState {
        pg_pool: Some(pool.clone()),
        engine: crate::engine::PolicyEngine::new(&config).unwrap(),
        config: Arc::new(config),
        batch_buffer: crate::server::ws::spawn_batch_flusher(broadcast_tx.clone()),
        broadcast_tx,
        health_registry: None,
        cluster_instance_id: Some("listing-node".to_string()),
    }
}

#[tokio::test]
async fn listing_get_and_cluster_owner_summary_consume_the_same_projection_pg() {
    let runtime_root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ScriptedTmux::install();
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    seed_joined_rows(&pool).await;
    seed_session(
        &pool,
        "s410-remote:AgentDesk-claude-handler-live",
        "turn_active",
        Some(RECENT),
        true,
    )
    .await;
    seed_session(
        &pool,
        "s410-remote:AgentDesk-claude-handler-stale",
        "turn_active",
        Some(STALE),
        true,
    )
    .await;
    let before = session_rows(&pool).await;
    let state = app_state(&pool, runtime_root.path());
    for include_all in [false, true] {
        let mut expected = legacy_list_dispatched_sessions_pg(&pool, include_all)
            .await
            .unwrap();
        crate::services::cluster::session_routing::enrich_session_owner_routing(
            &mut expected,
            Some("listing-node"),
            &[],
        );
        let (status, body) = crate::server::routes::dispatched_sessions::list_dispatched_sessions(
            State(state.clone()),
            Query(super::ListDispatchedSessionsQuery {
                include_merged: include_all.then(|| "1".to_string()),
            }),
        )
        .await
        .unwrap();
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(
            serde_json::to_vec(&body.0).unwrap(),
            serde_json::to_vec(&json!({"sessions": expected})).unwrap()
        );
    }
    let (status, body) = crate::server::routes::cluster::list_nodes(State(state.clone()))
        .await
        .unwrap();
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body.0["session_owners"]["total_active_sessions"], 1);
    assert_eq!(body.0["session_owners"]["local"], 1);
    assert_eq!(body.0["session_owner_error"], Value::Null);
    assert_eq!(session_rows(&pool).await, before);
    assert!(tmux.take_calls().is_empty());
    drop(state);
    pool.close().await;
    database.drop().await;
    drop(tmux);
    drop(runtime_root);
}
