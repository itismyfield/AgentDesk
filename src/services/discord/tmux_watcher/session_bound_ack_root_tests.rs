use crate::services::discord::tmux::tmux_watcher::session_bound_ack::{
    SessionBoundRelayAckOutcome, wait_for_session_bound_relay_delivery_ack,
};
use crate::services::discord::tmux::tmux_watcher::supervisor_relay::SessionBoundRelayAckTarget;

#[tokio::test]
async fn fenced_zero_delivery_resolves_exact_not_delivered_without_timeout_4909() {
    use crate::services::cluster::session_matcher::{MatchedChannel, expected_rollout_path_for};
    use crate::services::cluster::stream_relay::{
        RelaySink, TerminalCommitFence, spawn_stream_relay,
    };
    use crate::services::discord::health::HealthRegistry;
    use crate::services::discord::session_relay_sink::{
        PURE_SUBAGENT_ZERO_DELIVERY_PAYLOAD, SessionBoundDiscordRelaySink,
    };
    use crate::services::provider::ProviderKind;
    use std::sync::Arc;

    let channel_id = "4909";
    let session = ProviderKind::Claude.build_tmux_session_name(channel_id);
    let binding = MatchedChannel {
        channel_id: channel_id.to_string(),
        agent_id: format!("agent-{channel_id}"),
        provider: ProviderKind::Claude,
        expected_session_name: session.clone(),
        expected_rollout_path: expected_rollout_path_for(&session),
    };
    let sink: Arc<dyn RelaySink> = Arc::new(SessionBoundDiscordRelaySink::new(Arc::new(
        HealthRegistry::new(),
    )));
    let handle = spawn_stream_relay(binding, sink);
    let producer = handle.producer();
    let sent = producer.try_send_terminal_frame_with_sequence(
        PURE_SUBAGENT_ZERO_DELIVERY_PAYLOAD.to_string(),
        TerminalCommitFence {
            consumed_end: 256,
            turn_user_msg_id: 49_090,
            turn_started_at: "2026-07-27T00:00:00Z".to_string(),
            turn_start_offset: Some(64),
        },
    );
    let sequence = sent.sequence.expect("terminal frame enqueued");
    let metrics = producer.metrics().clone();
    let target = SessionBoundRelayAckTarget {
        metrics: metrics.clone(),
        sequence,
        turn_start_offset: Some(64),
    };

    let ack = wait_for_session_bound_relay_delivery_ack(
        Some(&target),
        std::time::Duration::from_millis(250),
    )
    .await;

    assert_eq!(
        ack,
        SessionBoundRelayAckOutcome::NotDelivered,
        "the real parser and sink must resolve the exact ACK instead of timing out"
    );
    assert_ne!(ack, SessionBoundRelayAckOutcome::TimedOut);
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.last_terminal_skipped_sequence, Some(sequence));
    assert_eq!(snapshot.last_terminal_committed_sequence, None);
    assert_eq!(snapshot.last_sink_error_sequence, None);
    handle.shutdown().await;
}
