use super::tests::{inflight_with_identity_offset, matched, terminal_frame_offset};
use super::*;
use crate::services::discord::inflight::RelayOwnerKind;

// Kills M6: removing the fenced-terminal disjunct must lose this terminal outcome.
#[tokio::test]
async fn fenced_terminal_without_parser_delivery_is_terminal_not_delivered() {
    let binding = matched("44001");
    let sink = SessionBoundDiscordRelaySink::new(Arc::new(HealthRegistry::new()));
    let terminal = terminal_frame_offset(
        &binding,
        "{\"type\":\"result\",\"result\":\"\"}\n",
        1,
        256,
        0,
        "2026-08-03T00:00:00Z",
        Some(64),
    );

    let outcome = sink
        .deliver(&terminal)
        .await
        .expect("a fenced terminal without parser delivery is known");

    assert_eq!(outcome, RelaySinkOutcome::TerminalNotDelivered);
}

// Kills M8: transport errors must escape instead of folding into NotDelivered.
#[tokio::test]
async fn relay_deliver_propagates_injected_transport_error() {
    let temp = tempfile::tempdir().expect("temp runtime root");
    let _root = crate::config::set_agentdesk_root_for_test(temp.path());
    let channel_id = 44_002;
    let binding = matched(&channel_id.to_string());
    let session = &binding.expected_session_name;
    let generation_path = crate::services::tmux_common::session_temp_path(session, "generation");
    std::fs::create_dir_all(
        std::path::Path::new(&generation_path)
            .parent()
            .expect("generation parent"),
    )
    .expect("generation directory");
    std::fs::write(&generation_path, b"transport-error").expect("generation marker");
    let started_at = "2026-08-03T00:00:01Z";
    let mut inflight = inflight_with_identity_offset(channel_id, session, 700, started_at, Some(0));
    inflight.set_relay_owner_kind(RelayOwnerKind::SessionBoundRelay);
    inflight.current_msg_id = 88_002;
    crate::services::discord::inflight::save_inflight_state(&inflight).expect("persist inflight");
    let registry = Arc::new(HealthRegistry::new());
    let shared = crate::services::discord::make_shared_data_for_tests();
    registry
        .register(ProviderKind::Claude.as_str().to_string(), shared)
        .await;
    let gateway = Arc::new(RelayContractFakeGateway::failing("fake transport failure"));
    let mut sink = SessionBoundDiscordRelaySink::new(registry);
    sink.test_gateway = Some(gateway.clone());
    let payload = concat!(
        "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"answer\"}]}}\n",
        "{\"type\":\"result\",\"result\":\"answer\"}\n"
    );
    let terminal = terminal_frame_offset(&binding, payload, 1, 256, 700, started_at, Some(0));

    let error = sink
        .deliver(&terminal)
        .await
        .expect_err("transport failure must escape RelaySink::deliver");

    assert!(matches!(error, RelaySinkError::Transient(_)), "{error:?}");
    assert_eq!(gateway.replace_calls.load(Ordering::Acquire), 1);
    crate::services::discord::inflight::clear_inflight_state(&ProviderKind::Claude, channel_id);
}

