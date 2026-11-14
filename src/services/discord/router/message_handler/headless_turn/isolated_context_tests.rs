//! A headless routine run through the real headless start on its agent's primary channel: the
//! channel's session, clear boundary and session row stay as they were from start to turn end.
use crate::services::discord::SharedData;
use crate::services::discord::admin_host_guard::tests::{ModeTmux, Recorder, Tmux, api_child};
use crate::services::discord::health::{
    HealthRegistry, reserve_headless_agent_turn,
    start_reserved_headless_agent_turn_with_owner_channel,
};
use crate::services::discord::host_defer_gate::tests::{map_channel, postgres};
use crate::services::discord::host_teardown_gate::test_support::{channel_key, shared_on};
use crate::services::discord::inflight::load_inflight_state_read_only;
use crate::services::discord::router::message_handler::headless_turn::STARTS_RESOLVED;
use crate::services::provider::ProviderKind;
use crate::services::routines::agent_executor::RoutineAgentExecutor;
use crate::services::routines::execution_mode::{ExecutionDefinition, RoutineExecutionMode};
use crate::services::routines::store::{ClaimedRoutineRun, NewRoutine, RoutineStore};
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
    discord: crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock::DiscordMockState,
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
        "echo run >> \"$d/spawned\"\n",
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
    // A real token hash shape, so the runtime API accepts the identity a session hook names.
    let mut shared = shared_on(&pool).await;
    Arc::get_mut(&mut shared).unwrap().token_hash = "discord_8117811781178117".to_string();
    let registry = Arc::new(HealthRegistry::new());
    registry
        .register("claude".to_string(), shared.clone())
        .await;
    use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
    let discord = discord_mock::DiscordMockState::new();
    for channel in [PRIMARY, THREAD] {
        discord.name_channel(channel.get(), "discord-named");
    }
    let (proxy, gateway, _server) = discord_mock::start(discord.clone()).await;
    let ctx = discord_mock::serenity_context(proxy, gateway).await;
    registry
        .register_http("claude".to_string(), ctx.http.clone())
        .await;
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
        discord,
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
    let rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT session_key, agent_id, claude_session_id FROM sessions
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

/// Releases the stub CLI and waits for the turn to release the mailbox and finish finalizing.
async fn turn_ended(fixture: &Fixture, channel: ChannelId) {
    std::fs::write(fixture.stubs.join("release"), "").unwrap();
    let finalizing = &fixture.shared.restart.finalizing_turns;
    let ended = async {
        while turn_active(fixture, channel).await
            || finalizing.load(std::sync::atomic::Ordering::Relaxed) > 0
        {
            tokio::task::yield_now().await;
        }
    };
    await_within(ended, WAIT_CAP).await;
    std::fs::remove_file(fixture.stubs.join("release")).unwrap();
}

