use super::*;

use std::sync::atomic::{AtomicBool, AtomicI64};

use crate::services::agent_protocol::{RuntimeHandoffKind, StreamMessage};
use crate::services::discord::formatting::ReplaceLongMessageOutcome;
use crate::services::discord::gateway::{GatewayFuture, HeadlessGateway, TurnGateway};
use crate::services::discord::tmux_watcher_registry::TmuxWatcherHandle;
use crate::services::discord::turn_bridge::{TurnBridgeContext, spawn_turn_bridge};
use crate::services::memory::TokenUsage;

#[derive(Default)]
struct RecordingDirectGateway(std::sync::Mutex<Vec<(&'static str, String)>>);

impl TurnGateway for RecordingDirectGateway {
    fn send_message<'a>(
        &'a self,
        _: ChannelId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>> {
        Box::pin(async move {
            self.0.lock().unwrap().push(("send", content.into()));
            Ok(MessageId::new(1_000_000_041))
        })
    }

    fn edit_message<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.0.lock().unwrap().push(("edit", content.into()));
            Ok(())
        })
    }

    fn replace_message_with_outcome<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
        Box::pin(async move {
            self.0.lock().unwrap().push(("replace", content.into()));
            Ok(ReplaceLongMessageOutcome::EditedOriginal)
        })
    }

    fn schedule_retry_with_history<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
        _: &'a str,
    ) -> GatewayFuture<'a, ()> {
        Box::pin(async {
            panic!("a successful quiet routine must not schedule a retry");
        })
    }

    fn dispatch_queued_turn<'a>(
        &'a self,
        _: ChannelId,
        _: &'a Intervention,
        _: &'a str,
        _: bool,
        _: Option<Arc<crate::services::turn_orchestrator::DispatchLease>>,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Err("no queued turns in this fixture".into()) })
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
        None
    }
}

