use super::*;
use crate::services::discord::inflight;
use crate::services::discord::runtime_store::rename_fault;
use crate::services::discord::turn_bridge::cancel_backstop_test_support::BridgePause;

#[test]
fn native_pin_direct_none_reader_retries_io_on_the_first_forwarded_frame() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let channel = ChannelId::new(775_600_201);
        let provider = ProviderKind::Claude;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let tmux = "AgentDesk-claude-direct-pin-retry-7756";
        let output = crate::config::runtime_root().unwrap().join("direct-pin-retry.jsonl");
        let prompt = "direct retry prompt";
        let user = serde_json::json!({"type":"user","uuid":"direct-retry-user","sessionId":"direct-retry-sid",
            "message":{"role":"user","content":prompt}}).to_string() + "\n";
        let assistant = serde_json::json!({"type":"assistant","sessionId":"direct-retry-sid",
            "message":{"role":"assistant","content":[{"type":"text","text":"first direct frame"}]}}).to_string() + "\n";
        std::fs::write(&output, format!("{user}{assistant}{{\"type\":\"system\",\"subtype\":\"stop_hook_summary\"}}\n")).unwrap();
        for (suffix, value) in [("spawn_nonce", "retry-live-pane"), ("generation", "1")] {
            std::fs::write(crate::services::tmux_common::session_temp_path(tmux, suffix), value).unwrap();
        }
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(tmux,
            crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: output.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("direct-retry-sid".into()),
                last_offset: user.len() as u64,
                relay_last_offset: None,
            });
        let mut lease = ExternalInputRelayLease::unassigned(Some(channel.get()));
        lease.turn_id = Some("direct-pin-retry-7756".into());
        lease.relay_owner = ExternalInputRelayOwner::BridgeAdapter;
        lease.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        let lease = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
            provider.as_str(), tmux, lease,
        );
        assert!(synthetic_start::claim_tui_direct_synthetic_turn(&shared, &provider, channel, tmux,
            prompt, MessageId::new(channel.get() + 1), &lease).await.claimed);
        // Finish anchor binding without dropping a captured bridge's live lease.
        let initial = inflight::load_inflight_state(&provider, channel.get()).unwrap();
        let mut locked = inflight::lock_inflight_episode(&provider, channel.get(),
            &inflight::InflightEpisodePin::from_state(&initial)).unwrap();
        assert_eq!(locked.bind_synthetic_anchor_under_guard(), inflight::GuardedSaveOutcome::Saved);
        synthetic_start::bridge_handoff::preserve_stamped_source(
            &inflight::InflightEpisodePin::from_state(&initial), locked.state(),
        );
        drop(locked);
        let path = inflight::inflight_state_path(&inflight::inflight_runtime_root().unwrap(), &provider, channel.get());
        let fault = rename_fault::Armed::new(&path);
        let paused = BridgePause::install(channel);

        let (raw_tx, raw_rx) = mpsc::channel();
        let (_, harvest) = crate::services::session_backend::read_claude_tui_output_file_until_result_with_harvest(
            output.to_str().unwrap(), user.len() as u64, raw_tx, None,
            crate::services::provider::SessionProbe::process(|| true),
        ).unwrap();
        assert!(harvest.decoded_terminal);
        let first_frame = raw_rx.recv().unwrap();
        assert_eq!(inflight::native_pin_observation_count(channel.get()), 0);
        let (rest_tx, rest_rx) = mpsc::channel();
        let (end_tx, end_rx) = tokio::sync::oneshot::channel();
        let source = claude_idle_bridge::IdleBridgeSource {
            tmux_session_name: tmux,
            output_path: &output,
            start_offset: user.len() as u64,
            prompt_text: prompt,
            lease: &lease,
        };
        let delivery = claude_idle_bridge::stream_tui_idle_response_with_gateway(
            &shared, provider.clone(), channel, source,
            (vec![first_frame], rest_rx, Some(end_rx)),
            Arc::new(S3Gateway { local_delivery: true, ..Default::default() }), 0,
        );
        let inspect = async {
            let retried = tokio::task::spawn_blocking(move || inflight::wait_native_pin_observations(
                channel.get(), 2, Duration::from_secs(3),
            )).await.unwrap();
            let durable = inflight::load_inflight_state(&provider, channel.get()).unwrap();
            let evidence = fault.evidence();
            drop(rest_tx);
            let _ = end_tx.send(Err("fixture stops after its first frame".into()));
            let entered = paused.wait_for_entry(Duration::from_secs(3)).await;
            let aborted = entered && paused.abort_and_wait(Duration::from_secs(3)).await;
            (retried, durable, evidence, entered, aborted)
        };
        let (delivered, (retried, durable, (renames, errors), entered, aborted)) = tokio::join!(delivery, inspect);
        let callbacks = inflight::native_pin_observation_count(channel.get());
        let diagnostic = format!("delivery={delivered:?}, callbacks={callbacks}, renames={renames}, errors={errors:?}");
        assert!(entered && aborted, "the actual bridge must enter and stop cleanly: {diagnostic}");
        assert!(retried, "the direct reader has no token, so its first bridge frame must retry pin persistence: {diagnostic}");
        assert_eq!(errors.len(), 1, "one real rename failed: {errors:?}");
        assert_eq!(renames, 2, "the first frame must make the next durable write");
        assert_eq!(callbacks, 2);
        let saved = serde_json::to_value(durable).unwrap();
        assert_eq!(saved["native_stop_user_pin"]["input"]["uuid"], "direct-retry-user");
        assert_eq!(saved["native_stop_user_pin"]["input"]["direct"], true);
    });
}
