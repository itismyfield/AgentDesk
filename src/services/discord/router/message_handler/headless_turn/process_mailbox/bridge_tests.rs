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
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
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

/// A scheduled turn's actual bridge completion records its outcome on the delivery
/// row before the row is dropped, and the scheduler's next production poll sends it.
#[tokio::test]
async fn a_scheduled_bridge_completion_settles_on_the_next_poll_pg() {
    use crate::services::discord::scheduled_turn::ScheduledTurnMark;
    use crate::services::scheduled_messages::turn_completion::tests::{
        delivery_state, launch_agent_delivery, poll_once,
    };
    const USER_MESSAGE: u64 = 9_100_000_000_000_007_420;
    const PLACEHOLDER: u64 = 9_100_000_000_000_007_421;
    const TURN: &str = "discord:i7420-bridge:1";
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    with_isolated_runtime_root(|| async {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        launch_agent_delivery(&pool, "i7420-bridge", TURN, true, "fail").await;
        let (fixture, lease) =
            Fixture::with_storage(ProviderKind::Codex, USER_MESSAGE, Some(pool.clone())).await;
        let mut row = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            fixture.channel.get(),
            None,
            7,
            USER_MESSAGE,
            PLACEHOLDER,
            "scheduled prompt".into(),
            None,
            None,
            None,
            None,
            0,
        );
        row.turn_nonce = fixture.token.turn_nonce().map(str::to_owned);
        row.runtime_kind = Some(RuntimeHandoffKind::ProcessBackend);
        row.scheduled = ScheduledTurnMark::Scheduled {
            turn_id: TURN.into(),
            isolated: true,
            outcome: None,
        };
        inflight::save_inflight_state(&row).unwrap();
        crate::services::discord::increment_global_active(&fixture.shared, "test_i7420_bridge");
        let completion = lease.watch(tokio::spawn(async {}));
        let shown = Arc::new(RecordingDirectGateway::default());
        let context = TurnBridgeContext {
            provider: ProviderKind::Codex,
            gateway: shown.clone(),
            channel_id: fixture.channel,
            user_msg_id: Some(MessageId::new(USER_MESSAGE)),
            user_text_owned: "scheduled prompt".into(),
            request_owner_name: "scheduled fixture".into(),
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
            result: "scheduled bridge answer".into(),
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
        .expect("the real bridge completion retires the turn");
        assert!(matches!(
            inflight::load_inflight_state_read_only_result(
                &fixture.shared.provider,
                fixture.channel.get()
            ),
            Ok(None)
        ));
        assert!(
            shown
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|(_, content)| content.contains("scheduled bridge answer"))
        );
        assert_eq!(
            delivery_state(&pool, TURN).await,
            ("running".to_string(), Some("delivered".to_string()))
        );
        assert!(poll_once(&pool).await, "the production poll settles it");
        assert_eq!(delivery_state(&pool, TURN).await.0, "sent");
        drop(fixture);
        pool.close().await;
        db.drop().await;
    })
    .await;
}

#[path = "routine_presentation_tests.rs"]
mod routine_presentation_tests;

/// After two rollover parts, a Done starting with them sends only the rest and ends the turn;
/// a Done that changed them takes the full replay, never joining its tail to the old parts.
#[tokio::test]
async fn stream_json_long_done_keeps_frozen_parts_only_when_it_starts_with_them() {
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    for provider in [ProviderKind::Antigravity, ProviderKind::Grok] {
        for intro in ["", "INTRO\n"] {
            with_isolated_runtime_root(|| long_done_after_frozen_parts(provider.clone(), intro))
                .await;
        }
    }
}

