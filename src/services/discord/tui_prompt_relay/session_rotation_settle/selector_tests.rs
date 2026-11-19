use super::*;
use crate::services::discord::admin_host_guard::tests::api_child;
use crate::services::discord::host_defer_gate::tests::{map_channel, postgres};
use crate::services::discord::host_teardown_gate::test_support::shared_on;
use crate::services::tui_prompt_dedupe::{self as dedupe, ClaudeSessionRotation};

const OLD: &str = "af3ece1a-0000-4000-8000-000000008261";
const NEW: &str = "fd530b40-0000-4000-8000-000000008261";
const TEST: &str = "services::discord::tui_prompt_relay::session_rotation_settle::selector_tests::";

// A channel whose cache still names the session an in-pane /clear left resumes the rotated one on
// its next turn, through both the cache and the stored selector; a cleared channel stays cleared.
#[tokio::test]
async fn retired_rotation_moves_the_channel_selector_to_the_rotated_session_pg() {
    if !api_child(&format!(
        "{TEST}retired_rotation_moves_the_channel_selector_to_the_rotated_session_pg"
    )) {
        return;
    }
    let f = Fixture::new(true).await;
    // A user /clear emptied the cache and the stored selector; the rotation must not refill them.
    f.set(None, true, None).await;
    f.rotate();
    settle_claude_session_rotations(&f.shared).await;
    assert!(dedupe::claude_session_rotation_for_tmux(&f.tmux).is_none());
    assert_eq!(f.turn_start_session().await, Some(None));
    assert_eq!(f.stored().await, (None, None));

    // A stored selector that no longer names the old session is newer and stays.
    let newer = "0e5a7c11-0000-4000-8000-000000008261";
    f.set(Some(OLD), false, Some(newer)).await;
    f.rotate();
    settle_claude_session_rotations(&f.shared).await;
    assert_eq!(f.stored().await.0.as_deref(), Some(newer));

    // An empty cache leaves the next turn to the stored selector, which follows the pane.
    f.set(None, false, Some(OLD)).await;
    f.rotate();
    settle_claude_session_rotations(&f.shared).await;
    let new = Some(NEW.to_string());
    assert_eq!(f.stored().await, (new.clone(), new.clone()));
    assert_eq!(
        f.restore().await,
        (new.clone(), "db_provider_session_restored")
    );

    // While a turn start holds the channel transition, between its selector read and its claim,
    // the settle pass moves nothing; that turn picks the pending rotation itself.
    f.set(None, false, Some(OLD)).await;
    f.rotate();
    let transition = f
        .shared
        .session_transition_lock(f.channel)
        .lock_owned()
        .await;
    settle_claude_session_rotations(&f.shared).await;
    assert!(dedupe::claude_session_rotation_for_tmux(&f.tmux).is_some());
    assert_eq!(f.stored().await.0.as_deref(), Some(OLD));
    let followed = (new.clone(), "claude_session_rotation_followed");
    assert_eq!(f.restore().await, followed);
    drop(transition);
    settle_claude_session_rotations(&f.shared).await;
    assert!(dedupe::claude_session_rotation_for_tmux(&f.tmux).is_none());
    assert_eq!(f.stored().await, (new.clone(), new.clone()));

    // With no row for the channel the rotation stays until a row takes the selector.
    f.set(Some(OLD), false, Some(OLD)).await;
    f.move_row("0").await;
    f.rotate();
    settle_claude_session_rotations(&f.shared).await;
    assert!(dedupe::claude_session_rotation_for_tmux(&f.tmux).is_some());
    f.move_row(&f.channel.get().to_string()).await;
    settle_claude_session_rotations(&f.shared).await;
    assert!(dedupe::claude_session_rotation_for_tmux(&f.tmux).is_none());
    assert_eq!(f.turn_start_session().await, Some(Some(NEW.to_string())));
    assert_eq!(f.stored().await, (new.clone(), new));
}

