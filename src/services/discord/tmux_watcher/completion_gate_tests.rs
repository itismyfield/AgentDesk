use super::*;
use crate::services::discord::turn_finalizer::completion_signal::CompletionSignalMode;

fn state_for_matched_session(
    provider: ProviderKind,
    tmux_session_name: &str,
    output_path: &str,
) -> crate::services::discord::inflight::InflightTurnState {
    let mut state: crate::services::discord::inflight::InflightTurnState =
        serde_json::from_value(serde_json::json!({
            "version": 9,
            "provider": provider.as_str(),
            "channel_id": 42,
            "channel_name": "relay-test",
            "watcher_owner_channel_id": 42,
            "logical_channel_id": 42,
            "request_owner_user_id": 7,
            "user_msg_id": 9001,
            "finalizer_turn_id": 9001,
            "current_msg_id": 9002,
            "current_msg_len": 0,
            "user_text": "typed over ssh",
            "source": "text",
            "session_id": "session-1",
            "tmux_session_name": tmux_session_name,
            "output_path": output_path,
            "input_fifo_path": "/tmp/input.fifo",
            "runtime_kind": "legacy_tmux_wrapper",
            "last_offset": 0,
            "turn_start_offset": 0,
            "full_response": "",
            "response_sent_offset": 0,
            "started_at": "2026-07-02 00:00:00",
            "updated_at": "2026-07-02 00:00:00",
            "born_generation": 0
        }))
        .expect("deserialize test inflight");
    state.turn_source = crate::services::discord::inflight::TurnSource::ExternalInput;
    state
}

fn write_jsonl(lines: &[&str]) -> tempfile::NamedTempFile {
    let file = tempfile::NamedTempFile::new().expect("temp jsonl");
    std::fs::write(file.path(), lines.join("\n")).expect("write jsonl");
    file
}

fn write_jsonl_strings(lines: &[String]) -> tempfile::NamedTempFile {
    let file = tempfile::NamedTempFile::new().expect("temp jsonl");
    std::fs::write(file.path(), lines.join("\n")).expect("write jsonl");
    file
}

fn matched_gate_signal_for_file(
    provider: ProviderKind,
    runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind,
    file: &std::path::Path,
    fixture_name: &str,
) -> crate::services::discord::turn_finalizer::CompletionSignal {
    let tmux_session_name = format!("AgentDesk-{}-{fixture_name}", provider.as_str());
    let mut state = state_for_matched_session(
        provider.clone(),
        &tmux_session_name,
        &file.display().to_string(),
    );
    state.runtime_kind = Some(runtime_kind);
    matched_session_completion_signal(
        &provider,
        Some(&state),
        &tmux_session_name,
        CompletionSignalMode::GateQuiescence,
    )
    .expect("matched session should produce a signal")
}