/// What the runtime API was sent about sessions: the keys of every status, the identity kinds
/// they named, the keys reported ended, and how many session rows were deleted.
fn status_posts(fixture: &Fixture) -> (Vec<String>, Vec<String>, Vec<String>, usize) {
    let (mut keys, mut kinds, mut ended, mut deleted) = (Vec::new(), Vec::new(), Vec::new(), 0);
    for call in fixture.api.take() {
        deleted += usize::from(call.starts_with("DELETE /api/dispatched-sessions/webhook"));
        let Some(body) = call.strip_prefix("POST /api/dispatched-sessions/webhook ") else {
            continue;
        };
        let body: Value = serde_json::from_str(body).unwrap();
        let key = body["session_key"].as_str().unwrap().to_string();
        if body["status"] == "disconnected" {
            ended.push(key.clone());
        }
        keys.push(key);
        kinds.extend(body["identity_kind"].as_str().map(str::to_owned));
    }
    keys.sort();
    keys.dedup();
    kinds.sort();
    kinds.dedup();
    (keys, kinds, ended, deleted)
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
    // Session status went only to the per-run key, never as the channel's own identity, and
    // the finished run's own row was reported ended and removed.
    let snapshot = vec!["scheduled_snapshot".to_string()];
    assert_eq!(
        status_posts(&fixture),
        (vec![per_run.clone()], snapshot, vec![per_run], 1)
    );

    // Positive control: the same routine as a shared-session fresh run does clear its channel.
    let shared_before = channel_state(&fixture, THREAD).await;
    let label = "routine-process 9200 nightly agent8117";
    assert_eq!(
        start(&fixture, THREAD, "thread-agent", routine(None), Some(label)).await,
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
        &ProviderKind::Claude.build_tmux_session_name(label),
    );
    turn_ended(&fixture, THREAD).await;
    // A turn that shares its channel's session never ends or removes that session's row.
    let channel = vec!["discord_channel".to_string()];
    assert_eq!(
        status_posts(&fixture),
        (vec![shared_key], channel, Vec::new(), 0)
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

type Claim = (
    RoutineStore,
    RoutineAgentExecutor,
    ClaimedRoutineRun,
    String,
);

/// A headless routine of the fixture's agent, claimed for the real launcher:
/// `(store, executor, claimed run, routine id)`.
async fn headless_claim(fixture: &Fixture) -> Claim {
    routine_claim(fixture, true, None, 0).await
}

/// A fresh routine claimed for the real launcher: headless, or null-mode with its saved thread.
async fn routine_claim(
    fixture: &Fixture,
    headless: bool,
    thread: Option<ChannelId>,
    retries: i32,
) -> Claim {
    routine_claim_with(fixture, headless, thread, (retries, None)).await
}

/// The same claim with its failure policy: `(max retries, fallback agent)`.
async fn routine_claim_with(
    fixture: &Fixture,
    headless: bool,
    thread: Option<ChannelId>,
    (retries, fallback): (i32, Option<&str>),
) -> Claim {
    let pool = &fixture.pool;
    // Only the routine attached here is due, so the claim below is its run.
    sqlx::query("UPDATE routines SET status = 'paused'")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_cc)
         VALUES ('agent8117', 'A', 'claude', $1) ON CONFLICT (id) DO NOTHING",
    )
    .bind(PRIMARY.get().to_string())
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
        fallback_agent_id: fallback.map(str::to_string),
        max_retries: Some(retries),
        script_ref: "nightly.js".into(),
        name: "nightly".into(),
        status: None,
        execution_strategy: "fresh".into(),
        schedule: None,
        next_due_at: Some(chrono::Utc::now() - chrono::Duration::minutes(1)),
        checkpoint: None,
        discord_thread_id: thread.map(|thread| thread.get().to_string()),
        timeout_secs: None,
    };
    let definition = ExecutionDefinition {
        mode: RoutineExecutionMode::Headless,
        runner: json!({"kind": "quickjs", "script_ref": "nightly.js"}),
    };
    let definition = headless.then_some(&definition);
    let attached = store.attach_routine_with_execution(routine_row, definition, 3);
    let routine_id = attached.await.unwrap().id;
    let claimed = store.claim_due_runs(1).await.unwrap().pop().unwrap();
    assert_eq!(claimed.routine_id, routine_id);
    let registry = Some(fixture.registry.clone());
    let executor = RoutineAgentExecutor::new(Arc::new(pool.clone()), registry, 60)
        .with_headless_fresh_agents(true);
    (store, executor, claimed, routine_id)
}

/// Makes a deferred or retrying run due now, its deferral window opened `opened_mins` ago.
async fn due_now(fixture: &Fixture, run: &str, opened_mins: i64) {
    let opened = (chrono::Utc::now() - chrono::Duration::minutes(opened_mins)).to_rfc3339();
    sqlx::query(
        "UPDATE routine_runs
         SET result_json = jsonb_set(result_json, '{deferred_since}', to_jsonb($2::text)),
             next_retry_at = NOW() - INTERVAL '1 second'
         WHERE id = $1",
    )
    .bind(run)
    .bind(opened)
    .execute(&fixture.pool)
    .await
    .unwrap();
}