// The turn pinned to the frozen transcript is settled with the cache already moved, and a stored
// selector that failed to move is written on a later pass, so a restart resumes the new session.
#[tokio::test]
async fn a_failed_selector_store_keeps_the_rotation_until_it_lands_pg() {
    if !api_child(&format!(
        "{TEST}a_failed_selector_store_keeps_the_rotation_until_it_lands_pg"
    )) {
        return;
    }
    let f = Fixture::new(true).await;
    f.set(Some(OLD), false, Some(OLD)).await;
    let (provider, path) = (ProviderKind::Claude, f.frozen.display().to_string());
    let len = std::fs::metadata(&f.frozen).unwrap().len();
    let (channel, tmux) = (f.channel.get(), Some(f.tmux.clone()));
    let (text, old) = ("before /clear".to_string(), Some(OLD.to_string()));
    let row = crate::services::discord::inflight::InflightTurnState::new(
        provider,
        channel,
        None,
        1,
        8261,
        8261,
        text,
        old,
        tmux,
        Some(path),
        None,
        len,
    );
    crate::services::discord::inflight::save_inflight_state(&row).unwrap();
    for statement in [
        "CREATE FUNCTION refuse_selector() RETURNS trigger LANGUAGE plpgsql AS
         $$ BEGIN RAISE EXCEPTION 'selector store unavailable'; END $$",
        "CREATE TRIGGER refuse_selector BEFORE UPDATE OF claude_session_id ON sessions
         FOR EACH ROW EXECUTE FUNCTION refuse_selector()",
    ] {
        sqlx::query(statement).execute(&f.pool).await.unwrap();
    }
    f.rotate();
    settle_claude_session_rotations(&f.shared).await;
    let load = crate::services::discord::inflight::load_inflight_state;
    assert!(load(&ProviderKind::Claude, channel).is_none());
    assert_eq!(f.turn_start_session().await, Some(Some(NEW.to_string())));
    assert!(dedupe::claude_session_rotation_for_tmux(&f.tmux).is_some());
    assert_eq!(f.stored().await.0.as_deref(), Some(OLD));

    sqlx::query("DROP TRIGGER refuse_selector ON sessions")
        .execute(&f.pool)
        .await
        .unwrap();
    settle_claude_session_rotations(&f.shared).await;
    assert!(dedupe::claude_session_rotation_for_tmux(&f.tmux).is_none());
    let new = Some(NEW.to_string());
    assert_eq!(f.stored().await, (new.clone(), new.clone()));
    // A restart rebuilds the cache from the stored selector at the next turn start.
    f.set(None, false, Some(NEW)).await;
    assert_eq!(f.restore().await, (new, "db_provider_session_restored"));
}

// A reconnected Herdr pane has no watcher, so its rotation keeps waiting for an owner, but its
// channel's stored selector follows the pane once the row names the execution the reconcile admitted.
#[tokio::test]
async fn a_watcherless_herdr_pane_moves_its_channel_selector_pg() {
    if !api_child(&format!(
        "{TEST}a_watcherless_herdr_pane_moves_its_channel_selector_pg"
    )) {
        return;
    }
    let f = Fixture::new(false).await;
    f.set(Some(OLD), false, Some(OLD)).await;
    f.host_on_herdr(&f.key, &f.shared.token_hash).await;
    // An admitted execution the channel row does not name moves nothing.
    dedupe::admit_herdr_execution(&f.tmux, "nonce-other");
    f.rotate();
    settle_claude_session_rotations(&f.shared).await;
    assert_eq!(f.stored().await.0.as_deref(), Some(OLD));

    dedupe::release_herdr_execution(&f.tmux, "nonce-other");
    dedupe::admit_herdr_execution(&f.tmux, "nonce-8261");
    settle_claude_session_rotations(&f.shared).await;
    let new = Some(NEW.to_string());
    assert_eq!(f.stored().await, (new.clone(), new));
    assert_eq!(f.turn_start_session().await, Some(Some(NEW.to_string())));
    let kept = dedupe::claude_session_rotation_for_tmux(&f.tmux);
    assert!(kept.is_some_and(|rotation| rotation.selector_followed));

    // A second hop before any Discord turn moves the selector on from the session it reflects now.
    let third = "c3c3c3c3-0000-4000-8000-000000008261";
    f.hop(NEW, third);
    settle_claude_session_rotations(&f.shared).await;
    let third = Some(third.to_string());
    assert_eq!(f.stored().await, (third.clone(), third.clone()));
    assert_eq!(f.turn_start_session().await, Some(third.clone()));
    // A session the user chose meanwhile stays through the next hop.
    let chosen = "0e5a7c11-0000-4000-8000-000000008261";
    f.set(Some(chosen), false, Some(chosen)).await;
    f.hop(
        third.as_deref().unwrap(),
        "d4d4d4d4-0000-4000-8000-000000008261",
    );
    settle_claude_session_rotations(&f.shared).await;
    let chosen = Some(chosen.to_string());
    assert_eq!(f.stored().await, (chosen.clone(), chosen.clone()));
    assert_eq!(f.turn_start_session().await, Some(chosen));
}

