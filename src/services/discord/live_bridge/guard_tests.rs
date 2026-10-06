use super::*;

#[tokio::test]
async fn same_slot_originals_overlap_and_both_exclude_recovery() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Codex;
    let first = Arc::new(CancelToken::new());
    let second = Arc::new(CancelToken::new());
    let one = register_original(&provider, 655_201_001, &first)
        .await
        .unwrap();
    let two = register_original(&provider, 655_201_001, &second)
        .await
        .unwrap();
    assert!(try_recovery(&provider, 655_201_001).is_err());
    drop(one);
    assert!(try_recovery(&provider, 655_201_001).is_err());
    drop(two);
    assert!(try_recovery(&provider, 655_201_001).is_ok());
}

#[tokio::test(start_paused = true)]
async fn recovery_first_bounds_original_start_without_provider_execution() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Codex;
    let recovery = try_recovery(&provider, 655_201_002).unwrap();
    let token = Arc::new(CancelToken::new());
    let start = tokio::time::Instant::now();
    assert!(
        register_original(&provider, 655_201_002, &token)
            .await
            .is_err()
    );
    assert_eq!(start.elapsed(), START_WAIT);
    drop(recovery);
    assert!(
        register_original(&provider, 655_201_002, &token)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn recovery_scope_reuses_admission_only_for_its_exact_slot() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Codex;
    let recovery = try_recovery(&provider, 655_201_003).unwrap();
    recovery
        .run(async {
            assert!(try_recovery(&provider, 655_201_003).is_ok());
            assert!(try_recovery(&ProviderKind::Claude, 655_201_003).is_ok());
        })
        .await;
    assert!(try_recovery(&provider, 655_201_003).is_err());
}

use crate::services::discord::formatting::ReplaceLongMessageOutcome;
use crate::services::discord::gateway::{GatewayFuture, TurnGateway};
use crate::services::discord::turn_bridge::{TurnBridgeContext, spawn_turn_bridge};
use crate::services::discord::{self as discord, Intervention, StreamMessage};
use crate::services::turn_orchestrator::{ChannelMailboxRegistry, DispatchLease};
use poise::serenity_prelude::{MessageId, UserId};
use std::sync::mpsc;

const BODY: &str = "original reader terminal body";
const NEXT: &str = "next queued item executed";

struct RecordingGateway {
    shared: Arc<SharedData>,
    bodies: std::sync::Mutex<Vec<String>>,
    dispatched: std::sync::Mutex<Vec<MessageId>>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    hold_body: std::sync::atomic::AtomicBool,
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
                let before_publish = self.hold_body.load(std::sync::atomic::Ordering::Relaxed);
                if before_publish {
                    self.entered.notify_one();
                    self.release.notified().await;
                }
                self.bodies.lock().unwrap().push(content.to_owned());
                if !before_publish {
                    self.entered.notify_one();
                    self.release.notified().await;
                }
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
            assert!(
                self.bodies
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|body| body.contains(BODY)),
                "next admission and watcher pause must follow confirmed BODY"
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
        memory_recall_usage: Default::default(),
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

fn row(channel: u64, token: &Arc<CancelToken>) -> InflightTurnState {
    let mut row = InflightTurnState::new(
        ProviderKind::Codex,
        channel,
        None,
        7,
        channel + 1,
        channel + 2,
        "original request".into(),
        Some("fixture-session".into()),
        Some(format!("AgentDesk-codex-{channel}")),
        None,
        None,
        0,
    );
    row.turn_nonce = token.turn_nonce().map(str::to_owned);
    row.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui);
    row
}

