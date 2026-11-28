//! Emit scenarios the upper-crate capture stress tests drive (KRX-TS). Each runs under the observability
//! runtime lock it takes itself; the unit tests in `emit::tests` call the same functions.

use super::*;

pub fn every_emit_surface_records_one_recent_event_scenario(before_observe: impl FnOnce()) {
    let _guard = super::super::test_runtime_lock();
    super::super::reset_for_tests();

    emit_turn_started(
        "Codex",
        42,
        Some("dispatch-started"),
        Some("session-started"),
        Some("turn-started"),
    );
    emit_turn_finished_with_dispatch_kind(
        "Codex",
        42,
        Some("dispatch-finished"),
        Some("session-finished"),
        Some("turn-finished"),
        " completed ",
        123,
        false,
        Some("manual"),
    );
    emit_guard_fired(
        "Codex",
        42,
        Some("dispatch-guard"),
        Some("session-guard"),
        Some("turn-guard"),
        "placeholder_suppress",
    );
    emit_watcher_replaced("Codex", 42, "stale_cancel");
    emit_recovery_fired(
        "Codex",
        42,
        Some("dispatch-recovery"),
        Some("session-recovery"),
        Some("turn-recovery"),
        Some("planner"),
        "watchdog",
    );
    emit_turn_cancelled(
        Some("Codex"),
        Some(42),
        Some("dispatch-cancelled"),
        Some("session-cancelled"),
        Some("turn-cancelled"),
        turn_lifecycle::TurnCancellationDetails::new(
            "operator stop",
            "text_stop",
            "mailbox_canonical",
            true,
            true,
            Some(1),
            Some(false),
            true,
        ),
    );
    emit_inflight_lifecycle_event(
        "Codex",
        42,
        Some("dispatch-inflight"),
        Some("session-inflight"),
        Some("turn-inflight"),
        "delegated_to_watcher",
        json!({"owner": "watcher"}),
    );
    assert!(!record_invariant_check(
        false,
        InvariantViolation {
            provider: Some("Codex"),
            channel_id: Some(42),
            dispatch_id: Some("dispatch-invariant"),
            session_key: Some("session-invariant"),
            turn_id: Some("turn-invariant"),
            invariant: "recent_ring_visibility",
            code_location: "src/services/observability/emit.rs:test",
            message: "test violation",
            details: json!({"test": true}),
        },
    ));
    emit_dispatch_result(
        "dispatch-result",
        Some("card-1"),
        Some("implementation"),
        Some("doing"),
        "done",
        "test",
        Some(&json!({"ok": true})),
    );
    emit_intake_placeholder_post_failed(
        "Codex",
        42,
        Some(1234),
        "queue",
        "message_preserved",
        "discord unavailable",
    );
    emit_agent_quality_event(AgentQualityEvent {
        source_event_id: Some("turn-quality".to_string()),
        correlation_id: Some("dispatch-quality".to_string()),
        agent_id: Some("agent-1".to_string()),
        provider: Some("Codex".to_string()),
        channel_id: Some("42".to_string()),
        card_id: Some("card-1".to_string()),
        dispatch_id: Some("dispatch-quality".to_string()),
        event_type: "review_pass".to_string(),
        payload: json!({"verdict": "pass"}),
    });

    before_observe();
    let events = crate::services::observability::events::test_capture::snapshot();
    for expected in [
        "turn_started",
        "turn_finished",
        "guard_fired",
        "watcher_replaced",
        "recovery_fired",
        "turn_cancelled",
        "inflight_lifecycle",
        "invariant_violation",
        "dispatch_result",
        "intake_placeholder_post_failed",
        "agent_quality_event",
    ] {
        let count = events
            .iter()
            .filter(|event| event.event_type == expected)
            .count();
        assert_eq!(count, 1, "{expected} should be recorded exactly once");
    }

    let dispatch_result = events
        .iter()
        .find(|event| event.event_type == "dispatch_result")
        .expect("dispatch_result should be in recent ring");
    assert_eq!(dispatch_result.payload["dispatch_id"], "dispatch-result");
    assert_eq!(dispatch_result.payload["status"], "done");

    let turn_finished = events
        .iter()
        .find(|event| event.event_type == "turn_finished")
        .expect("turn_finished should be in recent ring");
    assert_eq!(turn_finished.provider.as_deref(), Some("codex"));
    assert_eq!(turn_finished.channel_id, Some(42));
    assert_eq!(turn_finished.payload["dispatch_id"], "dispatch-finished");
    assert_eq!(turn_finished.payload["duration_ms"], 123);
    assert_eq!(turn_finished.payload["status"], "completed");

    let quality = events
        .iter()
        .find(|event| event.event_type == "agent_quality_event")
        .expect("agent_quality_event should be in recent ring");
    assert_eq!(quality.payload["quality_event_type"], "review_pass");
    assert_eq!(quality.payload["dispatch_id"], "dispatch-quality");
    assert_eq!(quality.payload["source_event_id"], "turn-quality");
    assert_eq!(quality.payload["status"], "review_pass");

    // #3562: recovery_fired must carry the turn identity (correlation column +
    // mirrored into payload_json) and the agent_id (payload_json only) so
    // consumers can back-trace which agent/turn triggered the recovery.
    let recovery = events
        .iter()
        .find(|event| event.event_type == "recovery_fired")
        .expect("recovery_fired should be in recent ring");
    assert_eq!(recovery.payload["turn_id"], "turn-recovery");
    assert_eq!(recovery.payload["agent_id"], "planner");
    assert_eq!(recovery.payload["reason"], "watchdog");
}

