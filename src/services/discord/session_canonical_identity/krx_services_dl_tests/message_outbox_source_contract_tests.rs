//! Moved from src/services/message_outbox.rs: checks enqueue acceptance against the discord send-source gate.

use crate::services::message_outbox::*;

#[tokio::test]
async fn enqueue_acceptance_matches_worker_loopback_source_gate_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_message_outbox_source_parity",
        "message_outbox enqueue/send parity tests",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let sources = [
        "system",
        "lifecycle_notifier",
        "routine-runtime",
        "scheduled_message",
        "headless_turn",
        "slo_alerter",
        "github_sync",
        "catch_up_too_old",
        "queue_overflow_notice",
        "outbox_delivery_alert",
        "long_turn_watchdog",
        "relay_signal_rollup",
        "dispatch_watchdog",
        "unregistered_policy_source",
        "SYSTEM",
        " system",
        "",
    ];
    for (index, source) in sources.into_iter().enumerate() {
        let target = format!("channel:{}", 4400 + index);
        let content = format!("source parity {index}");
        let message = OutboxMessage {
            target: &target,
            content: &content,
            bot: "notify",
            source,
            reason_code: None,
            session_key: None,
        };
        let worker_allows = crate::services::discord::outbound::send_gate::is_allowed_send_source_for(
            source,
            crate::services::discord::outbound::source_registry::SendCallerClass::LoopbackInternal,
        );
        let enqueue_result = enqueue_outbox_pg_returning_id_with_ttl(&pool, message, 0).await;
        assert_eq!(
            enqueue_result.is_ok(),
            worker_allows,
            "enqueue/send source decision drifted for `{source}`"
        );
    }
}