#[tokio::test]
async fn t08_fallback_death_respawn_preserves_row_one_reader_and_body_before_next() {
    let _absence = discord::health::watcher_respawn::lock_watcher_absence_for_test().await;
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    for terminal_first in [true, false] {
        let channel = ChannelId::new(655_202_001 + u64::from(terminal_first) * 100);
        let provider = ProviderKind::Codex;
        let mut shared = discord::make_shared_data_for_tests();
        let data = Arc::get_mut(&mut shared).unwrap();
        data.ui.status_panel_v2_enabled = false;
        data.ui.placeholder_live_events_enabled = false;
        data.provider = provider.clone();
        let token = Arc::new(CancelToken::new());
        let mut row = row(channel.get(), &token);
        row.turn_start_offset = Some(0);
        let rollout = root.path().join(format!("{}.jsonl", channel.get()));
        let native = format!(
            "{}\n",
            serde_json::json!({
                "type": "event_msg", "payload": {"type": "task_complete", "turn_id": "fixture-native-turn",
                "last_agent_message": BODY},
            })
        );
        std::fs::write(&rollout, native.as_bytes()).unwrap();
        let tmux = row.tmux_session_name.as_deref().unwrap();
        let _alive = crate::services::session_host::test_support::InjectedLivenessGuard::set(
            crate::services::session_host::HostSessionRef::tmux(tmux),
            crate::services::session_host::HostLiveness::Live,
        );
        crate::services::codex_tui::session::write_codex_tui_rollout_marker_with_start_offset(
            tmux,
            &rollout,
            Some("fixture-session"),
            Some(0),
        )
        .unwrap();
        std::fs::write(
            crate::services::tmux_common::session_temp_path(tmux, "generation"),
            b"1",
        )
        .unwrap();
        assert!(
            discord::mailbox_try_start_turn(
                &shared,
                channel,
                token.clone(),
                UserId::new(7),
                MessageId::new(row.user_msg_id)
            )
            .await
        );
        discord::increment_global_active(&shared, "fixture");
        let original = register_or_requeue(&shared, &provider, &row, &token)
            .await
            .unwrap();
        discord::inflight::save_inflight_state(&row).unwrap();
        let durable = discord::inflight::load_inflight_state(&provider, channel.get()).unwrap();
        let queued = ChannelMailboxRegistry::queued_for_test(channel.get() + 3);
        let queued_id = queued.message_id;
        shared
            .mailbox(channel)
            .replace_queue(
                vec![queued],
                discord::queue_persistence_context(&shared, &provider, channel),
            )
            .await;
        let recorder = discord::recovery_engine::o_cut_recorder::start(channel.get()).await;
        let registry = discord::health::HealthRegistry::new();
        registry
            .register(provider.as_str().into(), shared.clone())
            .await;
        discord::health::watcher_respawn::seed_live_bridge_respawn_test(channel);
        let gateway = Arc::new(RecordingGateway {
            shared: shared.clone(),
            bodies: Default::default(),
            dispatched: Default::default(),
            entered: Default::default(),
            release: Default::default(),
            hold_body: terminal_first.into(),
        });
        let mut context = bridge_context(durable.clone(), gateway.clone());
        let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
        context.completion_tx = Some(completion_tx);
        let (tx, rx) = mpsc::channel();
        spawn_turn_bridge(shared.clone(), token.clone(), rx, context);
        let producer_registration = original.clone();
        let original_readers = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reader_count = original_readers.clone();
        let (killed_tx, killed_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let terminal = StreamMessage::CodexTuiTerminalDone {
            result: BODY.into(),
            session_id: Some("fixture-session".into()),
            rollout_path: rollout.display().to_string(),
            tmux_session_name: row.tmux_session_name.clone().unwrap(),
            turn_nonce: token.turn_nonce().unwrap().into(),
            source_start: 0,
            complete_record_end: native.len() as u64,
            captured_source: None,
        };
        let producer = tokio::task::spawn_blocking(move || {
            let _registration = producer_registration;
            reader_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            killed_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            tx.send(terminal).unwrap();
        });
        killed_rx.await.unwrap();
        // The pane dies during fallback while the original producer still owns its registration.
        let handed_off = discord::tmux_restart_handoff::start_restart_handoff_from_state(
            channel,
            &recorder.http,
            &shared,
            &provider,
            durable.clone(),
            "",
        )
        .await;
        let preserved = discord::inflight::load_inflight_state(&provider, channel.get());
        assert!(
            preserved.is_some(),
            "death recovery must preserve the original row"
        );
        assert_eq!(preserved.unwrap().turn_nonce, durable.turn_nonce);
        assert!(!handed_off);
        assert!(
            recorder.calls().is_empty(),
            "no placeholder takeover before BODY"
        );
        let respawned = discord::health::watcher_respawn::retry_pending_watcher_respawn(
            &registry,
            &provider,
            &[shared.clone()],
            channel,
            1_000,
        )
        .await;
        assert_eq!(
            discord::health::watcher_respawn::live_bridge_respawn_test_counts(channel),
            [0, 0, 0],
            "snapshot/reclaim/failure budget must all remain untouched"
        );
        assert!(!respawned);
        assert!(
            shared.tmux_watchers.len() == 0,
            "the original reader remains the only reader"
        );
        assert_eq!(
            original_readers.load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        drop(original);
        release_tx.send(()).unwrap();
        producer.await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), gateway.entered.notified())
            .await
            .unwrap();
        let latched = discord::inflight::load_inflight_state(&provider, channel.get()).unwrap();
        let handed_off = discord::tmux_restart_handoff::start_restart_handoff_from_state(
            channel,
            &recorder.http,
            &shared,
            &provider,
            latched,
            BODY,
        )
        .await;
        assert!(
            recorder.calls().is_empty(),
            "no placeholder takeover while the original BODY awaits settlement"
        );
        assert!(discord::inflight::load_inflight_state(&provider, channel.get()).is_some());
        assert!(!handed_off);
        assert!(
            is_live(&provider, channel.get()),
            "provider exit must not end bridge protection"
        );
        assert!(try_recovery(&provider, channel.get()).is_err());
        let respawned = discord::health::watcher_respawn::retry_pending_watcher_respawn(
            &registry,
            &provider,
            &[shared.clone()],
            channel,
            1_001,
        )
        .await;
        assert_eq!(
            discord::health::watcher_respawn::live_bridge_respawn_test_counts(channel),
            [0, 0, 0]
        );
        assert!(!respawned);
        assert_eq!(
            gateway
                .bodies
                .lock()
                .unwrap()
                .iter()
                .filter(|body| body.contains(BODY))
                .count(),
            usize::from(!terminal_first)
        );
        assert!(
            gateway.dispatched.lock().unwrap().is_empty(),
            "no next-turn pause before BODY"
        );
        assert!(
            !discord::queue_io::mailbox_try_start_turn_behind_queue(
                &shared,
                channel,
                Arc::new(CancelToken::new()),
                UserId::new(7),
                MessageId::new(queued_id.get() + 10)
            )
            .await
        );

        gateway.release.notify_one();
        tokio::time::timeout(Duration::from_secs(10), completion_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            gateway
                .bodies
                .lock()
                .unwrap()
                .iter()
                .filter(|body| body.contains(BODY))
                .count(),
            1
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if !gateway.dispatched.lock().unwrap().is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(*gateway.dispatched.lock().unwrap(), vec![queued_id]);
        assert_eq!(
            shared
                .mailbox(channel)
                .snapshot()
                .await
                .active_user_message_id,
            Some(queued_id)
        );
    }
}

#[tokio::test]
async fn confirmed_original_registers_and_blocks_death_while_n1_rejects_synthetic_birth() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let channel = ChannelId::new(655_203_001);
    let _confirmed = crate::services::tui_o::turn_mode::TestConfirmation::new(channel.get());
    let shared = discord::make_shared_data_for_tests();
    let token = Arc::new(CancelToken::new());
    let row = row(channel.get(), &token);
    let original = register_or_requeue(&shared, &ProviderKind::Codex, &row, &token)
        .await
        .unwrap();
    discord::inflight::save_inflight_state(&row).unwrap();
    let recorder = discord::recovery_engine::o_cut_recorder::start(channel.get()).await;
    let handed_off = discord::tmux_restart_handoff::start_restart_handoff_from_state(
        channel,
        &recorder.http,
        &shared,
        &ProviderKind::Codex,
        row.clone(),
        BODY,
    )
    .await;
    assert!(
        recorder.calls().is_empty(),
        "confirmed death must not take over the placeholder"
    );
    assert_eq!(
        discord::inflight::load_inflight_state(&ProviderKind::Codex, channel.get())
            .unwrap()
            .turn_nonce,
        row.turn_nonce
    );
    assert!(!handed_off);
    assert!(
        original.is_some(),
        "confirmed original execution still registers"
    );
    drop(original);
    discord::inflight::clear_inflight_state(&ProviderKind::Codex, channel.get());
    assert!(
        !discord::tmux::tmux_watcher::liveness::reacquire_watcher_inflight_for_active_stream(
            &ProviderKind::Codex,
            channel,
            "fixture",
            "fixture-output",
            0,
            None,
            None,
            None
        )
    );
    assert!(discord::inflight::load_inflight_state(&ProviderKind::Codex, channel.get()).is_none());
}

