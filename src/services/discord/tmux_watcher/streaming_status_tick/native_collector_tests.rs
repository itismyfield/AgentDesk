//! Drive the real native collector before a terminal exists, with local HTTP.
use super::*;
use crate::services::cluster::relay_producer_registry::RelayProducerRegistry;
use crate::services::cluster::session_matcher::MatchedChannel;
use crate::services::cluster::stream_relay::{DiscardSink, SourceFileIdentity, spawn_stream_relay};
use crate::services::discord::delivery_lease_cell::source_epoch_observer as observer;
use std::sync::atomic::{AtomicI64, AtomicU64};

pub(super) fn seed_recovered_row(
    root: &std::path::Path,
    case: u64,
) -> (Fixture, InflightTurnState) {
    let mut fx = seed_row(root, case, false, false);
    let mut row = load_inflight_state(&fx.provider, fx.channel.get()).unwrap();
    std::fs::remove_file(fx.path()).unwrap();
    fx.provider = ProviderKind::Codex;
    row.provider = fx.provider.as_str().to_owned();
    row.current_msg_id = 0;
    row.current_msg_len = 3;
    row.turn_source = crate::services::discord::inflight::TurnSource::ExternalInput;
    row.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui);
    row.set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::SessionBoundRelay);
    row.set_restart_mode(crate::services::discord::InflightRestartMode::DrainRestart);
    row.turn_nonce = Some("recovered-original-5833".into());
    row.injected_prompt_message_id = Some(row.user_msg_id);
    save_inflight_state(&row).unwrap();
    fx.identity = InflightTurnIdentity::from_state(&row);
    (fx, row)
}

