use super::tests::{inflight_with_identity_offset, matched, terminal_frame_offset};
use super::*;
use crate::services::discord::inflight::RelayOwnerKind;

// Kills M6: removing the fenced-terminal disjunct must lose this terminal outcome.
#[tokio::test]
async fn fenced_terminal_without_parser_delivery_is_terminal_not_delivered() {
    let binding = matched("44001");
    let sink = SessionBoundDiscordRelaySink::new(Arc::new(HealthRegistry::new()));
    let terminal = terminal_frame_offset(
        &binding,
        "{\"type\":\"result\",\"result\":\"\"}\n",
        1,
        256,
        0,
        "2026-08-03T00:00:00Z",
        Some(64),
    );

    let outcome = sink
        .deliver(&terminal)
        .await
        .expect("a fenced terminal without parser delivery is known");

    assert_eq!(outcome, RelaySinkOutcome::TerminalNotDelivered);
}

// Kills M8: transport errors must escape instead of folding into NotDelivered.
#[tokio::test]
async fn relay_deliver_propagates_injected_transport_error() {
    let temp = tempfile::tempdir().expect("temp runtime root");
    let _root = crate::config::set_agentdesk_root_for_test(temp.path());
    let channel_id = 44_002;
    let binding = matched(&channel_id.to_string());
    let session = &binding.expected_session_name;
    let generation_path = crate::services::tmux_common::session_temp_path(session, "generation");
    std::fs::create_dir_all(
        std::path::Path::new(&generation_path)
            .parent()
            .expect("generation parent"),
    )
    .expect("generation directory");
    std::fs::write(&generation_path, b"transport-error").expect("generation marker");
    let started_at = "2026-08-03T00:00:01Z";
    let mut inflight = inflight_with_identity_offset(channel_id, session, 700, started_at, Some(0));
    inflight.set_relay_owner_kind(RelayOwnerKind::SessionBoundRelay);
    inflight.current_msg_id = 88_002;
    crate::services::discord::inflight::save_inflight_state(&inflight).expect("persist inflight");
    let registry = Arc::new(HealthRegistry::new());
    let shared = crate::services::discord::make_shared_data_for_tests();
    registry
        .register(ProviderKind::Claude.as_str().to_string(), shared)
        .await;
    let gateway = Arc::new(RelayContractFakeGateway::failing("fake transport failure"));
    let mut sink = SessionBoundDiscordRelaySink::new(registry);
    sink.test_gateway = Some(gateway.clone());
    let payload = concat!(
        "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"answer\"}]}}\n",
        "{\"type\":\"result\",\"result\":\"answer\"}\n"
    );
    let terminal = terminal_frame_offset(&binding, payload, 1, 256, 700, started_at, Some(0));

    let error = sink
        .deliver(&terminal)
        .await
        .expect_err("transport failure must escape RelaySink::deliver");

    assert!(matches!(error, RelaySinkError::Transient(_)), "{error:?}");
    assert_eq!(gateway.replace_calls.load(Ordering::Acquire), 1);
    crate::services::discord::inflight::clear_inflight_state(&ProviderKind::Claude, channel_id);
}

// Kills M10 and anchor-drop: persisted proof stays Delivered and records the tail anchor.
#[tokio::test]
async fn relay_deliver_preserves_tail_anchor_and_observes_persisted_proof() {
    let temp = tempfile::tempdir().expect("temp runtime root");
    let _root = crate::config::set_agentdesk_root_for_test(temp.path());
    let channel_id = 44_003;
    let binding = matched(&channel_id.to_string());
    let session = &binding.expected_session_name;
    let generation_path = crate::services::tmux_common::session_temp_path(session, "generation");
    std::fs::create_dir_all(
        std::path::Path::new(&generation_path)
            .parent()
            .expect("generation parent"),
    )
    .expect("generation directory");
    std::fs::write(&generation_path, b"persisted-proof").expect("generation marker");
    let generation = dr::current_generation_mtime_ns(session);
    let started_at = "2026-08-03T00:00:02Z";
    let mut inflight = inflight_with_identity_offset(channel_id, session, 701, started_at, Some(0));
    inflight.set_relay_owner_kind(RelayOwnerKind::SessionBoundRelay);
    inflight.current_msg_id = 88_003;
    crate::services::discord::inflight::save_inflight_state(&inflight).expect("persist inflight");
    let registry = Arc::new(HealthRegistry::new());
    let shared = crate::services::discord::make_shared_data_for_tests();
    registry
        .register(ProviderKind::Claude.as_str().to_string(), shared.clone())
        .await;
    let gateway = Arc::new(RelayContractFakeGateway::edited());
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let mut sink = SessionBoundDiscordRelaySink::new(registry);
    sink.test_gateway = Some(gateway.clone());
    sink.test_replace_anchor = Some(formatting::ReplaceLastChunkAnchor {
        msg_id: 99_003,
        text: "tail chunk".to_string(),
    });
    sink.test_delivery_outcomes = Some(outcomes.clone());
    sink.test_force_legacy_replace = true;
    let payload = concat!(
        "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"answer\"}]}}\n",
        "{\"type\":\"result\",\"result\":\"answer\"}\n"
    );
    let mut terminal = terminal_frame_offset(&binding, payload, 1, 256, 701, started_at, Some(0));
    terminal.relay_generation_mtime_ns = Some(generation);

    let outcome = sink.deliver(&terminal).await.expect("persisted delivery");

    assert_eq!(outcome, RelaySinkOutcome::TerminalDelivered);
    assert_eq!(
        outcomes.lock().expect("outcome probe").as_slice(),
        &[SessionRelayDeliveryOutcome::Delivered],
        "M10: persisted proof must remain a typed Delivered outcome"
    );
    let record = dr::read_record(&ProviderKind::Claude, channel_id).expect("delivery record");
    assert_eq!(
        record.delivered_frontier.expect("frontier").panel_msg_id,
        Some(99_003),
        "anchor-drop: legacy replace must retain the formatter tail anchor"
    );
    assert_eq!(gateway.replace_calls.load(Ordering::Acquire), 1);
    crate::services::discord::inflight::clear_inflight_state(&ProviderKind::Claude, channel_id);
}

