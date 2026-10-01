//! A tool-first idle turn on O's channel shows its tool in the live panel at once, through the
//! real idle-tail prefix, bridge and stream tick, with the default status interval.
use super::*;

fn panel_bodies(gateway: &S3Gateway) -> Vec<String> {
    gateway.bodies.lock().unwrap().clone()
}

fn tool(name: &str) -> StreamMessage {
    StreamMessage::ToolUse {
        name: name.into(),
        input: r#"{"command":"cargo test"}"#.into(),
        tool_use_id: Some(format!("toolu_{name}")),
    }
}

/// Runs one Claude idle turn whose first frame is a Bash call. `then_done` sends Done right
/// behind it; otherwise Read and then Done follow once the panel shows Bash. None when the
/// idle prefix stays shut.
fn tool_first_turn(o_owned: bool, then_done: bool, channel: u64) -> Option<Vec<String>> {
    let _telemetry = crate::services::observability::lock_env_then_runtime();
    let temp = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        temp.path(),
    );
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let runtime = RuntimeHandoffKind::ClaudeTui;
    let _o = o_owned.then(|| {
        crate::services::tui_o::cutover::test_override::force_channels(&[(channel, runtime)])
    });
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        Arc::get_mut(&mut shared)
            .unwrap()
            .ui
            .status_panel_v2_enabled = true;
        let (provider, channel) = (ProviderKind::Claude, ChannelId::new(channel));
        let tmux = format!("o-tool-first-{}", channel.get());
        let generation = crate::services::tmux_common::session_temp_path(&tmux, "generation");
        std::fs::write(&generation, b"1").unwrap();
        let output = temp.path().join("transcript.jsonl");
        std::fs::write(&output, b"").unwrap();
        let binding = crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: runtime,
            output_path: output.to_str().unwrap().into(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        };
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(&tmux, binding);
        let mut lease = ExternalInputRelayLease::unassigned(Some(channel.get()));
        lease.turn_id = Some(format!("o-tool-first-{}", channel.get()));
        lease.relay_owner = ExternalInputRelayOwner::BridgeAdapter;
        lease.runtime_kind = Some(runtime);
        let lease = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
            provider.as_str(),
            &tmux,
            lease,
        );
        let (anchor, prompt) = (MessageId::new(channel.get() + 1), "tool first prompt");
        let claim = synthetic_start::claim_tui_direct_synthetic_turn(
            &shared, &provider, channel, &tmux, prompt, anchor, &lease,
        );
        assert!(claim.await.claimed);

        let (tx, rx) = mpsc::channel();
        tx.send(tool("Bash")).unwrap();
        let done = || StreamMessage::Done {
            result: "ADK-C2A-tool-first-body".into(),
            session_id: None,
        };
        if then_done {
            tx.send(done()).unwrap();
        }
        let tool_opens = claude_idle_bridge::idle_tail_tool_opens(channel, &tmux);
        let (opened_tx, opened_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = opened_tx.send(claude_idle_bridge::buffer_idle_prefix(rx, tool_opens));
        });
        let Ok((prefix, true, rest)) = opened_rx.recv_timeout(Duration::from_secs(1)) else {
            drop(tx);
            return None;
        };

        let gateway = Arc::new(S3Gateway {
            local_delivery: true,
            ..Default::default()
        });
        let source = claude_idle_bridge::IdleBridgeSource {
            tmux_session_name: &tmux,
            output_path: &output,
            start_offset: 0,
            prompt_text: prompt,
            lease: &lease,
        };
        let delivery = claude_idle_bridge::stream_tui_idle_response_with_gateway(
            &shared,
            provider.clone(),
            channel,
            source,
            (prefix, rest, None),
            gateway.clone(),
            0,
        );
        let observe = async {
            if !then_done {
                // Well inside the 5s status interval: only the tool fast lane can edit by then.
                let shown = tokio::time::timeout(Duration::from_secs(2), async {
                    while !panel_bodies(&gateway)
                        .iter()
                        .any(|body| body.contains("Bash"))
                    {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                });
                let dump = || gateway.traffic_dump();
                assert!(
                    shown.await.is_ok(),
                    "Bash shows while the text is withheld:{}",
                    dump()
                );
                // Spinner ticks and a next tool wait for the interval: no edit storm.
                tokio::time::sleep(Duration::from_millis(1200)).await;
                tx.send(tool("Read")).unwrap();
                tokio::time::sleep(Duration::from_millis(1200)).await;
                assert_eq!(panel_bodies(&gateway).len(), 1, "{}", dump());
                tx.send(done()).unwrap();
            }
            drop(tx);
        };
        let delivery = tokio::time::timeout(Duration::from_secs(5), delivery);
        let (delivered, ()) = tokio::join!(delivery, observe);
        assert!(
            delivered.is_ok(),
            "the turn settles:{}",
            gateway.traffic_dump()
        );
        Some(panel_bodies(&gateway))
    })
}

#[test]
fn o_channel_panel_shows_a_first_tool_before_the_status_interval() {
    let opened = "a tool call opens O's idle stream";
    let bodies = tool_first_turn(true, false, 583_325_101).expect(opened);
    assert!(
        bodies.last().is_some_and(|body| body.contains("Read")),
        "the last tool still unshown at the end is shown once: {bodies:?}"
    );
    let quick = tool_first_turn(true, true, 583_325_201).expect(opened);
    assert!(
        quick.iter().any(|body| body.contains("Bash")),
        "a tool that ends inside the status interval is still shown once: {quick:?}"
    );
    let body_free = |bodies: &[String]| bodies.iter().all(|body| !body.contains("ADK-C2A"));
    assert!(
        body_free(&bodies) && body_free(&quick),
        "{bodies:?} {quick:?}"
    );
    assert!(
        tool_first_turn(false, false, 583_325_301).is_none(),
        "on a Legacy channel a tool call alone starts no bridge and no panel write"
    );
}