/// `(status, retry_count, result status)` of a run.
async fn run_state(fixture: &Fixture, run: &str) -> (String, i32, Option<String>) {
    sqlx::query_as(
        "SELECT status, retry_count, result_json ->> 'status' FROM routine_runs WHERE id = $1",
    )
    .bind(run)
    .fetch_one(&fixture.pool)
    .await
    .unwrap()
}

/// The channel's session rows, whole.
async fn session_rows(fixture: &Fixture, channel: ChannelId) -> Vec<String> {
    sqlx::query_scalar("SELECT to_jsonb(s)::text FROM sessions s WHERE channel_id = $1 ORDER BY id")
        .bind(channel.get().to_string())
        .fetch_all(&fixture.pool)
        .await
        .unwrap()
}

fn nothing_ran(fixture: &Fixture, channels: &[ChannelId]) -> bool {
    let idle = |channel: &ChannelId| {
        load_inflight_state_read_only(&ProviderKind::Claude, channel.get()).is_none()
    };
    !fixture.stubs.join("spawned").exists() && channels.iter().all(idle)
}

#[tokio::test]
async fn a_successful_headless_run_keeps_its_evidence_out_of_the_primary_context_pg() {
    use crate::db::session_transcripts as transcripts;
    if !api_child(&format!(
        "{TEST}a_successful_headless_run_keeps_its_evidence_out_of_the_primary_context_pg"
    )) {
        return;
    }
    const ANSWER: &str = "isolated-run-answer-8117";
    // No channel is cut over to the O writer, as on a production boot that decided so.
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let fixture = fixture().await;
    let api = real_api(&fixture).await;
    let (pool, primary) = (&fixture.pool, PRIMARY.get().to_string());
    let (store, executor, claimed, routine_id) = headless_claim(&fixture).await;
    let (run, since) = (claimed.run_id.clone(), chrono::Utc::now());
    let before = channel_state(&fixture, PRIMARY).await;
    let primary_rows = session_rows(&fixture, PRIMARY).await;

    // The real launcher starts the run it claimed as headless on the agent's primary channel.
    let prompt = "nightly report".to_string();
    let started = executor.start_agent_run(&store, claimed, prompt, None, None, None, None, false);
    let started = started.await.unwrap();
    assert_eq!(started.status, "running", "{started:?}");
    let launched = started.result_json.unwrap();
    assert_eq!(
        (&launched["channel_id"], &launched["discord_thread_id"]),
        (&json!(primary), &Value::Null),
        "no routine thread: {launched}"
    );
    assert_eq!(launched["execution_mode"], "process", "{launched}");
    let turn = launched["turn_id"].as_str().unwrap().to_string();
    // Its launch mark isolated the turn and keeps its body off Discord.
    let row = load_inflight_state_read_only(&ProviderKind::Claude, PRIMARY.get()).unwrap();
    assert!(row.routine_presentation.isolated_run() && row.silent_turn);
    let per_run = row.session_key.clone().unwrap();
    assert!(per_run.contains("routine-process"), "{per_run}");
    assert_eq!(channel_state(&fixture, PRIMARY).await, before, "at start");
    // While it runs, the run has a session row of its own, apart from the primary's.
    let own: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT session_key, identity_kind FROM sessions WHERE channel_id = $1 ORDER BY id",
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
    assert_eq!(
        own,
        [
            (primary_key, kind("discord_channel")),
            (per_run.clone(), kind("scheduled_snapshot"))
        ]
    );
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
    assert_eq!((answer.as_str(), in_context), (ANSWER, false));
    assert_eq!(key.as_deref(), Some(per_run.as_str()));
    use crate::services::routines::agent_executor::reliability::find_headless_turn_completion;
    let completion = find_headless_turn_completion(pool, &turn, since).await;
    let completion = completion.unwrap().expect("the run's completion evidence");
    assert_eq!(completion.assistant_message.as_deref(), Some(ANSWER));
    // The routine's own poll closes the run on that evidence; no thread was made for it.
    let polled = executor.poll_agent_runs(&store, 10, false).await.unwrap();
    let closed: Vec<_> = polled.iter().map(|run| run.status.as_str()).collect();
    assert_eq!(closed, ["succeeded"], "{polled:?}");
    let (status, thread): (String, Option<String>) = sqlx::query_as(
        "SELECT rr.status, r.discord_thread_id FROM routine_runs rr
         JOIN routines r ON r.id = rr.routine_id WHERE rr.id = $1 AND r.id = $2",
    )
    .bind(&run)
    .bind(&routine_id)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!((status.as_str(), thread), ("succeeded", None));

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

    // The finished run's own session row is gone; the primary's row is byte-for-byte as before.
    let over = async {
        while load_inflight_state_read_only(&ProviderKind::Claude, PRIMARY.get()).is_some() {
            tokio::task::yield_now().await;
        }
    };
    await_within(over, WAIT_CAP).await;
    assert_eq!(session_rows(&fixture, PRIMARY).await, primary_rows);
    api.abort();
    fixture.db.drop().await;
}

