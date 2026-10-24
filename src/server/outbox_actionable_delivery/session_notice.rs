//! Opt-in provider delivery; unmarked outbox rows retain their existing policy.

use poise::serenity_prelude::ChannelId;
use sqlx::PgPool;

use super::PendingMessageOutboxRow;
use crate::services::discord::health::{HealthRegistry, ManualOutboundOptions};

#[derive(serde::Deserialize)]
struct SessionNotice {
    version: u8,
    provider_hint: Option<String>,
    content: String,
}

fn unavailable(detail: impl Into<String>) -> (String, String) {
    ("503 Service Unavailable".into(), detail.into())
}

pub(super) async fn deliver(
    registry: &HealthRegistry,
    pg_pool: &PgPool,
    row: &PendingMessageOutboxRow,
) -> Option<(String, String)> {
    let raw = match sqlx::query_scalar::<_, Option<serde_json::Value>>(
        "SELECT session_notice FROM message_outbox WHERE id = $1",
    )
    .bind(row.id)
    .fetch_optional(pg_pool)
    .await
    {
        Ok(Some(None)) => return None,
        Ok(Some(Some(raw))) => raw,
        Ok(None) => return Some(unavailable("claimed session notice row disappeared")),
        Err(error) => return Some(unavailable(format!("load session notice: {error}"))),
    };
    Some(deliver_marked(registry, pg_pool, row, raw).await)
}

async fn deliver_marked(
    registry: &HealthRegistry,
    pg_pool: &PgPool,
    row: &PendingMessageOutboxRow,
    raw: serde_json::Value,
) -> (String, String) {
    let notice = match serde_json::from_value::<SessionNotice>(raw) {
        Ok(notice) => notice,
        Err(error) => return unavailable(format!("invalid session notice: {error}")),
    };
    if notice.version != 1
        || !notice.content.starts_with("-# ")
        || notice
            .content
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}'))
    {
        return unavailable("session notice requires version 1 and one inline subtext line");
    }
    let channel = match row
        .target
        .strip_prefix("channel:")
        .unwrap_or(&row.target)
        .parse::<u64>()
    {
        Ok(id) if id != 0 => ChannelId::new(id),
        _ => return unavailable("session notice requires an explicit Discord channel id"),
    };
    let provider = match crate::services::discord::session_notice_delivery::channel_provider(
        registry,
        channel,
        notice.provider_hint.as_deref(),
    )
    .await
    {
        Ok(provider) => provider,
        Err(error) => return unavailable(error),
    };
    let attachment = match row.attachment.clone() {
        Ok(attachment) => attachment,
        Err(error) => return ("400 Bad Request".into(), error.into()),
    };
    let options = ManualOutboundOptions {
        allow_unbound_internal_channel: true,
        trusted_internal_outbox: true,
        channel_owner_provider: Some(provider.clone()),
        image_attachment: attachment,
        ..Default::default()
    };
    let content = if crate::services::message_outbox::is_non_turn_operational_alert(
        &row.source,
        row.reason_code.as_deref(),
    ) {
        crate::services::discord::dispatch_policy::prepend_inline_operational_alert_origin(
            &notice.content,
        )
    } else {
        notice.content
    };
    let (correlation_id, semantic_event_id) = row.delivery_ids();
    let (status, detail) =
        crate::services::discord::health::send_message_with_backends_and_delivery_options(
            registry,
            Some(pg_pool),
            &row.target,
            &content,
            &row.source,
            provider.as_str(),
            None,
            Some(crate::services::discord::health::ManualOutboundDeliveryId {
                correlation_id: &correlation_id,
                semantic_event_id: &semantic_event_id,
            }),
            options,
        )
        .await;
    (status.into(), detail)
}
