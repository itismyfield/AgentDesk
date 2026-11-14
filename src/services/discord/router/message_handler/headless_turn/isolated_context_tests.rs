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
    _root: crate::config::TestRuntimeRootGuard,
    _tmux: ModeTmux,
    /// Directory of the stub CLIs and their `release` file.
    stubs: std::path::PathBuf,
    _clis: [crate::config::TestEnvVarGuard; 2],
    /// Records every runtime API call, the session status hook among them.
    api: Recorder,
    db: crate::db::auto_queue::test_support::TestPostgresDb,
    pool: PgPool,
    shared: Arc<SharedData>,
    registry: Arc<HealthRegistry>,
    _workdir: tempfile::TempDir,
}

/// The provider CLI is pinned to a stub that holds its turn open until `release` and then fails.
async fn fixture() -> Fixture {
    let root = crate::config::TestRuntimeRootGuard::with_isolated_config();
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
        "#!/bin/sh\n",
        "case \"$*\" in *--version*) echo 0.0.0-stub; exit 0 ;; esac\n",
        "while [ ! -e \"$(dirname \"$0\")/release\" ]; do sleep 0.05; done\nexit 1\n"
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
        _root: root,
        _tmux: tmux,
        stubs,
        _clis: clis,
        api,
        db,
        pool,
        shared,
        registry,
        _workdir: workdir,
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
    .map(|_| ())
    .map_err(|error| format!("{error:?}"))
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