#[tokio::test]
async fn live_original_blocks_both_pinned_and_unpinned_manual_rebind_before_preflight() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let channel = 655_204_001;
    let shared = discord::make_shared_data_for_tests();
    let token = Arc::new(CancelToken::new());
    let mut row = row(channel, &token);
    row.tmux_session_name =
        Some(ProviderKind::Codex.build_tmux_session_name("live-guard-fixture-cdx"));
    let output = root.path().join("rebind-native.jsonl");
    std::fs::write(&output, format!("{}\n", serde_json::json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"fixture-native-turn","last_agent_message":BODY}}))).unwrap();
    row.output_path = Some(output.display().to_string());
    row.turn_start_offset = Some(0);
    let tmux = row.tmux_session_name.as_deref().unwrap();
    crate::services::tmux_common::write_tmux_runtime_kind_marker(
        tmux,
        crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
    )
    .unwrap();
    crate::services::codex_tui::session::write_codex_tui_rollout_marker_with_start_offset(
        tmux,
        &output,
        Some("fixture-session"),
        Some(0),
    )
    .unwrap();
    let _original = register_original(&ProviderKind::Codex, channel, &token)
        .await
        .unwrap();
    discord::inflight::save_inflight_state(&row).unwrap();
    let durable = discord::inflight::load_inflight_state(&ProviderKind::Codex, channel).unwrap();
    let pin = discord::inflight::InflightEpisodePin::from_state(&durable);
    let tmux = row.tmux_session_name.as_deref().unwrap();
    let _alive = crate::services::session_host::test_support::InjectedLivenessGuard::set(
        crate::services::session_host::HostSessionRef::tmux(tmux),
        crate::services::session_host::HostLiveness::Live,
    );
    let recorder = discord::recovery_engine::o_cut_recorder::start(channel).await;
    for expected in [None, Some(&pin)] {
        let result = discord::recovery_engine::rebind_inflight_for_channel(
            &recorder.http,
            &shared,
            &ProviderKind::Codex,
            channel,
            Some(tmux.to_owned()),
            Default::default(),
            expected,
        )
        .await;
        let after = discord::inflight::load_inflight_state(&ProviderKind::Codex, channel).unwrap();
        assert_eq!(
            after.effective_relay_owner_kind(),
            durable.effective_relay_owner_kind(),
            "no coordinate adoption while original reader lives"
        );
        assert_eq!(after.output_path, durable.output_path);
        assert!(
            shared.tmux_watchers.len() == 0,
            "no recovery reader while original lives"
        );
        assert!(
            recorder.calls().is_empty(),
            "no external preflight or takeover while original reader lives"
        );
        assert_eq!(
            discord::inflight::load_inflight_state(&ProviderKind::Codex, channel)
                .unwrap()
                .turn_nonce,
            durable.turn_nonce
        );
        assert!(
            matches!(
                result,
                Err(discord::recovery_engine::RebindError::InflightAlreadyExists)
            ),
            "live original must stop rebind before external preflight: {result:?}"
        );
    }
    assert!(recorder.calls().is_empty());
}

