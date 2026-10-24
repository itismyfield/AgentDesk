//! Delivery policy for actionable operational outbox alerts (#4449).
//!
//! The durable row keeps its existing channel target and dedupe identity.  An
//! announce-bot post is the primary delivery so the configured operations
//! channel receives the notice through its resident role. Non-turn provenance
//! keeps the notice out of intervention intake. If that bot cannot deliver,
//! retry once with the notify bot so the human-visible alert survives an
//! announce credential/runtime failure. Informational rows never enter this
//! fallback path.

use sqlx::PgPool;

use super::PendingMessageOutboxRow;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::outbound::source_registry::RETIRED_SEND_SOURCES;

#[path = "outbox_actionable_delivery/session_notice.rs"]
mod session_notice;

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

pub(super) fn delivery_options(
    row: &PendingMessageOutboxRow,
) -> Result<crate::services::discord::health::ManualOutboundOptions, &'static str> {
    let attachment = row.attachment.clone()?;
    Ok(crate::services::discord::health::ManualOutboundOptions {
        allow_unbound_internal_channel: true,
        trusted_internal_outbox: true,
        channel_owner_provider: if row.source
            == crate::services::message_outbox::LIFECYCLE_NOTIFIER_SOURCE
            && row.reason_code.as_deref() == Some("lifecycle.background_task_complete")
        {
            Some(
                crate::services::provider::ProviderKind::from_str(&row.bot)
                    .ok_or("background completion requires its provider bot")?,
            )
        } else {
            None
        },
        image_attachment: attachment,
        ..Default::default()
    })
}

async fn deliver_with_bot(
    registry: &HealthRegistry,
    pg_pool: &PgPool,
    row: &PendingMessageOutboxRow,
    bot: &str,
) -> (&'static str, String) {
    let options = match delivery_options(row) {
        Ok(options) => options,
        Err(error) => return ("400 Bad Request", error.to_string()),
    };
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
        options,
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
    if let Some(result) = session_notice::deliver(registry, pg_pool, row).await {
        return result;
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

pub(super) fn failure_action(
    row: &PendingMessageOutboxRow,
    status: &str,
) -> super::MessageOutboxFailureAction {
    if row.attachment.is_err()
        || (row.attachment.as_ref().is_ok_and(Option::is_some) && status == "400 Bad Request")
        || RETIRED_SEND_SOURCES.contains(&row.source.as_str())
    {
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
    fn background_notice_selects_provider_owner_and_defers_without_notify_fallback() {
        let mut row = super::PendingMessageOutboxRow {
            id: 1,
            target: "channel:42".into(),
            content: "-# ✅ done".into(),
            bot: "claude".into(),
            source: "lifecycle_notifier".into(),
            reason_code: Some("lifecycle.background_task_complete".into()),
            session_key: Some("footer_background:ch:42:event".into()),
            retry_count: 0,
            attachment: Ok(None),
            claim_owner: "fixture".into(),
            claimed_at: chrono::Utc::now(),
        };
        let options = super::delivery_options(&row).unwrap();
        assert_eq!(
            options.channel_owner_provider,
            Some(crate::services::provider::ProviderKind::Claude)
        );
        assert!(options.trusted_internal_outbox);
        assert_eq!(
            super::delivery_content(&row.content, &row.source, row.reason_code.as_deref()),
            row.content
        );
        assert!(!should_fallback_to_notify(
            "503 Service Unavailable",
            "claude",
            &row.source,
            row.reason_code.as_deref()
        ));
        assert!(matches!(
            super::failure_action(&row, "503 Service Unavailable"),
            super::super::MessageOutboxFailureAction::Retry { .. }
        ));
        row.bot = "notify".into();
        assert!(super::delivery_options(&row).is_err());
        row.reason_code = Some("runtime_started".into());
        assert!(
            super::delivery_options(&row)
                .unwrap()
                .channel_owner_provider
                .is_none()
        );
    }

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
