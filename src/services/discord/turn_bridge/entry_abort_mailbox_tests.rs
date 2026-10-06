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

#[tokio::test]
async fn headless_entry_abort_releases_mailbox_without_touching_durable_owner() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    for (waiter, successor, caller) in [
        (true, false, "headless"),
        (false, false, "headless"),
        (true, true, "headless"),
        (true, false, "tui"),
        (true, false, "recovery"),
    ] {
        run_abort_case(waiter, successor, caller).await;
    }
}

async fn run_abort_case(waiter: bool, successor: bool, caller: &str) {
    let shared = discord::make_shared_data_for_tests();
    let mut row = InflightTurnState::new(
        shared.provider.clone(),
        6_333_001,
        None,
        1,
        77_013,
        18,
        String::new(),
        None,
        None,
        None,
        None,
        0,
    );
    let channel = ChannelId::new(row.channel_id);
    let message = MessageId::new(row.user_msg_id);
    let cancel = Arc::new(CancelToken::new());
    row.turn_nonce = cancel.turn_nonce().map(str::to_owned);
    if caller == "recovery" {
        assert!(
            discord::queue_io::mailbox_recovery_kickoff(
                &shared,
                channel,
                cancel.clone(),
                UserId::new(1),
                Some(message)
            )
            .await
            .activated_turn()
        );
    } else {
        let kind = if caller == "tui" {
            crate::services::turn_orchestrator::ActiveTurnKind::Background
        } else {
            crate::services::turn_orchestrator::ActiveTurnKind::UserOrAgent
        };
        assert!(
            discord::mailbox_try_start_turn_kinded(
                &shared,
                channel,
                cancel.clone(),
                UserId::new(1),
                message,
                kind
            )
            .await
        );
        discord::increment_global_active(&shared, "test_bridge_admission");
    }
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
    bridge.is_external_input_tui_direct = caller == "tui";
    bridge.user_msg_id = Some(message);
    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
    bridge.completion_tx = waiter.then_some(completion_tx);
    let (_tx, rx) = mpsc::channel();
    let replacement = Arc::new(CancelToken::from_persisted_turn_nonce(
        cancel.turn_nonce().map(str::to_owned),
    ));
    if successor {
        discord::mailbox_finish_turn(&shared, &shared.provider, channel).await;
        assert!(
            discord::mailbox_try_start_turn(
                &shared,
                channel,
                replacement.clone(),
                UserId::new(1),
                message
            )
            .await
        );
    }
    let mut signals = shared.inflight_signals.subscribe();
    spawn_turn_bridge(shared.clone(), cancel.clone(), rx, bridge);
    if waiter {
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), completion_rx)
                .await
                .unwrap()
                .unwrap(),
            BridgeCompletionSignal::EntryAborted
        );
    }
    if successor {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while Arc::strong_count(&cancel) > 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("aborted bridge task must finish");
        let snapshot = discord::mailbox_snapshot(&shared, channel).await;
        assert!(Arc::ptr_eq(
            snapshot.cancel_token.as_ref().unwrap(),
            &replacement
        ));
        assert!(
            !replacement
                .cancelled
                .load(std::sync::atomic::Ordering::Relaxed)
        );
        assert_eq!(
            shared
                .restart
                .global_active
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_eq!(std::fs::read(path).unwrap(), before);
        assert!(signals.try_recv().is_err());
        return;
    }
    // The bridge reports abort before its asynchronous mailbox unwind finishes.
    let idle = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let snapshot = discord::mailbox_snapshot(&shared, channel).await;
            if snapshot.cancel_token.is_none()
                && cancel.cancelled.load(std::sync::atomic::Ordering::Relaxed)
                && shared
                    .restart
                    .global_active
                    .load(std::sync::atomic::Ordering::Relaxed)
                    == 0
            {
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
    assert!(cancel.cancelled.load(std::sync::atomic::Ordering::Relaxed));
    assert!(signals.try_recv().is_err());
    let snapshot = discord::mailbox_snapshot(&shared, channel).await;
    if caller == "tui" {
        let readopted = discord::queue_io::mailbox_try_start_turn_adopting(
            &shared,
            channel,
            replacement,
            UserId::new(1),
            message,
            crate::services::turn_orchestrator::ActiveTurnKind::Background,
            cancel.turn_nonce().map(str::to_owned),
        )
        .await;
        assert!(!readopted.started && readopted.refused_released_episode);
    }
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

#[tokio::test(flavor = "current_thread")]
async fn c1_actual_spawn_bridge_retains_effect_until_future_disposal() {
    use super::super::input_runtime::fence::{self, Gate, effect};
    use futures::FutureExt;
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let shared = super::super::make_shared_data_for_tests();
    let channel = ChannelId::new(6_325_460);
    let gate = Gate::protect(ProviderKind::Codex, channel.get()).unwrap();
    let _health = fence::test_health::Clear::new(&gate);
    let row = InflightTurnState::new(
        ProviderKind::Codex,
        channel.get(),
        None,
        1,
        2,
        0,
        String::new(),
        None,
        None,
        None,
        None,
        0,
    );
    let bridge = seed_context("", row);
    let (captured_tx, captured_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    *super::resume_pin_tests::BRIDGE_CAPTURE_PROBE
        .lock()
        .unwrap() = Some((channel, captured_tx, resume_rx));
    let cancel = Arc::new(CancelToken::new());
    let (_tx, rx) = mpsc::channel();
    effect::scope(Some(gate.admit().unwrap()), async {
        spawn_turn_bridge_with_pin(shared, cancel.clone(), rx, bridge, None);
    })
    .await;
    let closing = gate.close().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), captured_rx)
        .await
        .unwrap()
        .unwrap();
    let held = closing.drain().now_or_never().is_none();
    resume_tx.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while Arc::strong_count(&cancel) > 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        held,
        "actual spawned bridge retains original effect while capture is pending"
    );
    tokio::time::timeout(std::time::Duration::from_secs(10), closing.drain())
        .await
        .unwrap();
}
