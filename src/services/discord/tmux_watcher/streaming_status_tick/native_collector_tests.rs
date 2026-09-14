//! Native collector -> sink -> receipt -> preview test: synthetic JSONL and local HTTP only.
use super::*;
use crate::services::cluster::relay_producer_registry::RelayProducerRegistry;
use crate::services::cluster::session_matcher::MatchedChannel;
use crate::services::cluster::stream_relay::{SourceFileIdentity, spawn_stream_relay};
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

#[test]
fn recovered_native_preview_terminal_has_one_visible_copy() {
    const CHILD: &str = "AGENTDESK_5833_NATIVE_COLLECTOR_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let qualified = format!(
            "{}::recovered_native_preview_terminal_has_one_visible_copy",
            module_path!().split_once("::").unwrap().1,
        );
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
    // Configuration installation has no uninstall, hence the isolated child.
    let mut config = crate::config::Config::default();
    config.runtime.relay_authority_mode = crate::config::RelayAuthorityMode::Enforce;
    config.runtime.relay_authority_cohort_percent = 100;
    crate::config_live_reload::install(config);
    capture_warns(async {
        let (mut fx, mut row) = seed_recovered_row(root.root.path(), 5834);
        let marker = crate::services::tmux_common::session_temp_path(&fx.tmux, "generation");
        std::fs::write(&marker, b"1").unwrap();
        let data = format!(
            "{}\n",
            serde_json::json!({"type":"response_item", "payload": {
                "id":"commentary-0", "type":"message", "role":"assistant",
                "phase":"commentary", "channel":"commentary",
                "content":[{"type":"output_text", "text":format!("0: {TRAILING_BODY}")}]
            }})
        )
        .into_bytes();
        std::fs::write(&fx.output_path, &data).unwrap();
        row.turn_start_offset = Some(0);
        row.last_offset = 0;
        save_inflight_state(&row).unwrap();
        fx.identity = InflightTurnIdentity::from_state(&row);
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        let ui = &mut Arc::get_mut(&mut shared).expect("unshared fixture").ui;
        ui.status_panel_v2_enabled = true;
        ui.two_message_panel_enabled = true;
        ui.placeholder_live_events_enabled = true;
        let rec = recorder_for_cycle(fx.channel, true, true).await;
        let actor = Arc::new(
            crate::services::provider::CancelToken::from_persisted_turn_nonce(
                row.turn_nonce.clone(),
            ),
        );
        assert!(
            crate::services::discord::mailbox_try_start_turn(
                &shared,
                fx.channel,
                actor,
                serenity::UserId::new(row.request_owner_user_id),
                serenity::MessageId::new(row.user_msg_id),
            )
            .await
        );
        let ctx = TurnStreamCollectorContext {
            http: rec.http.clone(),
            shared: shared.clone(),
            channel_id: fx.channel,
            watcher_provider: fx.provider.clone(),
            tmux_session_name: fx.tmux.clone(),
            output_path: fx.output_path.clone(),
            input_fifo_path: String::new(),
            watcher_thread_channel_id: None,
            cancel: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
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
        shared.tmux_watchers.insert(
            fx.channel,
            crate::services::discord::TmuxWatcherHandle {
                tmux_session_name: fx.tmux.clone(),
                output_path: fx.output_path.clone(),
                paused: ctx.paused.clone(),
                resume_offset: Arc::new(Mutex::new(None)),
                cancel: ctx.cancel.clone(),
                pause_epoch: ctx.pause_epoch.clone(),
                turn_delivered: ctx.turn_delivered.clone(),
                last_heartbeat_ts_ms: ctx.last_heartbeat_ts_ms.clone(),
            },
        );
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            &fx.tmux,
            crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
                output_path: fx.output_path.clone(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("synthetic-native".into()),
                last_offset: 0,
                relay_last_offset: None,
            },
        );
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
        let health = Arc::new(crate::services::discord::health::HealthRegistry::new());
        health.register("codex".into(), shared.clone()).await;
        let mut sink =
            crate::services::discord::session_relay_sink::SessionBoundDiscordRelaySink::new(health);
        sink.enable_delivery_for_test();
        sink.test_gateway = Some(Arc::new(
            crate::services::discord::gateway::DiscordGateway::new(
                rec.http.clone(),
                shared.clone(),
                fx.provider.clone(),
                None,
            ),
        ));
        let handle = spawn_stream_relay(
            MatchedChannel {
                channel_id: fx.channel.get().to_string(),
                agent_id: "native-collector".into(),
                provider: fx.provider.clone(),
                expected_session_name: fx.tmux.clone(),
                expected_rollout_path: fx.output_path.clone(),
            },
            Arc::new(sink),
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
        let finish_input = async {
            use std::io::Write;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while !rec
                .bodies
                .lock()
                .unwrap()
                .iter()
                .any(|body| body.contains(TRAILING_BODY))
                && tokio::time::Instant::now() < deadline
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(
                !rec.seen("POST").is_empty(),
                "preview must precede terminal input"
            );
            let terminal = concat!(
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"channel\":\"final\",\"content\":[{\"type\":\"output_text\",\"text\":\"ADK5833-final\"}]}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"last_agent_message\":\"ADK5833-final\"}}\n",
            );
            std::fs::OpenOptions::new()
                .append(true)
                .open(&fx.output_path)
                .unwrap()
                .write_all(terminal.as_bytes())
                .unwrap();
            ctx.jsonl_notify.notify_one();
        };
        let (outcome, ()) = tokio::join!(
            async {
                tokio::time::timeout(Duration::from_secs(30), run)
                    .await
                    .expect("bounded collector")
            },
            finish_input,
        );
        let CollectOutcome::Fallthrough(mut turn) = outcome else {
            panic!("terminal collector discarded turn")
        };
        assert!(turn.found_result);
        assert_eq!(
            turn.full_response,
            format!("0: {TRAILING_BODY}\n\nADK5833-final")
        );
        let target = ack
            .clone()
            .expect("terminal producer must retain exact ACK");
        assert_eq!(target.turn_start_offset, Some(0));
        let guard = run_pre_emit_guard(
            &PreEmitGuardContext {
                captured_turn: turn.startup_inflight_snapshot.as_ref(),
                cancel: &ctx.cancel,
                http: &rec.http,
                shared: &shared,
                channel_id: fx.channel,
                watcher_provider: &fx.provider,
                tmux_session_name: &fx.tmux,
                output_path: &fx.output_path,
                paused: &ctx.paused,
                pause_epoch: &ctx.pause_epoch,
                turn_delivered: &ctx.turn_delivered,
            },
            PreEmitGuardLocals {
                epoch_snapshot: 0,
                monitor_auto_turn_deferred: turn.monitor_auto_turn_deferred,
                placeholder_msg_id: turn.placeholder_msg_id,
                turn_data_start_offset: turn.turn_data_start_offset,
                current_offset: offset,
                response_sent_offset: turn.response_sent_offset,
                data_start_offset: 0,
                stale_resume_detected: turn.stale_resume_detected,
                last_edit_text: &turn.last_edit_text,
            },
            &mut PreEmitGuardState {
                monitor_auto_turn_claimed: &mut turn.monitor_auto_turn_claimed,
                monitor_auto_turn_finished: &mut turn.monitor_auto_turn_finished,
                monitor_auto_turn_synthetic_msg_id: &mut turn.monitor_auto_turn_synthetic_msg_id,
                monitor_auto_turn_ledger_generation: &mut turn.monitor_auto_turn_ledger_generation,
                all_data: &mut buffer,
                all_data_start_offset: &mut buffer_start,
                all_data_fully_mirrored_to_session_relay: &mut mirrored,
                all_data_session_bound_relay_ack: &mut ack,
                all_data_first_forwarded_relay_sequence: &mut first,
                last_relayed_offset: &mut None,
                last_observed_generation_mtime_ns: &mut None,
                full_response: &mut turn.full_response,
            },
        )
        .await;
        assert_eq!(guard, PreEmitGuardOutcome::Proceed);
        let before_relay = load_inflight_state(&fx.provider, fx.channel.get());
        let context = TerminalRelayPlanContext {
            http: &rec.http,
            shared: &shared,
            channel_id: fx.channel,
            watcher_provider: &fx.provider,
            tmux_session_name: &fx.tmux,
            output_path: &fx.output_path,
            inflight_before_relay: &before_relay,
            cached_relay_producer: &cached,
            prompt_anchor_present_before_relay: false,
            external_input_lease_before_relay: false,
            session_bound_relay_turn_fully_mirrored: turn.session_bound_relay_turn_fully_mirrored,
            session_bound_relay_turn_first_forwarded_sequence: turn
                .session_bound_relay_turn_first_forwarded_sequence,
            split_trailing_turn_follows: turn.split_trailing_turn_follows,
            startup_soft_terminal_authority: watcher_soft_terminal_has_turn_authority(
                turn.startup_inflight_snapshot.as_ref(),
                &fx.tmux,
                0,
                row.turn_nonce.as_deref(),
            ),
        };
        let mut plan_state = TerminalRelayPlanState {
            all_data_session_bound_relay_ack: &mut ack,
            monitor_auto_turn_claimed: &mut turn.monitor_auto_turn_claimed,
            monitor_auto_turn_finished: &mut turn.monitor_auto_turn_finished,
            monitor_auto_turn_synthetic_msg_id: &mut turn.monitor_auto_turn_synthetic_msg_id,
            monitor_auto_turn_ledger_generation: &mut turn.monitor_auto_turn_ledger_generation,
        };
        let (plan, ()) = tokio::join!(
            run_terminal_relay_plan(
                &context,
                TerminalRelayPlanLocals {
                    current_offset: offset,
                    data_start_offset: 0,
                    all_data: &buffer,
                    full_response: &turn.full_response,
                    current_response: &turn.full_response,
                    response_sent_offset: turn.response_sent_offset,
                    has_assistant_response: true,
                    terminal_kind: turn.terminal_kind,
                    task_notification_kind: turn.task_notification_kind,
                    assistant_text_seen: turn.assistant_text_seen,
                    fresh_assistant_text_seen: turn.fresh_assistant_text_seen,
                    tool_state: &turn.tool_state,
                    placeholder_msg_id: turn.placeholder_msg_id,
                    status_panel_msg_id: turn.status_panel_msg_id,
                },
                &mut plan_state,
            ),
            async {
                tokio::time::sleep(Duration::from_millis(25)).await;
                rec.terminal_gate.notify_one();
            }
        );
        let TerminalRelayPlanOutcome::Proceed(plan) = plan else {
            panic!("terminal plan lost obligation")
        };
        assert!(plan.session_bound_relay_owns_terminal_delivery);
        terminal_send::committed_placeholder_cleanup::reconcile_confirmed_preview(
            terminal_send::committed_placeholder_cleanup::ConfirmedPreviewCleanup {
                http: &rec.http,
                shared: &shared,
                provider: &fx.provider,
                channel: fx.channel,
                session: &fx.tmux,
                expected_turn: before_relay.as_ref(),
                range: (0, offset),
                sent_offset: turn.response_sent_offset,
                placeholder: &mut turn.placeholder_msg_id,
                restored: &mut turn.placeholder_from_restored_inflight,
                edit: &mut turn.last_edit_text,
                frozen: &mut turn.watcher_streaming_rollover_frozen_msg_ids,
            },
        )
        .await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while target
                .metrics
                .terminal_outcome_for_sequence(target.sequence)
                .is_none()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("terminal sink must resolve its exact sequence");
        assert_eq!(
            target
                .metrics
                .terminal_outcome_for_sequence(target.sequence),
            Some(crate::services::cluster::stream_relay::DeliveryOutcome::Delivered),
        );
        let receipt = crate::services::discord::outbound::delivery_record::read_record(
            &fx.provider,
            fx.channel.get(),
        )
        .unwrap()
        .delivered_frontier
        .unwrap();
        assert_eq!(receipt.range, (0, offset));
        assert_eq!(
            receipt.generation_mtime_ns,
            source_authority.generation_mtime_ns
        );
        let visible = rec.visible.lock().unwrap();
        assert!(visible[&receipt.panel_msg_id.unwrap()].contains("ADK5833-final"));
        assert_eq!(
            visible
                .values()
                .filter(|body| body.contains(TRAILING_BODY))
                .count(),
            1,
            "exact terminal delivery must leave one visible copy of the commentary",
        );
        drop(visible);
        handle.shutdown().await;
        crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&fx.tmux);
        std::fs::remove_file(marker).unwrap();
    });
}
