//! Regression coverage for terminal delivery epilogue routing.

use super::delivery_epilogue::*;
use super::*;

use std::{
    io::Write,
    sync::{Arc, Mutex},
};

use crate::services::discord::{formatting::ReplaceLongMessageOutcome, gateway::GatewayFuture};
use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone)]
struct CapturingWriter(Arc<Mutex<Vec<u8>>>);

impl Write for CapturingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("capturing writer lock")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CapturingWriter {
    type Writer = CapturingWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

struct NoopGateway;

impl TurnGateway for NoopGateway {
    fn send_message<'a>(
        &'a self,
        _channel_id: ChannelId,
        _content: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>> {
        panic!("delivery epilogue test must not send a message")
    }

    fn edit_message<'a>(
        &'a self,
        _channel_id: ChannelId,
        _message_id: MessageId,
        _content: &'a str,
    ) -> GatewayFuture<'a, Result<(), String>> {
        panic!("delivery epilogue test must not edit a message")
    }

    fn replace_message_with_outcome<'a>(
        &'a self,
        _channel_id: ChannelId,
        _message_id: MessageId,
        _content: &'a str,
    ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
        panic!("delivery epilogue test must not replace a message")
    }

    fn schedule_retry_with_history<'a>(
        &'a self,
        _channel_id: ChannelId,
        _user_message_id: MessageId,
        _user_text: &'a str,
    ) -> GatewayFuture<'a, ()> {
        panic!("delivery epilogue test must not schedule a retry")
    }

    fn dispatch_queued_turn<'a>(
        &'a self,
        _channel_id: ChannelId,
        _intervention: &'a Intervention,
        _request_owner_name: &'a str,
        _has_more_queued_turns: bool,
        _dispatch_lease: Option<Arc<crate::services::turn_orchestrator::DispatchLease>>,
    ) -> GatewayFuture<'a, Result<(), String>> {
        panic!("delivery epilogue test must not dispatch a queued turn")
    }

    fn validate_live_routing<'a>(
        &'a self,
        _channel_id: ChannelId,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn requester_mention(&self) -> Option<String> {
        None
    }

    fn can_chain_locally(&self) -> bool {
        false
    }

    fn bot_owner_provider(&self) -> Option<ProviderKind> {
        Some(ProviderKind::Claude)
    }
}

#[tokio::test]
async fn terminal_delivery_epilogue_routes_identity_mismatch_to_warn() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let temp = tempfile::TempDir::new().expect("runtime root");
    let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        temp.path(),
    );
    let channel_id = ChannelId::new(5_025);
    let current_msg_id = MessageId::new(5_026);
    let provider = ProviderKind::Claude;
    let mut stale = InflightTurnState::new(
        provider.clone(),
        channel_id.get(),
        Some("terminal-delivery-epilogue".to_string()),
        1,
        100,
        current_msg_id.get(),
        "stale turn".to_string(),
        None,
        None,
        None,
        None,
        0,
    );
    let newer = InflightTurnState::new(
        provider.clone(),
        channel_id.get(),
        Some("terminal-delivery-epilogue".to_string()),
        1,
        200,
        current_msg_id.get(),
        "newer turn".to_string(),
        None,
        None,
        None,
        None,
        0,
    );
    crate::services::discord::inflight::save_inflight_state(&newer).expect("seed newer owner");

    let shared = crate::services::discord::make_shared_data_for_tests();
    let gateway: Arc<dyn TurnGateway> = Arc::new(NoopGateway);
    let full_response = "delivered body".to_string();
    let delivery_response = full_response.clone();
    let spoken_delivery_response = full_response.clone();
    let adk_session_key = None;
    let adk_cwd = None;
    let dispatch_id = None;
    let turn_id = "terminal-delivery-epilogue-test".to_string();
    let user_text = "user prompt".to_string();
    let mut response_sent_offset = 0;
    let mut terminal_full_replay_cleanup_msg_ids = Vec::new();
    let mut bridge_should_emit_completion = false;
    let mut status_panel_terminal_committed = false;
    let mut busy_requeue_outcome = None;

    let buffer = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .without_time()
        .with_writer(CapturingWriter(buffer.clone()))
        .finish();
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);

    handle_delivery_epilogue(
        DeliveryEpilogueMessage::PostCommit,
        DeliveryEpilogueContext {
            shared_owned: &shared,
            gateway: &gateway,
            provider: &provider,
            channel_id,
            user_msg_id: None,
            current_msg_id,
            adk_session_key: &adk_session_key,
            adk_cwd: &adk_cwd,
            dispatch_id: &dispatch_id,
            turn_id: &turn_id,
            user_text_owned: &user_text,
            full_response: &full_response,
            delivery_response: &delivery_response,
            spoken_delivery_response: &spoken_delivery_response,
            cancelled: false,
            is_prompt_too_long: false,
            transport_error: false,
            recovery_retry: false,
            resume_failure_detected: false,
            claude_tui_followup_pre_submit_requeue_candidate: false,
            claude_tui_busy_requeue_pending: false,
            tui_error_classification: TuiErrorClassification::default(),
            #[cfg(unix)]
            bridge_tui_gate_outcome_early: Some(
                crate::services::discord::tmux::TuiCompletionGateOutcome::NotGated,
            ),
            terminal_delivery_committed: true,
            terminal_body_visible: true,
            preserve_inflight_for_cleanup_retry: false,
            should_complete_work_dispatch_after_delivery: false,
            should_fail_dispatch_after_delivery: false,
            bridge_relay_delegated_to_watcher: false,
            watcher_owner_channel_id: channel_id,
            can_chain_locally: false,
            inflight_generation: 0,
        },
        DeliveryEpilogueState {
            response_sent_offset: &mut response_sent_offset,
            inflight_state: &mut stale,
            terminal_full_replay_cleanup_msg_ids: &mut terminal_full_replay_cleanup_msg_ids,
            bridge_should_emit_completion: &mut bridge_should_emit_completion,
            status_panel_terminal_committed: &mut status_panel_terminal_committed,
            busy_requeue_outcome: &mut busy_requeue_outcome,
        },
    )
    .await;

    let logs = String::from_utf8(buffer.lock().expect("captured logs lock").clone())
        .expect("captured logs must be UTF-8");
    assert_eq!(response_sent_offset, full_response.len());
    assert!(
        logs.contains(
            "turn bridge delivered the terminal answer but could not mirror terminal_delivery_committed"
        ),
        "the production epilogue must route an identity-mismatch outcome to WARN; logs={logs}"
    );
}
