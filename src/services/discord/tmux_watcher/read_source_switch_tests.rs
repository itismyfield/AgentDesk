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
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64};
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
            // No progress tick: which message a crossed rebind draws into is a placeholder policy
            // outside this test, which checks the read-source switch and the single delivery.
            .env("AGENTDESK_STATUS_INTERVAL_SECS", "3600")
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

mod idle_sink_frontier {
    use crate::services::tmux_common as tc;
    #[test]
    fn s2a1_sink_ordered_idle_entry_commits_durably_preserves_noop_and_refuses_old_generation() {
        use crate::services::cluster::session_matcher::{
            MatchedChannel, expected_rollout_path_for,
        };
        use crate::services::cluster::stream_relay::StreamFrame;
        use crate::services::cluster::stream_relay::source_read_proof::tests::seal_with_frontier;
        use crate::services::discord::{
            self,
            health::HealthRegistry,
            session_relay_sink::{SessionBoundDiscordRelaySink, turn_parser::SessionRelayParser},
        };
        use crate::services::provider::ProviderKind;
        use poise::serenity_prelude::ChannelId;
        use std::sync::Arc;
        let _host = crate::config::pin_runtime_host_for_test();
        let channel = ChannelId::new(59_960_420);
        let provider = ProviderKind::Claude;
        let session = provider.build_tmux_session_name(&channel.get().to_string());
        let output = expected_rollout_path_for(&session);
        std::fs::write(&output, [b'x'; 512]).expect("source");
        let generation =
            crate::services::tmux_common::spawn_markers::stamp_session_generation_marker(&session)
                .expect("generation");
        let shared = discord::make_shared_data_for_tests();
        let coord = shared.tmux_relay_coord(channel);
        let (_source, seal) = seal_with_frontier(&session, 128, (generation, 0));
        assert_eq!(
            format!(
                "{:?}",
                coord.publish_confirmed_for_tests(128, generation, 0, Some(&seal))
            ),
            "Published"
        );
        let delivery = |frame_generation| {
            let frame = StreamFrame {
                session_name: session.clone(), binding: MatchedChannel { channel_id: channel.get().to_string(), agent_id: "s2a1".into(), provider: provider.clone(), expected_session_name: session.clone(), expected_rollout_path: output.clone() },
                payload: "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"idle answer\"}]}}\n{\"type\":\"result\",\"result\":\"idle answer\"}\n".into(),
                sequence: 1, terminal_consumed_end: None, turn_user_msg_id: 0, turn_started_at: String::new(), turn_start_offset: None, relay_range: Some((128,256)), relay_generation_mtime_ns: Some(frame_generation), relay_source_stamp: None, source_span: None, read_source: None,
            };
            SessionRelayParser::default()
                .ingest_frame(&frame)
                .pop()
                .expect("terminal parsed")
        };
        let sink = SessionBoundDiscordRelaySink::new(Arc::new(HealthRegistry::new()));
        assert!(!sink.advance_idle_range_after_confirmed_post(
            &shared,
            &provider,
            channel.get(),
            &session,
            &delivery(generation + 1)
        ));
        assert_eq!(coord.frontier_tuple_for_tests(), (128, generation, 0, true));
        let candidate = delivery(generation);
        assert!(!sink.idle_range_already_committed_before_transport(
            &shared,
            &provider,
            channel.get(),
            &session,
            &candidate
        ));
        assert!(sink.advance_idle_range_after_confirmed_post(
            &shared,
            &provider,
            channel.get(),
            &session,
            &candidate
        ));
        assert_eq!(
            coord.frontier_tuple_for_tests(),
            (256, generation, 0, false)
        );
        assert_eq!(
            discord::outbound::delivery_record::delivered_frontier_end_current_generation(
                &provider,
                channel,
                &session,
                Some(512)
            ),
            256
        );
        assert!(sink.idle_range_already_committed_before_transport(
            &shared,
            &provider,
            channel.get(),
            &session,
            &candidate
        ));
        let (_same, seal) = seal_with_frontier(&session, 256, (generation, 0));
        assert_eq!(
            format!(
                "{:?}",
                coord.publish_confirmed_for_tests(256, generation, 0, Some(&seal))
            ),
            "Published"
        );
        assert!(sink.advance_idle_range_after_confirmed_post(
            &shared,
            &provider,
            channel.get(),
            &session,
            &candidate
        ));
        assert_eq!(coord.frontier_tuple_for_tests(), (256, generation, 0, true));
        tc::cleanup_session_temp_files(&session);
    }
}

