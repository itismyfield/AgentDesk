//! A binary create-message reuses the manual reservation and durable row nonce.

use super::*;
use crate::services::message_outbox::OutboxAttachment;

pub(super) async fn deliver<C: ManualOutboundClient>(
    client: &C,
    notification: ManualNotification<'_>,
    attachment: OutboxAttachment<'_>,
) -> ManualDeliveryOutcome {
    let nonce = match preflight(notification, attachment) {
        Ok(nonce) => nonce,
        Err(detail) => {
            return ManualDeliveryOutcome::Failed {
                detail: detail.into(),
            };
        }
    };
    match client
        .post_binary_attachment(
            notification.channel_id,
            notification.content,
            attachment,
            &nonce,
        )
        .await
    {
        Ok(message_id) => ManualDeliveryOutcome::Sent {
            message_id,
            delivery: Some("binary_attachment"),
        },
        Err(error) => ManualDeliveryOutcome::Failed {
            detail: error.to_string(),
        },
    }
}

pub(super) fn preflight(
    notification: ManualNotification<'_>,
    attachment: OutboxAttachment<'_>,
) -> Result<String, &'static str> {
    let nonce = notification
        .delivery_id
        .and_then(|id| headless_nonce::image_delivery_nonce(notification.source, id))
        .ok_or("binary attachment requires a durable scheduled-message outbox identity")?;
    if !(1..=crate::db::scheduled_messages::MAX_IMAGE_BYTES).contains(&attachment.data.len())
        || notification.content.trim().is_empty()
        || crate::services::discord::formatting::discord_message_units(notification.content)
            > DISCORD_HARD_LIMIT_CHARS
    {
        return Err("invalid binary attachment size or content length");
    }
    Ok(nonce)
}

impl SerenityManualOutboundClient {
    pub(super) async fn post_image(
        &self,
        channel: &str,
        content: &str,
        attachment: OutboxAttachment<'_>,
        nonce: &str,
    ) -> Result<String, DispatchMessagePostError> {
        let channel = parse_channel_id_for_manual(channel).map_err(|_| {
            DispatchMessagePostError::new(
                DispatchMessagePostErrorKind::Other,
                "invalid binary target channel".to_string(),
            )
        })?;
        // Serenity owns the upload buffer; this copy occurs after the claim transaction.
        let file = serenity::CreateAttachment::bytes(attachment.data.to_vec(), attachment.filename);
        self.post_attachment(
            channel,
            CreateMessage::new()
                .content(content)
                .add_file(file)
                .nonce(serenity::model::channel::Nonce::String(nonce.to_string()))
                .enforce_nonce(true),
        )
        .await
    }
}