// With two Claude bots the idle loop runs on one; a watcherless Herdr pane of the other bot's
// channel moves that bot's row and cache, and the first bot's row for the channel stays.
#[tokio::test]
async fn a_watcherless_herdr_pane_follows_on_its_own_bot_runtime_pg() {
    if !api_child(&format!(
        "{TEST}a_watcherless_herdr_pane_follows_on_its_own_bot_runtime_pg"
    )) {
        return;
    }
    let f = Fixture::new(false).await;
    f.set(Some(OLD), false, Some(OLD)).await;
    f.host_on_herdr(&f.key, &f.shared.token_hash).await;
    let mut first = shared_on(&f.pool).await;
    let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
    let own = Arc::get_mut(&mut first).unwrap();
    (own.token_hash, own.health_registry) =
        ("discord_1111111111111111".into(), Arc::downgrade(&registry));
    let first_key = format!("claude/{}/lane-node:{}", first.token_hash, f.tmux);
    sqlx::query(
        "INSERT INTO sessions (session_key, provider, status, identity_kind,
                               discord_token_hash, channel_id, claude_session_id)
         VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3, $4)",
    )
    .bind(&first_key)
    .bind(&first.token_hash)
    .bind(f.channel.get().to_string())
    .bind(OLD)
    .execute(&f.pool)
    .await
    .unwrap();
    f.host_on_herdr(&first_key, &first.token_hash).await;
    first.settings.write().await.allowed_channel_ids = vec![f.channel.get() + 1];
    f.shared.settings.write().await.allowed_channel_ids = vec![f.channel.get()];
    registry.register("claude".to_string(), first.clone()).await;
    registry
        .register("claude".to_string(), f.shared.clone())
        .await;
    dedupe::admit_herdr_execution(&f.tmux, "nonce-8261");
    f.rotate();
    settle_claude_session_rotations(&first).await;
    let new = Some(NEW.to_string());
    assert_eq!(f.stored().await, (new.clone(), new));
    assert_eq!(f.turn_start_session().await, Some(Some(NEW.to_string())));
    let first_row: Option<String> =
        sqlx::query_scalar("SELECT claude_session_id FROM sessions WHERE session_key = $1")
            .bind(&first_key)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(first_row.as_deref(), Some(OLD));
}

struct Fixture {
    _root: crate::config::TestRuntimeRootGuard,
    _db: crate::db::auto_queue::test_support::TestPostgresDb,
    _api: tokio::task::AbortHandle,
    _dir: tempfile::TempDir,
    pool: sqlx::PgPool,
    shared: Arc<SharedData>,
    channel: ChannelId,
    tmux: String,
    key: String,
    frozen: std::path::PathBuf,
    rotated: std::path::PathBuf,
}

impl Fixture {
    async fn new(watcher: bool) -> Self {
        let root = crate::config::TestRuntimeRootGuard::new();
        let (db, pool) = postgres().await;
        let mut shared = shared_on(&pool).await;
        // A token hash shape the runtime API accepts on a session hook.
        Arc::get_mut(&mut shared).unwrap().token_hash = "discord_8261826182618261".to_string();
        let api = serve_api(&pool).await;
        let channel = ChannelId::new(1_479_671_302_387_082_610);
        let tmux = ProviderKind::Claude.build_tmux_session_name("rotation-selector");
        // The canonical row's own key, not the one the pane name would compose.
        let key = format!("claude/{}/lane-node:{}", shared.token_hash, tmux);
        let dir = tempfile::tempdir().unwrap();
        let (frozen, rotated) = (dir.path().join("old.jsonl"), dir.path().join("new.jsonl"));
        std::fs::write(&frozen, b"{}\n").unwrap();
        std::fs::write(&rotated, b"{}\n").unwrap();
        map_channel(&shared, channel, "rotation-selector").await;
        shared
            .core
            .lock()
            .await
            .sessions
            .get_mut(&channel)
            .unwrap()
            .current_path = Some(dir.path().display().to_string());
        if watcher {
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
        }
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
        Self {
            _root: root,
            _db: db,
            _api: api,
            _dir: dir,
            pool,
            shared,
            channel,
            tmux,
            key,
            frozen,
            rotated,
        }
    }

