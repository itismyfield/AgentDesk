//! A crossed-turn rebind restarts the Codex relay in a new, shorter file. The real
//! watchers read both files into one session relay, and Discord gets the new turn once.

use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::cluster::relay_producer_registry::global_relay_producer_registry;
use crate::services::cluster::session_matcher::MatchedChannel;
use crate::services::cluster::stream_relay::spawn_stream_relay;
use crate::services::discord::health::test_support::MockDiscord;
use crate::services::discord::inflight::{self, InflightTurnState, RelayOwnerKind};
use crate::services::provider::ProviderKind;
use crate::services::session_host::test_support::InjectedLivenessGuard;
use crate::services::session_host::{HostLiveness, HostSessionRef};
use poise::serenity_prelude::{ChannelId, Http};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const CHILD: &str = "AGENTDESK_7120_READ_SOURCE_SWITCH_CHILD";
const NEW_TURN: &str = "new-turn-7120";

fn rollout_line(value: serde_json::Value) -> String {
    format!("{value}\n")
}

fn user(text: &str) -> String {
    rollout_line(
        serde_json::json!({"timestamp":"2099-01-01T00:00:00Z", "type":"response_item",
        "payload":{"type":"message", "role":"user", "content":[{"type":"input_text", "text":text}]}}),
    )
}

fn answer(id: &str, text: &str) -> String {
    rollout_line(
        serde_json::json!({"type":"response_item", "payload":{"type":"message",
        "role":"assistant", "id":id, "content":[{"type":"output_text", "text":text}]}}),
    )
}

/// The row's turn, then the next prompt with its answer and completion.
fn crossed_rollout() -> String {
    let complete = serde_json::json!({"type":"event_msg",
        "payload":{"type":"task_complete", "last_agent_message":NEW_TURN}});
    format!(
        "{}{}{}{}{}",
        user("current turn"),
        answer("cur", "old-answer-7120"),
        user("next turn"),
        answer("new", NEW_TURN),
        rollout_line(complete)
    )
}

/// Message sends and edits `discord` received that carry `text`.
fn posted(discord: &MockDiscord, channel: u64, text: &str) -> usize {
    let requests = discord.requests_for(channel);
    let sends = requests
        .iter()
        .filter(|r| r.starts_with("POST ") || r.starts_with("PATCH "));
    sends.filter(|r| r.contains(text)).count()
}

/// Times the session relay moved this channel's open turn to another read file.
fn read_source_switches(channel: u64) -> u64 {
    let rows = crate::services::observability::metrics::global()
        .snapshot()
        .into_iter();
    rows.filter(|row| row.channel_id == channel)
        .map(|row| row.relay_read_source_switched)
        .sum()
}

/// Starts a real watcher on `relay` at `offset` and registers its handle, as a spawn does.
fn spawn_watcher(
    shared: &Arc<crate::services::discord::SharedData>,
    channel: ChannelId,
    http: &Arc<Http>,
    tmux: &str,
    relay: &str,
) {
    let cancel = Arc::new(AtomicBool::new(false));
    let (paused, resume) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(None)));
    let (epoch, delivered) = (
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicBool::new(false)),
    );
    let beat = Arc::new(AtomicI64::new(
        crate::services::discord::tmux_watcher_now_ms(),
    ));
    shared.tmux_watchers.insert(
        channel,
        crate::services::discord::TmuxWatcherHandle {
            tmux_session_name: tmux.to_string(),
            output_path: relay.to_string(),
            paused: paused.clone(),
            resume_offset: resume.clone(),
            cancel: cancel.clone(),
            pause_epoch: epoch.clone(),
            turn_delivered: delivered.clone(),
            last_heartbeat_ts_ms: beat.clone(),
        },
    );
    tokio::spawn(
        crate::services::discord::tmux::tmux_watcher::tmux_output_watcher_with_restore(
            channel,
            http.clone(),
            shared.clone(),
            relay.to_string(),
            tmux.to_string(),
            0,
            cancel,
            paused,
            resume,
            epoch,
            delivered,
            beat,
            None,
        ),
    );
}