// Kills M10 and anchor-drop: persisted proof stays Delivered and records the tail anchor.
#[tokio::test]
async fn relay_deliver_preserves_tail_anchor_and_observes_persisted_proof() {
    let temp = tempfile::tempdir().expect("temp runtime root");
    let _root = crate::config::set_agentdesk_root_for_test(temp.path());
    let channel_id = 44_003;
    let binding = matched(&channel_id.to_string());
    let session = &binding.expected_session_name;
    let generation_path = crate::services::tmux_common::session_temp_path(session, "generation");
    std::fs::create_dir_all(
        std::path::Path::new(&generation_path)
            .parent()
            .expect("generation parent"),
    )
    .expect("generation directory");
    std::fs::write(&generation_path, b"persisted-proof").expect("generation marker");
    let generation = dr::current_generation_mtime_ns(session);
    let started_at = "2026-08-03T00:00:02Z";
    let mut inflight = inflight_with_identity_offset(channel_id, session, 701, started_at, Some(0));
    inflight.set_relay_owner_kind(RelayOwnerKind::SessionBoundRelay);
    inflight.current_msg_id = 88_003;
    crate::services::discord::inflight::save_inflight_state(&inflight).expect("persist inflight");
    let registry = Arc::new(HealthRegistry::new());
    let shared = crate::services::discord::make_shared_data_for_tests();
    registry
        .register(ProviderKind::Claude.as_str().to_string(), shared.clone())
        .await;
    let gateway = Arc::new(RelayContractFakeGateway::edited());
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let mut sink = SessionBoundDiscordRelaySink::new(registry);
    sink.test_gateway = Some(gateway.clone());
    sink.test_replace_anchor = Some(formatting::ReplaceLastChunkAnchor {
        msg_id: 99_003,
        text: "tail chunk".to_string(),
    });
    sink.test_delivery_outcomes = Some(outcomes.clone());
    sink.test_force_legacy_replace = true;
    let payload = concat!(
        "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"answer\"}]}}\n",
        "{\"type\":\"result\",\"result\":\"answer\"}\n"
    );
    let mut terminal = terminal_frame_offset(&binding, payload, 1, 256, 701, started_at, Some(0));
    terminal.relay_generation_mtime_ns = Some(generation);
    std::fs::write(&binding.expected_rollout_path, format!("{payload: <256}")).unwrap();

    let outcome = sink.deliver(&terminal).await.expect("persisted delivery");

    assert_eq!(outcome, RelaySinkOutcome::TerminalDelivered);
    assert_eq!(
        outcomes.lock().expect("outcome probe").as_slice(),
        &[SessionRelayDeliveryOutcome::Delivered],
        "M10: persisted proof must remain a typed Delivered outcome"
    );
    let record = dr::read_record(&ProviderKind::Claude, channel_id).expect("delivery record");
    assert_eq!(
        record.delivered_frontier.expect("frontier").panel_msg_id,
        Some(99_003),
        "anchor-drop: legacy replace must retain the formatter tail anchor"
    );
    assert_eq!(gateway.replace_calls.load(Ordering::Acquire), 1);
    let mut normalized_idle = terminal.clone();
    normalized_idle.relay_range = Some((0, 256));
    assert_eq!(
        sink.deliver(&normalized_idle).await.unwrap(),
        RelaySinkOutcome::TerminalDelivered
    );
    assert_eq!(
        gateway.replace_calls.load(Ordering::Acquire),
        1,
        "normalized idle receipt replay remains deduplicated"
    );
    crate::services::discord::inflight::clear_inflight_state(&ProviderKind::Claude, channel_id);
    drop(_root);
    native_codex_restart_sink_fixture().await;
}