    /// The channel's cache and clear mark, and the stored selector.
    async fn set(&self, cached: Option<&str>, cleared: bool, stored: Option<&str>) {
        let mut core = self.shared.core.lock().await;
        let session = core.sessions.get_mut(&self.channel).unwrap();
        (session.session_id, session.cleared) = (cached.map(str::to_owned), cleared);
        drop(core);
        sqlx::query("UPDATE sessions SET claude_session_id = $2, raw_provider_session_id = $2 WHERE session_key = $1")
            .bind(&self.key)
            .bind(stored)
            .execute(&self.pool)
            .await
            .unwrap();
    }

    async fn move_row(&self, channel: &str) {
        sqlx::query("UPDATE sessions SET channel_id = $2 WHERE session_key = $1")
            .bind(&self.key)
            .bind(channel)
            .execute(&self.pool)
            .await
            .unwrap();
    }

    /// The session the next turn start's restore picks when the cache is empty.
    async fn restore(&self) -> (Option<String>, &'static str) {
        let (mut session, mut memento, mut reason) = (None, false, "before");
        let input_payload =
            crate::services::discord::router::message_handler::input_payload::restore_session;
        let state =
            crate::services::discord::router::message_handler::input_payload::SessionRestore {
                session_id: &mut session,
                memento_context_loaded: &mut memento,
                reason: &mut reason,
                force_fresh: false,
                cleared: false,
                reset: false,
                recreate: false,
                session_key: Some(&self.key),
                tmux: Some(&self.tmux),
            };
        input_payload(&self.shared, &ProviderKind::Claude, self.channel, state).await;
        (session, reason)
    }

    /// Binds `session_key`'s row to this pane's admitted Herdr execution, with the channel mirrored.
    async fn host_on_herdr(&self, session_key: &str, token: &str) {
        use crate::db::dispatched_sessions::hosted_execution::{HostedState, tests};
        let mut owner = tests::owner(&self.channel.get().to_string());
        (owner.discord_token_hash, owner.logical_key) = (token.to_owned(), self.tmux.clone());
        let hosted = tests::wire(&tests::record(&owner, "nonce-8261", HostedState::Bound));
        sqlx::query("UPDATE sessions SET hosted_execution = $2 WHERE session_key = $1")
            .bind(session_key)
            .bind(hosted)
            .execute(&self.pool)
            .await
            .unwrap();
        dedupe::register_tmux_channel(&self.tmux, self.channel.get());
    }

    /// A later hook adoption on the same pane, as the binding records it.
    fn hop(&self, from: &str, to: &str) {
        dedupe::record_claude_session_rotation(ClaudeSessionRotation {
            old_session_id: Some(from.to_string()),
            new_session_id: to.to_string(),
            ..self.rotation()
        });
    }

    fn rotate(&self) {
        dedupe::record_claude_session_rotation(self.rotation());
    }

    fn rotation(&self) -> ClaudeSessionRotation {
        ClaudeSessionRotation {
            tmux_session_name: self.tmux.clone(),
            old_session_id: Some(OLD.to_string()),
            old_output_path: self.frozen.display().to_string(),
            old_last_offset: 0,
            new_session_id: NEW.to_string(),
            new_output_path: self.rotated.display().to_string(),
            observed_drain_frontier: 0,
            polls_without_drain_progress: 0,
            selector_followed: false,
            left_session_ids: Vec::new(),
        }
    }

    /// The row's selector and what the turn-start restore reads through the runtime API.
    async fn stored(&self) -> (Option<String>, Option<String>) {
        let row: Option<String> =
            sqlx::query_scalar("SELECT claude_session_id FROM sessions WHERE session_key = $1")
                .bind(&self.key)
                .fetch_one(&self.pool)
                .await
                .unwrap();
        let fetched = crate::services::discord::adk_session::fetch_provider_session_id(
            &self.key,
            &ProviderKind::Claude,
            self.shared.api_port,
        )
        .await;
        (row, fetched)
    }

    async fn turn_start_session(&self) -> Option<Option<String>> {
        let mut core = self.shared.core.lock().await;
        crate::services::discord::router::load_session_runtime_state(
            &mut core.sessions,
            self.channel,
        )
        .map(|(session_id, _, _)| session_id)
    }
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
