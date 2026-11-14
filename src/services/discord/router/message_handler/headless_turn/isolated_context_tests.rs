//! A headless routine run through the real headless start on its agent's primary channel: the
//! channel's session, clear boundary and session row stay as they were from start to turn end.
use crate::services::discord::SharedData;
use crate::services::discord::admin_host_guard::tests::{ModeTmux, Recorder, Tmux, api_child};
use crate::services::discord::health::{
    HealthRegistry, reserve_headless_agent_turn,
    start_reserved_headless_agent_turn_with_owner_channel,
};
use crate::services::discord::host_defer_gate::tests::{map_channel, postgres};
use crate::services::discord::host_teardown_gate::test_support::{channel_key, runtime};
use crate::services::discord::inflight::load_inflight_state_read_only;
use crate::services::discord::router::message_handler::headless_turn::STARTS_RESOLVED;
use crate::services::provider::ProviderKind;
use crate::test_support::wait::{WAIT_CAP, await_within};
use crate::ui::ai_screen::{HistoryItem, HistoryType};
use poise::serenity_prelude::ChannelId;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;

const PRIMARY: ChannelId = ChannelId::new(1_479_671_302_387_081_171);
const THREAD: ChannelId = ChannelId::new(1_479_671_302_387_081_172);
const LABEL: &str = "routine-process 9100 nightly agent8117";
const TEST: &str =
    "services::discord::router::message_handler::headless_turn::isolated_context::tests::";

struct Fixture {
    /// Directory of the stub CLIs and their `release` file.
    stubs: std::path::PathBuf,
    /// Records every runtime API call, the session status hook among them.
    api: Recorder,
    db: crate::db::auto_queue::test_support::TestPostgresDb,
    pool: PgPool,
    shared: Arc<SharedData>,
    registry: Arc<HealthRegistry>,
    _workdir: tempfile::TempDir,
    // Environment guards restore in this order; the root guard holds the lock and goes last.
    _clis: [crate::config::TestEnvVarGuard; 2],
    _config: crate::config::TestEnvVarGuard,
    _tmux: ModeTmux,
    _root: crate::config::TestRuntimeRootGuard,
}

/// The provider CLI is pinned to a stub that holds its turn open until `release`, or until its
/// directory goes away with a failed test; it then answers with the `answer` file or fails.
async fn fixture() -> Fixture {
    let root = crate::config::TestRuntimeRootGuard::new();
    // A config override in the environment would hide the agent written under this root.
    let unset = crate::config::TestEnvVarGuard::remove_after_shared_test_env_lock;
    let config_override = unset("AGENTDESK_CONFIG");
    let workdir = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.agents.push(
        serde_yaml::from_str(&format!(
            "id: agent8117\nname: Test\nchannels:\n  claude:\n    id: '{}'\n    workspace: {}\n",
            PRIMARY.get(),
            workdir.path().display()
        ))
        .unwrap(),
    );
    let path = crate::runtime_layout::config_file_path(root.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_yaml::to_string(&config).unwrap()).unwrap();
    let tmux = ModeTmux::install();
    tmux.serve(Tmux::Live);
    let paths = std::env::var_os("PATH").unwrap();
    let stubs = std::env::split_paths(&paths).next().unwrap();
    let hold = concat!(
        "#!/bin/sh\nd=\"$(dirname \"$0\")\"\n",
        "case \"$*\" in *--version*) echo 0.0.0-stub; exit 0 ;; esac\n",
        "while [ -d \"$d\" ] && [ ! -e \"$d/release\" ]; do sleep 0.05; done\n",
        "[ -e \"$d/answer\" ] || exit 1\n",
        "printf '{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"%s\",",
        "\"session_id\":\"81170000-0000-4000-8000-000000000001\"}\\n' \"$(cat \"$d/answer\")\"\n"
    );
    for cli in ["claude", "codex"] {
        std::fs::write(stubs.join(cli), hold).unwrap();
        let mode = std::os::unix::fs::PermissionsExt::from_mode(0o755);
        std::fs::set_permissions(stubs.join(cli), mode).unwrap();
    }
    // The override skips binary discovery, so no installed CLI can be picked over the stub.
    let pin = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
    let clis = [
        pin("AGENTDESK_CLAUDE_PATH", &stubs.join("claude")),
        pin("AGENTDESK_CODEX_PATH", &stubs.join("codex")),
    ];
    let api = Recorder::start().await;
    crate::services::discord::internal_api::init(api.port, None);
    let (db, pool) = postgres().await;
    let (shared, registry) = runtime(&pool).await;
    use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
    let discord = discord_mock::DiscordMockState::new();
    for channel in [PRIMARY, THREAD] {
        discord.name_channel(channel.get(), "discord-named");
    }
    let (proxy, gateway, _server) = discord_mock::start(discord).await;
    let ctx = discord_mock::serenity_context(proxy, gateway).await;
    shared.http.cached_serenity_ctx.set(ctx).unwrap();
    let token = "test-token".to_string();
    shared.http.cached_bot_token.set(token).unwrap();
    for (channel, name) in [(PRIMARY, "primary-agent"), (THREAD, "thread-agent")] {
        map_channel(&shared, channel, name).await;
        let mut core = shared.core.lock().await;
        let session = core.sessions.get_mut(&channel).unwrap();
        session.current_path = Some(workdir.path().display().to_string());
        session.session_id = Some("primary-sid".into());
        session.cleared = true;
        session.history.push(HistoryItem {
            item_type: HistoryType::User,
            content: "primary talk".into(),
        });
        drop(core);
        let tmux_name = ProviderKind::Claude.build_tmux_session_name(name);
        sqlx::query(
            "INSERT INTO sessions (session_key, provider, status, identity_kind,
                                   discord_token_hash, channel_id, claude_session_id)
             VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3, 'primary-sid')",
        )
        .bind(channel_key(&shared, &tmux_name))
        .bind(&shared.token_hash)
        .bind(channel.get().to_string())
        .execute(&pool)
        .await
        .unwrap();
    }
    Fixture {
        stubs,
        api,
        db,
        pool,
        shared,
        registry,
        _workdir: workdir,
        _clis: clis,
        _config: config_override,
        _tmux: tmux,
        _root: root,
    }
}