#[test]
fn matched_session_terminal_jsonl_confirms_idle_without_turn_source_branch() {
    let file = write_jsonl(&[r#"{"type":"result","result":"done","session_id":"s"}"#]);
    let tmux_session_name = "AgentDesk-claude-relay-test";
    let state = state_for_matched_session(
        ProviderKind::Claude,
        tmux_session_name,
        &file.path().display().to_string(),
    );

    assert_eq!(
        matched_session_completion_signal(
            &ProviderKind::Claude,
            Some(&state),
            tmux_session_name,
            CompletionSignalMode::GateQuiescence,
        ),
        Some(crate::services::discord::turn_finalizer::CompletionSignal::Done)
    );
}

#[test]
fn turn_source_does_not_affect_jsonl_completion_probe() {
    let file = write_jsonl(&[r#"{"type":"result","result":"done","session_id":"s"}"#]);
    let tmux_session_name = "AgentDesk-claude-relay-test";
    let mut state = state_for_matched_session(
        ProviderKind::Claude,
        tmux_session_name,
        &file.path().display().to_string(),
    );
    state.turn_source = crate::services::discord::inflight::TurnSource::Managed;

    assert_eq!(
        matched_session_completion_signal(
            &ProviderKind::Claude,
            Some(&state),
            tmux_session_name,
            CompletionSignalMode::GateQuiescence,
        ),
        Some(crate::services::discord::turn_finalizer::CompletionSignal::Done)
    );
}

#[test]
fn gate_quiescence_preserves_pre_s2a_acceptance_matrix() {
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::discord::turn_finalizer::{
        CompletionSignal, completion_signal_from_transcript,
    };

    let cases = [
        (
            "claude_partial_tail_after_result",
            ProviderKind::Claude,
            RuntimeHandoffKind::ClaudeTui,
            &[
                r#"{"type":"result","result":"done","session_id":"s"}"#,
                r#"{"ty"#,
            ][..],
            true,
            false,
        ),
        (
            "claude_system_init",
            ProviderKind::Claude,
            RuntimeHandoffKind::ClaudeTui,
            &[r#"{"type":"system","subtype":"init","session_id":"s"}"#][..],
            true,
            false,
        ),
        (
            "codex_session_meta",
            ProviderKind::Codex,
            RuntimeHandoffKind::CodexTui,
            &[r#"{"type":"session_meta","payload":{"id":"s","cwd":"/repo"}}"#][..],
            true,
            false,
        ),
        (
            "codex_thread_started",
            ProviderKind::Codex,
            RuntimeHandoffKind::CodexTui,
            &[r#"{"type":"thread.started","thread_id":"t"}"#][..],
            true,
            false,
        ),
        (
            "codex_task_complete_payload",
            ProviderKind::Codex,
            RuntimeHandoffKind::CodexTui,
            &[r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#][..],
            true,
            false,
        ),
        (
            "codex_completed_agent_message",
            ProviderKind::Codex,
            RuntimeHandoffKind::CodexTui,
            &[r#"{"type":"item.completed","item":{"type":"agent_message","text":"done"}}"#][..],
            true,
            false,
        ),
        (
            "claude_result",
            ProviderKind::Claude,
            RuntimeHandoffKind::ClaudeTui,
            &[r#"{"type":"result","result":"done","session_id":"s"}"#][..],
            true,
            true,
        ),
        (
            "claude_turn_duration",
            ProviderKind::Claude,
            RuntimeHandoffKind::ClaudeTui,
            &[r#"{"type":"system","subtype":"turn_duration","session_id":"s"}"#][..],
            true,
            true,
        ),
        (
            "claude_stop_hook_summary",
            ProviderKind::Claude,
            RuntimeHandoffKind::ClaudeTui,
            &[r#"{"type":"system","subtype":"stop_hook_summary","session_id":"s"}"#][..],
            true,
            true,
        ),
        (
            "claude_interrupt_marker",
            ProviderKind::Claude,
            RuntimeHandoffKind::ClaudeTui,
            &[r#"{"type":"user","message":{"content":"[Request interrupted by user]"}}"#][..],
            true,
            true,
        ),
        (
            "codex_turn_completed",
            ProviderKind::Codex,
            RuntimeHandoffKind::CodexTui,
            &[r#"{"type":"turn.completed","usage":{"input_tokens":5,"output_tokens":3}}"#][..],
            true,
            true,
        ),
        (
            "claude_permission_mode_after_result",
            ProviderKind::Claude,
            RuntimeHandoffKind::ClaudeTui,
            &[
                r#"{"type":"result","result":"done","session_id":"s"}"#,
                r#"{"type":"permission-mode","mode":"bypassPermissions"}"#,
            ][..],
            false,
            true,
        ),
    ];

    for (name, provider, runtime_kind, lines, old_gate_confirms, finalize_confirms) in cases {
        let file = write_jsonl(lines);
        let gate_signal =
            matched_gate_signal_for_file(provider.clone(), runtime_kind, file.path(), name);
        let finalizer_signal =
            completion_signal_from_transcript(&provider, Some(runtime_kind), file.path());

        assert_eq!(
            gate_signal == CompletionSignal::Done,
            old_gate_confirms,
            "{name}: GateQuiescence drifted from pre-S2-a gate outcome"
        );
        assert_eq!(
            finalizer_signal == CompletionSignal::Done,
            finalize_confirms,
            "{name}: FinalizeAuthority drifted"
        );
    }
}

#[test]
fn gate_quiescence_keeps_old_64kb_tail_limit() {
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::discord::turn_finalizer::{
        CompletionSignal, completion_signal_from_transcript,
    };

    let mut lines = vec![r#"{"type":"result","result":"done","session_id":"s"}"#.to_string()];
    let filler = r#"{"type":"pr-link","url":"https://example.com/pr/very-long-path-padding-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#;
    let mut bytes = 0usize;
    while bytes < (64 * 1024) + 4096 {
        lines.push(filler.to_string());
        bytes += filler.len() + 1;
    }
    let file = write_jsonl_strings(&lines);

    let gate_signal = matched_gate_signal_for_file(
        ProviderKind::Claude,
        RuntimeHandoffKind::ClaudeTui,
        file.path(),
        "claude_terminator_beyond_64kb",
    );
    let finalizer_signal = completion_signal_from_transcript(
        &ProviderKind::Claude,
        Some(RuntimeHandoffKind::ClaudeTui),
        file.path(),
    );

    assert_eq!(
        gate_signal == CompletionSignal::Done,
        false,
        "GateQuiescence must keep the old 64KB gate limit and not confirm"
    );
    assert_eq!(
        finalizer_signal,
        CompletionSignal::Done,
        "FinalizeAuthority keeps the S2-a widened scan"
    );
}

#[test]
fn jsonl_terminal_completion_shortcut_uses_turn_shape_not_turn_source() {
    let mut state = state_for_matched_session(
        ProviderKind::Claude,
        "AgentDesk-claude-relay-test",
        "/tmp/unused.jsonl",
    );
    state.turn_source = crate::services::discord::inflight::TurnSource::Managed;
    state.current_msg_id = state.user_msg_id;
    state.status_message_id = None;
    assert!(jsonl_terminal_can_confirm_completion(Some(&state)));

    state.current_msg_id = state.user_msg_id + 1;
    assert!(!jsonl_terminal_can_confirm_completion(Some(&state)));

    state.current_msg_id = state.user_msg_id;
    state.rebind_origin = true;
    assert!(!jsonl_terminal_can_confirm_completion(Some(&state)));

    state.rebind_origin = false;
    state.tmux_session_name = None;
    assert!(!jsonl_terminal_can_confirm_completion(Some(&state)));
}

#[test]
fn jsonl_terminal_completion_accepts_session_bound_watcher_owned_placeholder() {
    let mut state = state_for_matched_session(
        ProviderKind::Claude,
        "AgentDesk-claude-watcher-owned",
        "/tmp/watcher-owned.jsonl",
    );
    state.current_msg_id = state.user_msg_id + 1;
    state.status_message_id = Some(state.current_msg_id + 1);
    state.set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::Watcher);

    assert!(
        jsonl_terminal_can_confirm_completion(Some(&state)),
        "session-bound watcher-owned terminal envelopes should finish cleanup even with a placeholder/status panel"
    );
}

#[test]
fn relay_ownership_only_turn_cannot_confirm_idle_via_tui_completion_gate() {
    let file = tempfile::NamedTempFile::new().expect("temp jsonl");
    std::fs::write(
        file.path(),
        r#"{"type":"result","result":"done","session_id":"s"}"#,
    )
    .expect("write jsonl");
    let tmux_session_name = "AgentDesk-claude-relay-only";
    let mut state = state_for_matched_session(
        ProviderKind::Claude,
        tmux_session_name,
        &file.path().display().to_string(),
    );
    state.channel_id = 4_018_001;
    state.current_msg_id = state.user_msg_id + 1;
    state.status_message_id = Some(state.current_msg_id + 1);
    state.set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::Watcher);
    state.relay_ownership_only = true;

    assert!(
        inflight_suppresses_tui_completion_lifecycle(Some(&state)),
        "relay-only synthetic rows must force the completion gate onto the suppressing path"
    );
    assert!(
        !jsonl_terminal_can_confirm_completion(Some(&state)),
        "relay-only synthetic rows must not use matched JSONL idle as terminal completion evidence"
    );
    assert!(
        !TuiCompletionGateOutcome::TimedOut.should_emit_completion(),
        "relay-only synthetic rows must suppress watcher/bridge visible completion"
    );
}

#[test]
fn jsonl_terminal_completion_accepts_watcher_owned_external_zero_message_claim() {
    let mut state = state_for_matched_session(
        ProviderKind::Claude,
        "AgentDesk-claude-watcher-external",
        "/tmp/watcher-external.jsonl",
    );
    state.user_msg_id = 0;
    state.current_msg_id = 0;
    state.rebind_origin = false;
    state.set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::Watcher);

    assert!(
        jsonl_terminal_can_confirm_completion(Some(&state)),
        "watcher-owned external pane claims should not need rebind_origin to finish cleanup"
    );
}

#[test]
fn jsonl_terminal_completion_accepts_managed_claude_tui_bridge_owned_placeholder() {
    let mut state = state_for_matched_session(
        ProviderKind::Claude,
        "AgentDesk-claude-bridge-owned",
        "/tmp/bridge-owned.jsonl",
    );
    state.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
    state.current_msg_id = state.user_msg_id + 1;
    state.status_message_id = Some(state.current_msg_id + 1);
    state.turn_start_offset = Some(10);
    state.last_offset = 42;

    assert!(
        jsonl_terminal_can_confirm_completion(Some(&state)),
        "matched ClaudeTui terminal JSONL should release bridge-owned placeholders instead of waiting forever on pane prompt detection"
    );

    state.set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::StandbyRelay);
    assert!(
        jsonl_terminal_can_confirm_completion(Some(&state)),
        "managed ClaudeTui terminal JSONL remains authoritative even if a relay-owner label is stale"
    );

    state.set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::None);
    state.turn_start_offset = Some(42);
    state.last_offset = 42;
    assert!(
        !jsonl_terminal_can_confirm_completion(Some(&state)),
        "a stale prior terminal envelope must not unlock a fresh turn that has not advanced the output offset"
    );

    state.turn_start_offset = None;
    state.last_offset = 99;
    state.full_response = "prior response".to_string();
    assert!(
        !jsonl_terminal_can_confirm_completion(Some(&state)),
        "without a current turn_start_offset anchor, non-empty full_response is not enough to unlock cleanup"
    );
}

#[test]
fn jsonl_terminal_completion_accepts_managed_process_backend_bridge_owned_placeholder() {
    let mut state = state_for_matched_session(
        ProviderKind::Codex,
        "AgentDesk-codex-process-backend",
        "/tmp/process-backend.jsonl",
    );
    state.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ProcessBackend);
    state.current_msg_id = state.user_msg_id + 1;
    state.status_message_id = Some(state.current_msg_id + 1);
    state.turn_start_offset = Some(100);
    state.last_offset = 150;

    assert!(
        jsonl_terminal_can_confirm_completion(Some(&state)),
        "process backend terminal JSONL should also release bridge-owned live placeholders"
    );
}

#[test]
fn jsonl_terminal_completion_rejects_unanchored_managed_runtime_shapes() {
    let mut state = state_for_matched_session(
        ProviderKind::Claude,
        "AgentDesk-claude-guarded",
        "/tmp/guarded.jsonl",
    );
    state.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
    state.current_msg_id = state.user_msg_id + 1;
    state.turn_start_offset = Some(1);
    state.last_offset = 2;
    assert!(jsonl_terminal_can_confirm_completion(Some(&state)));

    state.runtime_kind = None;
    assert!(!jsonl_terminal_can_confirm_completion(Some(&state)));

    state.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
    state.user_msg_id = 0;
    assert!(!jsonl_terminal_can_confirm_completion(Some(&state)));

    state.user_msg_id = 9001;
    state.current_msg_id = 0;
    assert!(!jsonl_terminal_can_confirm_completion(Some(&state)));

    state.current_msg_id = 9002;
    state.rebind_origin = true;
    assert!(!jsonl_terminal_can_confirm_completion(Some(&state)));
}

#[test]
fn jsonl_terminal_completion_accepts_monitor_auto_turn_shape() {
    let mut state = state_for_matched_session(
        ProviderKind::Claude,
        "AgentDesk-claude-monitor-relay",
        "/tmp/monitor-auto-turn.jsonl",
    );
    state.turn_source = crate::services::discord::inflight::TurnSource::MonitorTriggered;
    state.rebind_origin = true;
    state.user_msg_id = 0;
    state.current_msg_id = 0;
    state.status_message_id = None;

    assert!(jsonl_terminal_can_confirm_completion(Some(&state)));
}

#[test]
fn jsonl_terminal_completion_accepts_external_adopted_shape_without_turn_source_branch() {
    let mut state = state_for_matched_session(
        ProviderKind::Claude,
        "AgentDesk-claude-external-adopted",
        "/tmp/external-adopted.jsonl",
    );
    state.turn_source = crate::services::discord::inflight::TurnSource::ExternalAdopted;
    state.rebind_origin = true;
    state.user_msg_id = 0;
    state.current_msg_id = 0;
    state.status_message_id = None;
    assert!(jsonl_terminal_can_confirm_completion(Some(&state)));

    state.turn_source = crate::services::discord::inflight::TurnSource::Managed;
    assert!(
        jsonl_terminal_can_confirm_completion(Some(&state)),
        "completion eligibility is defined by the session-bound inflight shape, not turn_source"
    );
}

#[test]
fn session_bound_terminal_delivery_delegation_uses_inflight_shape() {
    let tmux_session_name = "AgentDesk-claude-session-bound";
    let mut state =
        state_for_matched_session(ProviderKind::Claude, tmux_session_name, "/tmp/out.jsonl");
    state.rebind_origin = true;
    state.user_msg_id = 0;
    state.current_msg_id = 0;

    assert!(session_bound_relay_should_own_terminal_delivery(
        true,
        true,
        true,
        Some(tmux_session_name),
        Some(&state),
        tmux_session_name,
    ));
    assert!(!session_bound_relay_should_own_terminal_delivery(
        false,
        true,
        true,
        Some(tmux_session_name),
        Some(&state),
        tmux_session_name,
    ));
    assert!(!session_bound_relay_should_own_terminal_delivery(
        true,
        false,
        true,
        Some(tmux_session_name),
        Some(&state),
        tmux_session_name,
    ));
    assert!(!session_bound_relay_should_own_terminal_delivery(
        true,
        true,
        false,
        Some(tmux_session_name),
        Some(&state),
        tmux_session_name,
    ));
    assert!(!session_bound_relay_should_own_terminal_delivery(
        true,
        true,
        true,
        Some("AgentDesk-claude-other"),
        Some(&state),
        tmux_session_name,
    ));
    assert!(
        session_bound_relay_should_own_terminal_delivery(
            true,
            true,
            true,
            Some(tmux_session_name),
            None,
            tmux_session_name,
        ),
        "matched session binding is enough for session relay ownership; inflight only selects edit metadata"
    );

    state.rebind_origin = false;
    state.user_msg_id = 9001;
    state.current_msg_id = 9001;
    assert!(
        !session_bound_relay_should_own_terminal_delivery(
            true,
            true,
            true,
            Some(tmux_session_name),
            Some(&state),
            tmux_session_name,
        ),
        "bridge-owned inflight remains on legacy/bridge delivery instead of the session relay sink"
    );
}

#[test]
fn post_terminal_jsonl_payload_allows_external_init_without_user_event() {
    let payload = concat!(
        "{\"type\":\"system\",\"subtype\":\"init\",\"tools\":[\"ScheduleWakeup\"]}\n",
        "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"[E2E:E13:WAKE]\"}]}}\n",
        "{\"type\":\"result\",\"result\":\"[E2E:E13:WAKE]\"}\n"
    );
    assert!(post_terminal_jsonl_payload_contains_init_without_user_event(payload.as_bytes()));
}

#[test]
fn post_terminal_jsonl_payload_rejects_active_tool_result() {
    let payload = concat!(
        "{\"type\":\"system\",\"subtype\":\"init\",\"tools\":[\"ScheduleWakeup\"]}\n",
        "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"ScheduleWakeup\"}]}}\n",
        "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":\"scheduled\"}]}}\n",
        "{\"type\":\"result\",\"result\":\"setup complete\"}\n"
    );
    assert!(!post_terminal_jsonl_payload_contains_init_without_user_event(payload.as_bytes()));
}

#[tokio::test]
async fn session_bound_relay_ack_success_commits_and_failure_outcomes_do_not() {
    let metrics =
        std::sync::Arc::new(crate::services::cluster::stream_relay::RelayMetrics::default());
    let target = SessionBoundRelayAckTarget {
        metrics: metrics.clone(),
        sequence: 7,
        turn_start_offset: None,
    };
    assert_eq!(
        wait_for_session_bound_relay_delivery_ack(
            Some(&target),
            std::time::Duration::from_millis(1),
        )
        .await,
        SessionBoundRelayAckOutcome::TimedOut
    );

    metrics.record_sink_error_sequence_for_test(7);
    assert_eq!(
        session_bound_relay_ack_snapshot_outcome(Some(&target)),
        Some(SessionBoundRelayAckOutcome::SinkError)
    );

    let dropped_metrics =
        std::sync::Arc::new(crate::services::cluster::stream_relay::RelayMetrics::default());
    let dropped_target = SessionBoundRelayAckTarget {
        metrics: dropped_metrics.clone(),
        sequence: 9,
        turn_start_offset: None,
    };
    dropped_metrics.record_dropped_sequence_for_test(9);
    assert_eq!(
        session_bound_relay_ack_snapshot_outcome(Some(&dropped_target)),
        Some(SessionBoundRelayAckOutcome::Dropped)
    );

    let skipped_metrics =
        std::sync::Arc::new(crate::services::cluster::stream_relay::RelayMetrics::default());
    let skipped_target = SessionBoundRelayAckTarget {
        metrics: skipped_metrics.clone(),
        sequence: 11,
        turn_start_offset: None,
    };
    skipped_metrics.record_terminal_skipped_sequence_for_test(11);
    assert_eq!(
        session_bound_relay_ack_snapshot_outcome(Some(&skipped_target)),
        Some(SessionBoundRelayAckOutcome::NotDelivered)
    );

    let delivered_metrics =
        std::sync::Arc::new(crate::services::cluster::stream_relay::RelayMetrics::default());
    let delivered_target = SessionBoundRelayAckTarget {
        metrics: delivered_metrics.clone(),
        sequence: 3,
        turn_start_offset: None,
    };
    delivered_metrics.record_delivered_sequence_for_test(3);
    assert_eq!(
        wait_for_session_bound_relay_delivery_ack(
            Some(&delivered_target),
            std::time::Duration::from_millis(1),
        )
        .await,
        SessionBoundRelayAckOutcome::TimedOut,
        "frame delivery ack alone must not count as terminal Discord commit"
    );
    delivered_metrics.record_terminal_committed_sequence_for_test(3);
    delivered_metrics.record_sink_error_sequence_for_test(4);
    assert_eq!(
        wait_for_session_bound_relay_delivery_ack(
            Some(&delivered_target),
            std::time::Duration::from_millis(1),
        )
        .await,
        SessionBoundRelayAckOutcome::Delivered
    );
    assert_eq!(
        wait_for_session_bound_relay_delivery_ack(None, std::time::Duration::from_millis(1)).await,
        SessionBoundRelayAckOutcome::MissingTarget
    );
}

// #3041 P1-3 R5 (per-sequence terminal-ACK correlation): turn A (seq N) was
// NOT delivered, turn B's tail (seq N+1) delivered in the same chunk. A's ACK
// must resolve on A's OWN sequence → NotDelivered (so the watcher reconciles /
// SendFull → A delivered, no black-hole), NOT Delivered from B bumping the
// committed high-water-mark to N+1. B's ACK at its own sequence → Delivered.
#[test]
fn session_bound_relay_ack_is_per_sequence_not_high_water_mark() {
    let metrics =
        std::sync::Arc::new(crate::services::cluster::stream_relay::RelayMetrics::default());
    // A at seq N=5 skipped, B at seq N+1=6 committed (higher → bumps HWM).
    metrics.record_terminal_skipped_sequence_for_test(5);
    metrics.record_terminal_committed_sequence_for_test(6);

    let a_target = SessionBoundRelayAckTarget {
        metrics: metrics.clone(),
        sequence: 5,
        turn_start_offset: None,
    };
    assert_eq!(
        session_bound_relay_ack_snapshot_outcome(Some(&a_target)),
        Some(SessionBoundRelayAckOutcome::NotDelivered),
        "A's ACK reads A's own seq-5 outcome (NotDelivered), not B's delivered HWM"
    );

    let b_target = SessionBoundRelayAckTarget {
        metrics: metrics.clone(),
        sequence: 6,
        turn_start_offset: None,
    };
    assert_eq!(
        session_bound_relay_ack_snapshot_outcome(Some(&b_target)),
        Some(SessionBoundRelayAckOutcome::Delivered),
        "B's ACK reads its own seq-6 committed outcome"
    );
}

// #3041 P1-3 R6: a single physical chunk holds `result(A) + assistant(B) +
// result(B)`. Turn A rides a TERMINAL frame (ack = A.seq, bound to A's pinned
// `turn_start_offset`); turn B completes ENTIRELY inside the split tail whose
// non-terminal frame sequence is DISCARDED. On B's processing pass the deferred
// forward emits NO frame (the leftover decoded text is empty) → NO fresh ack.
// B must NOT inherit A's stale ack: with B's own pinned identity, the carry
// helper resets the stored ack to `None` so B reconciles (None → MissingTarget
// → §3.2 committed-offset reconcile → SendFull/Skip) and is NEVER black-holed,
// even though A reported Delivered on A's own sequence.
#[test]
fn new_turn_does_not_inherit_finished_turn_stale_ack_target() {
    let metrics =
        std::sync::Arc::new(crate::services::cluster::stream_relay::RelayMetrics::default());
    // A's terminal frame (seq 5) bound to A's pinned turn_start_offset = 100.
    let a_ack = SessionBoundRelayAckTarget {
        metrics: metrics.clone(),
        sequence: 5,
        turn_start_offset: Some(100),
    };

    // B's processing pass: pinned identity is B's turn (turn_start_offset = 240,
    // the leftover/result(B) range), and the deferred forward produced NO fresh
    // ack (B's tail frame sequence was discarded). The stored ack still holds
    // A's seq-5 target. Carrying it forward for B must RESET to None.
    let carried = carry_session_bound_ack_for_turn(Some(a_ack.clone()), None, Some(240));
    assert!(
        carried.is_none(),
        "a NEW turn (different pinned turn_start_offset) with no fresh ack must \
             NOT inherit the finished turn's stale ack_target → reconcile, no black-hole"
    );

    // Sanity: the same turn (A's own later non-terminal pass) keeps A's ack so a
    // legitimately-set terminal ack is not clobbered within the SAME turn.
    let same_turn = carry_session_bound_ack_for_turn(Some(a_ack.clone()), None, Some(100));
    assert_eq!(
        same_turn.map(|ack| ack.sequence),
        Some(5),
        "an ack set earlier in THIS turn survives a later non-terminal pass"
    );
}

// #3041 P1-3 (codex P1-3 R7): the forward of a result-bearing physical chunk that
// ALSO carries a trailing later-turn tail MUST surface the turn-boundary signal
// (`trailing_turn_follows = true`) while still keeping the TERMINAL frame's ack as
// the wait target. A single-turn forward (no tail) must NOT raise the signal. This
// is the primitive the watcher latches to reset the stored ack at the boundary.
#[tokio::test]
async fn split_terminal_forward_signals_trailing_turn_and_keeps_terminal_ack() {
    use crate::services::cluster::session_matcher::MatchedChannel;
    use crate::services::cluster::session_matcher::expected_rollout_path_for;
    use crate::services::cluster::stream_relay::{
        DiscardSink, RelaySink, TerminalCommitFence, spawn_stream_relay,
    };
    use crate::services::provider::ProviderKind;

    let session = ProviderKind::Claude.build_tmux_session_name("c-r7-split");
    let matched = MatchedChannel {
        channel_id: "c-r7-split".to_string(),
        agent_id: "a-r7-split".to_string(),
        provider: ProviderKind::Claude,
        expected_session_name: session.clone(),
        expected_rollout_path: expected_rollout_path_for(&session),
    };
    let registry = std::sync::Arc::new(
        crate::services::cluster::relay_producer_registry::RelayProducerRegistry::new(),
    );
    let sink: std::sync::Arc<dyn RelaySink> = std::sync::Arc::new(DiscardSink);
    let handle = spawn_stream_relay(matched.clone(), sink);
    registry.register(session.clone(), handle.producer());
    let mut cached = None;

    // Turn A's result + turn B's first bytes in ONE physical chunk. After the
    // parse, `all_data` holds turn B's bytes → leftover_len = turn_b.len().
    let turn_a = "{\"type\":\"result\",\"result\":\"A done\"}\n";
    let turn_b = "{\"type\":\"assistant\",\"message\":{\"content\":[]}}\n";
    let combined = format!("{turn_a}{turn_b}");
    let fence = TerminalCommitFence {
        consumed_end: 240,
        turn_user_msg_id: 0,
        turn_started_at: "12:00:00".to_string(),
        // A's pinned identity — the SAME offset the watcher carry helper keys on.
        turn_start_offset: Some(100),
    };
    let split_forward = forward_terminal_chunk_with_trailing_to_supervisor_relay(
        &session,
        &combined,
        turn_b.len(),
        &registry,
        &mut cached,
        fence.clone(),
    );
    assert!(
        split_forward.trailing_turn_follows,
        "a result+next-turn split must signal that a later turn follows (R7 \
             turn-boundary)"
    );
    assert!(
        split_forward.ack_target.is_some(),
        "the split still waits on the TERMINAL frame's ack (turn A's delivery)"
    );
    assert_eq!(
        split_forward
            .ack_target
            .as_ref()
            .and_then(|ack| ack.turn_start_offset),
        Some(100),
        "the kept ack is bound to turn A's pinned offset"
    );
    let terminal_sequence = split_forward
        .ack_target
        .as_ref()
        .map(|ack| ack.sequence)
        .expect("split terminal forward has terminal sequence");
    assert_eq!(
        split_forward.first_forwarded_sequence,
        Some(terminal_sequence),
        "the current turn keeps the terminal frame as its first forwarded sequence"
    );
    assert_eq!(
        split_forward.trailing_first_forwarded_sequence,
        Some(terminal_sequence + 1),
        "the later turn tail carries its own first forwarded sequence"
    );

    // A single complete turn (no trailing tail) must NOT raise the signal.
    let single_forward = forward_terminal_chunk_with_trailing_to_supervisor_relay(
        &session,
        turn_a,
        0,
        &registry,
        &mut cached,
        fence,
    );
    assert!(
        !single_forward.trailing_turn_follows,
        "a single-turn terminal forward never crosses a turn boundary"
    );
    registry.deregister(&session);
    let _ = handle;
}

// #3041 P1-3 (codex P1-3 R7): END-TO-END boundary semantics. The R6 carry helper
// ALONE still black-holes turn B when `turn_identity_for_panel` is STILL pinned to
// A's offset on B's pass (B's inflight not yet established): the carry KEEPS A's
// ack, and A's `Delivered` falsely satisfies B's ACK. R7 closes this by RESETTING
// the stored ack to `None` at A's split boundary AFTER A consumes its own ack —
// independent of whether the pinned identity refreshed. This test models that exact
// sequence: A's split signals the boundary → reset → B (even with A's stale pinned
// offset) starts with NO inherited ack → MissingTarget → §3.2 reconcile → B NOT
// black-holed even though A reported Delivered and B's tail was skipped/dropped.
#[test]
fn split_boundary_reset_prevents_later_turn_inheriting_finished_turn_ack() {
    let metrics =
        std::sync::Arc::new(crate::services::cluster::stream_relay::RelayMetrics::default());
    // A's terminal frame ack (seq 5), pinned to A's turn_start_offset = 100.
    let a_ack = SessionBoundRelayAckTarget {
        metrics: metrics.clone(),
        sequence: 5,
        turn_start_offset: Some(100),
    };

    // A's pass forwards the split (result(A) + tail(B)) → the carry helper adopts
    // A's fresh terminal ack (A's own delivery resolves correctly on A's ack).
    let mut stored = carry_session_bound_ack_for_turn(None, Some(a_ack.clone()), Some(100));
    assert_eq!(
        stored.as_ref().map(|ack| ack.sequence),
        Some(5),
        "A's own delivery still uses A's ack (no spurious reset mid-A)"
    );

    // The split signalled a trailing turn. AFTER A's terminal block consumes A's
    // ack, the watcher resets the stored ack at the boundary (the R7 fix).
    let split_trailing_turn_follows = true;
    if split_trailing_turn_follows {
        stored = None;
    }

    // B's pass: `turn_identity_for_panel` is STILL pinned to A's offset (100) —
    // the exact R7 condition the R6 carry helper could NOT fix. B's deferred
    // forward produced NO fresh ack (B's tail was already mirrored, or skipped /
    // dropped on the failure path). WITHOUT the reset, the carry helper would KEEP
    // A's seq-5 ack here (same pinned offset) → black-hole. WITH the reset, the
    // stored ack is already `None`, so B reconciles.
    let carried_for_b = carry_session_bound_ack_for_turn(stored, None, Some(100));
    assert!(
        carried_for_b.is_none(),
        "after A's split boundary reset, turn B NEVER inherits A's finished ack — \
             even with `turn_identity_for_panel` STILL pinned to A's offset → B reads \
             MissingTarget → §3.2 reconcile (SendFull/Skip) → B not black-holed"
    );

    // Contrast: WITHOUT the boundary reset (pre-R7), the very same B pass with A's
    // stale pinned offset would KEEP A's ack — the regression R7 fixes.
    let without_reset = carry_session_bound_ack_for_turn(Some(a_ack.clone()), None, Some(100));
    assert_eq!(
        without_reset.map(|ack| ack.sequence),
        Some(5),
        "documents the R7 regression the boundary reset closes: the carry helper \
             alone keeps A's ack when the pinned offset is still A's"
    );
}

// #3041 P1-3 R6 (turn-boundary): a fresh turn with no terminal ack does not
// inherit the prior turn's ack_target, and a fresh `Some` always wins (the
// current turn's terminal frame ack replaces any stored value).
#[test]
fn carry_session_bound_ack_for_turn_is_turn_scoped() {
    let metrics =
        std::sync::Arc::new(crate::services::cluster::stream_relay::RelayMetrics::default());
    let prior = SessionBoundRelayAckTarget {
        metrics: metrics.clone(),
        sequence: 11,
        turn_start_offset: Some(50),
    };
    let fresh = SessionBoundRelayAckTarget {
        metrics: metrics.clone(),
        sequence: 12,
        turn_start_offset: Some(80),
    };

    // Fresh Some always adopted (THIS turn forwarded a real terminal frame).
    assert_eq!(
        carry_session_bound_ack_for_turn(Some(prior.clone()), Some(fresh.clone()), Some(80))
            .map(|ack| ack.sequence),
        Some(12),
        "a fresh terminal-frame ack for the current turn replaces the stored value"
    );

    // No fresh ack + different turn → reset (never inherit).
    assert!(
        carry_session_bound_ack_for_turn(Some(prior.clone()), None, Some(80)).is_none(),
        "a fresh turn with no terminal ack does not inherit the prior turn's ack_target"
    );

    // No fresh ack + unknown current turn binding → reset (defensive: never
    // reuse an ack we cannot prove belongs to the current turn).
    assert!(
        carry_session_bound_ack_for_turn(Some(prior.clone()), None, None).is_none(),
        "an ack is not reused when the current turn's pinned offset is unknown"
    );

    // No fresh ack + stored ack with no turn binding → reset.
    let unbound = SessionBoundRelayAckTarget {
        metrics: metrics.clone(),
        sequence: 13,
        turn_start_offset: None,
    };
    assert!(
        carry_session_bound_ack_for_turn(Some(unbound), None, Some(50)).is_none(),
        "an ack lacking a turn binding is never carried across a pass"
    );

    // Nothing stored, nothing fresh → None.
    assert!(carry_session_bound_ack_for_turn(None, None, Some(50)).is_none());
}

// #3041 P1-3 R5: a target sequence that was never terminally resolved (dropped
// before the sink, or evicted from the bounded ring) reads None → the snapshot
// outcome is None → the wait times out → the watcher reconciles (no false ACK,
// no black-hole).
#[tokio::test]
async fn session_bound_relay_ack_unresolved_sequence_times_out() {
    let metrics =
        std::sync::Arc::new(crate::services::cluster::stream_relay::RelayMetrics::default());
    // A different sequence committed; OUR target (42) was never resolved.
    metrics.record_terminal_committed_sequence_for_test(40);
    let target = SessionBoundRelayAckTarget {
        metrics: metrics.clone(),
        sequence: 42,
        turn_start_offset: None,
    };
    assert_eq!(
        session_bound_relay_ack_snapshot_outcome(Some(&target)),
        None
    );
    assert_eq!(
        wait_for_session_bound_relay_delivery_ack(
            Some(&target),
            std::time::Duration::from_millis(1),
        )
        .await,
        SessionBoundRelayAckOutcome::TimedOut,
        "unresolved/evicted target sequence times out → reconcile, never a false ACK"
    );
}

#[test]
fn session_bound_direct_fallback_selects_full_provider_body_over_tail_suffix() {
    let body = format!(
        "[E2E:E15:BEGIN]\n{}\n[E2E:E15:MID]\n{}\n[E2E:E15:END]",
        (1..=149)
            .map(|n| format!("E15-LINE-{n:03}\n"))
            .collect::<String>(),
        (150..=160)
            .map(|n| format!("E15-LINE-{n:03}\n"))
            .collect::<String>()
    );
    let tail_offset = body.find("E15-LINE-150").expect("tail marker");

    let ordinary_watcher_suffix =
        watcher_terminal_response_for_direct_send(&body, tail_offset, false);
    assert!(!ordinary_watcher_suffix.contains("[E2E:E15:BEGIN]"));
    assert!(ordinary_watcher_suffix.contains("E15-LINE-150"));
    assert!(ordinary_watcher_suffix.contains("[E2E:E15:END]"));

    let session_bound_fallback =
        watcher_terminal_response_for_direct_send(&body, tail_offset, true);
    assert!(session_bound_fallback.contains("[E2E:E15:BEGIN]"));
    assert!(session_bound_fallback.contains("[E2E:E15:MID]"));
    assert!(session_bound_fallback.contains("E15-LINE-150"));
    assert!(session_bound_fallback.contains("[E2E:E15:END]"));
    assert_eq!(session_bound_fallback, body);
}

#[test]
fn session_bound_full_body_fallback_uses_ordered_chunks_for_long_placeholder_response() {
    let body = format!(
        "[E2E:E15:BEGIN]\n{}\n[E2E:E15:MID]\n{}\n[E2E:E15:END]",
        "E15-LINE-010\n".repeat(90),
        "E15-LINE-150\n".repeat(90)
    );

    assert!(body.len() > crate::services::discord::DISCORD_MSG_LIMIT);
    assert!(watcher_should_send_ordered_new_chunks_for_terminal_fallback(true, &body));
    assert!(!watcher_should_send_ordered_new_chunks_for_terminal_fallback(false, &body));
    assert!(
        !watcher_should_send_ordered_new_chunks_for_terminal_fallback(
            true,
            "E15-LINE-150\n[E2E:E15:END]"
        )
    );
}

#[test]
fn frame_accepted_without_terminal_commit_uses_watcher_direct_fallback() {
    // Owner present: a non-Delivered ACK keeps the watcher-direct fallback.
    assert!(watcher_should_direct_send_after_session_bound_ack(
        true,
        SessionBoundRelayAckOutcome::TimedOut,
        true
    ));
    assert!(watcher_should_direct_send_after_session_bound_ack(
        true,
        SessionBoundRelayAckOutcome::NotDelivered,
        true
    ));
    assert!(watcher_should_direct_send_after_session_bound_ack(
        true,
        SessionBoundRelayAckOutcome::MissingTarget,
        true
    ));
    assert!(!watcher_should_direct_send_after_session_bound_ack(
        true,
        SessionBoundRelayAckOutcome::Delivered,
        true
    ));
    // #3041 P1-5: an ownerless `TimedOut` now ALSO returns true (intends a
    // re-send) — the gate is the precondition only; the §3.2 committed-offset
    // reconciliation at the call site decides Skip vs Full. (Previously #3042
    // blanket-suppressed this to false.)
    assert!(watcher_should_direct_send_after_session_bound_ack(
        true,
        SessionBoundRelayAckOutcome::TimedOut,
        false
    ));
    // should_direct_send=false still gates the precondition off regardless of
    // owner presence.
    assert!(!watcher_should_direct_send_after_session_bound_ack(
        false,
        SessionBoundRelayAckOutcome::TimedOut,
        true
    ));
}

/// #3041 P1-5 (was `ownerless_timeout_suppresses_watcher_direct_fallback`,
/// #3042): after a restart `restore_inflight` can leave the channel with
/// `relay_owner_kind=none`/`inflight_present=false`, so the session-bound
/// terminal-commit ACK never lands and every 10s poll reports `TimedOut`.
/// #3042 blanket-suppressed the gate to `false` there to avoid a 3× duplicate;
/// #3041 P1-5 REMOVES that band-aid because P1-3 Part (a) made the committed
/// offset authoritative on a confirmed post. The gate now returns `true` (intends
/// a re-send) for an ownerless `TimedOut` — JUST the precondition — and the §3.2
/// committed-offset reconciliation (`watcher_terminal_resend_action`) decides
/// Skip-vs-Full downstream. The actual no-duplicate / no-black-hole guarantees
/// are asserted by `ownerless_timed_out_reconciles_*` below.
#[test]
fn ownerless_timed_out_intends_resend_via_gate() {
    // The exact incident shape: should_direct_send=true, TimedOut, no owner.
    // Now PASSES the gate (was suppressed to false by #3042); §3.2 then
    // reconciles (see `ownerless_timed_out_reconciles_*`).
    assert!(watcher_should_direct_send_after_session_bound_ack(
        true,
        SessionBoundRelayAckOutcome::TimedOut,
        false
    ));
    // Owner present with the same TimedOut also intends the fallback —
    // universality: the gate no longer branches on owner presence.
    assert!(watcher_should_direct_send_after_session_bound_ack(
        true,
        SessionBoundRelayAckOutcome::TimedOut,
        true
    ));
    // Ownerless but a non-timeout (definitive) outcome still intends the fallback.
    assert!(watcher_should_direct_send_after_session_bound_ack(
        true,
        SessionBoundRelayAckOutcome::SinkError,
        false
    ));
    assert!(watcher_should_direct_send_after_session_bound_ack(
        true,
        SessionBoundRelayAckOutcome::NotDelivered,
        false
    ));
    // should_direct_send=false still gates the precondition off.
    assert!(!watcher_should_direct_send_after_session_bound_ack(
        false,
        SessionBoundRelayAckOutcome::TimedOut,
        false
    ));
}

/// #3041 P1-5 / #3042 REGRESSION GUARD: an ownerless (`relay_owner_present=false`)
/// `TimedOut` whose range is ALREADY committed at/past `end` (the sink posted and
/// P1-3 advanced `confirmed_end_offset`) reconciles to `SkipAlreadyCommitted` —
/// NO re-send. This is the principled replacement for #3042's blanket suppression:
/// the observed 3× duplicate is still prevented, but now via the committed-offset
/// authority rather than a blind owner-scoped mute (so a genuine non-delivery is
/// no longer black-holed — see `ownerless_timed_out_reconciles_full_when_not_committed`).
#[test]
fn ownerless_timed_out_reconciles_skip_when_committed_reaches_end() {
    // Ownerless TimedOut now passes the precondition gate (no longer suppressed).
    assert!(watcher_should_direct_send_after_session_bound_ack(
        true,
        SessionBoundRelayAckOutcome::TimedOut,
        false
    ));
    // The §3.2 reconciliation against the offset authority: committed has reached
    // (or passed) the consumed-terminal `end` → the sink delivered, ACK merely
    // lagged → SKIP. No duplicate (the #3042 3× incident cannot recur).
    let (start, end) = (1_000u64, 1_500u64);
    assert_eq!(
        watcher_terminal_resend_action(end, start, end),
        WatcherTerminalResendAction::SkipAlreadyCommitted,
    );
    assert_eq!(
        watcher_terminal_resend_action(end + 256, start, end),
        WatcherTerminalResendAction::SkipAlreadyCommitted,
    );
}

/// #3041 P1-5 (black-hole closed): an ownerless `TimedOut` whose range is NOT
/// committed (committed < end → the sink did NOT confirm a post) reconciles to
/// `SendFull` — the bytes are recovered. Under the old #3042 blanket suppression
/// this outcome neither reconciled nor resent: a potential black-hole.
#[test]
fn ownerless_timed_out_reconciles_full_when_not_committed() {
    // Same ownerless TimedOut precondition.
    assert!(watcher_should_direct_send_after_session_bound_ack(
        true,
        SessionBoundRelayAckOutcome::TimedOut,
        false
    ));
    // committed < end → genuinely undelivered → re-send the FULL response.
    let (start, end) = (1_000u64, 1_500u64);
    assert_eq!(
        watcher_terminal_resend_action(start, start, end),
        WatcherTerminalResendAction::SendFull,
    );
    // committed at/below start (the all-or-nothing sink delegation's not-delivered
    // shape) also re-sends.
    assert_eq!(
        watcher_terminal_resend_action(0, start, end),
        WatcherTerminalResendAction::SendFull,
    );
}

#[test]
fn session_sink_route_skip_uses_watcher_direct_fallback() {
    assert!(watcher_should_direct_send_after_session_bound_ack(
        true,
        SessionBoundRelayAckOutcome::SinkError,
        true
    ));
    assert!(watcher_should_direct_send_after_session_bound_ack(
        true,
        SessionBoundRelayAckOutcome::NotDelivered,
        true
    ));
    assert!(watcher_should_direct_send_after_session_bound_ack(
        true,
        SessionBoundRelayAckOutcome::Dropped,
        true
    ));
    assert!(!watcher_should_direct_send_after_session_bound_ack(
        true,
        SessionBoundRelayAckOutcome::Delivered,
        true
    ));
}

// #3041 P1-3 (Part b, §3.2): the watcher's terminal re-send reconciliation
// against the committed offset authority — REPLACING the blind re-send. These
// assert the exact 2-way skip/full decision so the failure-mode-① skip (no
// duplicate) and the black-hole guard (full still sent on committed<end) are
// pinned. codex BLOCKER 2: there is no SendSuffix for the watcher response-text
// path (coordinate mismatch + all-or-nothing sink) — `committed < end` always
// re-sends the FULL response (no black-hole, no mis-offset slice).
#[test]
fn watcher_terminal_resend_skips_when_range_already_committed() {
    // failure-mode-①: the sink delivered `[0, 100)` (Part a advanced the
    // authority to 100) but the terminal-commit ACK lagged the 10s wait →
    // committed >= end → SKIP. No duplicate.
    assert_eq!(
        watcher_terminal_resend_action(100, 0, 100),
        WatcherTerminalResendAction::SkipAlreadyCommitted,
        "committed == end must skip the re-send (no duplicate)"
    );
    assert_eq!(
        watcher_terminal_resend_action(150, 0, 100),
        WatcherTerminalResendAction::SkipAlreadyCommitted,
        "committed past end must skip the re-send (no duplicate)"
    );
}

#[test]
fn watcher_terminal_resend_partial_overlap_sends_full_not_suffix() {
    // codex BLOCKER 2: a partial overlap `start < committed < end` would in
    // principle allow a suffix-only re-send, but the watcher delivers RESPONSE
    // TEXT sliced by `response_sent_offset` (a render offset), NOT JSONL bytes
    // — so it cannot coherently map `[committed, end)`. The all-or-nothing sink
    // never actually produces this case (it advances to the FULL end on a
    // single confirmed post, or not at all). Decision: SendFull on committed<end
    // — no black-hole (missing content always re-delivered), no mis-offset send.
    assert_eq!(
        watcher_terminal_resend_action(60, 0, 100),
        WatcherTerminalResendAction::SendFull,
        "partial overlap must SendFull (no mis-offset suffix; no black-hole)"
    );
    // Boundary: committed just past start is still committed<end → SendFull.
    assert_eq!(
        watcher_terminal_resend_action(1, 0, 100),
        WatcherTerminalResendAction::SendFull,
        "committed just past start must SendFull (committed<end → re-send)"
    );
}

#[test]
fn watcher_terminal_resend_sends_full_when_nothing_committed() {
    // BLACK-HOLE GUARD: the sink did NOT deliver (committed < end, and
    // committed <= start) → the FULL range must still be sent. Removing the
    // blind re-send must NEVER drop an undelivered range.
    assert_eq!(
        watcher_terminal_resend_action(0, 0, 100),
        WatcherTerminalResendAction::SendFull,
        "committed == start (nothing delivered) must send the full range"
    );
    assert_eq!(
        watcher_terminal_resend_action(40, 50, 100),
        WatcherTerminalResendAction::SendFull,
        "committed below start must send the full range (no black-hole)"
    );
}

// #3041 P1-5 (§3.2 SAFETY INVARIANT): a cross-actor `Unknown` outcome (the
// ring recorded `Unknown` / the ACK timed out / target was missing / dropped /
// sink-errored) MUST route through committed-offset reconciliation, NOT a blind
// 10s re-send. So `Unknown` with `committed >= end` → SkipAlreadyCommitted (a
// foreign owner already committed the range; re-sending would duplicate), and
// `Unknown` with `committed < end` → SendFull (the range is uncovered; no
// black-hole). The decision is driven SOLELY by the committed offset — it
// consults the authority, never blind-sends on the Unknown signal alone.
#[test]
fn unknown_outcome_triggers_committed_offset_reconciliation_not_blind_resend() {
    use crate::services::cluster::stream_relay::DeliveryOutcome;
    // The fold: every failure/unconfirmed ACK arm collapses to `Unknown`.
    for ack in [
        SessionBoundRelayAckOutcome::RingUnknown,
        SessionBoundRelayAckOutcome::Dropped,
        SessionBoundRelayAckOutcome::SinkError,
        SessionBoundRelayAckOutcome::TimedOut,
        SessionBoundRelayAckOutcome::MissingTarget,
    ] {
        assert_eq!(
            session_bound_ack_delivery_outcome(ack),
            DeliveryOutcome::Unknown,
            "every failure/unconfirmed ACK arm folds to the cross-actor Unknown"
        );
    }

    // §3.2: an Unknown outcome reconciles against the committed offset. The
    // SAME `watcher_terminal_resend_action` gate that NotDelivered uses — no
    // separate blind-resend path exists for Unknown.
    let start = 100u64;
    let end = 356u64;
    // committed >= end: a foreign owner already committed the range → SKIP, NOT
    // a blind re-send (which would duplicate).
    assert_eq!(
        watcher_terminal_resend_action(end, start, end),
        WatcherTerminalResendAction::SkipAlreadyCommitted,
        "Unknown + committed>=end must consult the offset and SKIP (no blind 10s re-send / no duplicate)"
    );
    // committed < end: the range is genuinely uncovered → SendFull (no
    // black-hole). The decision came from the offset, not the Unknown signal.
    assert_eq!(
        watcher_terminal_resend_action(start, start, end),
        WatcherTerminalResendAction::SendFull,
        "Unknown + committed<end must SendFull via the offset authority (no black-hole)"
    );
}

// #3041 P1-5 (§3.2 SAFETY INVARIANT): a `NotDelivered` outcome (the former
// `Ok(Skipped)`, redefined this phase) must ALSO route through committed-offset
// reconciliation — there is NO blind-skip fast-path for NotDelivered. When a
// FOREIGN owner already committed the range (`committed >= end`), the watcher
// must SKIP its re-send (no duplicate), exactly like a delivered turn — proving
// NotDelivered consults the offset rather than blindly skipping or blindly
// re-sending.
#[test]
fn not_delivered_outcome_keeps_no_resend_when_foreign_owner_committed() {
    use crate::services::cluster::stream_relay::DeliveryOutcome;
    assert_eq!(
        session_bound_ack_delivery_outcome(SessionBoundRelayAckOutcome::NotDelivered),
        DeliveryOutcome::NotDelivered,
        "NotDelivered folds to the cross-actor NotDelivered (not Unknown, not Delivered)"
    );
    let start = 100u64;
    let end = 356u64;
    // A foreign owner committed the full range out from under this watcher.
    assert_eq!(
        watcher_terminal_resend_action(end, start, end),
        WatcherTerminalResendAction::SkipAlreadyCommitted,
        "NotDelivered + committed>=end (foreign owner committed) must SKIP (no duplicate, no blind-skip-without-checking)"
    );
    // But when NOTHING committed it still SendFulls — NotDelivered is never a
    // silent drop (no black-hole).
    assert_eq!(
        watcher_terminal_resend_action(start, start, end),
        WatcherTerminalResendAction::SendFull,
        "NotDelivered + committed<end must SendFull (no black-hole; not a blind skip)"
    );
}

// #3041 P1-3 codex BLOCKER 2: the sink-delegated terminal delivery is
// ALL-OR-NOTHING — the sink advances `confirmed_end_offset` to the FULL `end`
// on ONE confirmed post, or not at all. So for the sink-delegated path the
// reconciliation only ever sees committed == end (delivered → Skip) or
// committed == start (not delivered → Full); it NEVER sees a partial
// `start < committed < end`. This pins that no SendSuffix is reachable on the
// delegated path, and that committed<end ALWAYS re-sends the full body (no
// black-hole). The watcher response-text path never derives a suffix from the
// unrelated `response_sent_offset`.
#[test]
fn sink_delegated_path_is_all_or_nothing_skip_or_full_never_suffix() {
    let start = 100u64;
    let end = 356u64; // full consumed-terminal end after an all-or-nothing post

    // Delivered: the sink committed the FULL range → committed == end → Skip.
    assert_eq!(
        watcher_terminal_resend_action(end, start, end),
        WatcherTerminalResendAction::SkipAlreadyCommitted,
        "all-or-nothing delivered → committed==end → Skip (no duplicate, failure-mode-①)"
    );

    // Not delivered: the sink did NOT post → committed stays at the prior
    // turn's end (<= start) → committed < end → SendFull (no black-hole).
    assert_eq!(
        watcher_terminal_resend_action(start, start, end),
        WatcherTerminalResendAction::SendFull,
        "all-or-nothing not-delivered → committed<=start → SendFull (no black-hole)"
    );

    // No reachable input on the delegated path yields SendSuffix — the variant
    // does not exist. Even an (unreachable) partial value re-sends FULL, not a
    // mis-offset suffix.
    for committed in [start + 1, (start + end) / 2, end - 1] {
        assert_eq!(
            watcher_terminal_resend_action(committed, start, end),
            WatcherTerminalResendAction::SendFull,
            "committed<end must SendFull (no suffix, no mis-offset slice, no black-hole)"
        );
    }
}

// BLOCKER 2 payload guard: `watcher_terminal_response_for_direct_send` must
// deliver the FULL response (not the `full_response[response_sent_offset..]`
// streaming-offset slice) whenever the reconciled re-send is taken, so the
// body is coherent and never a mid-response tail driven by an unrelated offset.
#[test]
fn watcher_resend_delivers_full_response_not_render_offset_slice() {
    let full_response = "ANSWER-PREFIX|ANSWER-SUFFIX";
    // A non-zero render offset that has NOTHING to do with JSONL bytes — the
    // exact mismatch BLOCKER 2 flagged. The old SendSuffix path would have
    // returned an incoherent slice from here.
    let response_sent_offset = "ANSWER-PREFIX|".len();

    // SendFull path (session_bound_fallback_uses_full_body = true): the full,
    // coherent response is delivered — never the render-offset slice.
    assert_eq!(
        watcher_terminal_response_for_direct_send(full_response, response_sent_offset, true),
        full_response,
        "reconciled re-send must deliver the FULL response (no mis-offset slice)"
    );
    // And it is NOT the render-offset suffix that the removed SendSuffix path
    // would have sent.
    assert_ne!(
        watcher_terminal_response_for_direct_send(full_response, response_sent_offset, true),
        &full_response[response_sent_offset..],
        "must NOT send the unrelated full_response[response_sent_offset..] slice"
    );
}

// #3041 P1-3 (Part a, B1 — FRAME-CARRIED): the watcher's decision to ATTACH the
// commit fence (consumed_end + pinned identity) to the RESULT-bearing frame is
// gated by `watcher_terminal_commit_fence`: only the terminal chunk, only a real
// (end > start) consumed range, and only with a pinned identity for THIS tmux
// session. A non-terminal chunk, a zero/inverted range, a missing identity, or a
// cross-session snapshot yields no fence (the frame stays non-terminal).
#[test]
fn watcher_terminal_commit_fence_only_for_a_real_terminal_chunk() {
    use crate::services::discord::inflight::InflightTurnIdentity;
    let session = "AgentDesk-claude-77";
    let identity = InflightTurnIdentity {
        user_msg_id: 0,
        started_at: "2026-06-04T00:00:00Z".to_string(),
        tmux_session_name: Some(session.to_string()),
        turn_start_offset: Some(64),
    };
    // Real terminal chunk: found_result, end > start, identity for this session.
    let fence = watcher_terminal_commit_fence(true, 0, 256, Some(&identity), session)
        .expect("a real terminal chunk must carry a commit fence");
    assert_eq!(fence.consumed_end, 256);
    assert_eq!(fence.turn_user_msg_id, 0);
    assert_eq!(fence.turn_started_at, "2026-06-04T00:00:00Z");
    // #3041 P1-3 (codex P1-3 issue 2): the fence carries the pinned
    // turn_start_offset so the sink can disambiguate same-second turns.
    assert_eq!(fence.turn_start_offset, Some(64));
    // Not the result chunk → no fence.
    assert!(watcher_terminal_commit_fence(false, 0, 256, Some(&identity), session).is_none());
    // Zero range (end == start) → no fence.
    assert!(watcher_terminal_commit_fence(true, 256, 256, Some(&identity), session).is_none());
    // Inverted range → no fence.
    assert!(watcher_terminal_commit_fence(true, 300, 256, Some(&identity), session).is_none());
    // No pinned identity → no fence (sink would have nothing to identity-gate).
    assert!(watcher_terminal_commit_fence(true, 0, 256, None, session).is_none());
    // Cross-session snapshot → no fence (never seed a wrong-turn fence).
    let other_identity = InflightTurnIdentity {
        user_msg_id: 0,
        started_at: "2026-06-04T00:00:00Z".to_string(),
        tmux_session_name: Some("AgentDesk-claude-99".to_string()),
        turn_start_offset: Some(64),
    };
    assert!(watcher_terminal_commit_fence(true, 0, 256, Some(&other_identity), session).is_none());

    // #3041 P1-3 (codex P1-3 issue 2 R4): a fence MUST carry a real
    // turn_start_offset. If the pinned identity's offset is None, the producer
    // emits NO fence (forwards a non-terminal frame instead) so the sink's
    // STRICT offset gate never sees a None and the watcher reconciliation's
    // SendFull delivers this turn safely (no black-hole, no weak gate).
    let no_offset_identity = InflightTurnIdentity {
        user_msg_id: 0,
        started_at: "2026-06-04T00:00:00Z".to_string(),
        tmux_session_name: Some(session.to_string()),
        turn_start_offset: None,
    };
    assert!(
        watcher_terminal_commit_fence(true, 0, 256, Some(&no_offset_identity), session).is_none(),
        "a turn with no known turn_start_offset must NOT emit a fence (strict-offset guarantee)"
    );
}

// #3041 P1-3 (codex P1-3 issue 1): a single physical chunk can carry turn A's
// result PLUS turn B's first bytes. The split must put A's bytes on the terminal
// side and B's leftover on the trailing side so B is forwarded (not black-holed).
#[test]
fn split_decoded_chunk_isolates_terminal_turn_from_trailing_tail() {
    let turn_a = "{\"type\":\"result\",\"result\":\"A done\"}\n";
    let turn_b = "{\"type\":\"assistant\",\"message\":{\"content\":[]}}\n";
    let combined = format!("{turn_a}{turn_b}");
    // After the parse, `all_data` holds turn B's bytes → leftover_len = turn_b.len().
    let (terminal_part, tail_part) =
        split_decoded_chunk_at_terminal_boundary(&combined, turn_b.len());
    assert_eq!(terminal_part, turn_a, "terminal frame carries ONLY turn A");
    assert_eq!(tail_part, turn_b, "turn B's bytes ride a separate frame");

    // No leftover (single complete turn) → whole chunk is terminal, no tail.
    let (only_terminal, no_tail) = split_decoded_chunk_at_terminal_boundary(turn_a, 0);
    assert_eq!(only_terminal, turn_a);
    assert!(no_tail.is_empty());

    // Leftover >= chunk (turn A's result was entirely in a prior leftover): the
    // whole decoded chunk is the trailing tail; terminal side is empty (the
    // empty terminal frame is then a no-op, the tail is still forwarded).
    let (empty_terminal, all_tail) =
        split_decoded_chunk_at_terminal_boundary(turn_b, turn_b.len() + 10);
    assert!(empty_terminal.is_empty());
    assert_eq!(all_tail, turn_b);

    // UTF-8 boundary safety: a split that would fall mid-scalar is nudged to the
    // next char boundary (keeps the scalar whole on the terminal side).
    let multibyte = "ok한"; // '한' is 3 bytes
    // leftover_len = 1 would nominally split inside '한'; the helper keeps it whole.
    let (head, tail) = split_decoded_chunk_at_terminal_boundary(multibyte, 1);
    assert!(multibyte.is_char_boundary(head.len()));
    assert_eq!(format!("{head}{tail}"), multibyte, "no bytes dropped");
}

#[test]
fn watcher_terminal_resend_degenerate_range_defers_to_full() {
    // A zero/inverted range manufactures NO skip — it defers to the existing
    // downstream zero-range guards (which never lease/advance).
    assert_eq!(
        watcher_terminal_resend_action(100, 100, 100),
        WatcherTerminalResendAction::SendFull
    );
    assert_eq!(
        watcher_terminal_resend_action(0, 100, 50),
        WatcherTerminalResendAction::SendFull
    );
}

#[test]
fn missing_structured_matched_session_jsonl_is_paused_live_for_existing_inflight() {
    let missing_path = std::env::temp_dir().join(format!(
        "agentdesk-missing-external-jsonl-{}-{}.jsonl",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let _ = std::fs::remove_file(&missing_path);
    let tmux_session_name = "AgentDesk-claude-relay-test";
    let mut state = state_for_matched_session(
        ProviderKind::Claude,
        tmux_session_name,
        &missing_path.display().to_string(),
    );
    state.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);

    assert_eq!(
        matched_session_completion_signal(
            &ProviderKind::Claude,
            Some(&state),
            tmux_session_name,
            CompletionSignalMode::GateQuiescence,
        ),
        Some(crate::services::discord::turn_finalizer::CompletionSignal::PausedLive)
    );
}
