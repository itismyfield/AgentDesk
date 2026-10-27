use super::*;

#[derive(Clone, Copy)]
pub(in crate::services::discord::outbound) struct ManualNotification<'a> {
    pub channel_id: &'a str,
    pub content: &'a str,
    pub bot: &'a str,
    pub source: &'a str,
    pub summary: Option<&'a str>,
    pub delivery_id: Option<ManualOutboundDeliveryId<'a>>,
    pub headless_nonce_enabled: bool,
    pub attachment: Option<crate::services::message_outbox::OutboxAttachment<'a>>,
}

pub(in crate::services::discord::outbound) struct ResolvedManualMessage<'a> {
    pub channel_id_raw: u64,
    pub target: &'a str,
    pub notification: ManualNotification<'a>,
    pub pg_pool: Option<&'a PgPool>,
    pub record_transcript: bool,
    pub transcript_source_label: Option<&'a str>,
}

#[cfg(test)]
pub(in crate::services::discord::outbound) async fn send_resolved_manual_message<
    C: ManualOutboundClient,
>(
    client: &C,
    dedup: &OutboundDeduper,
    request: ResolvedManualMessage<'_>,
) -> (&'static str, String) {
    send_resolved_with_origin(client, dedup, request, None).await
}

async fn send_resolved_with_origin<C: ManualOutboundClient>(
    client: &C,
    dedup: &OutboundDeduper,
    request: ResolvedManualMessage<'_>,
    origin: Option<&crate::services::cluster::intake_router_hook::owner_record::output::Artifact>,
) -> (&'static str, String) {
    let ResolvedManualMessage {
        channel_id_raw,
        target,
        notification,
        pg_pool,
        record_transcript,
        transcript_source_label,
    } = request;
    let ManualNotification {
        content,
        source,
        bot,
        ..
    } = notification;
    if let Some(attachment) = notification.attachment {
        if let Err(detail) = image_delivery::preflight(notification, attachment) {
            return (
                "400 Bad Request",
                serde_json::json!({"ok": false, "error": detail}).to_string(),
            );
        }
    }
    let channel_id = ChannelId::new(channel_id_raw);
    use crate::services::cluster::intake_router_hook::owner_record::output;
    let scope = match (pg_pool, origin) {
        (Some(pool), Some(origin)) => Some(output::Scope {
            pool: pool.clone(),
            provider: origin.provider().into(),
        }),
        (Some(pool), None) if source == "headless_turn" => {
            if let Err(error) =
                output::reject_unstamped_managed(pool, &channel_id_raw.to_string()).await
            {
                return ("409 Conflict", error);
            }
            None
        }
        _ => None,
    };
    let guard =
        match output::acquire(scope.as_ref(), &channel_id_raw.to_string(), content, origin).await {
            Ok(guard) => guard,
            Err(error) => return ("409 Conflict", error),
        };
    let send_result = match guard
        .run(deliver_manual_notification(client, dedup, notification))
        .await
    {
        Ok(result) => result,
        Err(error) => return ("503 Service Unavailable", error),
    };
    match send_result {
        ManualDeliveryOutcome::Sent {
            message_id,
            delivery,
        } => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            let emoji = manual_delivery_log_emoji(bot);
            let delivery_tag = delivery
                .map(|value| format!(" +{value}"))
                .unwrap_or_default();
            tracing::info!(
                "  [{ts}] {emoji} ROUTE: [{source}] → channel {channel_id} (bot={bot}{delivery_tag})"
            );
            if record_transcript && !message_id.is_empty() && !content.trim().is_empty() {
                record_manual_message_transcript(
                    pg_pool,
                    channel_id_raw,
                    &message_id,
                    content,
                    transcript_source_label,
                )
                .await;
            }
            let mut response = serde_json::json!({
                "ok": true,
                "target": format!("channel:{channel_id}"),
                "channel_id": channel_id.get().to_string(),
                "message_id": message_id,
                "source": source,
                "bot": bot,
                "sent_at": chrono::Utc::now().to_rfc3339(),
            });
            if let Some(delivery) = delivery {
                response["delivery"] = serde_json::Value::String(delivery.to_string());
            }
            if target != format!("channel:{channel_id}") {
                response["requested_target"] = serde_json::Value::String(target.to_string());
            }
            ("200 OK", response.to_string())
        }
        ManualDeliveryOutcome::Failed { detail } => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!("  [{ts}] ⚠ ROUTE: failed to send to channel {channel_id}: {detail}");
            (
                "500 Internal Server Error",
                format!(
                    r#"{{"ok":false,"error":"Discord send failed: {}"}}"#,
                    detail
                ),
            )
        }
    }
}

pub(in crate::services::discord::outbound) async fn send_authorized_manual_message<
    'a,
    C: ManualOutboundClient,
>(
    client: &C,
    dedup: &OutboundDeduper,
    mut request: ResolvedManualMessage<'a>,
    options: &'a super::super::send_gate::ManualOutboundOptions,
) -> (&'static str, String) {
    request.notification.attachment = options
        .image_attachment
        .as_deref()
        .map(|image| image.borrowed());
    request.notification.headless_nonce_enabled = options.image_attachment.is_none()
        && headless_nonce::headless_discord_nonce_enabled(options.trusted_internal_outbox);
    send_resolved_with_origin(client, dedup, request, options.output_origin.as_ref()).await
}