mod terminal_ack {
    use crate::services::discord::tmux::tmux_watcher::session_bound_ack::{
        SessionBoundRelayAckOutcome, wait_for_session_bound_relay_delivery_ack,
    };
    use crate::services::discord::tmux::tmux_watcher::supervisor_relay::SessionBoundRelayAckTarget;

    #[tokio::test]
    async fn fenced_zero_delivery_resolves_exact_not_delivered_without_timeout_4909() {
        use crate::services::cluster::session_matcher::{
            MatchedChannel, expected_rollout_path_for,
        };
        use crate::services::cluster::stream_relay::{
            RelaySink, TerminalCommitFence, spawn_stream_relay,
        };
        use crate::services::discord::health::HealthRegistry;
        use crate::services::discord::session_relay_sink::{
            PURE_SUBAGENT_ZERO_DELIVERY_PAYLOAD, SessionBoundDiscordRelaySink,
        };
        use crate::services::provider::ProviderKind;
        use std::sync::Arc;

        let channel_id = "4909";
        let session = ProviderKind::Claude.build_tmux_session_name(channel_id);
        let binding = MatchedChannel {
            channel_id: channel_id.to_string(),
            agent_id: format!("agent-{channel_id}"),
            provider: ProviderKind::Claude,
            expected_session_name: session.clone(),
            expected_rollout_path: expected_rollout_path_for(&session),
        };
        let sink: Arc<dyn RelaySink> = Arc::new(SessionBoundDiscordRelaySink::new(Arc::new(
            HealthRegistry::new(),
        )));
        let handle = spawn_stream_relay(binding, sink);
        let producer = handle.producer();
        let sent = producer.try_send_terminal_frame_with_sequence(
            PURE_SUBAGENT_ZERO_DELIVERY_PAYLOAD.to_string(),
            TerminalCommitFence {
                consumed_end: 256,
                turn_user_msg_id: 49_090,
                turn_started_at: "2026-07-27T00:00:00Z".to_string(),
                turn_start_offset: Some(64),
            },
        );
        let sequence = sent.sequence.expect("terminal frame enqueued");
        let metrics = producer.metrics().clone();
        let target = SessionBoundRelayAckTarget {
            metrics: metrics.clone(),
            sequence,
            turn_start_offset: Some(64),
        };

        let ack = wait_for_session_bound_relay_delivery_ack(
            Some(&target),
            std::time::Duration::from_millis(250),
        )
        .await;

        assert_eq!(
            ack,
            SessionBoundRelayAckOutcome::NotDelivered,
            "the real parser and sink must resolve the exact ACK instead of timing out"
        );
        assert_ne!(ack, SessionBoundRelayAckOutcome::TimedOut);
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.last_terminal_skipped_sequence, Some(sequence));
        assert_eq!(snapshot.last_terminal_committed_sequence, None);
        assert_eq!(snapshot.last_sink_error_sequence, None);
        handle.shutdown().await;
    }
}

mod chunk_forward {
    use crate::services::cluster::relay_producer_registry::RelayProducerRegistry;
    use crate::services::cluster::session_matcher::{MatchedChannel, expected_rollout_path_for};
    use crate::services::cluster::stream_relay::{SourceFileIdentity, StreamFrame};
    use crate::services::discord::session_relay_sink::turn_parser::SessionRelayParser;
    use crate::services::discord::tmux::tmux_watcher::loop_poll_prologue::WatcherSourceAuthority;
    use crate::services::discord::tmux::tmux_watcher::turn_stream_collector::chunk_forward::*;
    use crate::services::discord::tmux::tmux_watcher::utf8_chunk_decoder::Utf8ChunkDecoder;
    use crate::services::observability::metrics;
    use crate::services::provider::ProviderKind;
    use std::sync::Arc;