#[tokio::test]
async fn a_headless_run_waits_behind_a_busy_primary_and_starts_no_provider_pg() {
    use crate::services::provider::CancelToken;
    use poise::serenity_prelude::{MessageId, UserId};
    if !api_child(&format!(
        "{TEST}a_headless_run_waits_behind_a_busy_primary_and_starts_no_provider_pg"
    )) {
        return;
    }
    let fixture = fixture().await;
    // A retry budget is left over: the closed window still ends the run.
    let (store, executor, claimed, _) = routine_claim(&fixture, true, None, 2).await;
    let run = claimed.run_id.clone();
    // A person's turn holds the primary mailbox.
    let human = Arc::new(CancelToken::new());
    let mailbox = fixture.shared.mailbox(PRIMARY);
    let claim = mailbox.try_start_turn(human.clone(), UserId::new(7), MessageId::new(77));
    assert!(claim.await);
    let before = channel_state(&fixture, PRIMARY).await;

    let prompt = "nightly report".to_string();
    let start = executor.start_agent_run(&store, claimed, prompt, None, None, None, None, false);
    let deferred = start.await.unwrap();
    assert_eq!(deferred.status, "deferred", "{deferred:?}");
    assert_eq!(
        deferred.result_json.unwrap()["deferred_reason"],
        "mailbox_busy"
    );

    // The deferral window closes with the person still at work: the run fails unstarted.
    due_now(&fixture, &run, 11).await;
    let polled = executor.poll_agent_runs(&store, 10, false).await.unwrap();
    let closed: Vec<_> = polled.iter().map(|run| run.status.as_str()).collect();
    assert_eq!(closed, ["failed"], "{polled:?}");

    // No provider was spawned, and the person's turn was neither stopped nor replaced.
    assert!(!fixture.stubs.join("spawned").exists());
    assert!(load_inflight_state_read_only(&ProviderKind::Claude, PRIMARY.get()).is_none());
    let active = crate::services::discord::mailbox_snapshot(&fixture.shared, PRIMARY).await;
    let active = active.cancel_token.expect("the person's turn");
    assert!(Arc::ptr_eq(&active, &human));
    assert!(!human.cancelled.load(std::sync::atomic::Ordering::Acquire));
    assert_eq!(channel_state(&fixture, PRIMARY).await, before);
    // The run names what it waited for.
    let waited = ("failed".to_string(), 0, Some("primary_busy".to_string()));
    assert_eq!(run_state(&fixture, &run).await, waited);
    fixture.db.drop().await;
}