fn routine(mode: Option<Value>) -> Value {
    let mut metadata = json!({"routine_id":"routine-8117","routine_run_id":"run-8117",
        "agent_id":"agent8117","execution_strategy":"fresh","execution_mode":"process",
        "is_dm":false});
    if let Some(mode) = mode {
        metadata["routine_run_mode"] = mode;
    }
    metadata
}

async fn start(
    fixture: &Fixture,
    channel: ChannelId,
    hint: &str,
    metadata: Value,
    label: Option<&str>,
) -> Result<(), String> {
    let started = start_turn(fixture, channel, hint, metadata, label).await;
    started.map(drop)
}

/// The started turn's id.
async fn start_turn(
    fixture: &Fixture,
    channel: ChannelId,
    hint: &str,
    metadata: Value,
    label: Option<&str>,
) -> Result<String, String> {
    start_reserved_headless_agent_turn_with_owner_channel(
        &fixture.registry,
        channel,
        channel,
        ProviderKind::Claude,
        "nightly report".to_string(),
        Some("routine".into()),
        Some(metadata),
        Some(hint.to_string()),
        label.map(str::to_owned),
        reserve_headless_agent_turn(channel),
    )
    .await
    .map(|started| started.turn_id)
    .map_err(|error| format!("{error:?}"))
}

/// Serves the real runtime API on the fixture's database, so session hooks write real rows.
async fn real_api(fixture: &Fixture) -> tokio::task::AbortHandle {
    let config = crate::config::Config::default();
    let engine = crate::engine::PolicyEngine::new(&config).unwrap();
    let broadcast = crate::eventbus::new_broadcast();
    let buffer = crate::eventbus::spawn_batch_flusher(broadcast.clone());
    let app = crate::server::routes::api_router_with_pg(
        engine,
        config,
        broadcast,
        buffer,
        Some(fixture.registry.clone()),
        Some(fixture.pool.clone()),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    crate::services::discord::internal_api::init(port, Some(fixture.pool.clone()));
    let serving = axum::Router::new().nest("/api", app);
    let serving = serving.into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, serving).await.unwrap() }).abort_handle()
}

/// Everything of the channel a later turn of its own would read back.
async fn channel_state(fixture: &Fixture, channel: ChannelId) -> String {
    let session = {
        let core = fixture.shared.core.lock().await;
        let session = &core.sessions[&channel];
        let history: Vec<_> = session.history.iter().map(|item| &item.content).collect();
        format!(
            "{:?}",
            (
                &session.session_id,
                session.memento_context_loaded,
                &history,
                session.pending_uploads.len(),
                session.cleared,
                &session.channel_name,
                &session.current_path,
                session.worktree.is_some(),
            )
        )
    };
    let id = channel.get().to_string();
    let boundaries: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM channel_session_clear_boundaries WHERE channel_id = $1",
    )
    .bind(&id)
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    let rows: Vec<(String, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT session_key, agent_id, claude_session_id, identity_kind FROM sessions
         WHERE channel_id = $1 AND identity_kind = 'discord_channel' ORDER BY id",
    )
    .bind(&id)
    .fetch_all(&fixture.pool)
    .await
    .unwrap();
    format!("{session} boundaries={boundaries} rows={rows:?}")
}

