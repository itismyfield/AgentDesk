use super::*;
use crate::services::session_host::test_support::InjectedLivenessGuard;
use crate::services::session_host::{HostLiveness, HostSessionRef};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

#[derive(Clone, Copy, PartialEq)]
enum Cursor {
    Raw,
    Normalized,
    OtherRaw,
    Absent,
    /// A normalized row whose rollout already holds the next user prompt.
    Crossed,
}

/// Which public rebind entry the case drives.
#[derive(Clone, Copy, PartialEq)]
enum Entry {
    Override,
    Floor,
    Default,
}

fn native_text(id: &str, text: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({"type":"response_item", "payload":{
            "type":"message", "role":"assistant", "id":id,
            "content":[{"type":"output_text", "text":text}]
        }})
    )
}

async fn discord_mock(channel: u64) -> (Arc<serenity::Http>, Arc<Mutex<Vec<String>>>) {
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request};
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let seen = bodies.clone();
    let app = axum::Router::new().fallback(move |request: Request<Body>| {
        let seen = seen.clone();
        async move {
            let channel_lookup = request.uri().path().ends_with(&format!("/channels/{channel}"));
            let method = request.method().clone();
            let bytes = to_bytes(request.into_body(), 1_000_000).await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
            if matches!(method, Method::POST | Method::PATCH) {
                seen.lock().unwrap().push(body["content"].as_str().unwrap_or_default().into());
            }
            let author = serde_json::json!({"id":"1", "username":"bot", "discriminator":"0001", "avatar":null, "bot":true});
            axum::Json(if channel_lookup {
                serde_json::json!({"id":channel.to_string(), "type":0, "guild_id":"2", "name":"raw-7120-cdx",
                    "position":0, "permission_overwrites":[], "nsfw":false, "parent_id":null})
            } else {
                serde_json::json!({"id":"712099", "channel_id":channel.to_string(), "content":body["content"].as_str().unwrap_or_default(),
                    "author":author, "timestamp":"2026-10-18T07:52:36Z", "edited_timestamp":null,
                    "tts":false, "mention_everyone":false, "mentions":[], "mention_roles":[],
                    "attachments":[], "embeds":[], "pinned":false, "type":0})
            })
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = serenity::HttpBuilder::new("test-token")
        .proxy(format!("http://{}", listener.local_addr().unwrap()))
        .ratelimiter_disabled(true)
        .build();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (Arc::new(http), bodies)
}

struct CancelWatcher(Arc<SharedData>, ChannelId);

impl Drop for CancelWatcher {
    fn drop(&mut self) {
        let current = self.0.tmux_watchers.get(&self.1).map(|handle| {
            (
                handle.tmux_session_name.clone(),
                handle.output_path.clone(),
                handle.cancel.clone(),
            )
        });
        if let Some((session, path, token)) = current {
            self.0
                .tmux_watchers
                .cancel_and_remove_channel_if_current(&self.1, &session, &path, &token);
        }
        // A failing main reproduction may have started the existing converter thread.
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

fn file_identity(
    path: &std::path::Path,
) -> crate::services::cluster::stream_relay::SourceFileIdentity {
    crate::services::cluster::stream_relay::SourceFileIdentity::from_open_file(
        &std::fs::File::open(path).unwrap(),
    )
}

fn codex_user(text: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({"timestamp":"2099-01-01T00:00:00Z", "type":"response_item", "payload":{
            "type":"message", "role":"user", "content":[{"type":"input_text", "text":text}]
        }})
    )
}

/// The row's turn, then the next user prompt with its answer and completion.
fn crossed_rollout() -> String {
    let complete = serde_json::json!({"type":"event_msg", "payload":{"type":"task_complete", "last_agent_message":"new-turn-7120"}});
    format!(
        "{}{}{}{}{complete}\n",
        codex_user("current turn"),
        native_text("cur", "old-answer-7120"),
        codex_user("next turn"),
        native_text("new", "new-turn-7120")
    )
}

/// The crossed case's old relay is longer than any rebuilt answer, so the old
/// watermark alone would hide every rebuilt byte.
fn normalized_relay(cursor: Cursor) -> String {
    normalized_line(&if cursor == Cursor::Crossed {
        "stale-normalized-".repeat(64)
    } else {
        "normalized".into()
    })
}

/// The rebuilt relay, read from the offset the rebind chose, reaches the sink
/// parser after the old relay's frames already set a higher watermark.
fn assert_crossed_rebuild_reaches_sink(
    channel_id: u64,
    tmux: &str,
    relay: &std::path::Path,
    before: crate::services::cluster::stream_relay::SourceFileIdentity,
    initial_offset: u64,
) {
    use crate::services::cluster::stream_relay::{ReadSource, StreamFrame};
    assert_eq!(
        initial_offset, 0,
        "a crossed-turn rebuild restarts the relay coordinates"
    );
    let after = file_identity(relay);
    assert_ne!(
        after, before,
        "restarted relay coordinates must come with a new relay file"
    );
    let binding = crate::services::cluster::session_matcher::MatchedChannel {
        channel_id: channel_id.to_string(),
        agent_id: "raw-7120-cdx".into(),
        provider: ProviderKind::Codex,
        expected_session_name: tmux.to_string(),
        expected_rollout_path: relay.display().to_string(),
    };
    let frame = |payload: String, sequence, file, producer| StreamFrame {
        session_name: tmux.to_string(),
        binding: binding.clone(),
        source_span: Some((0, payload.len() as u64)),
        payload,
        sequence,
        terminal_consumed_end: None,
        turn_user_msg_id: 0,
        turn_started_at: String::new(),
        turn_start_offset: None,
        relay_range: None,
        relay_generation_mtime_ns: None,
        relay_source_stamp: None,
        read_source: Some(ReadSource { file, producer }),
    };
    let mut parser =
        crate::services::discord::session_relay_sink::turn_parser::SessionRelayParser::default();
    assert!(
        parser
            .ingest_frame(&frame(normalized_relay(Cursor::Crossed), 1, before, 5))
            .is_empty()
    );
    let mut rebuilt = String::new();
    for _ in 0..50 {
        rebuilt = std::fs::read_to_string(relay).unwrap();
        if rebuilt.contains("\"type\":\"result\"") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if rebuilt.contains("\"type\":\"result\"") {
        println!("T-B2-6 writer->relay: PROVEN");
    } else {
        // Manual injection proves rebind planning and read->sink only.
        println!("T-B2-6 writer->relay: NOT_PROVEN");
        let manual = format!(
            "{}{}\n",
            normalized_line("new-turn-7120"),
            serde_json::json!({"type":"result", "result":"new-turn-7120"})
        );
        std::fs::OpenOptions::new()
            .append(true)
            .open(relay)
            .unwrap()
            .write_all(manual.as_bytes())
            .unwrap();
        rebuilt = std::fs::read_to_string(relay).unwrap();
    }
    assert_eq!(file_identity(relay), after);
    let deliveries = parser.ingest_frame(&frame(
        rebuilt[initial_offset as usize..].to_string(),
        2,
        after,
        6,
    ));
    assert_eq!(
        deliveries.len(),
        1,
        "the rebuilt turn completes in the sink"
    );
    assert!(
        deliveries[0].response_text.contains("new-turn-7120"),
        "{:?}",
        deliveries[0].response_text
    );
}

fn normalized_line(text: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({"type":"assistant", "message":{"content":[{"type":"text", "text":text}]}})
    )
}

fn rebind_case(cursor: Cursor, cancelled: bool, entry: Entry, alias: bool) {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        temp.path(),
    );
    let _codex = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "CODEX_HOME",
        temp.path(),
    );
    let _cutover = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let channel_id = 7_120_200_001;
        let channel = ChannelId::new(channel_id);
        let tmux = format!("AgentDesk-codex-raw7120-{}-cdx", uuid::Uuid::new_v4().simple());
        let _live = InjectedLivenessGuard::set(HostSessionRef::tmux(&tmux), HostLiveness::Live);
        assert_eq!(observe_liveness_with_retry(&tmux), crate::services::provider::session_probe::SessionLiveness::Alive, "fixture must reach the source decision on a live pane");
        crate::services::tmux_common::write_tmux_runtime_kind_marker(&tmux, RuntimeHandoffKind::CodexTui).unwrap();
        std::fs::write(crate::services::tmux_common::session_temp_path(&tmux, "generation"), b"raw-7120").unwrap();
        let session_id = uuid::Uuid::new_v4().to_string();
        let sessions = temp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let raw = sessions.join(format!("rollout-{session_id}.jsonl"));
        let raw_saved = if alias { sessions.join(".").join(raw.file_name().unwrap()) } else { raw.clone() };
        let other = sessions.join("other-rollout.jsonl");
        let relay = std::path::PathBuf::from(crate::services::tmux_common::session_temp_path(&tmux, "jsonl"));
        let old_turn = native_text("old", "old-turn-7120");
        let turn_start = old_turn.len() as u64;
        let head = format!("{old_turn}{}{}", "{}\n".repeat(8_192), native_text("prefix", "prefix-7120"));
        let cursor_offset = head.len() as u64;
        let gap = native_text("tail", "fresh-tail-7120");
        std::fs::write(&raw, if cursor == Cursor::Crossed { crossed_rollout() } else { format!("{head}{gap}") }).unwrap();
        std::fs::write(&other, &head).unwrap();
        if cursor != Cursor::Raw {
            std::fs::write(&relay, normalized_relay(cursor)).unwrap();
        }
        let relay_before = (cursor == Cursor::Crossed).then(|| file_identity(&relay));
        let saved_path = match cursor {
            Cursor::Raw => &raw_saved, Cursor::OtherRaw => &other, _ => &relay,
        };
        let mut row = inflight::InflightTurnState::new(ProviderKind::Codex, channel_id, Some("raw-7120-cdx".into()), 9, 712_001, 712_002,
            "current turn".into(), Some(session_id.clone()), Some(tmux.clone()), Some(saved_path.display().to_string()), None,
            if cursor == Cursor::Raw { cursor_offset } else { 0 });
        row.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
        row.turn_start_offset = Some(turn_start);
        row.set_relay_owner_kind(inflight::RelayOwnerKind::Watcher);
        if cursor != Cursor::Absent { inflight::save_inflight_state(&row).unwrap(); }
        let episode = (cursor != Cursor::Absent).then(|| inflight::InflightEpisodePin::from_state(&inflight::load_inflight_state(&ProviderKind::Codex, channel_id).unwrap()));
        assert!(crate::services::tui_prompt_dedupe::register_rehydrated_tmux_runtime_binding("codex", &tmux, channel_id,
            crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::CodexTui, output_path: raw.display().to_string(), relay_output_path: None,
                input_fifo_path: None, session_id: Some(session_id), last_offset: 0, relay_last_offset: None,
            }));
        let shared = crate::services::discord::make_shared_data_for_tests();
        let incumbent_path = if cursor == Cursor::Raw { &raw_saved } else { &relay };
        let original_cancel = Arc::new(AtomicBool::new(cancelled));
        shared.tmux_watchers.insert(channel, TmuxWatcherHandle {
            tmux_session_name: tmux.clone(), output_path: incumbent_path.display().to_string(),
            paused: Arc::new(AtomicBool::new(false)), resume_offset: Arc::new(Mutex::new(None)),
            cancel: original_cancel.clone(), pause_epoch: Arc::new(AtomicU64::new(0)),
            turn_delivered: Arc::new(AtomicBool::new(false)), last_heartbeat_ts_ms: Arc::new(AtomicI64::new(super::tmux_watcher_now_ms())),
        });
        let _cancel = CancelWatcher(shared.clone(), channel);
        let (http, _bodies) = discord_mock(channel_id).await;
        let _rest = crate::services::discord::shared_state::test_rest::install(http.clone());
        let outcome = match entry {
            Entry::Floor => rebind_inflight_for_channel_with_minimum_start_offset(&http, &shared, &ProviderKind::Codex, channel_id, Some(tmux.clone()), Some(std::fs::metadata(&raw).unwrap().len()), episode.as_ref()).await,
            Entry::Override => {
                let overrides = ManualRebindOverrides::validated(&ProviderKind::Codex, Some(raw.to_str().unwrap()), None).unwrap();
                rebind_inflight_for_channel(&http, &shared, &ProviderKind::Codex, channel_id, Some(tmux.clone()), overrides, episode.as_ref()).await
            }
            Entry::Default => rebind_inflight_for_channel(&http, &shared, &ProviderKind::Codex, channel_id, Some(tmux.clone()), ManualRebindOverrides::default(), episode.as_ref()).await,
        }.expect("live rebind must reach and complete its source decision");
        let actual_path = shared.tmux_watchers.get(&channel).unwrap().output_path.clone();
        let expected = if cursor == Cursor::Raw { &raw_saved } else { &relay };
        assert_eq!(actual_path, expected.display().to_string(), "manual rebind must keep a matching raw cursor; other cases retain normalized planning");
        if cursor == Cursor::Raw {
            assert!(!relay.exists(), "keeping raw A must not create session JSONL B");
            assert_eq!((outcome.watcher_spawned, outcome.watcher_replaced), (cancelled, cancelled));
            if !cancelled {
                assert!(Arc::ptr_eq(&shared.tmux_watchers.get(&channel).unwrap().cancel, &original_cancel), "healthy same-path claim must reuse the incumbent");
                assert!(!original_cancel.load(Ordering::Acquire));
            }
            assert_eq!(outcome.initial_offset, cursor_offset, "a later floor must not skip A's unread gap");
            let after = inflight::load_inflight_state(&ProviderKind::Codex, channel_id).unwrap();
            assert_eq!((after.last_offset, after.turn_start_offset), (cursor_offset, Some(turn_start)));
            if !alias && !cancelled {
                let terminal = format!("{}\n", serde_json::json!({"type":"event_msg", "payload":{"type":"task_complete", "last_agent_message":"fresh-final-7120"}}));
                std::fs::OpenOptions::new().append(true).open(&raw).unwrap().write_all(terminal.as_bytes()).unwrap();
                let mut reader = std::fs::File::open(&actual_path).unwrap();
                reader.seek(SeekFrom::Start(outcome.initial_offset)).unwrap();
                let mut fresh = String::new();
                reader.read_to_string(&mut fresh).unwrap();
                assert!(fresh.contains("fresh-tail-7120") && fresh.contains("fresh-final-7120"));
                use crate::services::discord::tmux::tmux_output_stream::read_native_codex_state;
                let identity = crate::services::cluster::stream_relay::SourceFileIdentity::from_open_file(&reader);
                let decoder = read_native_codex_state(&actual_path, turn_start, outcome.initial_offset, identity, &tmux,
                    crate::services::discord::tmux::read_generation_file_mtime_ns(&tmux), None).unwrap();
                let mut response = String::new();
                let mut tools = crate::services::discord::tmux::WatcherToolState::new();
                tools.set_provider(&ProviderKind::Codex);
                tools.restore_native_codex(decoder, &mut response);
                let mut lines = crate::services::session_backend::StreamLineState::new();
                let parsed = crate::services::discord::tmux::process_watcher_lines_for_turn(
                    &mut fresh, &mut lines, &mut response, &mut tools, Some(outcome.initial_offset), after.turn_start_offset);
                assert!(parsed.found_result, "A's terminal must complete the same recovered turn");
                assert!(response.contains("fresh-tail-7120") && response.contains("fresh-final-7120"), "{response:?}");
                assert_eq!(response.matches("prefix-7120").count(), 1);
                assert!(!response.contains("old-turn-7120"));
            }
        } else if let Some(relay_before) = relay_before {
            assert_crossed_rebuild_reaches_sink(channel_id, &tmux, &relay, relay_before, outcome.initial_offset);
        } else {
            assert!(!outcome.watcher_spawned && !outcome.watcher_replaced);
            assert!(Arc::ptr_eq(&shared.tmux_watchers.get(&channel).unwrap().cancel, &original_cancel));
            assert_eq!(inflight::load_inflight_state(&ProviderKind::Codex, channel_id).unwrap().output_path.as_deref(), relay.to_str());
        }
    });
}

