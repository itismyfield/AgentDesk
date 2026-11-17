use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::dispatched_sessions::provider_resume_fence as fence;
use std::sync::Arc;

#[tokio::test]
async fn selector_get_hides_fenced_ids_and_returns_the_new_id_after_release_pg() {
    let root = crate::config::TestRuntimeRootGuard::new();
    let _home = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "CLAUDE_CONFIG_DIR",
        root.path(),
    );
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let old = uuid::Uuid::new_v4().to_string();
    let new = uuid::Uuid::new_v4().to_string();
    let key = "claude/discord_0123456789abcdef/fixture:AgentDesk-claude-fence-api";
    sqlx::query(
        "INSERT INTO sessions (session_key, provider, identity_kind, discord_token_hash,
             channel_id, status, claude_session_id, raw_provider_session_id,
             claude_session_id_recorded_at, provider_resume_fence)
         VALUES ($1, 'claude', 'discord_channel', 'discord_0123456789abcdef',
             '42', 'idle', $2, $2, NOW(), $3)",
    )
    .bind(key)
    .bind(&old)
    .bind(fence::new_payload())
    .execute(&pool)
    .await
    .unwrap();
    let mut config = crate::config::Config::default();
    config.policies.hot_reload = false;
    config.policies.dir = root.path().join("policies");
    std::fs::create_dir_all(&config.policies.dir).unwrap();
    let broadcast_tx = crate::server::ws::new_broadcast();
    let state = AppState {
        pg_pool: Some(pool.clone()),
        engine: crate::engine::PolicyEngine::new(&config).unwrap(),
        config: Arc::new(config),
        batch_buffer: crate::server::ws::spawn_batch_flusher(broadcast_tx.clone()),
        broadcast_tx,
        health_registry: None,
        cluster_instance_id: None,
    };
    let params = || {
        Query(DeleteSessionQuery {
            session_key: key.into(),
            provider: Some("claude".into()),
        })
    };
    let (_, Json(hidden)) = get_claude_session_id(State(state.clone()), params())
        .await
        .unwrap();
    assert_eq!(
        hidden,
        json!({"claude_session_id":null,"session_id":null,"raw_provider_session_id":null})
    );
    let captured = fence::load_by_resolved_key_pg(&pool, key)
        .await
        .unwrap()
        .unwrap();
    sqlx::query(
        "UPDATE sessions SET claude_session_id=$2, raw_provider_session_id=$2 WHERE session_key=$1",
    )
    .bind(key)
    .bind(&new)
    .execute(&pool)
    .await
    .unwrap();
    assert!(fence::release_pg(&pool, &captured).await.unwrap());
    let (_, Json(visible)) = get_claude_session_id(State(state.clone()), params())
        .await
        .unwrap();
    assert_eq!(visible["claude_session_id"], new);
    assert_eq!(visible["session_id"], new);
    assert_eq!(visible["raw_provider_session_id"], new);
    sqlx::query("UPDATE sessions SET provider_resume_fence='{}'::jsonb WHERE session_key=$1")
        .bind(key)
        .execute(&pool)
        .await
        .unwrap();
    assert!(get_claude_session_id(State(state), params()).await.is_err());
    pool.close().await;
    db.drop().await;
}