async fn turn_active(fixture: &Fixture, channel: ChannelId) -> bool {
    let mailbox = crate::services::discord::mailbox_snapshot(&fixture.shared, channel).await;
    mailbox.cancel_token.is_some()
}

/// Releases the stub CLI and waits for the failed turn's completion to release the mailbox.
async fn turn_ended(fixture: &Fixture, channel: ChannelId) {
    std::fs::write(fixture.stubs.join("release"), "").unwrap();
    let ended = async {
        while turn_active(fixture, channel).await {
            tokio::task::yield_now().await;
        }
    };
    await_within(ended, WAIT_CAP).await;
    std::fs::remove_file(fixture.stubs.join("release")).unwrap();
}

/// The session keys every session status was sent for, and the identity kinds they named.
fn status_posts(fixture: &Fixture) -> (Vec<String>, Vec<String>) {
    let (mut keys, mut kinds) = (Vec::new(), Vec::new());
    for call in fixture.api.take() {
        let Some(body) = call.strip_prefix("POST /api/dispatched-sessions/webhook ") else {
            continue;
        };
        let body: Value = serde_json::from_str(body).unwrap();
        keys.push(body["session_key"].as_str().unwrap().to_string());
        kinds.extend(body["identity_kind"].as_str().map(str::to_owned));
    }
    keys.sort();
    keys.dedup();
    kinds.sort();
    kinds.dedup();
    (keys, kinds)
}

#[tokio::test]
async fn a_headless_run_leaves_its_primary_channel_session_untouched_pg() {
    if !api_child(&format!(
        "{TEST}a_headless_run_leaves_its_primary_channel_session_untouched_pg"
    )) {
        return;
    }
    let fixture = fixture().await;
    let headless = || routine(Some(json!("headless")));
    // A primary channel without a live session gets none from the run, at start or at its end.
    let live = fixture.shared.core.lock().await.sessions.remove(&PRIMARY);
    let earlier = Some("routine-process 9000 nightly agent8117");
    assert_eq!(
        start(&fixture, PRIMARY, "primary-agent", headless(), earlier).await,
        Ok(())
    );
    let sessionless = || async {
        !fixture
            .shared
            .core
            .lock()
            .await
            .sessions
            .contains_key(&PRIMARY)
    };
    assert!(sessionless().await, "at start");
    turn_ended(&fixture, PRIMARY).await;
    assert!(sessionless().await, "at end");
    // The failed turn keeps its row for a delivery retry; the next run starts from none.
    assert!(crate::services::discord::inflight::clear_inflight_state(
        &ProviderKind::Claude,
        PRIMARY.get()
    ));
    fixture
        .shared
        .core
        .lock()
        .await
        .sessions
        .insert(PRIMARY, live.unwrap());
    fixture.api.take();

    let before = channel_state(&fixture, PRIMARY).await;
    assert_eq!(
        start(&fixture, PRIMARY, "primary-agent", headless(), Some(LABEL)).await,
        Ok(())
    );
    // The run is pinned isolated under its own key and started cold.
    let row = load_inflight_state_read_only(&ProviderKind::Claude, PRIMARY.get()).unwrap();
    let per_run = channel_key(
        &fixture.shared,
        &ProviderKind::Claude.build_tmux_session_name(LABEL),
    );
    assert!(row.routine_presentation.isolated_run() && turn_active(&fixture, PRIMARY).await);
    assert_eq!(row.session_key.as_deref(), Some(per_run.as_str()));
    assert_eq!(row.session_id, None);
    let starts = STARTS_RESOLVED.lock().unwrap().clone();
    let resolved = starts.iter().rev().find(|(at, _)| *at == PRIMARY.get());
    let resolved = resolved.unwrap().1.clone();
    assert!(
        resolved.contains(&per_run) && resolved.ends_with("None"),
        "{resolved}"
    );
    assert_eq!(channel_state(&fixture, PRIMARY).await, before, "at start");
    turn_ended(&fixture, PRIMARY).await;
    assert_eq!(channel_state(&fixture, PRIMARY).await, before, "at end");
    // Session status went only to the per-run key, and never as the channel's own identity.
    assert_eq!(
        status_posts(&fixture),
        (vec![per_run], vec!["scheduled_snapshot".to_string()])
    );

    // Positive control: the same routine as a shared-session fresh run does clear its channel.
    let shared_before = channel_state(&fixture, THREAD).await;
    let label = Some("routine-process 9200 nightly agent8117");
    assert_eq!(
        start(&fixture, THREAD, "thread-agent", routine(None), label).await,
        Ok(())
    );
    let shared_after = channel_state(&fixture, THREAD).await;
    assert!(
        shared_before.starts_with("(Some(\"primary-sid\"),")
            && shared_before.contains("boundaries=0")
            && shared_after.starts_with("(None,")
            && shared_after.contains("boundaries=1"),
        "{shared_before} -> {shared_after}"
    );
    let shared_key = channel_key(
        &fixture.shared,
        &ProviderKind::Claude.build_tmux_session_name(label.unwrap()),
    );
    turn_ended(&fixture, THREAD).await;
    assert_eq!(
        status_posts(&fixture),
        (vec![shared_key], vec!["discord_channel".to_string()])
    );
    fixture.db.drop().await;
}