#[tokio::test]
async fn actual_dead_original_retains_existing_handoff_and_exact_successor_cleanup_fence() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let channel = ChannelId::new(655_205_001);
    let shared = discord::make_shared_data_for_tests();
    let token = Arc::new(CancelToken::new());
    let row = row(channel.get(), &token);
    let original = register_original(&ProviderKind::Codex, channel.get(), &token)
        .await
        .unwrap();
    discord::inflight::save_inflight_state(&row).unwrap();
    let durable =
        discord::inflight::load_inflight_state(&ProviderKind::Codex, channel.get()).unwrap();
    drop(original);
    let recorder = discord::recovery_engine::o_cut_recorder::start(channel.get()).await;
    assert!(
        discord::tmux_restart_handoff::start_restart_handoff_from_state(
            channel,
            &recorder.http,
            &shared,
            &ProviderKind::Codex,
            durable.clone(),
            BODY
        )
        .await
    );
    assert_eq!(
        recorder
            .contents()
            .iter()
            .filter(|text| text.contains(BODY))
            .count(),
        1
    );
    assert!(discord::inflight::load_inflight_state(&ProviderKind::Codex, channel.get()).is_none());
    let successor_token = Arc::new(CancelToken::new());
    for rebind_origin in [false, true] {
        let mut successor = row.clone();
        successor.rebind_origin = rebind_origin;
        successor.turn_nonce = successor_token.turn_nonce().map(str::to_owned);
        discord::inflight::save_inflight_state(&successor).unwrap();
        assert!(
            !discord::tmux_restart_handoff::start_restart_handoff_from_state(
                channel,
                &recorder.http,
                &shared,
                &ProviderKind::Codex,
                durable.clone(),
                ""
            )
            .await
        );
        assert_eq!(
            discord::inflight::load_inflight_state(&ProviderKind::Codex, channel.get())
                .unwrap()
                .turn_nonce,
            successor.turn_nonce
        );
    }
}

