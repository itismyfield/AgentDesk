use super::*;
use crate::services::discord::{
    self,
    health::mailbox::{ResidualOccupancy, mailbox_agent_turn_status},
};

fn seed_context(seed: &str, row: InflightTurnState) -> TurnBridgeContext {
    let gateway: std::sync::Arc<dyn TurnGateway> =
        std::sync::Arc::new(crate::services::discord::gateway::HeadlessGateway);
    TurnBridgeContext {
        provider: ProviderKind::Codex,
        gateway,
        channel_id: ChannelId::new(row.channel_id),
        user_msg_id: None,
        user_text_owned: String::new(),
        request_owner_name: String::new(),
        role_binding: None,
        adk_session_key: None,
        adk_session_name: None,
        adk_session_info: None,
        adk_cwd: None,
        dispatch_id: None,
        dispatch_kind: None,
        memory_recall_usage: TokenUsage::default(),
        context_window_tokens: 0,
        context_compact_percent: 0,
        current_msg_id: None,
        response_sent_offset: 0,
        full_response: seed.to_string(),
        tmux_last_offset: None,
        new_session_id: None,
        defer_watcher_resume: false,
        reuse_status_panel_message: false,
        completion_tx: None,
        is_external_input_tui_direct: false,
        inflight_state: row,
    }
}

fn seed_row() -> InflightTurnState {
    let mut row = InflightTurnState::new(
        ProviderKind::Codex,
        5_938_031,
        None,
        343_742_347_365_974_026,
        77_013,
        18,
        String::new(),
        None,
        None,
        None,
        None,
        0,
    );
    row.dispatch_id = Some("dispatch-5938-seed".to_string());
    row
}

#[tokio::test]
async fn headless_entry_abort_releases_mailbox_without_touching_durable_owner() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let shared = discord::make_shared_data_for_tests();
    let mut row = seed_row();
    row.provider = shared.provider.as_str().to_string();
    row.dispatch_id = None;
    let channel = ChannelId::new(row.channel_id);
    let message = MessageId::new(row.user_msg_id);
    let cancel = Arc::new(CancelToken::new());
    row.turn_nonce = cancel.turn_nonce().map(str::to_owned);
    assert!(
        discord::mailbox_try_start_turn(&shared, channel, cancel.clone(), UserId::new(1), message)
            .await
    );
    discord::increment_global_active(&shared, "headless_turn_start");
    let mut incumbent = row.clone();
    incumbent.user_msg_id += 1;
    incumbent.turn_nonce = Some("durable-incumbent".into());
    discord::inflight::save_inflight_state(&incumbent).unwrap();
    let path = discord::inflight::inflight_state_path(
        &discord::inflight::inflight_runtime_root().unwrap(),
        &shared.provider,
        channel.get(),
    );
    let before = std::fs::read(&path).unwrap();
    let mut bridge = seed_context("", row);
    bridge.provider = shared.provider.clone();
    bridge.user_msg_id = Some(message);
    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
    bridge.completion_tx = Some(completion_tx);
    let (_tx, rx) = mpsc::channel();
    spawn_turn_bridge(shared.clone(), cancel, rx, bridge);
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(5), completion_rx)
            .await
            .unwrap()
            .unwrap(),
        BridgeCompletionSignal::EntryAborted
    );
    // The bridge reports abort before its asynchronous mailbox unwind finishes.
    let idle = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let snapshot = discord::mailbox_snapshot(&shared, channel).await;
            if snapshot.cancel_token.is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        idle.is_ok(),
        "EntryAborted must release the headless mailbox cancel token"
    );
    let snapshot = discord::mailbox_snapshot(&shared, channel).await;
    assert_eq!(
        mailbox_agent_turn_status(snapshot.cancel_token.is_some(), ResidualOccupancy::None),
        "idle"
    );
    assert_eq!(
        shared
            .restart
            .global_active
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    assert_eq!(
        std::fs::read(path).unwrap(),
        before,
        "the durable incumbent must survive byte-for-byte"
    );
}
