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

use crate::services::discord::formatting::ReplaceLongMessageOutcome;
use crate::services::discord::gateway::GatewayFuture;

/// A gateway whose refusal-notice send either never resolves or fails at once.
struct NoticeGateway {
    pending: bool,
    started: tokio::sync::Notify,
    sends: std::sync::atomic::AtomicUsize,
}

impl TurnGateway for NoticeGateway {
    fn send_message<'a>(
        &'a self,
        _: ChannelId,
        _: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>> {
        self.sends.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.started.notify_one();
        if self.pending {
            Box::pin(std::future::pending())
        } else {
            Box::pin(async { Err("notice gateway down".to_string()) })
        }
    }
    fn edit_message<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
        _: &'a str,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn replace_message_with_outcome<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
        _: &'a str,
    ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
        Box::pin(async { Ok(ReplaceLongMessageOutcome::EditedOriginal) })
    }
    fn schedule_retry_with_history<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
        _: &'a str,
    ) -> GatewayFuture<'a, ()> {
        Box::pin(async {})
    }
    fn dispatch_queued_turn<'a>(
        &'a self,
        _: ChannelId,
        _: &'a Intervention,
        _: &'a str,
        _: bool,
        _: Option<Arc<crate::services::turn_orchestrator::DispatchLease>>,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn validate_live_routing<'a>(&'a self, _: ChannelId) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn requester_mention(&self) -> Option<String> {
        None
    }
    fn can_chain_locally(&self) -> bool {
        false
    }
    fn can_deliver_directly(&self) -> bool {
        true
    }
    fn bot_owner_provider(&self) -> Option<ProviderKind> {
        Some(ProviderKind::Codex)
    }
}

/// The refused turn unwinds before its notice is sent: while that send is pending or failing,
/// a provider parked before its input finds the token cancelled and submits nothing.
#[tokio::test(flavor = "current_thread")]
async fn a_pending_or_failing_refusal_notice_never_holds_the_unwind() {
    use std::sync::atomic::Ordering;
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    for (index, pending) in [true, false].into_iter().enumerate() {
        let shared = discord::make_shared_data_for_tests();
        let mut row = InflightTurnState::new(
            shared.provider.clone(),
            6_484_501 + index as u64,
            None,
            1,
            77_484,
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
        assert!(
            discord::mailbox_try_start_turn(
                &shared,
                channel,
                cancel.clone(),
                UserId::new(1),
                message
            )
            .await
        );
        discord::increment_global_active(&shared, "test_bridge_admission");
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
        let gateway = Arc::new(NoticeGateway {
            pending,
            started: Default::default(),
            sends: Default::default(),
        });
        let mut bridge = seed_context("", row);
        bridge.provider = shared.provider.clone();
        bridge.user_msg_id = Some(message);
        let dyn_gateway: Arc<dyn TurnGateway> = gateway.clone();
        bridge.gateway = dyn_gateway;
        let (_tx, rx) = mpsc::channel();
        let inputs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let barrier = Arc::new(tokio::sync::Notify::new());
        let provider = tokio::spawn({
            let (cancel, inputs, barrier) = (cancel.clone(), inputs.clone(), barrier.clone());
            async move {
                barrier.notified().await;
                if !cancel.cancelled.load(Ordering::Acquire) {
                    inputs.fetch_add(1, Ordering::SeqCst);
                }
            }
        });

        spawn_turn_bridge(shared.clone(), cancel.clone(), rx, bridge);
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            gateway.started.notified(),
        )
        .await
        .expect("the refusal notice is sent");
        barrier.notify_one();
        provider.await.unwrap();

        assert_eq!(
            inputs.load(Ordering::SeqCst),
            0,
            "pending={pending}: input after refusal"
        );
        assert!(cancel.cancelled.load(Ordering::Acquire));
        let snapshot = discord::mailbox_snapshot(&shared, channel).await;
        assert!(
            snapshot.cancel_token.is_none(),
            "pending={pending}: slot released"
        );
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(gateway.sends.load(Ordering::SeqCst), 1);
    }
}

#[cfg(unix)]
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