async fn long_done_after_frozen_parts(provider: ProviderKind, intro: &'static str) {
    const USER_MESSAGE: u64 = 9_100_000_000_000_008_160;
    const PLACEHOLDER: u64 = 9_100_000_000_000_008_161;
    let lines: Vec<String> = std::iter::once("START-r1".to_string())
        .chain((1..=55).map(|n| format!("{n:02} {}", "0123456789".repeat(8))))
        .chain(std::iter::once("END-r1".to_string()))
        .collect();
    let body = lines.join("\n");
    let frozen = format!("{}\n", lines[..45].join("\n"));
    let result = format!("{intro}{body}");
    // Process-routine capture admits only Claude and Codex, so claim an ordinary episode.
    let mut shared = make_shared_data_for_tests_with_storage(None);
    Arc::get_mut(&mut shared).unwrap().provider = provider.clone();
    shared.settings.write().await.provider = provider.clone();
    let channel = ChannelId::new(NEXT_CHANNEL.fetch_add(1, Ordering::Relaxed));
    let token = Arc::new(CancelToken::new());
    let mailbox = shared.mailbox(channel);
    assert!(
        mailbox
            .try_start_turn(token.clone(), UserId::new(7), MessageId::new(USER_MESSAGE))
            .await
    );
    let mut row = inflight::InflightTurnState::new(
        provider.clone(),
        channel.get(),
        None,
        7,
        USER_MESSAGE,
        PLACEHOLDER,
        "long answer".into(),
        None,
        None,
        None,
        None,
        0,
    );
    row.turn_nonce = token.turn_nonce().map(str::to_owned);
    row.full_response = frozen.clone();
    row.response_sent_offset = frozen.len();
    inflight::save_inflight_state(&row).unwrap();
    crate::services::discord::increment_global_active(&shared, "test_8160_bridge");
    let shown = Arc::new(RecordingDirectGateway::default());
    let context = TurnBridgeContext {
        provider: provider.clone(),
        gateway: shown.clone(),
        channel_id: channel,
        user_msg_id: Some(MessageId::new(USER_MESSAGE)),
        user_text_owned: "long answer".into(),
        request_owner_name: "stream-json answer".into(),
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
        response_sent_offset: frozen.len(),
        full_response: frozen.clone(),
        tmux_last_offset: None,
        new_session_id: None,
        defer_watcher_resume: false,
        reuse_status_panel_message: false,
        completion_tx: None,
        is_external_input_tui_direct: false,
        inflight_state: row,
    };
    let (tx, rx) = std::sync::mpsc::channel();
    spawn_turn_bridge(shared.clone(), token.clone(), rx, context);
    tx.send(StreamMessage::Text {
        content: body[frozen.len()..].to_string(),
    })
    .unwrap();
    tx.send(StreamMessage::Done {
        result: result.clone(),
        session_id: None,
    })
    .unwrap();
    drop(tx);
    let stripped = |text: &str| {
        text.lines()
            .filter(|l| !l.starts_with("-# "))
            .collect::<Vec<_>>()
            .join("\n")
    };
    if !intro.is_empty() {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !crate::services::discord::turn_bridge::STREAM_TEST_EVENTS
                .lock()
                .unwrap()
                .iter()
                .any(|(ch, event)| *ch == channel.get() && event == "replay")
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("a Done that changed the posted parts takes the full replay");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mixed = result[frozen.len()..].trim_end();
        let delivered = shown.0.lock().unwrap();
        assert!(
            delivered
                .iter()
                .all(|(_, text)| stripped(text).trim_end() != mixed),
            "{provider:?}: the new tail was joined to the old parts: {delivered:?}"
        );
        return;
    }
    tokio::time::timeout(Duration::from_secs(15), async {
        while mailbox.snapshot().await.cancel_token.is_some()
            || inflight::load_inflight_state_read_only(&shared.provider, channel.get()).is_some()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the turn ends and releases its lease");
    let delivered = shown.0.lock().unwrap();
    assert!(
        delivered.iter().all(|(_, text)| !text.contains("START-r1")),
        "frozen parts are not posted again: {delivered:?}"
    );
    let tail = delivered
        .iter()
        .rev()
        .find(|(_, text)| text.contains("END-r1"))
        .map(|(_, text)| stripped(text))
        .expect("the tail reaches the current message");
    assert_eq!(format!("{frozen}{}", tail.trim_end()), body);
}