async fn native_codex_restart_sink_fixture() {
    use crate::services::discord::{inflight, mailbox_snapshot, mailbox_try_start_turn};
    use crate::services::provider::CancelToken;
    for (successor, retained_sink) in [(false, false), (false, true), (true, false)] {
        let temp = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(temp.path());
        let channel =
            ChannelId::new(50710031 + u64::from(successor) * 2 + u64::from(retained_sink));
        let path = temp.path().join("rollout-restart.jsonl");
        let binding = super::tests::matched_codex(&channel.get().to_string());
        assert_ne!(binding.expected_rollout_path, path.to_string_lossy());
        let tmux = &binding.expected_session_name;
        let generation_path = crate::services::tmux_common::session_temp_path(tmux, "generation");
        std::fs::create_dir_all(std::path::Path::new(&generation_path).parent().unwrap()).unwrap();
        std::fs::write(&generation_path, "117").unwrap();
        let generation = dr::current_generation_mtime_ns(tmux);
        let prefix = "{\"type\":\"session_meta\",\"payload\":{\"id\":\"original-codex\"}}\n";
        let captured = concat!(
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"commentary\",\"content\":[{\"type\":\"output_text\",\"text\":\"working first\"}]}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"name\":\"exec_command\",\"call_id\":\"pending\",\"arguments\":\"{}\"}}\n",
        );
        let body = concat!(
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"ADK5071-native\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"ADK5071-native\"}]}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"last_agent_message\":\"ADK5071-native\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call_output\",\"call_id\":\"pending\",\"output\":\"done\"}}\n",
        );
        std::fs::write(&path, format!("{prefix}{captured}{body}")).unwrap();
        let start = prefix.len() as u64;
        let cursor = start + captured.len() as u64;
        let end = std::fs::metadata(&path).unwrap().len();
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            tmux,
            crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
                output_path: path.to_string_lossy().into_owned(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("original-codex".into()),
                last_offset: cursor,
                relay_last_offset: None,
            },
        );
        let file_identity =
            crate::services::cluster::stream_relay::SourceFileIdentity::from_open_file(
                &std::fs::File::open(&path).unwrap(),
            );
        // Native recovery needs the actual opened file even in the default Legacy mode.
        let witness = crate::services::discord::tmux::tmux_output_stream::watcher_source_witness(
            &ProviderKind::Codex,
            tmux,
            path.to_str().unwrap(),
        )
        .unwrap();
        let stamp =
            crate::services::discord::delivery_lease_cell::source_epoch_observer::source_stamp(
                tmux,
                witness,
                file_identity,
            )
            .unwrap();
        let mut row = inflight_with_identity_offset(
            channel.get(),
            tmux,
            1548524702677471305,
            "2026-09-13 11:44:19",
            Some(start),
        );
        row.provider = "codex".into();
        row.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui);
        row.last_offset = cursor;
        row.full_response = "working first".into();
        row.turn_source = TurnSource::ExternalInput;
        row.injected_prompt_message_id = Some(row.user_msg_id);
        row.output_path = Some(path.display().to_string());
        row.turn_nonce = Some("37157420-570f-43c9-9c65-561a7ee8fddc".into());
        row.set_restart_mode(crate::services::discord::InflightRestartMode::DrainRestart);
        row.set_relay_owner_kind(RelayOwnerKind::SessionBoundRelay);
        assert_eq!(row.current_msg_id, 0);
        assert!(row.session_id.is_none());
        let shared = crate::services::discord::make_shared_data_for_tests();
        let actor = Arc::new(CancelToken::from_persisted_turn_nonce(
            row.turn_nonce.clone(),
        ));
        assert!(
            mailbox_try_start_turn(
                &shared,
                channel,
                actor.clone(),
                serenity::model::id::UserId::new(1),
                MessageId::new(row.user_msg_id)
            )
            .await
        );
        let mut replacement = row.clone();
        if successor {
            replacement.user_msg_id += 1;
            replacement.turn_nonce = Some("successor".into());
        }
        inflight::save_inflight_state(&replacement).unwrap();
        let registry = Arc::new(HealthRegistry::new());
        registry.register("codex".into(), shared.clone()).await;
        let gateway = Arc::new(RelayContractFakeGateway::edited());
        let mut sink = SessionBoundDiscordRelaySink::new(registry);
        sink.test_gateway = Some(gateway.clone());
        let source = std::fs::read_to_string(&path).unwrap();
        let mut unread = source[cursor as usize..].to_string();
        let mut parser_state = crate::services::session_backend::StreamLineState::new();
        let mut response = row.full_response.clone();
        let mut tools = crate::services::discord::tmux::WatcherToolState::new();
        tools.set_provider(&ProviderKind::Codex);
        let decoder = crate::services::discord::tmux::tmux_output_stream::read_native_codex_state(
            path.to_str().unwrap(),
            start,
            cursor,
            file_identity,
            tmux,
            generation,
            Some(stamp),
        )
        .unwrap();
        tools.restore_native_codex(decoder, &mut response);
        let output_offset = unread.rfind("{\"type\":\"response_item\"").unwrap();
        let output = unread.split_off(output_offset);
        let pending = crate::services::discord::tmux::process_watcher_lines_for_turn(
            &mut unread,
            &mut parser_state,
            &mut response,
            &mut tools,
            Some(cursor),
            Some(start),
        );
        assert!(
            !pending.found_result,
            "restart must retain the pre-cursor pending tool"
        );
        assert!(unread.is_empty());
        unread.push_str(&output);
        let parsed = crate::services::discord::tmux::process_watcher_lines_for_turn(
            &mut unread,
            &mut parser_state,
            &mut response,
            &mut tools,
            Some(cursor + output_offset as u64),
            Some(start),
        );
        assert!(parsed.found_result);
        assert_eq!(response, "working first\n\nADK5071-native");
        let terminal_start = parsed.terminal_evidence_offset.unwrap();
        let terminal_len = source[terminal_start as usize..].find('\n').unwrap() + 1;
        let parsed_end = terminal_start + terminal_len as u64;
        assert_eq!(parsed_end, end - unread.len() as u64);
        let mut terminal = terminal_frame_offset(
            &binding,
            &source[cursor as usize..],
            1,
            parsed_end,
            row.user_msg_id,
            &row.started_at,
            Some(start),
        );
        terminal.relay_generation_mtime_ns = Some(generation);
        terminal.relay_source_stamp = Some(stamp);
        let next_turn = concat!(
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"next prompt\"}]}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"second turn\"}]}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"last_agent_message\":\"second turn\"}}\n",
        );
        std::fs::write(&path, format!("{source}{next_turn}")).unwrap();
        let mut native_idle = terminal.clone();
        native_idle.payload = format!("{}{next_turn}", &source[start as usize..]);
        let idle_end = start + native_idle.payload.len() as u64;
        native_idle.terminal_consumed_end = None;
        native_idle.relay_range = Some((start, idle_end));
        assert_eq!(
            sink.deliver(&native_idle).await.unwrap(),
            RelaySinkOutcome::TerminalNotDelivered
        );
        let committed = dr::effective_committed_offset(
            &shared,
            &ProviderKind::Codex,
            channel,
            tmux,
            Some(idle_end),
        );
        assert_eq!(
            committed, 0,
            "a two-turn idle range cannot advance past either turn"
        );
        assert!(
            dr::read_record(&ProviderKind::Codex, channel.get())
                .and_then(|r| r.delivered_frontier)
                .is_none()
        );
        assert_eq!(gateway.send_calls.load(Ordering::Acquire), 0);
        let retained = inflight::load_inflight_state(&ProviderKind::Codex, channel.get()).unwrap();
        assert_eq!(
            (retained.last_offset, retained.turn_nonce),
            (replacement.last_offset, replacement.turn_nonce.clone())
        );
        assert_eq!(
            crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux)
                .unwrap()
                .last_offset,
            cursor
        );
        assert_eq!(
            super::idle_relay_range_action(
                native_idle.payload.as_bytes(),
                start,
                idle_end,
                committed,
                true,
                false,
                true
            ),
            super::IdleRelayRangeAction::SendPendingSuffixFrom(start)
        );
        let mut stale_parser = super::turn_parser::SessionRelayParser::default();
        let mut unfenced = terminal.clone();
        unfenced.payload = body[..body.rfind("{\"type\":\"response_item\"").unwrap()].into();
        unfenced.terminal_consumed_end = None;
        assert!(stale_parser.ingest_frame(&unfenced).is_empty());
        unfenced.payload = "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"next actor\"}]}}\n".into();
        unfenced.relay_range = Some((start, end));
        assert!(
            stale_parser.ingest_frame(&unfenced).is_empty(),
            "an unfenced prior completion cannot finalize the next actor's message"
        );
        for missing_witness in [false, true] {
            let mut unproven = terminal.clone();
            if missing_witness {
                unproven.relay_source_stamp = None;
            } else {
                unproven.relay_generation_mtime_ns = Some(generation.wrapping_add(1));
            }
            assert!(matches!(
                sink.deliver(&unproven).await,
                Err(RelaySinkError::Transient(_))
            ));
            assert_eq!(gateway.send_calls.load(Ordering::Acquire), 0);
        }
        let mut wrong_file = terminal.clone();
        let foreign_path = temp.path().join("foreign-rollout.jsonl");
        std::fs::write(&foreign_path, &source).unwrap();
        wrong_file.relay_source_stamp.as_mut().unwrap().file =
            crate::services::cluster::stream_relay::SourceFileIdentity::from_open_file(
                &std::fs::File::open(&foreign_path).unwrap(),
            );
        assert!(matches!(
            sink.deliver(&wrong_file).await,
            Err(RelaySinkError::Transient(_))
        ));
        if retained_sink {
            let mut partial = terminal.clone();
            partial.payload = captured.into();
            partial.terminal_consumed_end = None;
            assert_eq!(
                sink.deliver(&partial).await.unwrap(),
                RelaySinkOutcome::FrameAccepted
            );
        }
        let result = sink.deliver(&terminal).await.unwrap();
        if successor {
            assert_eq!(result, RelaySinkOutcome::TerminalNotDelivered);
            assert_eq!(gateway.send_calls.load(Ordering::Acquire), 0);
            let kept = inflight::load_inflight_state(&ProviderKind::Codex, channel.get()).unwrap();
            assert_eq!(kept.user_msg_id, replacement.user_msg_id);
            assert_eq!(kept.turn_nonce, replacement.turn_nonce);
            assert!(
                dr::read_record(&ProviderKind::Codex, channel.get())
                    .and_then(|r| r.delivered_frontier)
                    .is_none()
            );
        } else {
            assert_eq!(result, RelaySinkOutcome::TerminalDelivered);
            assert_eq!(gateway.send_calls.load(Ordering::Acquire), 1);
            assert_eq!(
                gateway.sent_contents.lock().unwrap().as_slice(),
                &["working first\n\nADK5071-native"]
            );
            let receipt = dr::read_record(&ProviderKind::Codex, channel.get())
                .unwrap()
                .delivered_frontier
                .unwrap();
            assert_eq!(receipt.range, (start, end));
            assert_eq!(receipt.generation_mtime_ns, generation);
            assert_eq!(receipt.panel_msg_id, Some(gateway.sent_message_id.get()));
            let mut covered_idle = native_idle.clone();
            covered_idle.relay_range = Some((start, end));
            covered_idle.payload = source[start as usize..].into();
            assert_eq!(
                sink.deliver(&covered_idle).await.unwrap(),
                RelaySinkOutcome::TerminalDelivered
            );
            assert_eq!(
                sink.deliver(&native_idle).await.unwrap(),
                RelaySinkOutcome::TerminalNotDelivered
            );
            assert_eq!(gateway.send_calls.load(Ordering::Acquire), 1);
            assert_eq!(
                super::idle_relay_range_action(
                    native_idle.payload.as_bytes(),
                    start,
                    idle_end,
                    end,
                    true,
                    false,
                    true
                ),
                super::IdleRelayRangeAction::SendPendingSuffixFrom(end)
            );
        }
        let current = mailbox_snapshot(&shared, channel).await;
        if let Some(current) = current.cancel_token {
            assert!(
                Arc::ptr_eq(&current, &actor),
                "sink must never invent a replacement actor"
            );
        }
        assert_eq!(actor.turn_nonce(), row.turn_nonce.as_deref());
        inflight::clear_inflight_state(&ProviderKind::Codex, channel.get());
        crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(tmux);
    }
}