/// One way a launch can lose its contract.
type LaunchEdit<'a> = &'a dyn Fn(&mut Value);

#[tokio::test]
async fn a_headless_claim_whose_launch_lost_its_contract_is_refused_before_admission_pg() {
    if !api_child(&format!(
        "{TEST}a_headless_claim_whose_launch_lost_its_contract_is_refused_before_admission_pg"
    )) {
        return;
    }
    let fixture = fixture().await;
    let (_store, _executor, claimed, _) = headless_claim(&fixture).await;
    let before = channel_state(&fixture, PRIMARY).await;
    // The launch names its claimed run, as the launcher's metadata does.
    let launch = |edit: LaunchEdit| {
        let mut metadata = routine(Some(json!("headless")));
        metadata["routine_run_id"] = json!(claimed.run_id);
        metadata["silent"] = json!(true);
        edit(&mut metadata);
        metadata
    };
    let cases: [(&str, LaunchEdit); 6] = [
        ("no mark", &|launch| {
            drop(launch.as_object_mut().unwrap().remove("routine_run_mode"))
        }),
        ("null mark", &|launch| {
            launch["routine_run_mode"] = Value::Null
        }),
        ("another mode", &|launch| {
            launch["routine_run_mode"] = json!("thread")
        }),
        ("unreadable mark", &|launch| {
            launch["routine_run_mode"] = json!(true)
        }),
        ("not silent", &|launch| launch["silent"] = json!(false)),
        ("silent missing", &|launch| {
            drop(launch.as_object_mut().unwrap().remove("silent"))
        }),
    ];
    for (case, edit) in cases {
        let refused = start(
            &fixture,
            PRIMARY,
            "primary-agent",
            launch(edit),
            Some(LABEL),
        )
        .await;
        assert!(
            refused
                .as_ref()
                .is_err_and(|error| error.contains("lost the headless contract")),
            "{case}: {refused:?}"
        );
        assert!(nothing_ran(&fixture, &[PRIMARY]), "{case}");
        assert!(!turn_active(&fixture, PRIMARY).await, "{case}");
        assert_eq!(channel_state(&fixture, PRIMARY).await, before, "{case}");
    }
    // The whole contract starts the isolated run.
    let whole = launch(&|_| {});
    assert_eq!(
        start(&fixture, PRIMARY, "primary-agent", whole, Some(LABEL)).await,
        Ok(())
    );
    let row = load_inflight_state_read_only(&ProviderKind::Claude, PRIMARY.get()).unwrap();
    assert!(row.routine_presentation.isolated_run());
    turn_ended(&fixture, PRIMARY).await;
    fixture.db.drop().await;
}

#[tokio::test]
async fn a_headless_run_keeps_its_retries_for_starts_that_submitted_nothing_pg() {
    if !api_child(&format!(
        "{TEST}a_headless_run_keeps_its_retries_for_starts_that_submitted_nothing_pg"
    )) {
        return;
    }
    const ANSWER: &str = "retried-run-answer-8117";
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let fixture = fixture().await;
    let api = real_api(&fixture).await;
    let (store, executor, claimed, _) = routine_claim(&fixture, true, None, 2).await;
    let run = claimed.run_id.clone();
    let channel = |value: Option<String>| async {
        sqlx::query("UPDATE agents SET discord_channel_cc = $1 WHERE id = 'agent8117'")
            .bind(value)
            .execute(&fixture.pool)
            .await
            .unwrap();
    };
    let polled = || async {
        due_now(&fixture, &run, 0).await;
        let polled = executor.poll_agent_runs(&store, 10, false).await.unwrap();
        polled
            .iter()
            .map(|run| run.status.clone())
            .collect::<Vec<_>>()
    };

    // The agent has no primary channel: both starts fail before anything is submitted, and
    // each only spends one retry.
    channel(None).await;
    let prompt = "nightly report".to_string();
    let start = executor.start_agent_run(&store, claimed, prompt, None, None, None, None, false);
    assert_eq!(start.await.unwrap().status, "running");
    assert_eq!(run_state(&fixture, &run).await.1, 1);
    assert_eq!(polled().await, ["running"]);
    assert_eq!(run_state(&fixture, &run).await.1, 2);
    assert!(nothing_ran(&fixture, &[PRIMARY]));

    // The channel is back: the same run starts once and succeeds.
    channel(Some(PRIMARY.get().to_string())).await;
    assert_eq!(polled().await, ["running"]);
    assert!(turn_active(&fixture, PRIMARY).await);
    std::fs::write(fixture.stubs.join("answer"), ANSWER).unwrap();
    turn_ended(&fixture, PRIMARY).await;
    let closed = async {
        loop {
            let polled = executor.poll_agent_runs(&store, 10, false).await.unwrap();
            if let Some(run) = polled.first() {
                break run.status.clone();
            }
            tokio::task::yield_now().await;
        }
    };
    assert_eq!(await_within(closed, WAIT_CAP).await, "succeeded");
    let spawned = std::fs::read_to_string(fixture.stubs.join("spawned")).unwrap();
    assert_eq!(spawned.lines().count(), 1);
    api.abort();
    fixture.db.drop().await;
}