#[tokio::test]
async fn a_headless_mark_that_cannot_be_isolated_is_refused_without_a_shared_fallback_pg() {
    if !api_child(&format!(
        "{TEST}a_headless_mark_that_cannot_be_isolated_is_refused_without_a_shared_fallback_pg"
    )) {
        return;
    }
    let fixture = fixture().await;
    let before = channel_state(&fixture, PRIMARY).await;
    let headless = || routine(Some(json!("headless")));
    let mut tmux_backend = headless();
    tmux_backend["execution_mode"] = json!("tmux");
    let mut persistent = headless();
    persistent["execution_strategy"] = json!("persistent");
    let mut no_agent = headless();
    no_agent["agent_id"] = Value::Null;
    for (case, metadata, label) in [
        ("no label", headless(), None),
        ("blank label", headless(), Some("  ")),
        ("channel's own key", headless(), Some("primary-agent")),
        ("tmux backend", tmux_backend, Some(LABEL)),
        ("persistent", persistent, Some(LABEL)),
        ("no agent", no_agent, Some(LABEL)),
        ("unreadable mark", routine(Some(json!(true))), Some(LABEL)),
    ] {
        let refused = start(&fixture, PRIMARY, "primary-agent", metadata, label).await;
        assert!(
            refused
                .as_ref()
                .is_err_and(|error| error.contains("headless routine run refused")),
            "{case}: {refused:?}"
        );
        assert!(
            load_inflight_state_read_only(&ProviderKind::Claude, PRIMARY.get()).is_none(),
            "{case}"
        );
        assert!(!turn_active(&fixture, PRIMARY).await, "{case}");
        assert_eq!(channel_state(&fixture, PRIMARY).await, before, "{case}");
    }
    fixture.db.drop().await;
}