#[test]
fn rebind_keeps_matching_raw_incumbent_and_delivers_new_tail() {
    rebind_case(Cursor::Raw, false, Entry::Override, false);
}

#[test]
fn cancelled_raw_successor_does_not_skip_gap_to_a_later_floor() {
    rebind_case(Cursor::Raw, true, Entry::Floor, false);
}

#[test]
fn matching_raw_canonical_alias_keeps_saved_coordinates() {
    rebind_case(Cursor::Raw, false, Entry::Override, true);
}

#[test]
fn normalized_cursor_still_uses_normalized_plan() {
    rebind_case(Cursor::Normalized, false, Entry::Override, false);
}

#[test]
fn another_raw_rollout_still_uses_normalized_plan() {
    rebind_case(Cursor::OtherRaw, false, Entry::Override, false);
}

#[test]
fn absent_inflight_still_uses_normalized_plan() {
    rebind_case(Cursor::Absent, false, Entry::Override, false);
}

/// The stall watchdog's reattach rebinds with default overrides and a pin; that
/// input must reuse the healthy raw incumbent the runtime binding names.
#[test]
fn default_override_rebind_reuses_healthy_matching_raw_incumbent() {
    rebind_case(Cursor::Raw, false, Entry::Default, false);
}

/// The stall watchdog's pinned rebind of a normalized row whose rollout moved to
/// the next prompt rebuilds the relay, and the new turn reaches the sink.
#[test]
fn crossed_turn_rebind_rebuilds_into_a_new_relay_file_and_delivers_the_new_turn() {
    rebind_case(Cursor::Crossed, false, Entry::Default, false);
}