#[tokio::test(start_paused = true)]
async fn recovery_first_timeout_requeues_original_input_and_unwinds_only_its_actor() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let channel = ChannelId::new(655_206_001);
    let mut shared = discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared).unwrap().provider = ProviderKind::Codex;
    let token = Arc::new(CancelToken::new());
    let mut row = row(channel.get(), &token);
    row.set_followup_requeue_context(
        Some("original reply context".into()),
        true,
        false,
        Vec::new(),
        None,
        true,
    );
    assert!(
        discord::mailbox_try_start_turn(
            &shared,
            channel,
            token.clone(),
            UserId::new(7),
            MessageId::new(row.user_msg_id)
        )
        .await
    );
    discord::increment_global_active(&shared, "fixture");
    let recovery = try_recovery(&ProviderKind::Codex, channel.get()).unwrap();
    let start = tokio::time::Instant::now();
    assert!(matches!(
        register_or_requeue(&shared, &ProviderKind::Codex, &row, &token).await,
        Err(true)
    ));
    assert_eq!(start.elapsed(), START_WAIT);
    let snapshot = shared.mailbox(channel).snapshot().await;
    assert!(snapshot.cancel_token.is_none());
    assert_eq!(snapshot.intervention_queue.len(), 1);
    assert_eq!(snapshot.intervention_queue[0].text, row.user_text);
    assert_eq!(
        snapshot.intervention_queue[0].reply_context,
        row.followup_reply_context
    );
    assert!(snapshot.intervention_queue[0].has_reply_boundary);
    assert_eq!(
        snapshot.intervention_queue[0].message_id.get(),
        row.user_msg_id
    );
    assert!(discord::inflight::load_inflight_state(&ProviderKind::Codex, channel.get()).is_none());
    assert!(!is_live(&ProviderKind::Codex, channel.get()));
    drop(recovery);
}

#[tokio::test]
async fn row_loss_keeps_original_registration_and_blocks_self_heal_until_both_owners_exit() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let channel = ChannelId::new(655_207_001);
    let token = Arc::new(CancelToken::new());
    let producer = register_original(&ProviderKind::Codex, channel.get(), &token)
        .await
        .unwrap();
    let bridge = retain_original(&ProviderKind::Codex, channel.get(), &token);
    assert!(
        !discord::tmux::tmux_watcher::liveness::reacquire_watcher_inflight_for_active_stream(
            &ProviderKind::Codex,
            channel,
            "fixture",
            "fixture-output",
            0,
            None,
            None,
            None
        )
    );
    assert!(discord::inflight::load_inflight_state(&ProviderKind::Codex, channel.get()).is_none());
    drop(bridge);
    assert!(
        try_recovery(&ProviderKind::Codex, channel.get()).is_err(),
        "reader-only protection remains"
    );
    drop(producer);
    assert!(try_recovery(&ProviderKind::Codex, channel.get()).is_ok());
    let successor = register_original(
        &ProviderKind::Codex,
        channel.get(),
        &Arc::new(CancelToken::new()),
    )
    .await
    .unwrap();
    assert!(
        try_recovery(&ProviderKind::Codex, channel.get()).is_err(),
        "old drop must not unregister successor"
    );
    drop(successor);
}

#[test]
fn original_and_restore_source_coverage_preserves_admission_order() {
    for (source, call) in [
        (
            include_str!("../router/message_handler/intake_turn.rs"),
            "register_or_requeue(",
        ),
        (
            include_str!("../router/message_handler/headless_turn.rs"),
            "register_headless_original(",
        ),
    ] {
        let register = source.find(call).unwrap();
        let create = source[register..]
            .find("save_inflight_state_create_new(")
            .unwrap()
            + register;
        let producer = source[create..].find("spawn_blocking(move ||").unwrap() + create;
        assert!(register < create && create < producer);
        assert!(!source[register..create].contains("transcript_turns"));
        assert!(source[producer..].contains("let _original_registration = producer_registration;"));
        assert!(source.contains("producer_registration = original_registration.clone()"));
    }
    let restore = include_str!("../recovery_engine/restore_inflight.rs");
    for reader in ["spawn_observed_tmux_watcher(", "read_restored_output("] {
        let start = restore.find(reader).unwrap();
        assert!(restore[..start].contains(if reader.starts_with("spawn") {
            "reregister_active_turn_from_inflight("
        } else {
            "mailbox_recovery_kickoff("
        }));
    }
}

