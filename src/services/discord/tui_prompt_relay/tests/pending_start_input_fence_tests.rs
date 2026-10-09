//! A pending-start worker admitted on a protected open channel completes its real claim: the
//! claimed row is written and returning releases the input drain.
use super::*;
use crate::services::discord::input_runtime::{self, fence::Gate};
use crate::services::discord::{inflight, tui_direct_pending_start};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c2b_admitted_pending_start_worker_writes_its_claimed_row_and_releases_the_drain() {
    let root = tempfile::tempdir().expect("isolated runtime root");
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let shared = super::super::super::make_shared_data_for_tests();
    let channel = ChannelId::new(6_325_960);
    let tmux = "c2b-pending-start-claim";
    let transcript = root.path().join("pending-start.jsonl");
    std::fs::write(&transcript, b"").expect("empty transcript");
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        tmux,
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: transcript.to_str().expect("utf8 path").to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    let anchor = 6_325_961u64;
    let record = tui_direct_pending_start::TuiDirectPendingStart {
        provider: "claude".to_string(),
        channel_id: channel.get(),
        tmux_session_name: tmux.to_string(),
        prompt_text: "prompt".to_string(),
        anchor_message_id: anchor,
        lease_relay_owner: "bridge_adapter".to_string(),
        lease_runtime_kind: Some("claude_tui".to_string()),
        lease_turn_id: Some("turn-c2b-pending-start".to_string()),
        lease_session_key: None,
        generation: 0,
        created_at_ms: 0,
        observed_at_ms: 0,
        state: tui_direct_pending_start::PendingStartState::Waiting,
        attempt_count: 0,
        captured_source: None,
        native_turn_id: None,
    };
    tui_direct_pending_start::persist(&record).expect("persist record");
    let gate = Gate::protect(ProviderKind::Claude, channel.get()).unwrap();
    let _health = input_runtime::fence::test_health::Clear::new(&gate);

    tui_direct_pending_start::spawn_worker(
        shared.clone(),
        record,
        synthetic_start::pending_start_view_fn(),
        synthetic_start::pending_start_claim_fn(),
        synthetic_start::pending_start_abort_cleanup_fn(),
        synthetic_orphan_reclaim::pending_start_reclaim_orphan_fn(),
    );
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while tui_direct_pending_start::pending_synthetic_start_present("claude", channel.get())
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let row = inflight::load_inflight_state(&ProviderKind::Claude, channel.get())
        .expect("the admitted claim wrote its row");
    assert_eq!(row.tmux_session_name.as_deref(), Some(tmux));
    assert!(
        !tui_direct_pending_start::pending_synthetic_start_present("claude", channel.get()),
        "the claimed record is consumed"
    );
    assert!(
        !input_runtime::health_reasons()
            .iter()
            .any(|reason| reason.contains(&format!("channel={}", channel.get()))),
        "the admitted claim saw a refused writer"
    );
    let closing = gate.close().unwrap();
    let drained = tokio::time::timeout(std::time::Duration::from_secs(5), closing.drain()).await;
    assert!(drained.is_ok(), "the finished worker released the drain");
}