#[tokio::test]
async fn a_successful_headless_run_keeps_its_evidence_out_of_the_primary_context_pg() {
    use crate::db::session_transcripts as transcripts;
    use crate::services::routines::store::{NewRoutine, RoutineStore};
    if !api_child(&format!(
        "{TEST}a_successful_headless_run_keeps_its_evidence_out_of_the_primary_context_pg"
    )) {
        return;
    }
    const ANSWER: &str = "isolated-run-answer-8117";
    let fixture = fixture().await;
    let api = real_api(&fixture).await;
    let (pool, primary) = (&fixture.pool, PRIMARY.get().to_string());
    sqlx::query("INSERT INTO agents (id, name, provider) VALUES ('agent8117', 'A', 'claude')")
        .execute(pool)
        .await
        .unwrap();
    let store = RoutineStore::new_with_timezone_and_checkpoint_limit(
        Arc::new(pool.clone()),
        "Asia/Seoul",
        256 * 1024,
    );
    let routine_row = NewRoutine {
        agent_id: Some("agent8117".into()),
        fallback_agent_id: None,
        max_retries: Some(0),
        script_ref: "nightly.js".into(),
        name: "nightly".into(),
        status: None,
        execution_strategy: "fresh".into(),
        schedule: None,
        next_due_at: Some(chrono::Utc::now() - chrono::Duration::minutes(1)),
        checkpoint: None,
        discord_thread_id: None,
        timeout_secs: None,
    };
    store
        .attach_routine_with_execution(routine_row, None, 3)
        .await
        .unwrap();
    let run = store.claim_due_runs(1).await.unwrap().pop().unwrap().run_id;
    let since = chrono::Utc::now() - chrono::Duration::minutes(1);

    let headless = routine(Some(json!("headless")));
    let turn = start_turn(&fixture, PRIMARY, "primary-agent", headless, Some(LABEL)).await;
    let turn = turn.unwrap();
    // The routine attempt owns the turn on the primary channel, as the launcher records it.
    let result = json!({"channel_id": primary});
    let marked = store.mark_agent_turn_started(&run, &turn, Some(result), "agent8117", "primary");
    assert!(marked.await.unwrap());
    std::fs::write(fixture.stubs.join("answer"), ANSWER).unwrap();
    turn_ended(&fixture, PRIMARY).await;

    // The run's completion evidence is stored under its own key and the primary channel id.
    let evidence = || async {
        let stored: Option<(String, bool, Option<String>, i64)> = sqlx::query_as(
            "SELECT assistant_message, channel_context, session_key, id
             FROM session_transcripts WHERE turn_id = $1 AND channel_id = $2",
        )
        .bind(&turn)
        .bind(&primary)
        .fetch_optional(pool)
        .await
        .unwrap();
        stored
    };
    let stored = async {
        loop {
            match evidence().await {
                Some(stored) => break stored,
                None => tokio::task::yield_now().await,
            }
        }
    };
    let (answer, in_context, key, run_row) = await_within(stored, WAIT_CAP).await;
    let per_run = channel_key(
        &fixture.shared,
        &ProviderKind::Claude.build_tmux_session_name(LABEL),
    );
    assert_eq!((answer.as_str(), in_context), (ANSWER, false));
    assert_eq!(key.as_deref(), Some(per_run.as_str()));
    use crate::services::routines::agent_executor::reliability::find_headless_turn_completion;
    let completion = find_headless_turn_completion(pool, &turn, since).await;
    let completion = completion.unwrap().expect("the run's completion evidence");
    assert_eq!(completion.assistant_message.as_deref(), Some(ANSWER));

    // The primary conversation reads its own turns only, live and through a snapshot.
    let own = transcripts::PersistSessionTranscript {
        turn_id: "primary-own-turn",
        session_key: None,
        channel_id: Some(&primary),
        agent_id: None,
        provider: Some("claude"),
        dispatch_id: None,
        user_message: "primary question",
        assistant_message: "primary answer",
        events: &[],
        duration_ms: None,
        turn_started_at_millis: None,
    };
    assert!(transcripts::persist_turn_db(Some(pool), own).await.unwrap());
    let own_pair = vec![transcripts::ChannelTranscriptPair {
        user_message: "primary question".into(),
        assistant_message: "primary answer".into(),
    }];
    let recent = transcripts::fetch_recent_channel_pairs(pool, &primary, 10).await;
    assert_eq!(recent.unwrap(), own_pair);
    let mut tx = pool.begin().await.unwrap();
    let frontier = transcripts::fetch_channel_frontier_tx(&mut tx, &primary).await;
    let frontier = frontier.unwrap();
    let snapshot =
        transcripts::fetch_channel_pairs_up_to_frontier_tx(&mut tx, &primary, i64::MAX, 10).await;
    assert_eq!(snapshot.unwrap(), own_pair);
    drop(tx);
    assert!(frontier > run_row, "the frontier is the primary's own turn");
    sqlx::query("DELETE FROM session_transcripts WHERE turn_id = 'primary-own-turn'")
        .execute(pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    let frontier = transcripts::fetch_channel_frontier_tx(&mut tx, &primary).await;
    assert_eq!(
        frontier.unwrap(),
        0,
        "the run's row is no frontier of the channel"
    );
    drop(tx);

    // The run's session row is its own; the primary's row keeps its provider session.
    let rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT session_key, identity_kind, claude_session_id FROM sessions
         WHERE channel_id = $1 ORDER BY id",
    )
    .bind(&primary)
    .fetch_all(pool)
    .await
    .unwrap();
    let primary_key = channel_key(
        &fixture.shared,
        &ProviderKind::Claude.build_tmux_session_name("primary-agent"),
    );
    let kind = |kind: &str| Some(kind.to_string());
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(
        rows[0],
        (primary_key, kind("discord_channel"), kind("primary-sid"))
    );
    assert_eq!(
        (&rows[1].0, &rows[1].1),
        (&per_run, &kind("scheduled_snapshot"))
    );
    let observed: Vec<(String, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT status, claude_session_id, thread_channel_id, agent_id FROM sessions
         WHERE session_key = $1",
    )
    .bind(&per_run)
    .fetch_all(pool)
    .await
    .unwrap();
    eprintln!("PER_RUN_ROW status/claude_session_id/thread_channel_id/agent_id = {observed:?}");
    api.abort();
    fixture.db.drop().await;
}
