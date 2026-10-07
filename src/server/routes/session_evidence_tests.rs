use super::*;
use crate::services::discord::host_defer_gate::tests::{ScriptedTmux, postgres};
use axum::{
    extract::{Path, State},
    http::StatusCode,
};
use sqlx::Row;
use std::sync::Arc;

fn state(pool: sqlx::PgPool) -> AppState {
    let config = crate::config::Config::default();
    let broadcast_tx = crate::eventbus::new_broadcast();
    AppState {
        pg_pool: Some(pool),
        engine: crate::engine::PolicyEngine::new(&config).unwrap(),
        config: Arc::new(config),
        batch_buffer: crate::eventbus::spawn_batch_flusher(broadcast_tx.clone()),
        broadcast_tx,
        health_registry: None,
        cluster_instance_id: None,
    }
}

async fn seed(pool: &sqlx::PgPool) {
    sqlx::query("INSERT INTO agents (id, name, provider, discord_channel_id) VALUES ('evidence-agent', 'evidence', 'claude', '1509350490461180105')")
        .execute(pool).await.unwrap();
    sqlx::query("INSERT INTO task_dispatches (id, status) VALUES ('evidence-dispatch', 'pending')")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO sessions (agent_id, provider, session_key, channel_id, status, active_dispatch_id, last_heartbeat, claude_session_id, raw_provider_session_id) VALUES ('evidence-agent', 'claude', 'claude/evidence/host:AgentDesk-claude-evidence', '1509350490461180105', 'turn_active', 'evidence-dispatch', NOW() - INTERVAL '7 hours', 'legacy-selector', 'native-selector')")
        .execute(pool).await.unwrap();
}

async fn statuses(pool: &sqlx::PgPool) -> (String, Option<String>, String) {
    let row = sqlx::query("SELECT s.status, s.active_dispatch_id, d.status AS dispatch_status FROM sessions s CROSS JOIN task_dispatches d WHERE s.session_key = 'claude/evidence/host:AgentDesk-claude-evidence' AND d.id = 'evidence-dispatch'")
        .fetch_one(pool).await.unwrap();
    (
        row.get("status"),
        row.get("active_dispatch_id"),
        row.get("dispatch_status"),
    )
}

