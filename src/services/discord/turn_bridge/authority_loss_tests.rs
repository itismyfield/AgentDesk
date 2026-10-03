use super::*;
use crate::services::discord::formatting::ReplaceLongMessageOutcome;
use crate::services::discord::{self as discord, gateway::GatewayFuture};
use crate::services::turn_orchestrator::{ChannelMailboxRegistry, DispatchLease};

const BODY: &str = "original reader terminal body";
const NEXT: &str = "next queued item executed";

pub(super) struct TestBarrier {
    pub(super) channel: ChannelId,
    pub(super) entered: tokio::sync::Notify,
    pub(super) resume: tokio::sync::Notify,
    pub(super) lost: tokio::sync::Notify,
}

static BARRIER: std::sync::Mutex<Option<Arc<TestBarrier>>> = std::sync::Mutex::new(None);

pub(super) async fn before_stream(channel: ChannelId) {
    let barrier = BARRIER
        .lock()
        .unwrap()
        .clone()
        .filter(|barrier| barrier.channel == channel);
    if let Some(barrier) = barrier {
        barrier.entered.notify_one();
        barrier.resume.notified().await;
    }
}

pub(super) fn authority_lost(channel: ChannelId) {
    if let Some(barrier) = BARRIER
        .lock()
        .unwrap()
        .as_ref()
        .filter(|barrier| barrier.channel == channel)
    {
        barrier.lost.notify_one();
    }
}

struct RecordingGateway {
    shared: Arc<SharedData>,
    bodies: std::sync::Mutex<Vec<String>>,
    dispatched: std::sync::Mutex<Vec<MessageId>>,
}

impl TurnGateway for RecordingGateway {
    fn send_message<'a>(
        &'a self,
        _channel: ChannelId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>> {
        Box::pin(async move {
            self.bodies.lock().unwrap().push(content.to_owned());
            Ok(MessageId::new(655_200_900))
        })
    }
    fn edit_message<'a>(
        &'a self,
        _channel: ChannelId,
        _message: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if content.contains(BODY) {
                self.bodies.lock().unwrap().push(content.to_owned());
            }
            Ok(())
        })
    }
    fn replace_message_with_outcome<'a>(
        &'a self,
        _channel: ChannelId,
        _message: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
        Box::pin(async move {
            if content.contains(BODY) {
                self.bodies.lock().unwrap().push(content.to_owned());
            }
            Ok(ReplaceLongMessageOutcome::EditedOriginal)
        })
    }
    fn schedule_retry_with_history<'a>(
        &'a self,
        _channel: ChannelId,
        _message: MessageId,
        _text: &'a str,
    ) -> GatewayFuture<'a, ()> {
        Box::pin(async {})
    }
    fn dispatch_queued_turn<'a>(
        &'a self,
        channel: ChannelId,
        intervention: &'a Intervention,
        _request_owner_name: &'a str,
        _has_more: bool,
        _lease: Option<Arc<DispatchLease>>,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async move {
            assert!(
                discord::mailbox_try_start_turn(
                    &self.shared,
                    channel,
                    Arc::new(CancelToken::new()),
                    intervention.author_id,
                    intervention.message_id
                )
                .await
            );
            self.dispatched
                .lock()
                .unwrap()
                .push(intervention.message_id);
            self.send_message(channel, NEXT).await?;
            Ok(())
        })
    }
    fn validate_live_routing<'a>(
        &'a self,
        _channel: ChannelId,
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

fn bridge_context(row: InflightTurnState, gateway: Arc<dyn TurnGateway>) -> TurnBridgeContext {
    TurnBridgeContext {
        provider: ProviderKind::Codex,
        gateway,
        channel_id: ChannelId::new(row.channel_id),
        user_msg_id: Some(MessageId::new(row.user_msg_id)),
        user_text_owned: "original request".into(),
        request_owner_name: "fixture".into(),
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
        current_msg_id: Some(MessageId::new(row.current_msg_id)),
        response_sent_offset: 0,
        full_response: String::new(),
        tmux_last_offset: None,
        new_session_id: None,
        defer_watcher_resume: false,
        reuse_status_panel_message: false,
        completion_tx: None,
        is_external_input_tui_direct: false,
        inflight_state: row,
    }
}

