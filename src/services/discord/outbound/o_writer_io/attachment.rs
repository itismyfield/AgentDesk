//! The O writer's image upload: one message carrying a caption and one image file.

use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::ChannelId;

use crate::services::tui_o::writer::PostOutcome;

/// POSTs `caption` with `bytes` attached as `filename`, flagged by the current push scope.
pub(super) async fn post(
    http: Arc<serenity::Http>,
    channel: u64,
    caption: String,
    bytes: Vec<u8>,
    filename: String,
) -> PostOutcome {
    let message = super::super::notification_policy::scoped_message(channel)
        .content(caption)
        .allowed_mentions(crate::services::message_outbox::mentions::relay_allowed_mentions())
        .add_file(serenity::CreateAttachment::bytes(bytes, filename));
    let sent = crate::services::legacy_body_census::request_context::census_scope(
        None,
        ChannelId::new(channel).send_message(&*http, message),
    )
    .await;
    match sent {
        Ok(message) => {
            crate::services::legacy_body_census::transport::record_o_created(
                channel,
                message.id.get(),
                None,
            );
            PostOutcome::Created(super::seen(message))
        }
        Err(error) => super::refusal(super::http_status(&error)).map_or_else(
            || PostOutcome::Uncertain(error.to_string()),
            PostOutcome::Refused,
        ),
    }
}
