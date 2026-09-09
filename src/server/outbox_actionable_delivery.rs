//! Retired sources terminate; actionable alerts can fall back from announce to notify.
use sqlx::PgPool;

use super::PendingMessageOutboxRow;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::outbound::source_registry::RETIRED_SEND_SOURCES;

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

fn delivery_content(content: &str, source: &str, reason_code: Option<&str>) -> String {
    if crate::services::message_outbox::is_non_turn_operational_alert(source, reason_code) {
        crate::services::discord::dispatch_policy::prepend_operational_alert_origin(content)
    } else {
        content.to_string()
    }
}

async fn deliver_with_bot(
    registry: &HealthRegistry,
    pg_pool: &PgPool,
    row: &PendingMessageOutboxRow,
    bot: &str,
) -> (&'static str, String) {
    let (correlation_id, semantic_event_id) = row.delivery_ids();
    let content = delivery_content(&row.content, &row.source, row.reason_code.as_deref());
    crate::services::discord::health::send_message_with_backends_and_delivery_options(
        registry,
        Some(pg_pool),
        &row.target,
        &content,
        &row.source,
        bot,
        None,
        Some(crate::services::discord::health::ManualOutboundDeliveryId {
            correlation_id: &correlation_id,
            semantic_event_id: &semantic_event_id,
        }),
        crate::services::discord::health::ManualOutboundOptions {
            allow_unbound_internal_channel: true,
            trusted_internal_outbox: true,
            ..Default::default()
        },
    )
    .await
}

pub(super) async fn deliver(
    registry: &HealthRegistry,
    pg_pool: &PgPool,
    row: &PendingMessageOutboxRow,
) -> (String, String) {
    if RETIRED_SEND_SOURCES.contains(&row.source.as_str()) {
        return ("retired_source".into(), "retired_source".into());
    }
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
    let (fallback_status, fallback_error) = deliver_with_bot(
        registry,
        pg_pool,
        row,
        crate::services::discord::bot_role::UtilityBotRole::Notify.alias(),
    )
    .await;
    (fallback_status.to_string(), fallback_error)
}

pub(super) fn failure_action(row: &PendingMessageOutboxRow) -> super::MessageOutboxFailureAction {
    if RETIRED_SEND_SOURCES.contains(&row.source.as_str()) {
        super::MessageOutboxFailureAction::Fail {
            retry_count: row.retry_count,
        }
    } else {
        super::message_outbox_failure_action(row.retry_count)
    }
}

#[cfg(test)]
mod tests {
    use super::{delivery_content, should_fallback_to_notify};

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

    #[test]
    fn operational_alert_delivery_adds_non_turn_provenance() {
        let marked = delivery_content(
            "watchdog alert",
            "dispatch_watchdog",
            Some("dispatch_stuck"),
        );
        assert!(crate::services::discord::dispatch_policy::has_non_turn_provenance(&marked));
        assert_eq!(
            delivery_content("handoff", "lifecycle_notifier", Some("runtime_started")),
            "handoff"
        );
    }
}