// Kills M11: stale proof must remain distinguishable from Delivered before public folding.
#[tokio::test]
async fn relay_deliver_observes_landed_stale_proof() {
    let temp = tempfile::tempdir().expect("temp runtime root");
    let _root = crate::config::set_agentdesk_root_for_test(temp.path());
    let channel_id = 44_004;
    let binding = matched(&channel_id.to_string());
    let session = &binding.expected_session_name;
    let generation_path = crate::services::tmux_common::session_temp_path(session, "generation");
    std::fs::create_dir_all(
        std::path::Path::new(&generation_path)
            .parent()
            .expect("generation parent"),
    )
    .expect("generation directory");
    std::fs::write(&generation_path, b"landed-stale").expect("generation marker");
    let generation = dr::current_generation_mtime_ns(session);
    let started_at = "2026-08-03T00:00:03Z";
    let mut inflight = inflight_with_identity_offset(channel_id, session, 702, started_at, Some(0));
    inflight.set_relay_owner_kind(RelayOwnerKind::SessionBoundRelay);
    inflight.current_msg_id = 88_004;
    crate::services::discord::inflight::save_inflight_state(&inflight).expect("persist inflight");
    let registry = Arc::new(HealthRegistry::new());
    let shared = crate::services::discord::make_shared_data_for_tests();
    registry
        .register(ProviderKind::Claude.as_str().to_string(), shared.clone())
        .await;
    let gateway = Arc::new(RelayContractFakeGateway {
        on_transport: Some(Arc::new(move || {
            crate::services::discord::inflight::clear_inflight_state(
                &ProviderKind::Claude,
                channel_id,
            );
        })),
        ..RelayContractFakeGateway::edited()
    });
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let mut sink = SessionBoundDiscordRelaySink::new(registry);
    sink.test_gateway = Some(gateway);
    sink.test_delivery_outcomes = Some(outcomes.clone());
    let payload = concat!(
        "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"answer\"}]}}\n",
        "{\"type\":\"result\",\"result\":\"answer\"}\n"
    );
    let mut terminal = terminal_frame_offset(&binding, payload, 1, 256, 702, started_at, Some(0));
    terminal.relay_generation_mtime_ns = Some(generation);

    let outcome = sink
        .deliver(&terminal)
        .await
        .expect("landed stale delivery");

    assert_eq!(outcome, RelaySinkOutcome::TerminalDelivered);
    assert_eq!(
        outcomes.lock().expect("outcome probe").as_slice(),
        &[SessionRelayDeliveryOutcome::LandedStale],
        "M11: stale proof must remain a typed LandedStale outcome"
    );
    assert!(
        dr::read_record(&ProviderKind::Claude, channel_id)
            .and_then(|record| record.delivered_frontier)
            .is_none(),
        "stale source authority must not persist a delivered frontier"
    );
    crate::services::discord::inflight::clear_inflight_state(&ProviderKind::Claude, channel_id);
}