#[tokio::test]
async fn off_entries_do_not_register_wait_or_block_existing_recovery() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let _off = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "AGENTDESK_CODEX_LIVE_BRIDGE_GUARD",
        std::ffi::OsStr::new("0"),
    );
    let provider = ProviderKind::Codex;
    let channel = 655_208_001;
    let held = slot(&provider, channel).gate.clone().write_owned().await;
    let token = Arc::new(CancelToken::new());
    assert!(
        register_original(&provider, channel, &token)
            .await
            .unwrap()
            .is_none()
    );
    assert!(!is_live(&provider, channel));
    assert!(retain_original(&provider, channel, &token).is_none());
    assert!(try_recovery(&provider, channel).unwrap().slot.is_none());
    drop(held);
    let shared = discord::make_shared_data_for_tests();
    let recorder = discord::recovery_engine::o_cut_recorder::start(channel).await;
    assert!(
        discord::tmux_restart_handoff::start_restart_handoff_from_state(
            ChannelId::new(channel),
            &recorder.http,
            &shared,
            &provider,
            row(channel, &token),
            BODY
        )
        .await
    );
    assert_eq!(
        recorder
            .contents()
            .iter()
            .filter(|text| text.contains(BODY))
            .count(),
        1
    );
}

#[tokio::test]
async fn real_bridge_entry_abort_keeps_the_original_reader_registered() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let channel = ChannelId::new(655_209_001);
    let mut shared = discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared).unwrap().provider = ProviderKind::Codex;
    let token = Arc::new(CancelToken::new());
    let original = row(channel.get(), &token);
    assert!(
        discord::mailbox_try_start_turn(
            &shared,
            channel,
            token.clone(),
            UserId::new(7),
            MessageId::new(original.user_msg_id)
        )
        .await
    );
    discord::increment_global_active(&shared, "fixture");
    let producer = register_original(&ProviderKind::Codex, channel.get(), &token)
        .await
        .unwrap();
    let mut incumbent = original.clone();
    incumbent.user_msg_id += 10;
    incumbent.turn_nonce = Some("incumbent".into());
    discord::inflight::save_inflight_state(&incumbent).unwrap();
    let mut context = bridge_context(original, Arc::new(discord::gateway::HeadlessGateway));
    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
    context.completion_tx = Some(completion_tx);
    let (_tx, rx) = mpsc::channel();
    spawn_turn_bridge(shared, token, rx, context);
    let signal = tokio::time::timeout(Duration::from_secs(5), completion_rx)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        signal,
        discord::turn_bridge::BridgeCompletionSignal::EntryAborted
    ));
    assert!(try_recovery(&ProviderKind::Codex, channel.get()).is_err());
    drop(producer);
    tokio::time::timeout(Duration::from_secs(5), async {
        while is_live(&ProviderKind::Codex, channel.get()) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(try_recovery(&ProviderKind::Codex, channel.get()).is_ok());
}

#[tokio::test]
async fn thread_registration_uses_the_row_channel_and_runtime_learning_keeps_the_same_token() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let token = Arc::new(CancelToken::new());
    let child = 655_210_001;
    let parent = 655_210_002;
    let producer = register_original(&ProviderKind::Codex, child, &token)
        .await
        .unwrap();
    let mut learned = row(child, &token);
    learned.watcher_owner_channel_id = Some(parent);
    learned.output_path = Some("learned-rollout".into());
    learned.session_id = Some("learned-session".into());
    assert!(retain_original(&ProviderKind::Codex, learned.channel_id, &token).is_some());
    assert!(try_recovery(&ProviderKind::Codex, child).is_err());
    assert!(try_recovery(&ProviderKind::Codex, parent).is_ok());
    drop(producer);
}

