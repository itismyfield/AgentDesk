use poise::serenity_prelude as serenity;

use super::super::SharedData;
use crate::services::provider::ProviderKind;

pub(super) async fn recover_status_panel_transition_intents(
    http: &serenity::Http,
    shared: &SharedData,
    provider: &ProviderKind,
) -> usize {
    super::super::status_panel_transition::recover_with_transport(
        provider,
        &shared.token_hash,
        |channel_id, content, nonce| async move {
            super::super::http::send_channel_message_with_nonce(
                http,
                serenity::ChannelId::new(channel_id),
                &content,
                &nonce,
            )
            .await
            .map(|message| message.id.get())
            .map_err(|error| {
                super::super::status_panel_transition::classify_create_failure_status(
                    match &error {
                        serenity::Error::Http(
                            serenity::http::HttpError::UnsuccessfulRequest(response),
                        ) => Some(response.status_code.as_u16()),
                        _ => None,
                    },
                )
            })
        },
        |channel_id, message_id| async move {
            match serenity::ChannelId::new(channel_id)
                .delete_message(http, serenity::MessageId::new(message_id))
                .await
            {
                Ok(()) => {
                    super::super::status_panel_transition::StatusPanelRetirementOutcome::Removed
                }
                Err(error)
                    if super::super::status_panel_orphan_store::delete_error_is_permanent(&error) =>
                {
                    super::super::status_panel_transition::StatusPanelRetirementOutcome::PermanentAbsent
                }
                Err(_) => {
                    super::super::status_panel_transition::StatusPanelRetirementOutcome::Deferred
                }
            }
        },
    )
    .await
}
