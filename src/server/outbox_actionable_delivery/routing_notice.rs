//! Routing notices post only through their owner pointer, stored nonce and cursor.

use sqlx::PgPool;

use super::PendingMessageOutboxRow;
use crate::services::cluster::intake_router_hook::owner_record::event::{self, Step};
use crate::services::discord::health::HealthRegistry;

/// Terminal for the outbox row; the owner pointer keeps the original event identity.
pub(super) const UNRESOLVED: &str = "409 Conflict";

pub(super) async fn deliver(
    registry: &HealthRegistry,
    pg_pool: &PgPool,
    row: &PendingMessageOutboxRow,
) -> (String, String) {
    let event = match event::begin_post(pg_pool, row.id).await {
        Ok(Step::Post(event)) => event,
        Ok(Step::Sent(message_id)) => return sent(&message_id),
        Ok(Step::Uncertain) => {
            return (
                UNRESOLVED.into(),
                "routing notice may already be visible; not reposting".into(),
            );
        }
        Ok(Step::Unowned) => {
            return (
                UNRESOLVED.into(),
                "routing notice has no owner pointer".into(),
            );
        }
        Err(error) => return ("503 Service Unavailable".into(), error.to_string()),
    };
    let Some(channel) = event
        .target
        .strip_prefix("channel:")
        .and_then(|id| id.parse::<u64>().ok())
        .filter(|id| *id != 0)
    else {
        return (
            UNRESOLVED.into(),
            "routing notice target is not a channel".into(),
        );
    };
    #[cfg(test)]
    let nonce =
        if crate::services::cluster::intake_router_hook::owner_record::cleanup::tests::mutant(
            "notice-nonce",
        ) {
            row.delivery_ids().1
        } else {
            event.nonce.clone()
        };
    #[cfg(not(test))]
    let nonce = event.nonce.clone();
    let content =
        crate::services::discord::dispatch_policy::prepend_operational_alert_origin(&event.content);
    let message_id = match crate::services::discord::outbound::send_gate::post_routing_notice(
        registry, channel, &content, &event.bot, &nonce,
    )
    .await
    {
        Ok(message_id) => message_id,
        Err((status, error)) => return (status.into(), error),
    };
    match event::record_sent(pg_pool, row.id, &event.nonce, &message_id).await {
        Ok(true) => sent(&message_id),
        Ok(false) => (
            "503 Service Unavailable".into(),
            "routing notice cursor was not posting".into(),
        ),
        Err(error) => ("503 Service Unavailable".into(), error.to_string()),
    }
}

fn sent(message_id: &str) -> (String, String) {
    (
        "200 OK".into(),
        serde_json::json!({"ok": true, "message_id": message_id}).to_string(),
    )
}