async fn until(what: &str, done: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out on {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The replacement watcher's read below the old watermark joins the open turn, and Discord
/// shows it once; the rebind leaves the row watcher-owned, so either side may post it.
#[test]
fn crossed_rebind_relay_reaches_discord_once_below_the_old_watermark() {
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        let qualified = format!(
            "{}::crossed_rebind_relay_reaches_discord_once_below_the_old_watermark",
            module_path!().split_once("::").unwrap().1
        );
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &qualified, "--nocapture"])
            .env(CHILD, "1")
            .env("AGENTDESK_ROOT_DIR", root.path())
            .env("CODEX_HOME", root.path())
            .env("AGENTDESK_STATUS_INTERVAL_SECS", "0")
            .env(
                crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
                "[]",
            )
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stdout}\n{stderr}");
        assert!(stdout.contains("1 passed; 0 failed"), "{stdout}");
        return;
    }
    let root = std::path::PathBuf::from(std::env::var_os("AGENTDESK_ROOT_DIR").unwrap());
    crate::config_live_reload::install(crate::config::Config::default());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let channel_id = 7_120_300_001;
        let channel = ChannelId::new(channel_id);
        let codex = ProviderKind::Codex;
        let tmux = format!(
            "AgentDesk-codex-switch7120-{}-cdx",
            uuid::Uuid::new_v4().simple()
        );
        let _live = InjectedLivenessGuard::set(HostSessionRef::tmux(&tmux), HostLiveness::Live);
        let generation = crate::services::tmux_common::session_temp_path(&tmux, "generation");
        std::fs::create_dir_all(std::path::Path::new(&generation).parent().unwrap()).unwrap();
        std::fs::write(&generation, b"switch-7120").unwrap();
        crate::services::tmux_common::write_tmux_runtime_kind_marker(
            &tmux,
            RuntimeHandoffKind::CodexTui,
        )
        .unwrap();
        let session_id = uuid::Uuid::new_v4().to_string();
        let sessions = root.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let rollout = sessions.join(format!("rollout-{session_id}.jsonl"));
        std::fs::write(&rollout, crossed_rollout()).unwrap();
        let relay = crate::services::tmux_common::session_temp_path(&tmux, "jsonl");
        let stale = rollout_line(serde_json::json!({"type":"assistant",
            "message":{"content":[{"type":"text", "text":"stale-relay-".repeat(64)}]}}));
        std::fs::write(&relay, &stale).unwrap();

        let mut row = InflightTurnState::new(
            codex.clone(),
            channel_id,
            Some("switch-7120-cdx".into()),
            9,
            712_301,
            712_302,
            "current turn".into(),
            Some(session_id.clone()),
            Some(tmux.clone()),
            Some(relay.clone()),
            None,
            0,
        );
        row.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
        row.turn_start_offset = Some(0);
        row.set_relay_owner_kind(RelayOwnerKind::Watcher);
        inflight::save_inflight_state(&row).unwrap();
        let episode = inflight::InflightEpisodePin::from_state(
            &inflight::load_inflight_state(&codex, channel_id).unwrap(),
        );
        assert!(
            crate::services::tui_prompt_dedupe::register_rehydrated_tmux_runtime_binding(
                "codex",
                &tmux,
                channel_id,
                crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                    runtime_kind: RuntimeHandoffKind::CodexTui,
                    output_path: rollout.display().to_string(),
                    relay_output_path: None,
                    input_fifo_path: None,
                    session_id: Some(session_id),
                    last_offset: 0,
                    relay_last_offset: None,
                },
            )
        );
        let shared = crate::services::discord::make_shared_data_for_tests();

        // The sink posts through its own Discord, the watchers and the rebind through another.
        let sink_discord = MockDiscord::start().await;
        let health = Arc::new(crate::services::discord::health::HealthRegistry::new());
        health.register("codex".into(), shared.clone()).await;
        let mut sink =
            crate::services::discord::session_relay_sink::SessionBoundDiscordRelaySink::new(health);
        sink.enable_delivery_for_test();
        sink.test_gateway = Some(Arc::new(
            crate::services::discord::gateway::DiscordGateway::new(
                sink_discord.http.clone(),
                shared.clone(),
                codex.clone(),
                None,
            ),
        ));
        let relay_handle = spawn_stream_relay(
            MatchedChannel {
                channel_id: channel_id.to_string(),
                agent_id: "switch-7120".into(),
                provider: codex.clone(),
                expected_session_name: tmux.clone(),
                expected_rollout_path: relay.clone().into(),
            },
            Arc::new(sink),
        );
        global_relay_producer_registry().register(tmux.clone(), relay_handle.producer());

        let channel_json = serde_json::json!({"id":channel_id.to_string(), "type":0,
            "guild_id":"2", "name":"switch-7120-cdx", "position":0,
            "permission_overwrites":[], "nsfw":false, "parent_id":null});
        let lookup = format!("/channels/{channel_id}");
        let watcher_discord = MockDiscord::start_with(Arc::new(move |method, path| {
            (method == axum::http::Method::GET && path.ends_with(&lookup))
                .then(|| (200, channel_json.clone()))
        }))
        .await;
        let http = watcher_discord.http.clone();
        let _rest = crate::services::discord::shared_state::test_rest::install(http.clone());

        spawn_watcher(&shared, channel, &http, &tmux, &relay);
        let metrics = relay_handle.metrics().clone();
        until("the old relay frame", || {
            metrics.snapshot().frames_received >= 1
        })
        .await;
        let old_file = crate::services::cluster::stream_relay::SourceFileIdentity::from_open_file(
            &std::fs::File::open(&relay).unwrap(),
        );

        let outcome = crate::services::discord::recovery_engine::rebind_inflight_for_channel(
            &http,
            &shared,
            &codex,
            channel_id,
            Some(tmux.clone()),
            crate::services::discord::recovery_engine::ManualRebindOverrides::default(),
            Some(&episode),
        )
        .await
        .expect("the crossed rebind completes");
        let output_path = shared
            .tmux_watchers
            .get(&channel)
            .unwrap()
            .output_path
            .clone();
        assert_eq!(
            (output_path.as_str(), outcome.initial_offset),
            (relay.as_str(), 0)
        );

        let copies = || {
            posted(&sink_discord, channel_id, NEW_TURN)
                + posted(&watcher_discord, channel_id, NEW_TURN)
        };
        until("the new turn on Discord", || copies() > 0).await;
        let new_file = crate::services::cluster::stream_relay::SourceFileIdentity::from_open_file(
            &std::fs::File::open(&relay).unwrap(),
        );
        let rebuilt = std::fs::read_to_string(&relay).unwrap();
        assert!(
            new_file != old_file && rebuilt.len() < stale.len() && rebuilt.contains(NEW_TURN),
            "the new file starts over below the old watermark: {rebuilt:?}"
        );
        until("the session relay to take the new file", || {
            read_source_switches(channel_id) == 1
        })
        .await;
        // Both copy sources are spent once the relay holds the new file and the turn's row is gone.
        until("the turn to finish", || {
            inflight::load_inflight_state(&codex, channel_id).is_none()
        })
        .await;
        assert_eq!(
            copies(),
            1,
            "{:#?}",
            watcher_discord.requests_for(channel_id)
        );

        let current = shared.tmux_watchers.get(&channel).map(|handle| {
            (
                handle.tmux_session_name.clone(),
                handle.output_path.clone(),
                handle.cancel.clone(),
            )
        });
        if let Some((session, path, token)) = current {
            shared
                .tmux_watchers
                .cancel_and_remove_channel_if_current(&channel, &session, &path, &token);
        }
        global_relay_producer_registry().deregister(&tmux);
    });
}