#[test]
fn displaced_codex_terminal_releases_exact_mailbox_and_dispatches_next_once() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for terminal_first in [false, true] {
                let iteration_offset = if terminal_first { 1_000 } else { 0 };
                let mut shared = discord::make_shared_data_for_tests();
                let ui = &mut Arc::get_mut(&mut shared).unwrap().ui;
                ui.status_panel_v2_enabled = false;
                ui.placeholder_live_events_enabled = false;
                let channel = ChannelId::new(655_200_001 + iteration_offset);
                let original = MessageId::new(655_200_002 + iteration_offset);
                let actor = Arc::new(CancelToken::new());
                assert!(
                    discord::mailbox_try_start_turn(
                        &shared,
                        channel,
                        actor.clone(),
                        UserId::new(7),
                        original
                    )
                    .await
                );
                discord::increment_global_active(&shared, "fixture");
                let mut row = InflightTurnState::new(
                    ProviderKind::Codex,
                    channel.get(),
                    None,
                    7,
                    original.get(),
                    655_200_003 + iteration_offset,
                    "original request".into(),
                    Some("fixture-session".into()),
                    Some("fixture-codex-reader".into()),
                    None,
                    None,
                    389918,
                );
                row.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui);
                row.turn_nonce = actor.turn_nonce().map(str::to_owned);
                row.turn_start_offset = Some(389918);
                discord::inflight::save_inflight_state(&row).unwrap();
                let queued = ChannelMailboxRegistry::queued_for_test(655_200_004 + iteration_offset);
                let queued_id = queued.message_id;
                shared
                    .mailbox(channel)
                    .replace_queue(
                        vec![queued],
                        discord::queue_persistence_context(&shared, &ProviderKind::Codex, channel),
                    )
                    .await;
                let gateway = Arc::new(RecordingGateway {
                    shared: shared.clone(),
                    bodies: Default::default(),
                    dispatched: Default::default(),
                });
                let barrier = Arc::new(TestBarrier {
                    channel,
                    entered: Default::default(),
                    resume: Default::default(),
                    lost: Default::default(),
                });
                *BARRIER.lock().unwrap() = Some(barrier.clone());
                let mut events =
                    discord::turn_completion_events::subscribe_turn_completion_events(&shared);
                let mut context = bridge_context(row.clone(), gateway.clone());
                let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
                context.completion_tx = Some(completion_tx);
                let (tx, rx) = mpsc::sync_channel(0);
                spawn_turn_bridge(shared.clone(), actor.clone(), rx, context);
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    barrier.entered.notified(),
                )
                .await
                .unwrap();
                discord::inflight::clear_inflight_state(&ProviderKind::Codex, channel.get());
                let mut rebound = row.clone();
                rebound.user_msg_id = 0;
                rebound.current_msg_id = 0;
                rebound.turn_nonce = None;
                rebound.rebind_origin = true;
                rebound
                    .set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::Watcher);
                rebound.turn_start_offset = Some(0);
                discord::inflight::save_inflight_state(&rebound).unwrap();
                gateway.send_message(channel, BODY).await.unwrap();
                let commit = discord::inflight::commit_watcher_terminal_delivery_locked(
                    &ProviderKind::Codex,
                    channel.get(),
                    &discord::inflight::InflightTurnIdentity::from_state(&rebound),
                    "fixture-codex-reader",
                    discord::inflight::WatcherTerminalCommitPatch {
                        full_response: BODY.into(),
                        last_offset: 443,
                        last_watcher_relayed_offset: Some(443),
                        last_watcher_relayed_generation_mtime_ns: None,
                    },
                );
                assert_eq!(
                    commit,
                    discord::inflight::WatcherTerminalCommitOutcome::Skipped
                );
                let rebound_path = discord::inflight::inflight_state_path(
                    &discord::inflight::inflight_runtime_root().unwrap(),
                    &ProviderKind::Codex,
                    channel.get(),
                );
                let before = std::fs::read(&rebound_path).unwrap();
                let terminal = StreamMessage::CodexTuiTerminalDone {
                    result: BODY.into(),
                    session_id: Some("fixture-session".into()),
                    rollout_path: "/fixture/original-rollout".into(),
                    tmux_session_name: "fixture-codex-reader".into(),
                    turn_nonce: actor.turn_nonce().unwrap().into(),
                    source_start: 389918,
                    complete_record_end: 446221,
                    captured_source: None,
                };
                let first = if terminal_first {
                    terminal.clone()
                } else {
                    StreamMessage::Text {
                        content: BODY.into(),
                    }
                };
                tx.send(first).unwrap();
                // The second rendezvous proves the adapter queued the first frame before resume.
                // Only that first frame can carry the terminal witness in the terminal-first case.
                tx.send(StreamMessage::Text {
                    content: String::new(),
                })
                .unwrap();
                barrier.resume.notify_one();
                tokio::time::timeout(std::time::Duration::from_secs(5), barrier.lost.notified())
                    .await
                    .unwrap();
                if !terminal_first {
                    assert!(
                        shared
                            .mailbox(channel)
                            .snapshot()
                            .await
                            .cancel_token
                            .is_some(),
                        "authority loss alone must not release a running provider"
                    );
                    let _ = tx.send(terminal);
                }
                drop(tx);
                tokio::time::timeout(std::time::Duration::from_secs(5), completion_rx)
                    .await
                    .unwrap()
                    .unwrap();
                let mut queue_eligible = false;
                while let Ok(event) = events.try_recv() {
                    queue_eligible |= event.channel_id == channel
                        && event.turn_id == Some(original.get())
                        && event.queue_is_eligible();
                }
                shared.restart.finalizing_turns.store(1, Ordering::Relaxed);
                shared.restart.global_finalizing.store(1, Ordering::Relaxed);
                finalize_epilogue::finalize_and_drain_queued_turns(
                    shared.clone(),
                    queue_eligible,
                    false,
                    gateway.clone(),
                    channel,
                    ProviderKind::Codex,
                    "fixture".into(),
                    None,
                    None,
                    false,
                )
                .await;
                assert_eq!(
                    *gateway.dispatched.lock().unwrap(),
                    vec![queued_id],
                    "the displaced provider terminal must let the next queued item execute; terminal_first={terminal_first}"
                );
                assert!(
                    queue_eligible,
                    "the exact original turn must publish queue admission"
                );
                assert_eq!(
                    gateway
                        .bodies
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|body| body.contains(BODY))
                        .count(),
                    1,
                    "the existing publication fence must block the original reader's duplicate; terminal_first={terminal_first}"
                );
                assert_eq!(
                    gateway
                        .bodies
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|body| body.as_str() == NEXT)
                        .count(),
                    1
                );
                assert_eq!(
                    std::fs::read(rebound_path).unwrap(),
                    before,
                    "mailbox completion cannot borrow rebind row mutation authority; terminal_first={terminal_first}"
                );
                *BARRIER.lock().unwrap() = None;
            }
        });
}

