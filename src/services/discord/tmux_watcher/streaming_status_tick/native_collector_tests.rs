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
    let captured = name.starts_with("captured_native_collector_");
    let physical = name == "captured_native_collector_physical_batches_reach_http";
    if std::env::var_os(CHILD).is_none() {
        let qualified = format!("{}::{name}", module_path!().split_once("::").unwrap().1);
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &qualified, "--nocapture"])
            .args(captured.then_some("--ignored"))
            .env(CHILD, "1")
            .env(
                "AGENTDESK_STATUS_INTERVAL_SECS",
                if captured { "5" } else { "0" },
            )
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
    if captured {
        // install has no uninstall; this executes only in the isolated child.
        let mut config = crate::config::Config::default();
        config.runtime.relay_authority_mode = crate::config::RelayAuthorityMode::Enforce;
        config.runtime.relay_authority_cohort_percent = 100;
        crate::config_live_reload::install(config);
    }
    capture_warns(async {
        let (mut fx, mut row) = seed_recovered_row(root.root.path(), 5834);
        let _pane = captured.then(|| {
            fx.tmux = format!(
                "AgentDesk-codex-5833-test-{}",
                uuid::Uuid::new_v4().simple()
            );
            row.tmux_session_name = Some(fx.tmux.clone());
            let created =
                crate::services::platform::tmux::create_session(&fx.tmux, None, "sleep 60")
                    .expect("create isolated liveness-only pane");
            assert!(created.status.success());
            OwnedCapturePane(fx.tmux.clone())
        });
        let marker = crate::services::tmux_common::session_temp_path(&fx.tmux, "generation");
        std::fs::write(&marker, b"1").unwrap();
        let generated = (0..repeats).map(|index| format!(
            "{}\n",
            serde_json::json!({"type":"response_item", "payload": {
                "id":format!("commentary-{index}"), "type":"message", "role":"assistant",
                "phase":"commentary", "channel":"commentary",
                "content":[{"type":"output_text", "text":format!("{index}: {TRAILING_BODY}")}]
            }})
        )).collect::<String>().into_bytes();
        let data = if captured {
            let path = std::env::var("AGENTDESK_5833_CAPTURED_SOURCE")
                .expect("manual diagnostic requires an immutable private source copy");
            assert!(std::fs::metadata(&path).unwrap().len() <= 64 * 1024 * 1024);
            std::fs::read(path).unwrap()
        } else {
            generated
        };
        let source_bytes = data.len();
        std::fs::write(&fx.output_path, &data).unwrap();
        let source_start = if captured {
            std::env::var("AGENTDESK_5833_CAPTURED_START")
                .expect("manual diagnostic requires the original absolute start offset")
                .parse::<u64>()
                .unwrap()
        } else {
            0
        };
        assert!(source_start < source_bytes as u64);
        let prose_witnesses = if captured {
            String::from_utf8_lossy(&data[source_start as usize..])
                .lines()
                .filter_map(|line| {
                    let event: serde_json::Value = serde_json::from_str(line).ok()?;
                    let payload = &event["payload"];
                    if event["type"] != "response_item"
                        || payload["type"] != "message"
                        || payload["role"] != "assistant"
                    {
                        return None;
                    }
                    payload["content"].as_array()?.iter().find_map(|part| {
                        let text = part["text"].as_str()?.trim();
                        (text.chars().count() >= 16).then(|| {
                            let formatted = crate::services::discord::formatting::format_for_discord_with_status_panel(
                                text, &ProviderKind::Codex);
                            (formatted.trim().chars().take(32).collect::<String>(),
                                text.chars().take(32).collect::<String>())
                        })
                    })
                })
                .collect::<Vec<_>>()
        } else {
            vec![(TRAILING_BODY.to_string(), TRAILING_BODY.to_string())]
        };
        assert!(
            !prose_witnesses.is_empty(),
            "capture needs nontrivial assistant prose"
        );
        let initial_end = if physical {
            (source_start as usize + 16_384).min(data.len())
        } else {
            data.len()
        };
        let data = data[source_start as usize..initial_end].to_vec();
        row.turn_start_offset = Some(source_start);
        row.last_offset = source_start;
        save_inflight_state(&row).unwrap();
        fx.identity = InflightTurnIdentity::from_state(&row);
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        if captured {
            let ui = &mut Arc::get_mut(&mut shared).expect("unshared fixture").ui;
            ui.status_panel_v2_enabled = true;
            ui.two_message_panel_enabled = true;
            ui.placeholder_live_events_enabled = true;
        }
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
        if physical {
            crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
                &fx.tmux,
                crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                    runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
                    output_path: fx.output_path.clone(),
                    relay_output_path: None,
                    input_fifo_path: None,
                    session_id: Some("captured-native".into()),
                    last_offset: source_start,
                    relay_last_offset: None,
                },
            );
        }
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
        let mut offset = initial_end as u64;
        let mut buffer = String::new();
        let mut buffer_start = source_start;
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
                data_start_offset: source_start,
                epoch_snapshot: 0,
                source_authority,
            },
            &mut parser,
            &mut relay,
            &mut monitor,
            &mut render,
        );
        let stop = async {
            let deadline =
                tokio::time::Instant::now() + Duration::from_secs(if captured { 15 } else { 1 });
            while !rec.bodies.lock().unwrap().iter().any(|body| {
                prose_witnesses
                    .iter()
                    .any(|(formatted, _)| body.contains(formatted))
            }) && tokio::time::Instant::now() < deadline
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            cancel.store(true, Ordering::Release);
        };
        let (outcome, ()) = tokio::join!(run, stop);
        if captured {
            if let CollectOutcome::Fallthrough(turn) = &outcome {
                eprintln!(
                    "captured collector source_bytes={source_bytes} source_start={source_start} body_bytes={} body_units={} terminal={} pane_dead={} posts={}",
                    turn.full_response.len(),
                    crate::services::discord::formatting::discord_message_units(
                        &turn.full_response
                    ),
                    turn.found_result,
                    turn.active_read_state
                        .as_ref()
                        .is_some_and(|state| state.tmux_death_observed),
                    rec.seen("POST").len()
                );
            } else {
                eprintln!(
                    "captured collector continued before streaming; source_bytes={source_bytes}"
                );
            }
        } else {
            assert_eq!(
                handle.metrics().snapshot().frames_received,
                1,
                "both states feed the passive relay"
            );
        }
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
            assert!(
                !turn.full_response.trim().is_empty(),
                "collector must decode assistant prose"
            );
            if !captured {
                assert!(turn.full_response.contains(TRAILING_BODY));
            }
            if repeats > 1 {
                assert!(
                    crate::services::discord::formatting::discord_message_units(
                        &turn.full_response
                    ) > 2000,
                    "long replay fixture must decode to more than one Discord message"
                );
                assert!(
                    turn.full_response
                        .contains(&format!("{}: {TRAILING_BODY}", repeats - 1)),
                    "collector must decode the final unique commentary record"
                );
            }
            if !captured {
                assert!(
                    rec.bodies
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|body| body.contains(TRAILING_BODY)),
                    "the captured POST must contain assistant prose, not only a status panel"
                );
            }
            assert!(
                rec.bodies.lock().unwrap().iter().any(|body| prose_witnesses
                    .iter()
                    .any(|(formatted, raw)| body.contains(formatted)
                        && turn.full_response.contains(raw))),
                "first visible HTTP body must contain the captured assistant commentary witness"
            );
            assert!(
                turn.placeholder_msg_id.is_some(),
                "first Discord POST must succeed"
            );
            assert!(
                rec.bodies
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|body| body.encode_utf16().count() <= 2000),
                "native replay must split before its first HTTP publication"
            );
        }
        handle.shutdown().await;
        if physical {
            crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&fx.tmux);
        }
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

/// Root supplies a private immutable copy of the original turn range. Never
/// commit the capture; reduce any failure to synthetic records before shipping.
#[test]
#[ignore = "requires private immutable source capture; root runs explicitly"]
fn captured_native_collector_first_frame_reaches_http() {
    collector_case(
        false,
        0,
        "captured_native_collector_first_frame_reaches_http",
    );
}

#[test]
#[ignore = "requires private immutable source capture; root runs explicitly"]
fn captured_native_collector_physical_batches_reach_http() {
    collector_case(
        false,
        0,
        "captured_native_collector_physical_batches_reach_http",
    );
}

struct OwnedCapturePane(String);
impl Drop for OwnedCapturePane {
    fn drop(&mut self) {
        crate::services::platform::tmux::kill_session(
            &self.0,
            "5833 private collector fixture cleanup",
        );
    }
}