#[cfg(unix)]
#[tokio::test]
async fn canonical_root_alias_keeps_the_slot_before_and_after_first_row_creation() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let alias_dir = tempfile::tempdir().unwrap();
    let alias = alias_dir.path().join("alias");
    std::os::unix::fs::symlink(root.path(), &alias).unwrap();
    let token = Arc::new(CancelToken::new());
    let channel = 655_211_001;
    let original = register_original(&ProviderKind::Codex, channel, &token)
        .await
        .unwrap();
    let _alias = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        &alias,
    );
    discord::inflight::save_inflight_state(&row(channel, &token)).unwrap();
    assert!(try_recovery(&ProviderKind::Codex, channel).is_err());
    drop(original);
}

#[tokio::test(start_paused = true)]
async fn recovery_first_release_revalidates_actor_and_preserves_successor() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = ChannelId::new(655_212_001);
    let mut shared = discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared).unwrap().provider = ProviderKind::Codex;
    let token = Arc::new(CancelToken::new());
    let original = row(channel.get(), &token);
    assert!(
        discord::mailbox_try_start_turn(
            &shared,
            channel,
            token.clone(),
            UserId::new(7),
            MessageId::new(original.user_msg_id)
        )
        .await
    );
    discord::increment_global_active(&shared, "fixture");
    let recovery = try_recovery(&ProviderKind::Codex, channel.get()).unwrap();
    let start = register_or_requeue(&shared, &ProviderKind::Codex, &original, &token);
    tokio::pin!(start);
    assert!(futures::poll!(&mut start).is_pending());
    let released = discord::mailbox_finish::mailbox_finish_turn_if_matches_episode_started_before_with_actor_without_completion(
        &shared, &ProviderKind::Codex, channel, MessageId::new(original.user_msg_id), original.turn_nonce.clone(), std::time::Instant::now(), Some(token.clone()),
    ).await;
    assert!(released.removed_token.is_some());
    discord::saturating_decrement_global_active(&shared);
    let successor = Arc::new(CancelToken::new());
    let successor_id = MessageId::new(original.user_msg_id + 10);
    assert!(
        discord::mailbox_try_start_turn(
            &shared,
            channel,
            successor.clone(),
            UserId::new(7),
            successor_id
        )
        .await
    );
    discord::increment_global_active(&shared, "fixture successor");
    drop(recovery);
    assert!(matches!(start.await, Err(true)));
    let snapshot = shared.mailbox(channel).snapshot().await;
    assert!(Arc::ptr_eq(
        snapshot.cancel_token.as_ref().unwrap(),
        &successor
    ));
    assert_eq!(snapshot.active_user_message_id, Some(successor_id));
    assert_eq!(snapshot.intervention_queue.len(), 1);
    assert_eq!(
        snapshot.intervention_queue[0].message_id.get(),
        original.user_msg_id
    );
    assert!(!is_live(&ProviderKind::Codex, channel.get()));
}

#[tokio::test]
async fn other_providers_do_not_register_or_block_recovery() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let token = Arc::new(CancelToken::new());
    let provider = ProviderKind::Gemini;
    assert!(
        register_original(&provider, 655_213_001, &token)
            .await
            .unwrap()
            .is_none()
    );
    assert!(try_recovery(&provider, 655_213_001).unwrap().slot.is_none());
    assert!(
        try_respawn_recovery(&provider, 655_213_001)
            .unwrap()
            .slot
            .is_none()
    );
    assert!(!is_live(&provider, 655_213_001));
}

#[tokio::test]
async fn a_claude_original_blocks_only_watcher_respawn() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Claude;
    let token = Arc::new(CancelToken::new());
    let original = register_original(&provider, 655_213_002, &token)
        .await
        .unwrap()
        .expect("a Claude original registers");
    assert!(is_live(&provider, 655_213_002));
    assert!(retain_original(&provider, 655_213_002, &token).is_some());
    assert!(try_recovery(&provider, 655_213_002).unwrap().slot.is_none());
    assert!(try_respawn_recovery(&provider, 655_213_002).is_err());
    drop(original);
    assert!(!is_live(&provider, 655_213_002));
    assert!(
        try_respawn_recovery(&provider, 655_213_002)
            .unwrap()
            .is_guarded()
    );
    let _off = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "AGENTDESK_CLAUDE_LIVE_BRIDGE_GUARD",
        std::ffi::OsStr::new("0"),
    );
    let held = slot(&provider, 655_213_002)
        .gate
        .clone()
        .write_owned()
        .await;
    assert!(
        register_original(&provider, 655_213_002, &token)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        try_respawn_recovery(&provider, 655_213_002)
            .unwrap()
            .slot
            .is_none()
    );
    drop(held);
}