// Kills M11: stale proof must remain distinguishable from Delivered before public folding.
#[tokio::test]
async fn relay_deliver_observes_landed_stale_proof() {
    let temp = tempfile::tempdir().expect("temp runtime root");
    let _root = crate::config::set_agentdesk_root_for_test(temp.path());
    let channel_id = 44_004;
    let binding = matched(&channel_id.to_string());
    let session = &binding.expected_session_name;
    let generation_path = crate::services::tmux_common::session_temp_path(session, "generation");
    std::fs::create_dir_all(
        std::path::Path::new(&generation_path)
            .parent()
            .expect("generation parent"),
    )
    .expect("generation directory");
    std::fs::write(&generation_path, b"landed-stale").expect("generation marker");
    let generation = dr::current_generation_mtime_ns(session);
    let started_at = "2026-08-03T00:00:03Z";
    let mut inflight = inflight_with_identity_offset(channel_id, session, 702, started_at, Some(0));
    inflight.set_relay_owner_kind(RelayOwnerKind::SessionBoundRelay);
    inflight.current_msg_id = 88_004;
    crate::services::discord::inflight::save_inflight_state(&inflight).expect("persist inflight");
    let registry = Arc::new(HealthRegistry::new());
    let shared = crate::services::discord::make_shared_data_for_tests();
    registry
        .register(ProviderKind::Claude.as_str().to_string(), shared.clone())
        .await;
    let gateway = Arc::new(RelayContractFakeGateway {
        on_transport: Some(Arc::new(move || {
            crate::services::discord::inflight::clear_inflight_state(
                &ProviderKind::Claude,
                channel_id,
            );
        })),
        ..RelayContractFakeGateway::edited()
    });
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let mut sink = SessionBoundDiscordRelaySink::new(registry);
    sink.test_gateway = Some(gateway);
    sink.test_delivery_outcomes = Some(outcomes.clone());
    let payload = concat!(
        "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"answer\"}]}}\n",
        "{\"type\":\"result\",\"result\":\"answer\"}\n"
    );
    let mut terminal = terminal_frame_offset(&binding, payload, 1, 256, 702, started_at, Some(0));
    terminal.relay_generation_mtime_ns = Some(generation);

    let outcome = sink
        .deliver(&terminal)
        .await
        .expect("landed stale delivery");

    assert_eq!(outcome, RelaySinkOutcome::TerminalDelivered);
    assert_eq!(
        outcomes.lock().expect("outcome probe").as_slice(),
        &[SessionRelayDeliveryOutcome::LandedStale],
        "M11: stale proof must remain a typed LandedStale outcome"
    );
    assert!(
        dr::read_record(&ProviderKind::Claude, channel_id)
            .and_then(|record| record.delivered_frontier)
            .is_none(),
        "stale source authority must not persist a delivered frontier"
    );
    crate::services::discord::inflight::clear_inflight_state(&ProviderKind::Claude, channel_id);
}