#[tokio::test]
async fn an_unreachable_routine_thread_defers_the_run_and_never_runs_on_the_primary_pg() {
    if !api_child(&format!(
        "{TEST}an_unreachable_routine_thread_defers_the_run_and_never_runs_on_the_primary_pg"
    )) {
        return;
    }
    let fixture = fixture().await;
    // Discord answers the saved thread as a thread under the primary channel.
    fixture.discord.unname_channel(THREAD.get());
    fixture.discord.add_thread(THREAD.get(), PRIMARY.get());
    let before = channel_state(&fixture, PRIMARY).await;
    let prompt = || "nightly report".to_string();
    let pool = &fixture.pool;
    let thread_of = |routine: String| async move {
        let saved: Option<String> =
            sqlx::query_scalar("SELECT discord_thread_id FROM routines WHERE id = $1")
                .bind(routine)
                .fetch_one(pool)
                .await
                .unwrap();
        saved
    };

    // Discord answers 500 for the saved thread: the run waits for that same thread.
    let (store, executor, claimed, routine_id) =
        routine_claim(&fixture, false, Some(THREAD), 0).await;
    let run = claimed.run_id.clone();
    fixture.discord.fail_channel(THREAD.get());
    let start = executor.start_agent_run(&store, claimed, prompt(), None, None, None, None, false);
    let deferred = start.await.unwrap();
    assert_eq!(deferred.status, "deferred", "{deferred:?}");
    assert_eq!(
        deferred.result_json.unwrap()["deferred_reason"],
        "thread_unavailable"
    );
    assert!(nothing_ran(&fixture, &[PRIMARY, THREAD]));
    assert_eq!(channel_state(&fixture, PRIMARY).await, before);
    let saved = Some(THREAD.get().to_string());
    assert_eq!(thread_of(routine_id.clone()).await, saved);

    // The thread answers again inside the window: the same run starts on the same thread.
    fixture.discord.heal_channel(THREAD.get());
    due_now(&fixture, &run, 1).await;
    let polled = executor.poll_agent_runs(&store, 10, false).await.unwrap();
    assert_eq!(polled.len(), 1, "{polled:?}");
    let started = polled[0].result_json.clone().unwrap();
    assert_eq!(
        (polled[0].status.as_str(), &started["channel_id"]),
        ("running", &json!(THREAD.get().to_string())),
        "{started}"
    );
    assert!(turn_active(&fixture, THREAD).await && !turn_active(&fixture, PRIMARY).await);
    assert_eq!(thread_of(routine_id).await, saved);
    turn_ended(&fixture, THREAD).await;
    assert_eq!(channel_state(&fixture, PRIMARY).await, before);
    sqlx::query("UPDATE routine_runs SET status = 'failed' WHERE id = $1")
        .bind(&run)
        .execute(&fixture.pool)
        .await
        .unwrap();
    std::fs::remove_file(fixture.stubs.join("spawned")).unwrap();
    assert!(crate::services::discord::inflight::clear_inflight_state(
        &ProviderKind::Claude,
        THREAD.get()
    ));

    // The thread stays unreachable past the window: the run ends for that reason, unstarted,
    // whatever retry budget and fallback agent it has left.
    let spare = (2, Some("fallback8117"));
    let (store, executor, claimed, _) =
        routine_claim_with(&fixture, false, Some(THREAD), spare).await;
    let run = claimed.run_id.clone();
    fixture.discord.fail_channel(THREAD.get());
    let start = executor.start_agent_run(&store, claimed, prompt(), None, None, None, None, false);
    assert_eq!(start.await.unwrap().status, "deferred");
    due_now(&fixture, &run, 11).await;
    let polled = executor.poll_agent_runs(&store, 10, false).await.unwrap();
    let closed: Vec<_> = polled.iter().map(|run| run.status.as_str()).collect();
    assert_eq!(closed, ["failed"], "{polled:?}");
    let unavailable = Some("thread_unavailable".to_string());
    let ended = ("failed".to_string(), 0, unavailable);
    assert_eq!(run_state(&fixture, &run).await, ended);
    // No second window opens: the thread answering again starts nothing for that run.
    fixture.discord.heal_channel(THREAD.get());
    let polled = executor.poll_agent_runs(&store, 10, false).await.unwrap();
    assert!(polled.is_empty(), "{polled:?}");
    let retry_at: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT next_retry_at FROM routine_runs WHERE id = $1")
            .bind(&run)
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    assert_eq!(retry_at, None);
    assert_eq!(run_state(&fixture, &run).await, ended);
    assert!(nothing_ran(&fixture, &[PRIMARY, THREAD]));

    // Discord refuses for good (the saved thread and the replacement both 404): the run fails
    // at once and still does not run on the primary.
    let gone = ChannelId::new(1_479_671_302_387_081_173);
    let (store, executor, claimed, _) =
        routine_claim_with(&fixture, false, Some(gone), spare).await;
    let run = claimed.run_id.clone();
    let start = executor.start_agent_run(&store, claimed, prompt(), None, None, None, None, false);
    let failed = start.await.unwrap();
    assert_eq!(failed.status, "failed", "{failed:?}");
    let error = failed.error.unwrap();
    assert!(error.contains("create routine discord thread"), "{error}");
    assert_eq!(run_state(&fixture, &run).await, ended);
    assert!(nothing_ran(&fixture, &[PRIMARY, THREAD]));
    assert_eq!(channel_state(&fixture, PRIMARY).await, before);
    fixture.db.drop().await;
}