#[tokio::test]
async fn actual_quiet_bridge_completion_retires_its_actor_and_durable_inflight() {
    with_isolated_runtime_root(|| async {
        const USER_MESSAGE: u64 = 9_100_000_000_000_000_123;
        const PLACEHOLDER: u64 = 9_100_000_000_000_000_456;
        assert!(crate::services::discord::is_synthetic_headless_message_id_raw(USER_MESSAGE));
        assert!(crate::services::discord::is_synthetic_headless_message_id_raw(PLACEHOLDER));
        for silent in [true, false] {
            let (fixture, lease) =
                Fixture::with_user_message(ProviderKind::Codex, USER_MESSAGE).await;
            let mut row = inflight::InflightTurnState::new(
                ProviderKind::Codex,
                fixture.channel.get(),
                None,
                7,
                USER_MESSAGE,
                PLACEHOLDER,
                "quiet routine".into(),
                None,
                None,
                None,
                None,
                0,
            );
            row.turn_nonce = fixture.token.turn_nonce().map(str::to_owned);
            row.runtime_kind = Some(RuntimeHandoffKind::ProcessBackend);
            row.silent_turn = silent;
            inflight::save_inflight_state(&row).unwrap();
            assert!(
                inflight::load_inflight_state_read_only(
                    &fixture.shared.provider,
                    fixture.channel.get()
                )
                .is_some()
            );
            fixture.assert_same_actor_retained();
            crate::services::discord::increment_global_active(
                &fixture.shared,
                "test_process_mailbox_bridge",
            );
            let completion = lease.watch(tokio::spawn(async {}));
            let shown = Arc::new(RecordingDirectGateway::default());
            let gateway: Arc<dyn TurnGateway> = if silent {
                Arc::new(HeadlessGateway)
            } else {
                shown.clone()
            };
            let context = TurnBridgeContext {
                provider: ProviderKind::Codex,
                gateway,
                channel_id: fixture.channel,
                user_msg_id: Some(MessageId::new(USER_MESSAGE)),
                user_text_owned: "quiet routine".into(),
                request_owner_name: "routine fixture".into(),
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
                current_msg_id: Some(MessageId::new(PLACEHOLDER)),
                response_sent_offset: 0,
                full_response: String::new(),
                tmux_last_offset: None,
                new_session_id: None,
                defer_watcher_resume: false,
                reuse_status_panel_message: false,
                completion_tx: Some(completion),
                is_external_input_tui_direct: false,
                inflight_state: row,
            };
            let (tx, rx) = std::sync::mpsc::channel();
            spawn_turn_bridge(fixture.shared.clone(), fixture.token.clone(), rx, context);
            tx.send(StreamMessage::Done {
                result: "NO_REPLY".into(),
                session_id: None,
            })
            .unwrap();
            drop(tx);
            tokio::time::timeout(Duration::from_secs(10), async {
                while fixture.shared.mailbox_peek(fixture.channel).is_some() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("actual bridge Finalized completion must retire the captured idle actor");
            assert!(
                matches!(
                    inflight::load_inflight_state_read_only_result(
                        &fixture.shared.provider,
                        fixture.channel.get()
                    ),
                    Ok(None)
                ),
                "the real bridge must settle durable inflight before retirement"
            );
            assert!(fixture.token.is_completion_cleanup());
            assert!(fixture.token.cancelled.load(Ordering::Acquire));
            assert_eq!(
                fixture.shared.restart.global_active.load(Ordering::Acquire),
                0
            );
            let delivered = shown.0.lock().unwrap();
            if silent {
                assert!(
                    delivered.is_empty(),
                    "silent HeadlessGateway completion needs no transport"
                );
            } else {
                assert!(
                    delivered
                        .iter()
                        .any(|(operation, content)| (*operation == "replace"
                            || *operation == "send")
                            && content.contains("NO_REPLY")),
                    "ordinary NO_REPLY completion must reach the successful fake terminal transport"
                );
            }
        }
    })
    .await;
}

#[tokio::test]
async fn idle_actor_with_a_live_foreign_watcher_retains_the_exact_handle_and_markers() {
    with_isolated_runtime_root(|| async {
        let (fixture, lease) = Fixture::new(ProviderKind::Codex).await;
        fixture.settle(false).await;
        let name = format!("fixture-incumbent-watcher-{}", fixture.channel);
        let output = format!("/fixture/{name}.jsonl");
        let paused = Arc::new(AtomicBool::new(true));
        let resume = Arc::new(std::sync::Mutex::new(Some(314_159)));
        let cancel = Arc::new(AtomicBool::new(false));
        let epoch = Arc::new(AtomicU64::new(9));
        let delivered = Arc::new(AtomicBool::new(true));
        let heartbeat_value =
            crate::services::discord::tmux_watcher_registry::tmux_watcher_now_ms();
        let heartbeat = Arc::new(AtomicI64::new(heartbeat_value));
        fixture.shared.tmux_watchers.insert(
            fixture.channel,
            TmuxWatcherHandle {
                tmux_session_name: name.clone(),
                output_path: output.clone(),
                paused: paused.clone(),
                resume_offset: resume.clone(),
                cancel: cancel.clone(),
                pause_epoch: epoch.clone(),
                turn_delivered: delivered.clone(),
                last_heartbeat_ts_ms: heartbeat.clone(),
            },
        );
        assert!(fixture.mailbox.snapshot().await.cancel_token.is_none());
        finalize(lease).await;
        fixture.assert_same_actor_retained();
        let watcher = fixture
            .shared
            .tmux_watchers
            .get(&fixture.channel)
            .expect("live watcher remains bound");
        assert!(!watcher.heartbeat_stale());
        assert_eq!(watcher.tmux_session_name, name);
        assert_eq!(watcher.output_path, output);
        assert!(Arc::ptr_eq(&watcher.paused, &paused));
        assert!(Arc::ptr_eq(&watcher.resume_offset, &resume));
        assert!(Arc::ptr_eq(&watcher.cancel, &cancel));
        assert!(Arc::ptr_eq(&watcher.pause_epoch, &epoch));
        assert!(Arc::ptr_eq(&watcher.turn_delivered, &delivered));
        assert!(Arc::ptr_eq(&watcher.last_heartbeat_ts_ms, &heartbeat));
        assert!(paused.load(Ordering::Acquire));
        assert_eq!(*resume.lock().unwrap(), Some(314_159));
        assert!(!cancel.load(Ordering::Acquire));
        assert_eq!(epoch.load(Ordering::Acquire), 9);
        assert!(delivered.load(Ordering::Acquire));
        assert_eq!(heartbeat.load(Ordering::Acquire), heartbeat_value);
        drop(watcher);
        assert_eq!(
            fixture
                .shared
                .tmux_watchers
                .owner_channel_for_tmux_session(&name),
            Some(fixture.channel)
        );
    })
    .await;
}