#[tokio::test]
async fn relay_deliver_observes_landed_unrecorded_proof() {
    let temp = tempfile::tempdir().expect("temp runtime root");
    let _root = crate::config::set_agentdesk_root_for_test(temp.path());
    let channel_id = 44_005;
    let binding = matched(&channel_id.to_string());
    let session = &binding.expected_session_name;
    let generation_path = crate::services::tmux_common::session_temp_path(session, "generation");
    std::fs::create_dir_all(
        std::path::Path::new(&generation_path)
            .parent()
            .expect("generation parent"),
    )
    .expect("generation directory");
    std::fs::write(&generation_path, b"landed-unrecorded").expect("generation marker");
    let generation = dr::current_generation_mtime_ns(session);
    let started_at = "2026-08-03T00:00:04Z";
    let mut inflight = inflight_with_identity_offset(channel_id, session, 703, started_at, Some(0));
    inflight.set_relay_owner_kind(RelayOwnerKind::SessionBoundRelay);
    inflight.current_msg_id = 88_005;
    crate::services::discord::inflight::save_inflight_state(&inflight).expect("persist inflight");
    let runtime = temp.path().join("runtime");
    std::fs::create_dir_all(&runtime).expect("runtime directory");
    std::fs::write(runtime.join("discord_delivery_records"), b"not a directory")
        .expect("block delivery record directory");
    let registry = Arc::new(HealthRegistry::new());
    let shared = crate::services::discord::make_shared_data_for_tests();
    registry
        .register(ProviderKind::Claude.as_str().to_string(), shared)
        .await;
    let gateway = Arc::new(RelayContractFakeGateway::edited());
    let mut sink = SessionBoundDiscordRelaySink::new(registry);
    sink.test_gateway = Some(gateway);
    let payload = concat!(
        "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"answer\"}]}}\n",
        "{\"type\":\"result\",\"result\":\"answer\"}\n"
    );
    let mut terminal = terminal_frame_offset(&binding, payload, 1, 256, 703, started_at, Some(0));
    terminal.relay_generation_mtime_ns = Some(generation);

    let outcome = sink
        .deliver(&terminal)
        .await
        .expect("landed unrecorded delivery");

    assert_eq!(outcome, RelaySinkOutcome::TerminalDelivered);
    crate::services::discord::inflight::clear_inflight_state(&ProviderKind::Claude, channel_id);
}

struct RelayContractFakeGateway {
    replace_outcome: ReplaceLongMessageOutcome,
    transport_error: Option<String>,
    sent_message_id: MessageId,
    replace_calls: AtomicU64,
    send_calls: AtomicU64,
    on_transport: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl RelayContractFakeGateway {
    fn edited() -> Self {
        Self {
            replace_outcome: ReplaceLongMessageOutcome::EditedOriginal,
            transport_error: None,
            sent_message_id: MessageId::new(91_001),
            replace_calls: AtomicU64::new(0),
            send_calls: AtomicU64::new(0),
            on_transport: None,
        }
    }

    fn failing(message: &str) -> Self {
        let mut gateway = Self::edited();
        gateway.transport_error = Some(message.to_string());
        gateway
    }
}

impl crate::services::discord::gateway::TurnGateway for RelayContractFakeGateway {
    fn send_message<'a>(
        &'a self,
        _channel_id: ChannelId,
        _content: &'a str,
    ) -> crate::services::discord::gateway::GatewayFuture<'a, Result<MessageId, String>> {
        Box::pin(async move {
            self.send_calls.fetch_add(1, Ordering::AcqRel);
            if let Some(on_transport) = &self.on_transport {
                on_transport();
            }
            match &self.transport_error {
                Some(error) => Err(error.clone()),
                None => Ok(self.sent_message_id),
            }
        })
    }

    fn edit_message<'a>(
        &'a self,
        _channel_id: ChannelId,
        _message_id: MessageId,
        _content: &'a str,
    ) -> crate::services::discord::gateway::GatewayFuture<'a, Result<(), String>> {
        panic!("relay contract fake does not use edit_message")
    }

    fn replace_message_with_outcome<'a>(
        &'a self,
        _channel_id: ChannelId,
        _message_id: MessageId,
        _content: &'a str,
    ) -> crate::services::discord::gateway::GatewayFuture<
        'a,
        Result<ReplaceLongMessageOutcome, String>,
    > {
        Box::pin(async move {
            self.replace_calls.fetch_add(1, Ordering::AcqRel);
            if let Some(on_transport) = &self.on_transport {
                on_transport();
            }
            match &self.transport_error {
                Some(error) => Err(error.clone()),
                None => Ok(self.replace_outcome.clone()),
            }
        })
    }

    fn schedule_retry_with_history<'a>(
        &'a self,
        _channel_id: ChannelId,
        _user_message_id: MessageId,
        _user_text: &'a str,
    ) -> crate::services::discord::gateway::GatewayFuture<'a, ()> {
        panic!("relay contract fake does not schedule retries")
    }

    fn dispatch_queued_turn<'a>(
        &'a self,
        _channel_id: ChannelId,
        _intervention: &'a crate::services::discord::Intervention,
        _output_path: &'a str,
        _skip_hook: bool,
        _dispatch_lease: Option<Arc<crate::services::turn_orchestrator::DispatchLease>>,
    ) -> crate::services::discord::gateway::GatewayFuture<'a, Result<(), String>> {
        panic!("relay contract fake does not dispatch turns")
    }

    fn validate_live_routing<'a>(
        &'a self,
        _channel_id: ChannelId,
    ) -> crate::services::discord::gateway::GatewayFuture<'a, Result<(), String>> {
        panic!("relay contract fake does not validate routing")
    }

    fn requester_mention(&self) -> Option<String> {
        None
    }

    fn can_chain_locally(&self) -> bool {
        false
    }

    fn bot_owner_provider(&self) -> Option<ProviderKind> {
        None
    }
}
