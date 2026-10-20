use super::{
    dedupe_key_for_message, delivery_bot_for_target_session, is_actionable_ops_alert,
    is_non_turn_operational_alert,
};

#[test]
fn actionable_ops_alert_registry_is_exact_and_excludes_recovery_noise() {
    for (source, reason) in [
        ("outbox_delivery_alert", "outbox_delivery_failed"),
        ("github_sync", "github_sync.terminal_open_issue"),
        ("long_turn_watchdog", "long_turn_cluster"),
        ("relay_signal_rollup", "relay_signal.threshold"),
        ("slo_alerter", "slo_threshold_breach"),
        ("dispatch_watchdog", "dispatch_stuck"),
        ("routine-runtime", "routine_paused_stale"),
        ("routine-runtime", "routine_consecutive_failures"),
        ("auto-queue", "auto_queue.entry_dispatch_failed"),
        ("auto-queue-monitor", "auto_queue.monitor_stuck"),
        ("auto-queue-monitor", "auto_queue.monitor_anomaly"),
    ] {
        assert!(
            is_actionable_ops_alert(source, Some(reason)),
            "missing actionable alert registration for {source}/{reason}"
        );
    }

    for (source, reason) in [
        ("auto-queue-monitor", "auto_queue.monitor_review_long"),
        ("auto-queue-monitor", "auto_queue.monitor_recovery"),
        ("routine-runtime", "routine_recovery_resumed"),
        ("lifecycle_notifier", "runtime_started"),
        ("catch_up", "catch_up.too_old"),
    ] {
        assert!(
            !is_actionable_ops_alert(source, Some(reason)),
            "informational notice must stay notify-only: {source}/{reason}"
        );
    }
    assert!(!is_actionable_ops_alert("github_sync", None));
    assert!(!is_actionable_ops_alert(
        "wrong-source",
        Some("dispatch_stuck")
    ));
}

#[test]
fn operational_alert_registry_includes_notify_only_alerts() {
    for (source, reason) in [
        ("auto-queue-monitor", "auto_queue.monitor_review_long"),
        ("stall_watchdog", "stall_watchdog_suspected_stall"),
        ("quality_regression_alerter", "agent_quality.regression"),
        ("queue_overflow_notice", "queue_overflow.evict"),
        (
            "stall_watchdog",
            "relay_reattach_circuit_open:discord:channel-42",
        ),
        ("auto-queue-monitor", "auto_queue.monitor_recovery"),
    ] {
        assert!(
            is_non_turn_operational_alert(source, Some(reason)),
            "missing non-turn provenance registration for {source}/{reason}"
        );
    }
    assert!(!is_non_turn_operational_alert(
        "lifecycle_notifier",
        Some("runtime_started")
    ));
}

#[test]
fn reason_code_dedupe_key_ignores_content() {
    let first = dedupe_key_for_message(
        "channel:123",
        "first rendered lifecycle text",
        Some("relay_terminal_ack_timeout"),
        Some("session-abc"),
    );
    let second = dedupe_key_for_message(
        "channel:123",
        "second rendered lifecycle text",
        Some("relay_terminal_ack_timeout"),
        Some("session-abc"),
    );

    assert_eq!(first, second);
}

#[test]
fn content_dedupe_key_keeps_content_identity_without_reason_code() {
    let first = dedupe_key_for_message("channel:123", "first", None, Some("session-abc"));
    let second = dedupe_key_for_message("channel:123", "second", None, Some("session-abc"));

    assert_ne!(first, second);
}

#[test]
fn dm_session_routes_outbox_delivery_through_provider_bot() {
    let bot = delivery_bot_for_target_session(
        "channel:1479662682909966490",
        "announce",
        Some("claude/tok/mac-mini:AgentDesk-claude-dm-343742347"),
    );

    assert_eq!(bot.as_ref(), "claude");
    let notify_bot = delivery_bot_for_target_session(
        "channel:1479662682909966490",
        "notify",
        Some("claude/tok/mac-mini:AgentDesk-claude-dm-343742347"),
    );
    assert_eq!(notify_bot.as_ref(), "claude");
    let raw_tmux_bot = delivery_bot_for_target_session(
        "channel:1479662682909966490",
        "announce",
        Some("AgentDesk-claude-dm-343742347"),
    );
    assert_eq!(raw_tmux_bot.as_ref(), "claude");
}

#[test]
fn guild_session_keeps_configured_announce_bot() {
    let bot = delivery_bot_for_target_session(
        "channel:1504455726595051591",
        "announce",
        Some("claude/tok/mac-mini:AgentDesk-claude-adk-cc"),
    );

    assert_eq!(bot.as_ref(), "announce");
}

#[test]
fn notify_guild_delivery_keeps_info_only_bot() {
    let bot = delivery_bot_for_target_session(
        "channel:1504455726595051591",
        "notify",
        Some("codex/tok/mac-mini:AgentDesk-codex-adk-cc"),
    );

    assert_eq!(bot.as_ref(), "notify");
}