/// #5500: the persisted event must carry the severity. Before this, the
/// analytics record was byte-for-byte identical across Error and Warn — the
/// distinction existed only as a tracing log LEVEL — so the level a stored
/// violation was logged at could not be read back off the record.
/// `relay_signal_alert` counts these rows by column with a default threshold
/// of 1 for `offset_invariant_violation`, so the collapse left an alerting
/// operator with no in-record discriminator to start from.

pub fn invariant_severity_routes_to_exact_log_level_and_suffix_4422() {
    let ((), logs) = super::invariant_log_test_capture::capture(|| {
        assert!(record_invariant_check_with_severity(
            true,
            InvariantViolation {
                provider: Some("Codex"),
                channel_id: Some(44_220),
                dispatch_id: None,
                session_key: None,
                turn_id: None,
                invariant: "condition_true_must_not_emit",
                code_location: "src/services/observability/emit.rs:test",
                message: "no emission",
                details: json!({}),
            },
            InvariantSeverity::Warn,
        ));
        assert!(!record_invariant_check_with_severity(
            false,
            InvariantViolation {
                provider: Some("Codex"),
                channel_id: Some(44_220),
                dispatch_id: None,
                session_key: None,
                turn_id: None,
                invariant: "warn_route_4422",
                code_location: "src/services/observability/emit.rs:test",
                message: "warn-selected rewind",
                details: json!({}),
            },
            InvariantSeverity::Warn,
        ));
        assert!(!record_invariant_check_with_severity(
            false,
            InvariantViolation {
                provider: Some("Codex"),
                channel_id: Some(44_220),
                dispatch_id: None,
                session_key: None,
                turn_id: None,
                invariant: "error_route_4422",
                code_location: "src/services/observability/emit.rs:test",
                message: "persisted rewind",
                details: json!({}),
            },
            InvariantSeverity::Error,
        ));
    });

    assert_eq!(logs.len(), 2, "condition=true must not emit: {logs:?}");
    assert_eq!(logs[0].severity, InvariantSeverity::Warn);
    assert_eq!(logs[0].invariant, "warn_route_4422");
    assert_eq!(
        logs[0].rendered_message,
        format!("[invariant] warn-selected rewind {INVARIANT_WARN_DOWNGRADE_SUFFIX}")
    );
    assert_eq!(logs[1].severity, InvariantSeverity::Error);
    assert_eq!(logs[1].invariant, "error_route_4422");
    assert_eq!(logs[1].rendered_message, "[invariant] persisted rewind");
}

pub const ROOT_CAUSE_ACTIONS: [fn(u64, &str); 3] = [
    metrics::record_relay_terminal_ack_timeout,
    metrics::record_relay_uncommitted_inflight_cleared,
    metrics::record_relay_owner_unknown,
];

pub fn relay_root_cause_metric_wrappers_record_persistent_events_scenario(
    actions: &[fn(u64, &str)],
    before_observe: impl FnOnce(),
) {
    let _guard = super::super::test_runtime_lock();
    super::super::reset_for_tests();

    for action in actions {
        action(77, "Codex");
    }

    before_observe();
    let events = crate::services::observability::events::test_capture::snapshot();
    let counters = events
        .iter()
        .filter(|event| event.event_type == "relay_root_cause_counter")
        .map(|event| event.payload["counter"].as_str().unwrap_or_default())
        .collect::<Vec<_>>();
    assert_eq!(
        counters,
        vec![
            "relay_terminal_ack_timeout",
            "relay_uncommitted_inflight_cleared",
            "relay_owner_unknown"
        ]
    );
    assert!(
        events
            .iter()
            .all(|event| event.provider.as_deref() == Some("codex"))
    );
    assert!(events.iter().all(|event| event.channel_id == Some(77)));
}