/// A Herdr turn that took a user stop and ends without its provider's terminal keeps its mailbox
/// slot, token and row; the same turn without the stop finalizes and releases the slot.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn herdr_stopped_turn_without_provider_terminal_stays_held() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let held = herdr_stop_case(5_340_330_001, true).await;
    assert_eq!(held, (BridgeCompletionSignal::Unresolved, true, true));
    let released = herdr_stop_case(5_340_330_002, false).await;
    assert!(
        !released.1,
        "without a stop the turn frees its slot: {released:?}"
    );
}

/// (completion signal, mailbox still holds the token, inflight row present)
#[cfg(unix)]
async fn herdr_stop_case(channel_id: u64, stopped: bool) -> (BridgeCompletionSignal, bool, bool) {
    use crate::db::dispatched_sessions::hosted_execution::HostedOwner;
    let shared = discord::make_shared_data_for_tests();
    let mut row = InflightTurnState::new(
        ProviderKind::Codex,
        channel_id,
        None,
        1,
        77_100,
        18,
        String::new(),
        None,
        None,
        None,
        None,
        0,
    );
    let channel = ChannelId::new(channel_id);
    let message = MessageId::new(row.user_msg_id);
    let cancel = Arc::new(CancelToken::new());
    row.turn_nonce = cancel.turn_nonce().map(str::to_owned);
    let owner = HostedOwner {
        provider: "codex".into(),
        discord_token_hash: shared.token_hash.clone(),
        channel_id: channel_id.to_string(),
        logical_key: format!("AgentDesk-codex-held-{channel_id}"),
        owner_node: "node".into(),
        runtime_root: "/tmp".into(),
    };
    let intent = cancel.prepare_herdr_interrupt(ProviderKind::Codex, &owner);
    intent
        .user_stop
        .store(stopped, std::sync::atomic::Ordering::SeqCst);
    assert!(
        discord::mailbox_try_start_turn(&shared, channel, cancel.clone(), UserId::new(1), message)
            .await
    );
    discord::increment_global_active(&shared, "test_bridge_admission");
    discord::inflight::save_inflight_state(&row).unwrap();
    let mut bridge = seed_context("", row);
    bridge.provider = ProviderKind::Codex;
    bridge.user_msg_id = Some(message);
    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
    bridge.completion_tx = Some(completion_tx);
    let (tx, rx) = mpsc::channel();
    tx.send(StreamMessage::Text {
        content: "partial".into(),
    })
    .unwrap();
    tx.send(StreamMessage::Done {
        result: "partial".into(),
        session_id: None,
    })
    .unwrap();
    drop(tx);
    spawn_turn_bridge(shared.clone(), cancel.clone(), rx, bridge);
    let signal = tokio::time::timeout(std::time::Duration::from_secs(20), completion_rx)
        .await
        .expect("the bridge reports")
        .unwrap();
    let settle = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut holds = true;
    while std::time::Instant::now() < settle {
        let snapshot = discord::mailbox_snapshot(&shared, channel).await;
        holds = snapshot
            .cancel_token
            .as_ref()
            .is_some_and(|token| Arc::ptr_eq(token, &cancel));
        if !stopped && !holds {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(!cancel.cancelled.load(std::sync::atomic::Ordering::SeqCst) || !stopped);
    let row = discord::inflight::load_inflight_state(&ProviderKind::Codex, channel_id);
    (signal, holds, row.is_some())
}

/// A Herdr Codex turn from the real rollout reader through admission, the bridge and the finalizer.
#[cfg(unix)]
mod herdr_settlement {
    use crate::db::dispatched_sessions::hosted_execution::HostedOwner;
    use crate::services::discord::formatting::ReplaceLongMessageOutcome;
    use crate::services::discord::gateway::{GatewayFuture, TurnGateway};
    use crate::services::discord::inflight::InflightTurnState;
    use crate::services::discord::turn_bridge::{BridgeCompletionSignal, TurnBridgeContext};
    use crate::services::discord::{self, Intervention};
    use crate::services::provider::cancel_token_claude_interrupt::{
        HerdrSubmission, HerdrTurnStart,
    };
    use crate::services::provider::{CancelToken, ProviderKind};
    use crate::services::tui_prompt_dedupe::binding_events as events;
    use poise::serenity_prelude::{ChannelId, MessageId, UserId};
    use std::os::unix::fs::MetadataExt;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Recorder {
        bodies: Mutex<Vec<String>>,
        retries: Mutex<usize>,
    }

    impl TurnGateway for Recorder {
        fn send_message<'a>(
            &'a self,
            _: ChannelId,
            content: &'a str,
        ) -> GatewayFuture<'a, Result<MessageId, String>> {
            self.bodies.lock().unwrap().push(content.to_owned());
            Box::pin(async { Ok(MessageId::new(5_340_900)) })
        }
        fn edit_message<'a>(
            &'a self,
            _: ChannelId,
            _: MessageId,
            content: &'a str,
        ) -> GatewayFuture<'a, Result<(), String>> {
            self.bodies.lock().unwrap().push(content.to_owned());
            Box::pin(async { Ok(()) })
        }
        fn replace_message_with_outcome<'a>(
            &'a self,
            _: ChannelId,
            _: MessageId,
            content: &'a str,
        ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
            self.bodies.lock().unwrap().push(content.to_owned());
            Box::pin(async { Ok(ReplaceLongMessageOutcome::EditedOriginal) })
        }
        fn schedule_retry_with_history<'a>(
            &'a self,
            _: ChannelId,
            _: MessageId,
            _: &'a str,
        ) -> GatewayFuture<'a, ()> {
            *self.retries.lock().unwrap() += 1;
            Box::pin(async {})
        }
        fn dispatch_queued_turn<'a>(
            &'a self,
            _: ChannelId,
            _: &'a Intervention,
            _: &'a str,
            _: bool,
            _: Option<Arc<crate::services::turn_orchestrator::DispatchLease>>,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }
        fn validate_live_routing<'a>(
            &'a self,
            _: ChannelId,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }
        fn requester_mention(&self) -> Option<String> {
            None
        }
        fn can_chain_locally(&self) -> bool {
            false
        }
        fn can_deliver_directly(&self) -> bool {
            true
        }
        fn bot_owner_provider(&self) -> Option<ProviderKind> {
            Some(ProviderKind::Codex)
        }
    }

    fn codex(kind: &str) -> String {
        turn(kind, "t1")
    }

    fn turn(kind: &str, id: &str) -> String {
        let record =
            serde_json::json!({"type": "event_msg", "payload": {"type": kind, "turn_id": id}});
        format!("{record}\n")
    }

    fn reply(text: &str) -> String {
        let record = serde_json::json!({"type": "response_item", "payload": {"type": "message",
            "role": "assistant", "content": [{"type": "output_text", "text": text}]}});
        format!("{record}\n")
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Evidence {
        /// The pane is marked Herdr and its Source was logged by this token's execution.
        Logged,
        /// The pane carries no Herdr marker, so admission has no Source to judge.
        Unmarked,
    }

    #[derive(Debug)]
    struct Outcome {
        signal: BridgeCompletionSignal,
        holds: bool,
        row: bool,
        cancelled: bool,
        completion_cleanup: bool,
        tombstone: bool,
        bodies: Vec<String>,
        retries: usize,
    }

    /// Runs one submitted Herdr turn whose rollout is `body`: a real user-stop intent when `stopped`,
    /// the real tail (its pane dead after a moment when `dead`), then the real bridge.
    async fn run(
        root: &std::path::Path,
        n: u64,
        body: &str,
        stopped: bool,
        dead: bool,
        evidence: Evidence,
    ) -> Outcome {
        let channel_id = 5_340_350_000 + n;
        let channel = ChannelId::new(channel_id);
        let logical = format!("AgentDesk-codex-settle-{n}");
        let rollout = root.join(format!("{logical}.jsonl"));
        std::fs::write(&rollout, body).unwrap();
        let rollout = std::fs::canonicalize(rollout).unwrap();
        let meta = std::fs::metadata(&rollout).unwrap();
        let nonce = format!("{n:032x}");
        if evidence == Evidence::Logged {
            let marker = crate::services::tmux_common::session_temp_path(&logical, "host_kind");
            std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
            std::fs::write(&marker, "herdr").unwrap();
        }
        let event = events::BindingEvent {
            seq: 1,
            channel_id,
            provider: "codex".into(),
            tmux_session: logical.clone(),
            execution_nonce: Some(nonce.clone()),
            old: None,
            new: events::BindingTarget::Source(events::SourceId {
                session_id: "herdr-session".into(),
                path: rollout.clone(),
                dev: meta.dev(),
                ino: meta.ino(),
            }),
            cause: events::BindingCause::Startup,
            parent_hint: None,
            evidence: events::BindingEvidence {
                hook_event: Some("SessionStart".into()),
                received_at: chrono::Utc::now(),
            },
            committed_at: chrono::Utc::now(),
        };
        let log = root.join(events::BINDING_EVENTS_DIR);
        std::fs::create_dir_all(&log).unwrap();
        std::fs::write(
            log.join(format!("{channel_id}.log")),
            format!("{}\n", serde_json::to_string(&event).unwrap()),
        )
        .unwrap();

        let shared = discord::make_shared_data_for_tests();
        let cancel = Arc::new(CancelToken::new());
        let owner = HostedOwner {
            provider: "codex".into(),
            discord_token_hash: shared.token_hash.clone(),
            channel_id: channel_id.to_string(),
            logical_key: logical.clone(),
            owner_node: "node".into(),
            runtime_root: root.display().to_string(),
        };
        let state = cancel.prepare_herdr_interrupt(ProviderKind::Codex, &owner);
        cancel.bind_unmanaged_session_name(&logical);
        assert!(state.record_turn_start(HerdrTurnStart {
            execution_nonce: nonce,
            source: rollout.clone(),
            file: Some((meta.dev(), meta.ino())),
            offset: 0,
            submitted_at: None,
        }));
        *state.submission.lock().unwrap() = HerdrSubmission::Submitted;
        let mut row = InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            None,
            1,
            77_100,
            18,
            String::new(),
            None,
            None,
            None,
            None,
            0,
        );
        row.turn_nonce = cancel.turn_nonce().map(str::to_owned);
        let message = MessageId::new(row.user_msg_id);
        assert!(
            discord::mailbox_try_start_turn(
                &shared,
                channel,
                cancel.clone(),
                UserId::new(1),
                message
            )
            .await
        );
        discord::increment_global_active(&shared, "test_bridge_admission");
        discord::inflight::save_inflight_state(&row).unwrap();
        if stopped {
            let stop = shared
                .mailbox(channel)
                .admit_herdr_user_stop_if_current(cancel.clone(), "stop".into())
                .await;
            assert!(stop.token.is_some_and(|token| Arc::ptr_eq(&token, &cancel)));
        }
        assert!(
            !cancel.cancelled.load(std::sync::atomic::Ordering::SeqCst),
            "the intent cancels nothing"
        );

        let recorder = Arc::new(Recorder::default());
        let gateway: Arc<dyn TurnGateway> = recorder.clone();
        let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
        let bridge = TurnBridgeContext {
            provider: ProviderKind::Codex,
            gateway,
            channel_id: channel,
            user_msg_id: Some(message),
            user_text_owned: String::new(),
            request_owner_name: String::new(),
            role_binding: None,
            adk_session_key: None,
            adk_session_name: None,
            adk_session_info: None,
            adk_cwd: None,
            dispatch_id: None,
            dispatch_kind: None,
            memory_recall_usage: Default::default(),
            context_window_tokens: 0,
            context_compact_percent: 0,
            current_msg_id: Some(MessageId::new(5_340_901)),
            response_sent_offset: 0,
            full_response: String::new(),
            tmux_last_offset: None,
            new_session_id: None,
            defer_watcher_resume: false,
            reuse_status_panel_message: false,
            completion_tx: Some(completion_tx),
            is_external_input_tui_direct: false,
            inflight_state: row,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = {
            let cancel = cancel.clone();
            let until = std::time::Instant::now()
                + std::time::Duration::from_millis(if dead { 300 } else { 20_000 });
            std::thread::spawn(move || {
                crate::services::codex_tui::rollout_tail::tail_rollout_file_from_offset(
                    &rollout,
                    0,
                    Some("herdr-session"),
                    tx,
                    Some(cancel),
                    move || std::time::Instant::now() < until,
                )
            })
        };
        discord::turn_bridge::spawn_turn_bridge(shared.clone(), cancel.clone(), rx, bridge);
        let signal = tokio::time::timeout(std::time::Duration::from_secs(20), completion_rx)
            .await
            .expect("the bridge reports")
            .unwrap();
        let _ = reader.join().unwrap();
        let settle = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut holds = true;
        while std::time::Instant::now() < settle {
            let snapshot = discord::mailbox_snapshot(&shared, channel).await;
            holds = snapshot
                .cancel_token
                .as_ref()
                .is_some_and(|token| Arc::ptr_eq(token, &cancel));
            if !holds {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        Outcome {
            signal,
            holds,
            row: discord::inflight::load_inflight_state(&ProviderKind::Codex, channel_id).is_some(),
            cancelled: cancel.cancelled.load(std::sync::atomic::Ordering::SeqCst),
            completion_cleanup: cancel.is_completion_cleanup(),
            tombstone: discord::tmux::recent_turn_stop_for_channel(channel).is_some(),
            bodies: recorder.bodies.lock().unwrap().clone(),
            retries: *recorder.retries.lock().unwrap(),
        }
    }

    /// A stopped turn's own admitted abort settles as a cancel through the finalizer, freeing the slot
    /// without a tombstone, retry or success; its own completion after a stop stays a completion.
    #[tokio::test(flavor = "current_thread")]
    async fn a_herdr_turn_settles_on_its_own_admitted_terminal_after_a_stop() {
        let root = tempfile::tempdir().unwrap();
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        events::set_test_root(Some(root.path()));
        let aborted = codex("task_started") + &reply("partial") + &codex("turn_aborted");
        let outcome = run(root.path(), 1, &aborted, true, false, Evidence::Logged).await;
        // The cancel keeps a non-tmux host's row for its owner, as every Herdr cancel does.
        assert!(!outcome.holds && !outcome.tombstone, "{outcome:?}");
        assert!(
            outcome.cancelled && !outcome.completion_cleanup,
            "a cancel, not a completion: {outcome:?}"
        );
        assert_eq!(outcome.retries, 0, "{outcome:?}");

        let completed = codex("task_started") + &reply("answer") + &codex("task_complete");
        let outcome = run(root.path(), 2, &completed, true, false, Evidence::Logged).await;
        assert!(!outcome.holds && !outcome.tombstone, "{outcome:?}");
        assert!(
            outcome.completion_cleanup,
            "a stop does not turn a completion into a cancel: {outcome:?}"
        );
    }

    /// A submitted Herdr turn whose reader ends without an admitted terminal keeps its slot, row and
    /// token, stopped or not: a dead pane, a range mixing another turn, or a refused admission.
    #[tokio::test(flavor = "current_thread")]
    async fn a_herdr_turn_without_an_admitted_terminal_stays_held() {
        let root = tempfile::tempdir().unwrap();
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        events::set_test_root(Some(root.path()));
        let running = codex("task_started") + &reply("partial");
        let completed = codex("task_started") + &reply("answer") + &codex("task_complete");
        let aborted = codex("task_started") + &reply("partial") + &codex("turn_aborted");
        let next_head = codex("task_started")
            + &turn("task_started", "t2")
            + &reply("answer")
            + &codex("task_complete");
        let prior_tail = turn("task_complete", "t0") + &completed;
        for (n, body, stopped, dead, evidence) in [
            (11, running.clone(), true, true, Evidence::Logged),
            (15, running, false, true, Evidence::Logged),
            (16, next_head, false, true, Evidence::Logged),
            (17, prior_tail, false, true, Evidence::Logged),
            (12, completed.clone(), false, false, Evidence::Unmarked),
            (13, completed, true, false, Evidence::Unmarked),
            (14, aborted, true, false, Evidence::Unmarked),
        ] {
            let outcome = run(root.path(), n, &body, stopped, dead, evidence).await;
            assert_eq!(
                outcome.signal,
                BridgeCompletionSignal::Unresolved,
                "{n}: {outcome:?}"
            );
            assert!(
                outcome.holds && outcome.row && !outcome.cancelled,
                "{n}: {outcome:?}"
            );
            assert!(
                !outcome.completion_cleanup && !outcome.tombstone && outcome.retries == 0,
                "{n}: {outcome:?}"
            );
            assert!(
                !outcome.bodies.iter().any(|body| body.contains("answer")),
                "{n}: {outcome:?}"
            );
        }
    }
}