#[test]
fn displaced_terminal_keeps_successor_actor_even_when_id_and_nonce_match() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for reuse_nonce in [true, false] {
                let shared = discord::make_shared_data_for_tests();
                let channel = ChannelId::new(655_200_100 + u64::from(reuse_nonce));
                let original = MessageId::new(655_200_200);
                let actor = Arc::new(CancelToken::new());
                assert!(
                    discord::mailbox_try_start_turn(
                        &shared,
                        channel,
                        actor.clone(),
                        UserId::new(7),
                        original
                    )
                    .await
                );
                discord::increment_global_active(&shared, "fixture");
                let mut row = InflightTurnState::new(
                    ProviderKind::Codex,
                    channel.get(),
                    None,
                    7,
                    original.get(),
                    655_200_300,
                    String::new(),
                    None,
                    None,
                    None,
                    None,
                    0,
                );
                row.turn_nonce = actor.turn_nonce().map(str::to_owned);
                let mut context =
                    bridge_context(row.clone(), Arc::new(discord::gateway::HeadlessGateway));
                let (mut guard, mut cleanup) =
                    make_bridge_guards(&mut context, &row, &shared, &ProviderKind::Codex);
                cleanup.defuse();
                let replacement =
                    Arc::new(CancelToken::from_persisted_turn_nonce(if reuse_nonce {
                        row.turn_nonce.clone()
                    } else {
                        Some("successor-episode".into())
                    }));
                shared
                    .mailbox(channel)
                    .restore_active_turn(replacement.clone(), UserId::new(7), original)
                    .await;
                let mut events =
                    discord::turn_completion_events::subscribe_turn_completion_events(&shared);
                guard
                    .settle_displaced_terminal(&ProviderKind::Codex, &actor, &row)
                    .await;
                let current = shared.mailbox(channel).snapshot().await;
                assert!(Arc::ptr_eq(
                    current.cancel_token.as_ref().unwrap(),
                    &replacement
                ));
                assert!(!replacement.cancelled.load(Ordering::Relaxed));
                assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
                assert!(events.try_recv().is_err());
            }
        });
}

