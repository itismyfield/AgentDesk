//! Delivery policy for actionable operational outbox alerts (#4449).
//!
//! The durable row keeps its existing channel target and dedupe identity.  An
//! announce-bot post is the primary delivery because messages in the configured
//! operations channel are ingested by its resident AgentDesk role.  If that bot
//! cannot deliver, retry once with the notify bot so the human-visible alert
//! survives an announce credential/runtime failure.  Informational rows never
//! enter this fallback path.

use sqlx::PgPool;

use super::PendingMessageOutboxRow;
use crate::services::discord::health::HealthRegistry;

fn should_fallback_to_notify(
    status: &str,
    primary_bot: &str,
    source: &str,
    reason_code: Option<&str>,
) -> bool {
    status != "200 OK"
        && primary_bot == crate::services::message_outbox::ACTIONABLE_OPS_ALERT_BOT
        && crate::services::message_outbox::is_actionable_ops_alert(source, reason_code)
}

async fn deliver_with_bot(
    registry: &HealthRegistry,
    pg_pool: &PgPool,
    row: &PendingMessageOutboxRow,
    bot: &str,
) -> (&'static str, String) {
    let (correlation_id, semantic_event_id) = row.delivery_ids();
    crate::services::discord::health::send_message_with_backends_and_delivery_options(
        registry,
        Some(pg_pool),
        &row.target,
        &row.content,
        &row.source,
        bot,
        None,
        Some(crate::services::discord::health::ManualOutboundDeliveryId {
            correlation_id: &correlation_id,
            semantic_event_id: &semantic_event_id,
        }),
        crate::services::discord::health::ManualOutboundOptions {
            allow_unbound_internal_channel: true,
        },
    )
    .await
}

pub(super) async fn deliver(
    registry: &HealthRegistry,
    pg_pool: &PgPool,
    row: &PendingMessageOutboxRow,
) -> (String, String) {
    let primary_bot = crate::services::message_outbox::delivery_bot_for_target_session(
        &row.target,
        &row.bot,
        row.session_key.as_deref(),
    );
    let (status, error) = deliver_with_bot(registry, pg_pool, row, primary_bot.as_ref()).await;
    if !should_fallback_to_notify(
        status,
        primary_bot.as_ref(),
        &row.source,
        row.reason_code.as_deref(),
    ) {
        return (status.to_string(), error);
    }

    tracing::warn!(
        outbox_id = row.id,
        source = row.source,
        reason_code = row.reason_code.as_deref(),
        primary_status = status,
        primary_error = %error,
        "actionable ops alert announce delivery failed; falling back to notify bot"
    );
    let (fallback_status, fallback_error) =
        deliver_with_bot(registry, pg_pool, row, "notify").await;
    (fallback_status.to_string(), fallback_error)
}

#[cfg(test)]
mod tests {
    use super::should_fallback_to_notify;

    #[test]
    fn notify_fallback_requires_a_failed_actionable_announce_delivery() {
        assert!(should_fallback_to_notify(
            "500 Internal Server Error",
            "announce",
            "dispatch_watchdog",
            Some("dispatch_stuck"),
        ));
        assert!(!should_fallback_to_notify(
            "200 OK",
            "announce",
            "dispatch_watchdog",
            Some("dispatch_stuck"),
        ));
        assert!(!should_fallback_to_notify(
            "500 Internal Server Error",
            "notify",
            "dispatch_watchdog",
            Some("dispatch_stuck"),
        ));
        assert!(!should_fallback_to_notify(
            "500 Internal Server Error",
            "announce",
            "auto-queue-monitor",
            Some("auto_queue.monitor_recovery"),
        ));
    }
}
