use super::message_outbox_drain::{
    is_terminal_turn_delivery_outbox_source, session_can_be_released_for_terminal_outbox_failure,
};
use super::outbox_delivery_alert::outbox_alert_snippet;
use super::{MessageOutboxFailureAction, message_outbox_failure_action};
use crate::dispatch::test_support::DispatchPostgresTestDb;

#[test]
fn outbox_alert_snippet_truncates_and_handles_empty() {
    assert_eq!(outbox_alert_snippet("  hello  "), "hello");
    assert_eq!(outbox_alert_snippet("   "), "(빈 내용)");
    let long: String = "a".repeat(200);
    let snippet = outbox_alert_snippet(&long);
    assert!(
        snippet.chars().count() <= 121,
        "120 content chars + one ellipsis"
    );
    assert!(snippet.ends_with('…'));
}

#[test]
fn message_outbox_failure_action_retries_then_fails() {
    assert_eq!(
        message_outbox_failure_action(0),
        MessageOutboxFailureAction::Retry {
            retry_count: 1,
            backoff_secs: 60,
        }
    );
    assert_eq!(
        message_outbox_failure_action(3),
        MessageOutboxFailureAction::Retry {
            retry_count: 4,
            backoff_secs: 3600,
        }
    );
    assert_eq!(
        message_outbox_failure_action(4),
        MessageOutboxFailureAction::Fail { retry_count: 5 }
    );
}

#[test]
fn session_release_is_limited_to_terminal_turn_delivery_sources() {
    assert!(is_terminal_turn_delivery_outbox_source("headless_turn"));
    assert!(is_terminal_turn_delivery_outbox_source(
        "turn_bridge_tmux_handoff"
    ));

    assert!(!is_terminal_turn_delivery_outbox_source(
        crate::services::message_outbox::LIFECYCLE_NOTIFIER_SOURCE
    ));
    assert!(!is_terminal_turn_delivery_outbox_source("routine-runtime"));
    assert!(!is_terminal_turn_delivery_outbox_source("system"));
}

#[test]
fn session_release_requires_same_failed_outbox_session_when_present() {
    assert!(session_can_be_released_for_terminal_outbox_failure(
        Some("session-current"),
        Some("session-current"),
        Some(42),
        42,
    ));
    assert!(!session_can_be_released_for_terminal_outbox_failure(
        Some("session-current"),
        Some("session-old"),
        Some(42),
        42,
    ));
    assert!(session_can_be_released_for_terminal_outbox_failure(
        Some("session-current"),
        Some("  "),
        Some(42),
        42,
    ));
}

#[test]
fn session_release_requires_matching_terminal_delivery_outbox_marker() {
    assert!(!session_can_be_released_for_terminal_outbox_failure(
        Some("session-current"),
        Some("session-current"),
        None,
        42,
    ));
    assert!(!session_can_be_released_for_terminal_outbox_failure(
        Some("session-current"),
        Some("session-current"),
        Some(41),
        42,
    ));
    assert!(session_can_be_released_for_terminal_outbox_failure(
        Some("session-current"),
        Some("session-current"),
        Some(42),
        42,
    ));
}

#[tokio::test]
async fn relay_signal_threshold_report_enqueues_nothing_pg() {
    let Some(pg_db) = DispatchPostgresTestDb::try_create(
        "agentdesk_relay_signal_report",
        "relay signal threshold report",
    )
    .await
    else {
        return;
    };
    super::relay_signal_threshold_report_enqueues_nothing_pg(pg_db).await;
}

#[tokio::test]
async fn old_owner_completion_after_stale_reclaim_is_noop_pg() {
    let Some(pg_db) = DispatchPostgresTestDb::try_create(
        "agentdesk_message_outbox_lease",
        "message outbox lease fencing tests",
    )
    .await
    else {
        return;
    };
    super::old_owner_completion_after_stale_reclaim_is_noop_pg(pg_db).await;
}

#[tokio::test]
async fn stall_alert_dm_row_drains_with_provider_bot_pg() {
    let Some(pg_db) = DispatchPostgresTestDb::try_create(
        "agentdesk_stall_alert_outbox_drain",
        "stall alert message-outbox drain routing tests",
    )
    .await
    else {
        return;
    };
    super::stall_alert_dm_row_drains_with_provider_bot_pg(pg_db).await;
}

#[tokio::test]
async fn drain_fences_superseded_circuit_row_before_delivery_pg() {
    let Some(pg_db) = DispatchPostgresTestDb::try_create(
        "agentdesk_drain_delivery_fence",
        "worker delivery fence drain integration",
    )
    .await
    else {
        return;
    };
    super::drain_fences_superseded_circuit_row_before_delivery_pg(pg_db).await;
}