#[tokio::test]
async fn people_who_write_while_a_headless_run_holds_the_primary_wait_once_and_outlive_it_pg() {
    use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
    use poise::serenity_prelude::{self as serenity, UserId};
    if !api_child(&format!(
        "{TEST}people_who_write_while_a_headless_run_holds_the_primary_wait_once_and_outlive_it_pg"
    )) {
        return;
    }
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let fixture = fixture().await;
    fixture.discord.allow_channel(PRIMARY.get());
    {
        let mut settings = fixture.shared.settings.write().await;
        settings.owner_user_id = Some(discord_mock::USER_ID);
        settings.allow_all_users = true;
    }
    let (store, executor, claimed, _) = headless_claim(&fixture).await;
    let actor = fixture.shared.mailbox(PRIMARY);
    let prompt = "nightly report".to_string();
    let start = executor.start_agent_run(&store, claimed, prompt, None, None, None, None, false);
    assert_eq!(start.await.unwrap().status, "running");
    let run_turn = crate::services::discord::mailbox_snapshot(&fixture.shared, PRIMARY).await;
    let run_turn = run_turn.cancel_token.expect("the headless turn");

    // Two messages reach the production intake while the run holds the primary mailbox.
    let ctx = fixture
        .shared
        .http
        .cached_serenity_ctx
        .get()
        .unwrap()
        .clone();
    let inputs = [
        (1_479_671_302_387_090_001_u64, "first question"),
        (1_479_671_302_387_090_002_u64, "second question"),
    ];
    for (id, text) in inputs {
        let mut message = discord_mock::user_message(id, text);
        (message.channel_id, message.author.id) = (PRIMARY, UserId::new(id));
        let event = serenity::FullEvent::Message {
            new_message: message,
        };
        let data = crate::services::discord::Data {
            shared: fixture.shared.clone(),
            token: "test-token".to_string(),
            provider: ProviderKind::Claude,
        };
        let ctx = ctx.clone();
        let intake = tokio::spawn(async move {
            let handled = crate::services::discord::router::handle_event(&ctx, &event, &data);
            Box::pin(handled).await.map_err(|error| error.to_string())
        });
        assert_eq!(
            await_within(intake, WAIT_CAP).await.unwrap(),
            Ok(()),
            "{text}"
        );
    }
    // Each input waits exactly once, in order, behind the run that still holds the mailbox.
    let waiting = crate::services::discord::mailbox_snapshot(&fixture.shared, PRIMARY).await;
    let units: Vec<u64> = waiting
        .intervention_queue
        .iter()
        .map(|unit| unit.message_id.get())
        .collect();
    let sources: Vec<u64> = waiting
        .intervention_queue
        .iter()
        .flat_map(|unit| unit.source_message_ids.iter().map(|id| id.get()))
        .collect();
    assert_eq!(sources, [inputs[0].0, inputs[1].0], "{units:?}");
    assert!(
        waiting
            .cancel_token
            .is_some_and(|token| Arc::ptr_eq(&token, &run_turn))
    );
    // The run ends and lets go of the mailbox: nothing waiting is lost or repeated.
    std::fs::write(fixture.stubs.join("answer"), "headless-answer-8117").unwrap();
    std::fs::write(fixture.stubs.join("release"), "").unwrap();
    let held = |now: &crate::services::turn_orchestrator::ChannelMailboxSnapshot| {
        let token = now.cancel_token.as_ref();
        token.is_some_and(|token| Arc::ptr_eq(token, &run_turn))
    };
    // What the mailbox holds for the waiting inputs: `(being served, still waiting)`.
    let holds = |now: &crate::services::turn_orchestrator::ChannelMailboxSnapshot| {
        let taken = [now.pending_user_dispatch, now.active_user_message_id];
        let mut serving: Vec<u64> = taken.iter().flatten().map(|id| id.get()).collect();
        serving.retain(|id| units.contains(id));
        serving.dedup();
        let waiting: Vec<u64> = now
            .intervention_queue
            .iter()
            .map(|unit| unit.message_id.get())
            .collect();
        (serving, waiting)
    };
    let released = async {
        loop {
            let now = crate::services::discord::mailbox_snapshot(&fixture.shared, PRIMARY).await;
            if !held(&now) {
                break holds(&now);
            }
            tokio::task::yield_now().await;
        }
    };
    let (serving, waiting) = await_within(released, WAIT_CAP).await;
    assert_eq!([serving, waiting].concat(), units, "after the run");
    let spawned = std::fs::read_to_string(fixture.stubs.join("spawned")).unwrap();
    assert_eq!(
        spawned.lines().count(),
        1,
        "only the run's own provider ran"
    );

    let current = fixture.shared.mailbox_peek(PRIMARY);
    assert!(current.is_some_and(|current| current.same_actor(&actor)));
    fixture.db.drop().await;
}

#[path = "isolated_context_thread_tests.rs"]
mod thread_tests;