#[tokio::test]
async fn relay_deliver_observes_landed_unrecorded_proof() {
    let temp = tempfile::tempdir().expect("temp runtime root");
    let _root = crate::config::set_agentdesk_root_for_test(temp.path());
    let channel_id = 44_005;
    let binding = matched(&channel_id.to_string());
    let session = &binding.expected_session_name;
    let generation_path = crate::services::tmux_common::session_temp_path(session, "generation");
    std::fs::create_dir_all(
        std::path::Path::new(&generation_path)
            .parent()
            .expect("generation parent"),
    )
    .expect("generation directory");
    std::fs::write(&generation_path, b"landed-unrecorded").expect("generation marker");
    let generation = dr::current_generation_mtime_ns(session);
    let started_at = "2026-08-03T00:00:04Z";
    let mut inflight = inflight_with_identity_offset(channel_id, session, 703, started_at, Some(0));
    inflight.set_relay_owner_kind(RelayOwnerKind::SessionBoundRelay);
    inflight.current_msg_id = 88_005;
    crate::services::discord::inflight::save_inflight_state(&inflight).expect("persist inflight");
    let runtime = temp.path().join("runtime");
    std::fs::create_dir_all(&runtime).expect("runtime directory");
    std::fs::write(runtime.join("discord_delivery_records"), b"not a directory")
        .expect("block delivery record directory");
    let registry = Arc::new(HealthRegistry::new());
    let shared = crate::services::discord::make_shared_data_for_tests();
    registry
        .register(ProviderKind::Claude.as_str().to_string(), shared)
        .await;
    let gateway = Arc::new(RelayContractFakeGateway::edited());
    let mut sink = SessionBoundDiscordRelaySink::new(registry);
    sink.test_gateway = Some(gateway);
    let payload = concat!(
        "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"answer\"}]}}\n",
        "{\"type\":\"result\",\"result\":\"answer\"}\n"
    );
    let mut terminal = terminal_frame_offset(&binding, payload, 1, 256, 703, started_at, Some(0));
    terminal.relay_generation_mtime_ns = Some(generation);

    let outcome = sink
        .deliver(&terminal)
        .await
        .expect("landed unrecorded delivery");

    assert_eq!(outcome, RelaySinkOutcome::TerminalDelivered);
    crate::services::discord::inflight::clear_inflight_state(&ProviderKind::Claude, channel_id);
}

