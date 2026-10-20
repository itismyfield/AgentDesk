//! Reject image input until the transport and fleet capability gates are ready.

use serde::{Deserialize, Deserializer, de::Error};
use serde_json::{Map, Value as JsonValue};

use super::{AppError, StatusCode, app_error};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateScheduledMessageBody {
    #[serde(
        default,
        rename = "imageAttachment",
        deserialize_with = "reject_create"
    )]
    pub _image_attachment: (),
    pub content: String,
    #[serde(default)]
    pub discord_mention_user_ids: Option<Vec<String>>,
    pub title: Option<String>,
    pub target_channel_id: Option<String>,
    pub bot: Option<String>,
    pub delivery_kind: Option<String>,
    pub agent_id: Option<String>,
    pub agent_instruction: Option<String>,
    pub on_agent_failure: Option<String>,
    pub scheduled_at: String,
    pub schedule: Option<String>,
    pub timezone: Option<String>,
    pub expires_at: Option<String>,
    pub source: Option<String>,
    pub created_by: Option<String>,
    pub dedupe_key: Option<String>,
    pub provider_targets: Option<JsonValue>,
    /// 'fresh' or a source-channel context `snapshot`.
    pub context_strategy: Option<String>,
    /// 'fail' (default, fail-closed) or 'fresh' (opt-in degrade).
    pub on_context_failure: Option<String>,
}

pub(super) fn reject_create<'de, D: Deserializer<'de>>(_: D) -> Result<(), D::Error> {
    Err(D::Error::custom("imageAttachment is not enabled"))
}

pub(super) fn reject_patch(body: &Map<String, JsonValue>) -> Result<(), AppError> {
    if body.contains_key("imageAttachment") {
        return Err(app_error(
            StatusCode::BAD_REQUEST,
            "imageAttachment is not enabled",
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "dormant_image_tests.rs"]
mod tests;
