//! Count-only delivery history enrichment across Discord and external outboxes.

use serde_json::{Value as JsonValue, json};
use sqlx::PgPool;

use crate::db::scheduled_messages as db;

pub(super) fn needs_discord(message: &db::ScheduledMessageRow) -> bool {
    message.delivery_kind == db::KIND_AGENT || message.target_channel_id.is_some()
}

pub(super) async fn render_deliveries(
    pool: &PgPool,
    deliveries: Vec<db::DeliveryRow>,
) -> Vec<JsonValue> {
    let outbox_ids: Vec<i64> = deliveries
        .iter()
        .flat_map(|delivery| [delivery.outbox_id, delivery.fallback_outbox_id])
        .flatten()
        .collect();
    let statuses = db::outbox_statuses_for_deliveries_pg(pool, &outbox_ids)
        .await
        .unwrap_or_default();
    let delivery_ids = deliveries
        .iter()
        .map(|delivery| delivery.id.clone())
        .collect::<Vec<_>>();
    let external_deliveries = db::list_external_deliveries_pg(pool, &delivery_ids)
        .await
        .unwrap_or_default();
    let status_of = |id: Option<i64>| {
        id.and_then(|id| {
            statuses
                .iter()
                .find(|(outbox_id, _)| *outbox_id == id)
                .map(|(_, status)| status.clone())
        })
    };
    deliveries
        .into_iter()
        .map(|delivery| {
            let mut rendered = delivery.to_api_json();
            if let Some(object) = rendered.as_object_mut() {
                object.insert(
                    "outboxStatus".to_string(),
                    json!(status_of(delivery.outbox_id)),
                );
                object.insert(
                    "fallbackOutboxStatus".to_string(),
                    json!(status_of(delivery.fallback_outbox_id)),
                );
                object.insert(
                    "externalDeliveries".to_string(),
                    json!(
                        external_deliveries
                            .iter()
                            .filter(|external| external.scheduled_delivery_id == delivery.id)
                            .map(|external| external.to_api_json())
                            .collect::<Vec<_>>()
                    ),
                );
            }
            rendered
        })
        .collect()
}