    fn authority() -> WatcherSourceAuthority {
        WatcherSourceAuthority {
            source_file: SourceFileIdentity::Unavailable,
            generation_mtime_ns: 0,
            reset_incarnation: 0,
            source_stamp: None,
        }
    }

    fn binding(channel: &str) -> MatchedChannel {
        let session = ProviderKind::Claude.build_tmux_session_name(channel);
        MatchedChannel {
            channel_id: channel.to_string(),
            agent_id: format!("agent-{channel}"),
            provider: ProviderKind::Claude,
            expected_session_name: session.clone(),
            expected_rollout_path: expected_rollout_path_for(&session),
        }
    }

    /// The frame `supervisor_relay` mints from a forwarded chunk.
    fn frame(binding: &MatchedChannel, chunk: &ForwardedChunk<'_>, sequence: u64) -> StreamFrame {
        let (text, source_span, read_source) = chunk.forwarded_read_for_test();
        StreamFrame {
            session_name: binding.expected_session_name.clone(),
            binding: binding.clone(),
            payload: text.to_string(),
            sequence,
            terminal_consumed_end: None,
            turn_user_msg_id: 0,
            turn_started_at: String::new(),
            turn_start_offset: None,
            relay_range: None,
            relay_generation_mtime_ns: None,
            relay_source_stamp: None,
            source_span,
            read_source,
        }
    }