#[test]
fn rejected_displaced_terminal_cannot_borrow_mailbox_completion() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let shared = discord::make_shared_data_for_tests();
            let channel = ChannelId::new(655_200_401);
            let actor = Arc::new(CancelToken::new());
            let message = MessageId::new(655_200_402);
            assert!(
                discord::mailbox_try_start_turn(
                    &shared,
                    channel,
                    actor.clone(),
                    UserId::new(7),
                    message
                )
                .await
            );
            discord::increment_global_active(&shared, "fixture");
            let mut row = InflightTurnState::new(
                ProviderKind::Codex,
                channel.get(),
                None,
                7,
                message.get(),
                655_200_403,
                String::new(),
                None,
                Some("original-codex-reader".into()),
                None,
                None,
                64,
            );
            row.turn_nonce = actor.turn_nonce().map(str::to_owned);
            row.turn_start_offset = Some(64);
            row.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui);
            let mut context =
                bridge_context(row.clone(), Arc::new(discord::gateway::HeadlessGateway));
            let (mut guard, mut cleanup) =
                make_bridge_guards(&mut context, &row, &shared, &ProviderKind::Codex);
            cleanup.defuse();
            let valid = StreamMessage::CodexTuiTerminalDone {
                result: BODY.into(),
                session_id: None,
                rollout_path: "/fixture/original-rollout".into(),
                tmux_session_name: "original-codex-reader".into(),
                turn_nonce: actor.turn_nonce().unwrap().into(),
                source_start: 64,
                complete_record_end: 128,
                captured_source: None,
            };
            let mut wrong_nonce = valid.clone();
            if let StreamMessage::CodexTuiTerminalDone { turn_nonce, .. } = &mut wrong_nonce {
                *turn_nonce = "other-episode".into();
            }
            let mut wrong_range = valid.clone();
            if let StreamMessage::CodexTuiTerminalDone { source_start, .. } = &mut wrong_range {
                *source_start += 1;
            }
            let foreign_actor = Arc::new(CancelToken::from_persisted_turn_nonce(
                row.turn_nonce.clone(),
            ));
            let mut wrong_actor = valid.clone();
            if let StreamMessage::CodexTuiTerminalDone {
                captured_source, ..
            } = &mut wrong_actor
            {
                *captured_source =
                    Some(crate::services::agent_protocol::CapturedTuiTerminalSource {
                        generation_mtime_ns: 1,
                        source_file_dev: 1,
                        source_file_ino: 1,
                        actor: Arc::downgrade(&foreign_actor),
                    });
            }
            let (tx, rx) = mpsc::channel();
            drop(tx);
            let mut rx = spawn_stream_message_receiver_adapter(rx);
            let mut pending = VecDeque::from([wrong_nonce, wrong_range, wrong_actor]);
            authority_loss::settle_displaced_terminal(
                (&ProviderKind::Codex, &row, &actor),
                (&mut rx, &mut pending),
                &mut guard,
            )
            .await;
            assert!(Arc::ptr_eq(
                shared
                    .mailbox(channel)
                    .snapshot()
                    .await
                    .cancel_token
                    .as_ref()
                    .unwrap(),
                &actor
            ));
            assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
            let (tx, rx) = mpsc::channel();
            drop(tx);
            let mut rx = spawn_stream_message_receiver_adapter(rx);
            let mut pending = VecDeque::from([valid]);
            authority_loss::settle_displaced_terminal(
                (&ProviderKind::Codex, &row, &actor),
                (&mut rx, &mut pending),
                &mut guard,
            )
            .await;
            assert!(
                shared
                    .mailbox(channel)
                    .snapshot()
                    .await
                    .cancel_token
                    .is_none()
            );
            assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
        });
}
