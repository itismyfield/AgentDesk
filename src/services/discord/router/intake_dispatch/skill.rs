use poise::serenity_prelude as serenity;

use super::{IntakeOrigin, IntakeSubmission, dispatch_text_intake};
use crate::services::provider::ProviderKind;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn dispatch_skill_intake(
    deps: &super::super::message_handler::IntakeDeps<'_>,
    provider: ProviderKind,
    channel_id: serenity::ChannelId,
    user_msg_id: serenity::MessageId,
    request_owner: serenity::UserId,
    request_owner_name: String,
    prompt: String,
    origin: IntakeOrigin,
    preloaded_uploads: Vec<String>,
) -> Result<(), super::super::super::Error> {
    dispatch_text_intake(
        deps,
        IntakeSubmission {
            provider,
            request: super::super::message_handler::IntakeRequest {
                channel_id,
                user_msg_id,
                request_owner,
                request_owner_name,
                user_text: prompt,
                reply_to_user_message: false,
                defer_watcher_resume: false,
                wait_for_completion: false,
                merge_consecutive: false,
                reply_context: None,
                has_reply_boundary: false,
                dm_hint: None,
                turn_kind: super::super::TurnKind::Foreground,
                preserve_on_cancel: false,
            },
            origin,
            preserve_on_cancel: false,
            has_nonportable_uploads: !preloaded_uploads.is_empty(),
            preloaded_uploads,
            voice_announcement: None,
        },
    )
    .await
}