    fn assistant(text: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({
                "type": "assistant",
                "message": {"content": [{"type": "text", "text": text}]}
            })
        )
    }

    fn suppressed_for(channel_id: u64) -> u64 {
        metrics::global()
            .snapshot()
            .into_iter()
            .find(|row| row.channel_id == channel_id)
            .map_or(0, |row| row.relay_resend_suppressed)
    }

    /// `all_data` holds 4 bytes from an earlier read plus this 6-byte read,
    /// anchored at 1000; 5 pre-turn bytes are skipped, 4 of them from the
    /// earlier read, so the forwarded suffix is `[1005, 1010)`.
    #[test]
    fn forwarded_span_names_the_suffix_the_buffer_ends_with() {
        let chunk = forwarded_chunk("abcdef", 10, 1_000, 5, authority(), 1);
        assert_eq!(chunk.forwarded_read_for_test().0, "bcdef");
        assert_eq!(chunk.forwarded_read_for_test().1, Some((1_005, 1_010)));

        let empty = forwarded_chunk("abcdef", 10, 1_000, 10, authority(), 1);
        assert_eq!(empty.forwarded_read_for_test().0, "");
        assert_eq!(empty.forwarded_read_for_test().1, None);
    }

    /// #5979 (I18): the terminal-delivery rewind in `tmux_watcher.rs` resets
    /// `current_offset` and empties `all_data` but never the decoder. With a
    /// split scalar buffered from the abandoned read, the replay must still be
    /// anchored at the rewind offset so its span is in FILE coordinates and the
    /// sink recognises the already-folded prefix.
    #[test]
    fn rewind_replay_keeps_file_coordinates_and_the_sink_suppresses_the_resend() {
        let binding = binding("597901");
        let before = suppressed_for(597_901);
        let head = assistant("alpha");
        let tail = format!(
            "{}{}\n",
            assistant("안녕"),
            serde_json::json!({"type": "result", "result": "done"})
        );
        let transcript = format!("{head}{tail}");
        let bytes = transcript.as_bytes();
        let cut = head.len() + tail.find('안').expect("korean text") + 1;
        let base = 4_096u64;

        let mut decoder = Utf8ChunkDecoder::default();
        let mut parser = SessionRelayParser::default();
        let mut all_data = String::new();

        // Read 1 ends one byte into `안`; the collector anchors the empty buffer
        // at the decoded start.
        let first = decoder.decode_source_for_buffer(&bytes[..cut], base, authority(), &all_data);
        let anchor = first.start_offset.expect("decoded text");
        all_data.push_str(&first.text);
        let first_len = all_data.len() as u64;
        let chunk = forwarded_chunk(&first.text, all_data.len(), anchor, 0, authority(), 1);
        assert_eq!(
            chunk.forwarded_read_for_test().1,
            Some((base, base + first_len))
        );
        assert!(parser.ingest_frame(&frame(&binding, &chunk, 1)).is_empty());
        assert_eq!(parser.turn_source_end(), Some(base + first_len));

        // Rewind: cursor back to `base`, buffer emptied, decoder untouched; the
        // collector's refill of the empty buffer is what drops the stale tail.
        all_data.clear();
        let replay = decoder.decode_source_for_buffer(bytes, base, authority(), &all_data);
        let anchor = replay.start_offset.expect("decoded text");
        assert_eq!(
            anchor, base,
            "the abandoned read's tail re-anchored the replay"
        );
        all_data.push_str(&replay.text);
        let chunk = forwarded_chunk(&replay.text, all_data.len(), anchor, 0, authority(), 1);
        assert_eq!(
            chunk.forwarded_read_for_test().1,
            Some((base, base + transcript.len() as u64))
        );

        let deliveries = parser.ingest_frame(&frame(&binding, &chunk, 2));
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].response_text.matches("alpha").count(), 1);
        assert_eq!(deliveries[0].response_text.matches("안녕").count(), 1);
        assert_eq!(suppressed_for(597_901) - before, 1);
    }

    #[derive(Default)]
    struct CapturingSink(std::sync::Mutex<Vec<StreamFrame>>);

    #[async_trait::async_trait]
    impl crate::services::cluster::stream_relay::RelaySink for CapturingSink {
        async fn deliver(
            &self,
            frame: &StreamFrame,
        ) -> Result<
            crate::services::cluster::stream_relay::RelaySinkOutcome,
            crate::services::cluster::stream_relay::RelaySinkError,
        > {
            self.0.lock().unwrap().push(frame.clone());
            Ok(crate::services::cluster::stream_relay::RelaySinkOutcome::FrameAccepted)
        }
    }

    /// The frame leaving the watcher names the descriptor it read and the
    /// watcher that read it, with no source stamp (the Legacy default).
    #[tokio::test]
    async fn watcher_forward_carries_opened_fd_identity_without_stamp() {
        use crate::services::cluster::stream_relay::{ReadSource, spawn_stream_relay};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-a.jsonl");
        let line = assistant("from file A");
        std::fs::write(&path, &line).unwrap();
        let identity = SourceFileIdentity::from_open_file(&std::fs::File::open(&path).unwrap());
        assert_ne!(identity, SourceFileIdentity::Unavailable);
        let read = WatcherSourceAuthority {
            source_file: identity,
            ..authority()
        };
        let mut decoder = Utf8ChunkDecoder::default();
        let decoded = decoder.decode_source_for_buffer(line.as_bytes(), 0, read, "");
        let chunk = forwarded_chunk(&decoded.text, decoded.text.len(), 0, 0, read, 7);

        let binding = binding("712011");
        let sink = Arc::new(CapturingSink::default());
        let relay = spawn_stream_relay(binding.clone(), sink.clone());
        let registry = Arc::new(RelayProducerRegistry::new());
        registry.register(binding.expected_session_name.clone(), relay.producer());
        let mut cached = None;
        let session = &binding.expected_session_name;
        forward_turn_chunk_to_supervisor_relay(
            session,
            &chunk,
            0,
            &registry,
            &mut cached,
            None,
            None,
        );
        for _ in 0..200 {
            if !sink.0.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        let frames = sink.0.lock().unwrap().clone();
        assert_eq!(frames.len(), 1, "the forward reaches the sink");
        let expected = ReadSource {
            file: identity,
            producer: 7,
        };
        assert_eq!(frames[0].read_source, Some(expected));
        assert_eq!(frames[0].relay_source_stamp, None);
    }
}
