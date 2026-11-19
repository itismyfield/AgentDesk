use super::*;
use crate::services::discord::admin_host_guard::tests::api_child;
use crate::services::discord::host_defer_gate::tests::{map_channel, postgres};
use crate::services::discord::host_teardown_gate::test_support::{channel_key, shared_on};
use crate::services::tui_prompt_dedupe::{self as dedupe, ClaudeSessionRotation};

const OLD: &str = "af3ece1a-0000-4000-8000-000000008261";
const NEW: &str = "fd530b40-0000-4000-8000-000000008261";

// A channel whose cache still names the session an in-pane /clear left resumes the rotated one on
// its next turn, through both the cache and the stored selector; a cleared channel stays cleared.
#[tokio::test]
async fn retired_rotation_moves_the_channel_selector_to_the_rotated_session_pg() {
    let test = "services::discord::tui_prompt_relay::session_rotation_settle::selector_tests::retired_rotation_moves_the_channel_selector_to_the_rotated_session_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (_db, pool) = postgres().await;
    let mut shared = shared_on(&pool).await;
    // A token hash shape the runtime API accepts on a session hook.
    Arc::get_mut(&mut shared).unwrap().token_hash = "discord_8261826182618261".to_string();
    let _api = serve_api(&pool).await;
    let channel = ChannelId::new(1_479_671_302_387_082_610);
    let tmux = ProviderKind::Claude.build_tmux_session_name("rotation-selector");
    let key = channel_key(&shared, &tmux);
    let dir = tempfile::tempdir().unwrap();
    let (frozen, rotated) = (dir.path().join("old.jsonl"), dir.path().join("new.jsonl"));
    std::fs::write(&frozen, b"{}\n").unwrap();
    std::fs::write(&rotated, b"{}\n").unwrap();
    map_channel(&shared, channel, "rotation-selector").await;
    shared.tmux_watchers.insert(
        channel,
        crate::services::discord::TmuxWatcherHandle {
            tmux_session_name: tmux.clone(),
            output_path: rotated.display().to_string(),
            paused: Arc::new(false.into()),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel: Arc::new(false.into()),
            pause_epoch: Arc::new(0.into()),
            turn_delivered: Arc::new(false.into()),
            last_heartbeat_ts_ms: Arc::new(0.into()),
        },
    );
    sqlx::query(
        "INSERT INTO sessions (session_key, provider, status, identity_kind,
                               discord_token_hash, channel_id, claude_session_id)
         VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3, NULL)",
    )
    .bind(&key)
    .bind(&shared.token_hash)
    .bind(channel.get().to_string())
    .execute(&pool)
    .await
    .unwrap();
    let rotate = || {
        dedupe::record_claude_session_rotation(ClaudeSessionRotation {
            tmux_session_name: tmux.clone(),
            old_session_id: Some(OLD.to_string()),
            old_output_path: frozen.display().to_string(),
            old_last_offset: 0,
            new_session_id: NEW.to_string(),
            new_output_path: rotated.display().to_string(),
            observed_drain_frontier: 0,
            polls_without_drain_progress: 0,
        })
    };
    let stored = || async {
        let row: Option<String> =
            sqlx::query_scalar("SELECT claude_session_id FROM sessions WHERE session_key = $1")
                .bind(&key)
                .fetch_one(&pool)
                .await
                .unwrap();
        let fetched = crate::services::discord::adk_session::fetch_provider_session_id(
            &key,
            &ProviderKind::Claude,
            shared.api_port,
        )
        .await;
        (row, fetched)
    };
    let turn_start_session = || async {
        let mut core = shared.core.lock().await;
        crate::services::discord::router::load_session_runtime_state(&mut core.sessions, channel)
            .map(|(session_id, _, _)| session_id)
    };
    let workdir = Some(dir.path().display().to_string());

    // A user /clear emptied the cache and the stored selector; the rotation must not refill them.
    {
        let mut core = shared.core.lock().await;
        let session = core.sessions.get_mut(&channel).unwrap();
        (session.current_path, session.cleared) = (workdir.clone(), true);
    }
    rotate();
    settle_claude_session_rotations(&shared).await;
    assert!(dedupe::claude_session_rotation_for_tmux(&tmux).is_none());
    assert_eq!(turn_start_session().await, Some(None));
    assert_eq!(stored().await, (None, None));

    {
        let mut core = shared.core.lock().await;
        let session = core.sessions.get_mut(&channel).unwrap();
        (session.session_id, session.cleared) = (Some(OLD.to_string()), false);
    }
    sqlx::query("UPDATE sessions SET claude_session_id = $2 WHERE session_key = $1")
        .bind(&key)
        .bind(OLD)
        .execute(&pool)
        .await
        .unwrap();
    rotate();
    settle_claude_session_rotations(&shared).await;
    assert!(dedupe::claude_session_rotation_for_tmux(&tmux).is_none());
    assert_eq!(turn_start_session().await, Some(Some(NEW.to_string())));
    let new = Some(NEW.to_string());
    assert_eq!(stored().await, (new.clone(), new));
}

/// Serves the real runtime API on `pool`, so the selector hook writes and reads real rows.
async fn serve_api(pool: &sqlx::PgPool) -> tokio::task::AbortHandle {
    let config = crate::config::Config::default();
    let engine = crate::engine::PolicyEngine::new(&config).unwrap();
    let broadcast = crate::eventbus::new_broadcast();
    let buffer = crate::eventbus::spawn_batch_flusher(broadcast.clone());
    let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
    let app = crate::server::routes::api_router_with_pg(
        engine,
        config,
        broadcast,
        buffer,
        Some(registry),
        Some(pool.clone()),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    crate::services::discord::internal_api::init(port, Some(pool.clone()));
    let serving = axum::Router::new().nest("/api", app);
    let serving = serving.into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, serving).await.unwrap() }).abort_handle()
}