fn collector_case(paused: bool, repeats: usize, name: &str) {
    const CHILD: &str = "AGENTDESK_5833_NATIVE_COLLECTOR_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let qualified = format!("{}::{name}", module_path!().split_once("::").unwrap().1);
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &qualified, "--nocapture"])
            .env(CHILD, "1")
            .env("AGENTDESK_STATUS_INTERVAL_SECS", "0")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "native collector child: {result:?}"
        );
        assert!(String::from_utf8_lossy(&result.stdout).contains("1 passed; 0 failed"));
        return;
    }
    let (_lock, root) = isolate_root();
    capture_warns(async {
        let (fx, row) = seed_recovered_row(root.root.path(), 5834);
        let marker = crate::services::tmux_common::session_temp_path(&fx.tmux, "generation");
        std::fs::write(&marker, b"1").unwrap();
        let data = format!(
            "{}\n",
            serde_json::json!({"type":"response_item", "payload": {
                "type":"message", "role":"assistant", "phase":"commentary",
                "content":[{"type":"output_text", "text":TRAILING_BODY}]
            }})
        )
        .repeat(repeats)
        .into_bytes();
        std::fs::write(&fx.output_path, &data).unwrap();
        let shared = crate::services::discord::make_shared_data_for_tests();
        let rec = recorder(fx.channel, true).await;
        let cancel = Arc::new(AtomicBool::new(false));
        let ctx = TurnStreamCollectorContext {
            http: rec.http.clone(),
            shared: shared.clone(),
            channel_id: fx.channel,
            watcher_provider: fx.provider.clone(),
            tmux_session_name: fx.tmux.clone(),
            output_path: fx.output_path.clone(),
            input_fifo_path: String::new(),
            watcher_thread_channel_id: None,
            cancel: cancel.clone(),
            paused: Arc::new(AtomicBool::new(paused)),
            pause_epoch: Arc::new(AtomicU64::new(0)),
            turn_delivered: Arc::new(AtomicBool::new(false)),
            last_heartbeat_ts_ms: Arc::new(AtomicI64::new(
                crate::services::discord::tmux_watcher_now_ms(),
            )),
            jsonl_notify: Arc::new(tokio::sync::Notify::new()),
            dead_marker_notify: Arc::new(tokio::sync::Notify::new()),
            turn_result_relayed: false,
            restored_injected_prompt_message_id: row.injected_prompt_message_id,
        };
        let file = std::fs::File::open(&fx.output_path).unwrap();
        let source_file = SourceFileIdentity::from_open_file(&file);
        let source_authority = WatcherSourceAuthority {
            source_file,
            generation_mtime_ns:
                crate::services::discord::outbound::delivery_record::current_generation_mtime_ns(
                    &fx.tmux,
                ),
            reset_incarnation: shared.relay_frontier_token(fx.channel).reset_incarnation,
            source_stamp: observer::source_stamp(
                &fx.tmux,
                observer::read_source_epoch_witness(&fx.tmux),
                source_file,
            ),
        };
        let handle = spawn_stream_relay(
            MatchedChannel {
                channel_id: fx.channel.get().to_string(),
                agent_id: "native-collector".into(),
                provider: fx.provider.clone(),
                expected_session_name: fx.tmux.clone(),
                expected_rollout_path: fx.output_path.clone(),
            },
            Arc::new(DiscardSink),
        );
        let registry = Arc::new(RelayProducerRegistry::new());
        registry.register(fx.tmux.clone(), handle.producer());
        let mut offset = data.len() as u64;
        let mut buffer = String::new();
        let mut buffer_start = 0;
        let mut decoder = Utf8ChunkDecoder::default();
        let mut pending = None;
        let mut restored = None;
        let mut rewind_key = None;
        let mut attempts = 0;
        let identity = Some(fx.identity.clone());
        let mut heartbeat = None;
        let mut reacquire = false;
        let mut cached = None;
        let mut mirrored = true;
        let mut ack = None;
        let mut first = None;
        let mut parser = TurnParseState {
            current_offset: &mut offset,
            all_data: &mut buffer,
            all_data_start_offset: &mut buffer_start,
            utf8_decoder: &mut decoder,
            pending_terminal_rewind_seed: &mut pending,
            restored_turn: &mut restored,
            terminal_rewind_attempt_key: &mut rewind_key,
            terminal_rewind_attempts: &mut attempts,
            watcher_turn_identity: &identity,
            last_activity_heartbeat_at: &mut heartbeat,
            active_stream_inflight_reacquire_logged: &mut reacquire,
        };
        let mut relay = SupervisorRelayState {
            producer_registry: &registry,
            cached_relay_producer: &mut cached,
            all_data_fully_mirrored_to_session_relay: &mut mirrored,
            all_data_session_bound_relay_ack: &mut ack,
            all_data_first_forwarded_relay_sequence: &mut first,
        };
        let mut monitor = MonitorAutoTurnState::default();
        let mut render = RenderSeedState::default();
        let run = collect_turn_stream_until_terminal(
            &ctx,
            TurnStreamCollectorIo {
                data,
                data_start_offset: 0,
                epoch_snapshot: 0,
                source_authority,
            },
            &mut parser,
            &mut relay,
            &mut monitor,
            &mut render,
        );
        let stop = async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
            while rec.seen("POST").is_empty() && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            cancel.store(true, Ordering::Release);
        };
        let (outcome, ()) = tokio::join!(run, stop);
        assert_eq!(
            handle.metrics().snapshot().frames_received,
            1,
            "both states feed the passive relay"
        );
        assert_eq!(
            rec.seen("POST").is_empty(),
            paused,
            "paused={paused}: first native frame must reach HTTP only for a running collector"
        );
        if !paused {
            let CollectOutcome::Fallthrough(turn) = outcome else {
                panic!("unpaused collector discarded its frame")
            };
            assert!(
                !turn.found_result,
                "HTTP happened before any terminal source event"
            );
            assert!(turn.full_response.contains(TRAILING_BODY));
            assert!(
                turn.placeholder_msg_id.is_some(),
                "first Discord POST must succeed"
            );
            assert!(
                rec.bodies
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|body| body.chars().count() <= 2000),
                "native replay must split before its first HTTP publication"
            );
        }
        handle.shutdown().await;
        std::fs::remove_file(marker).unwrap();
    });
}

#[test]
fn recovered_native_collector_first_frame_reaches_http() {
    collector_case(
        false,
        1,
        "recovered_native_collector_first_frame_reaches_http",
    );
}

#[test]
fn recovered_native_collector_paused_still_enqueues_without_http() {
    collector_case(
        true,
        1,
        "recovered_native_collector_paused_still_enqueues_without_http",
    );
}

#[test]
fn recovered_native_collector_long_commentary_first_post_is_bounded() {
    collector_case(
        false,
        200,
        "recovered_native_collector_long_commentary_first_post_is_bounded",
    );
}
