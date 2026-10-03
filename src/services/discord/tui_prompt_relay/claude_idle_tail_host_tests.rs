use super::*;
use crate::services::provider_teardown::tests::test_support::FakeTmux;

fn lease(channel_id: ChannelId, tmux: &str) -> ExternalInputRelayLease {
    let lease = ExternalInputRelayLease {
        channel_id: Some(channel_id.get()),
        turn_id: Some(format!("external:claude:{}:p5c:1", channel_id.get())),
        session_key: Some(format!("host:{tmux}")),
        relay_owner: ExternalInputRelayOwner::BridgeAdapter,
        runtime_kind: Some(RuntimeHandoffKind::ClaudeTui),
        generation:
            crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
    };
    crate::services::tui_prompt_dedupe::record_external_input_turn_lease("claude", tmux, lease)
}

// A missing tmux session ends the reader; another host keeps polling without a tmux probe.
#[tokio::test]
async fn the_idle_tail_reads_a_session_dead_only_through_its_tmux_host() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    for (n, marker) in [None, Some("herdr")].into_iter().enumerate() {
        let tmux = format!("AgentDesk-claude-p5c-idle-tail-{n}");
        let fake = FakeTmux::install(&tmux);
        fake.mark_missing();
        let channel = ChannelId::new(940_000_000_005_340 + n as u64);
        if let Some(body) = marker {
            let path = crate::services::tmux_common::session_temp_path(&tmux, "host_kind");
            std::fs::create_dir_all(Path::new(&path).parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        let transcript = dir.path().join(format!("tail-{n}.jsonl"));
        std::fs::write(&transcript, "").unwrap();
        let probe = idle_tail_probe(&tmux, transcript.to_str().unwrap());
        assert_eq!((probe.is_alive)(), marker.is_some(), "{marker:?}");
        let calls = fake.take_calls();
        if marker.is_some() {
            assert!(
                calls.is_empty(),
                "another host must not probe tmux: {calls:?}"
            );
            continue;
        }
        assert!(calls.iter().any(|call| call.starts_with("has-session")));
        let tail = run_claude_idle_response_tail(
            crate::services::discord::make_shared_data_for_tests(),
            tmux.clone(),
            channel,
            transcript.clone(),
            0,
            "direct input".to_string(),
            lease(channel, &tmux),
        );
        let ended = tokio::time::timeout(Duration::from_secs(30), tail)
            .await
            .is_ok();
        assert!(ended, "the reader must end after confirmed session absence");
        assert!(
            fake.take_calls()
                .iter()
                .any(|call| call.starts_with("has-session"))
        );
        std::fs::remove_file(&transcript).unwrap();
    }
}

// The continuation adoption's pane check reads live only for a local tmux session.
#[test]
fn a_session_marked_for_another_host_never_reads_a_live_pane() {
    const NAME: &str = "AgentDesk-claude-p5c-idle-adopt";
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = FakeTmux::install(NAME);
    assert!(local_tmux_pane_live(NAME), "the fake reports a live pane");
    let path = crate::services::tmux_common::session_temp_path(NAME, "host_kind");
    std::fs::create_dir_all(Path::new(&path).parent().unwrap()).unwrap();
    std::fs::write(path, "herdr").unwrap();
    let _ = tmux.take_calls();
    assert!(!local_tmux_pane_live(NAME));
    assert!(
        tmux.take_calls().is_empty(),
        "no tmux probe for another host"
    );
}