#[tokio::test]
async fn session_evidence_stale_working_pending_is_observation_only_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ScriptedTmux::install();
    tmux.fail_probes(true);
    assert!(
        crate::services::platform::tmux::version().is_err(),
        "poisoned executable must fail"
    );
    tmux.take_calls();
    let (db, pool) = postgres().await;
    seed(&pool).await;
    let before = statuses(&pool).await;
    let state = state(pool.clone());
    let mut events = state.broadcast_tx.subscribe();
    for identifier in ["evidence-agent", "1509350490461180105"] {
        let body = get(State(state.clone()), Path(identifier.into()))
            .await
            .unwrap()
            .0;
        assert_eq!(body.agent_id, "evidence-agent");
        assert_eq!(body.channel_id.as_deref(), Some("1509350490461180105"));
        assert_eq!(
            body.raw_provider_session_id.as_deref(),
            Some("native-selector")
        );
        assert_eq!(statuses(&pool).await, before, "observer mutated stale rows");
    }
    assert!(
        tmux.take_calls().is_empty(),
        "observer must never execute a tmux probe"
    );
    assert!(matches!(
        events.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn session_evidence_missing_raw_identity_is_null_pg() {
    let (db, pool) = postgres().await;
    seed(&pool).await;
    sqlx::query("UPDATE sessions SET raw_provider_session_id = NULL")
        .execute(&pool)
        .await
        .unwrap();
    let body = get(State(state(pool.clone())), Path("evidence-agent".into()))
        .await
        .unwrap()
        .0;
    assert!(
        body.raw_provider_session_id.is_none(),
        "legacy selector cannot replace missing raw identity"
    );
    assert!(serde_json::to_value(body).unwrap()["raw_provider_session_id"].is_null());
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn session_evidence_ambiguous_owner_and_sessions_fail_closed_pg() {
    let (db, pool) = postgres().await;
    seed(&pool).await;
    let state = state(pool.clone());
    sqlx::query("INSERT INTO agents (id, name, provider, discord_channel_id) VALUES ('other-agent', 'other', 'claude', '1509350490461180105')")
        .execute(&pool).await.unwrap();
    assert_eq!(
        get(State(state.clone()), Path("1509350490461180105".into()))
            .await
            .unwrap_err()
            .status(),
        StatusCode::CONFLICT
    );
    sqlx::query("INSERT INTO sessions (agent_id, provider, session_key, channel_id, raw_provider_session_id) VALUES ('evidence-agent', 'claude', 'unrelated-channel', '999', 'unrelated-native')")
        .execute(&pool).await.unwrap();
    assert_eq!(
        get(State(state.clone()), Path("evidence-agent".into()))
            .await
            .unwrap()
            .0
            .raw_provider_session_id
            .as_deref(),
        Some("native-selector")
    );
    sqlx::query("UPDATE sessions SET channel_id = '1509350490461180105' WHERE session_key = 'unrelated-channel'")
        .execute(&pool).await.unwrap();
    assert_eq!(
        get(State(state), Path("evidence-agent".into()))
            .await
            .unwrap_err()
            .status(),
        StatusCode::CONFLICT
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn session_evidence_provider_channels_ignore_unbound_history_pg() {
    let (db, pool) = postgres().await;
    seed(&pool).await;
    sqlx::query("UPDATE agents SET discord_channel_alt = ' 101 ', discord_channel_cdx = ' ' WHERE id = 'evidence-agent'")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO sessions (agent_id, provider, session_key, raw_provider_session_id) VALUES ('evidence-agent', 'claude', 'unbound-history', 'history-native')")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO sessions (agent_id, provider, session_key, channel_id, raw_provider_session_id) VALUES ('evidence-agent', 'codex', 'codex-bound', '101', 'codex-native')")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO sessions (agent_id, provider, session_key, channel_id, identity_kind, raw_provider_session_id) VALUES ('evidence-agent', 'claude', 'scheduled-classified', '1509350490461180105', 'scheduled_snapshot', 'scheduled-native'), ('evidence-agent', 'claude', 'claude/hash/host:AgentDesk-claude-scheduled-smsg_abc', '1509350490461180105', NULL, 'scheduled-legacy')")
        .execute(&pool).await.unwrap();
    let state = state(pool.clone());
    let primary = get(State(state.clone()), Path("evidence-agent".into()))
        .await
        .unwrap()
        .0;
    assert_eq!(
        primary.raw_provider_session_id.as_deref(),
        Some("native-selector")
    );
    let alt = get(State(state.clone()), Path("101".into()))
        .await
        .unwrap()
        .0;
    assert_eq!(alt.provider.as_deref(), Some("codex"));
    assert_eq!(alt.raw_provider_session_id.as_deref(), Some("codex-native"));
    sqlx::query("UPDATE agents SET provider = 'codex' WHERE id = 'evidence-agent'")
        .execute(&pool)
        .await
        .unwrap();
    let primary = get(State(state.clone()), Path("evidence-agent".into()))
        .await
        .unwrap()
        .0;
    assert_eq!(primary.channel_id.as_deref(), Some("101"));
    assert_eq!(
        primary.raw_provider_session_id.as_deref(),
        Some("codex-native")
    );
    sqlx::query("UPDATE agents SET provider = 'claude' WHERE id = 'evidence-agent'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE sessions SET channel_id = '999' WHERE session_key = 'claude/evidence/host:AgentDesk-claude-evidence'")
        .execute(&pool).await.unwrap();
    let absent = get(State(state), Path("evidence-agent".into()))
        .await
        .unwrap()
        .0;
    assert!(
        absent.raw_provider_session_id.is_none(),
        "scheduled/free-slot rows cannot replace fixed evidence"
    );
    pool.close().await;
    db.drop().await;
}