struct RelayContractFakeGateway {
    replace_outcome: ReplaceLongMessageOutcome,
    transport_error: Option<String>,
    sent_message_id: MessageId,
    replace_calls: AtomicU64,
    send_calls: AtomicU64,
    sent_contents: Mutex<Vec<String>>,
    on_transport: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl RelayContractFakeGateway {
    fn edited() -> Self {
        Self {
            replace_outcome: ReplaceLongMessageOutcome::EditedOriginal,
            transport_error: None,
            sent_message_id: MessageId::new(91_001),
            replace_calls: AtomicU64::new(0),
            send_calls: AtomicU64::new(0),
            sent_contents: Mutex::new(Vec::new()),
            on_transport: None,
        }
    }

    fn failing(message: &str) -> Self {
        let mut gateway = Self::edited();
        gateway.transport_error = Some(message.to_string());
        gateway
    }
}

impl crate::services::discord::gateway::TurnGateway for RelayContractFakeGateway {
    fn send_message<'a>(
        &'a self,
        _channel_id: ChannelId,
        content: &'a str,
    ) -> crate::services::discord::gateway::GatewayFuture<'a, Result<MessageId, String>> {
        Box::pin(async move {
            self.send_calls.fetch_add(1, Ordering::AcqRel);
            self.sent_contents.lock().unwrap().push(content.to_string());
            if let Some(on_transport) = &self.on_transport {
                on_transport();
            }
            match &self.transport_error {
                Some(error) => Err(error.clone()),
                None => Ok(self.sent_message_id),
            }
        })
    }

    fn edit_message<'a>(
        &'a self,
        _channel_id: ChannelId,
        _message_id: MessageId,
        _content: &'a str,
    ) -> crate::services::discord::gateway::GatewayFuture<'a, Result<(), String>> {
        panic!("relay contract fake does not use edit_message")
    }

    fn replace_message_with_outcome<'a>(
        &'a self,
        _channel_id: ChannelId,
        _message_id: MessageId,
        _content: &'a str,
    ) -> crate::services::discord::gateway::GatewayFuture<
        'a,
        Result<ReplaceLongMessageOutcome, String>,
    > {
        Box::pin(async move {
            self.replace_calls.fetch_add(1, Ordering::AcqRel);
            if let Some(on_transport) = &self.on_transport {
                on_transport();
            }
            match &self.transport_error {
                Some(error) => Err(error.clone()),
                None => Ok(self.replace_outcome.clone()),
            }
        })
    }

    fn schedule_retry_with_history<'a>(
        &'a self,
        _channel_id: ChannelId,
        _user_message_id: MessageId,
        _user_text: &'a str,
    ) -> crate::services::discord::gateway::GatewayFuture<'a, ()> {
        panic!("relay contract fake does not schedule retries")
    }

    fn dispatch_queued_turn<'a>(
        &'a self,
        _channel_id: ChannelId,
        _intervention: &'a crate::services::discord::Intervention,
        _output_path: &'a str,
        _skip_hook: bool,
        _dispatch_lease: Option<Arc<crate::services::turn_orchestrator::DispatchLease>>,
    ) -> crate::services::discord::gateway::GatewayFuture<'a, Result<(), String>> {
        panic!("relay contract fake does not dispatch turns")
    }

    fn validate_live_routing<'a>(
        &'a self,
        _channel_id: ChannelId,
    ) -> crate::services::discord::gateway::GatewayFuture<'a, Result<(), String>> {
        panic!("relay contract fake does not validate routing")
    }

    fn requester_mention(&self) -> Option<String> {
        None
    }

    fn can_chain_locally(&self) -> bool {
        false
    }

    fn bot_owner_provider(&self) -> Option<ProviderKind> {
        None
    }
}
