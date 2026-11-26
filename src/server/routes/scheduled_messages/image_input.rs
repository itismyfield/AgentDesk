//! Bounded JSON uploads and effective image-definition validation.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::Value as JsonValue;

use super::{AppError, StatusCode, app_error};
use crate::services::discord::outbound::image_format;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateScheduledMessageBody {
    pub image_attachment: Option<JsonValue>,
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
    /// #4658: 'fresh' or a source-channel context `snapshot`.
    pub context_strategy: Option<String>,
    /// #4658: 'fail' (default, fail-closed) or 'fresh' (opt-in degrade).
    pub on_context_failure: Option<String>,
}

pub(super) const BODY_LIMIT: usize = 12 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Upload<'a> {
    filename: &'a str,
    content_type: &'a str,
    data_base64: &'a str,
}

fn invalid(message: &str) -> AppError {
    app_error(StatusCode::BAD_REQUEST, message)
}

pub(super) fn decode(
    value: Option<&JsonValue>,
) -> Result<Option<crate::db::scheduled_messages::ScheduledMessageImageAttachment>, AppError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let upload = Upload::deserialize(value).map_err(|_| {
        invalid("imageAttachment requires filename, contentType and dataBase64 strings")
    })?;
    if upload.filename.is_empty()
        || upload.filename.len() > 128
        || !upload.filename.as_bytes()[0].is_ascii_alphanumeric()
        || !upload
            .filename
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        return Err(invalid(
            "imageAttachment.filename must be a plain ASCII filename up to 128 bytes",
        ));
    }
    let content_type = upload.content_type.to_ascii_lowercase();
    if !image_format::extension_matches(upload.filename, &content_type) {
        return Err(invalid(
            "imageAttachment filename extension and MIME must match PNG, JPEG, GIF or WebP",
        ));
    }
    if !image_format::encoded_fits(upload.data_base64.len()) {
        return Err(invalid("imageAttachment exceeds 8 MiB"));
    }
    let data = STANDARD
        .decode(upload.data_base64)
        .map_err(|_| invalid("imageAttachment.dataBase64 must be canonical standard base64"))?;
    image_format::check_bytes(&content_type, &data).map_err(|rejected| {
        invalid(match rejected {
            image_format::Rejected::Size => "imageAttachment must contain 1 byte to 8 MiB",
            image_format::Rejected::Magic => "imageAttachment magic must match MIME",
        })
    })?;
    Ok(Some(
        crate::db::scheduled_messages::ScheduledMessageImageAttachment {
            filename: upload.filename.to_owned(),
            content_type,
            data,
        },
    ))
}

pub(super) fn reject_mixed(has_provider_targets: bool) -> Result<(), AppError> {
    if has_provider_targets {
        return Err(invalid(
            "imageAttachment cannot be combined with providerTargets",
        ));
    }
    Ok(())
}

pub(super) struct Context<'a> {
    pub kind: &'a str,
    pub target: Option<&'a str>,
    pub providers: bool,
    pub content: &'a str,
    pub mentions: &'a [String],
}

pub(super) fn validate_context(context: Context<'_>) -> Result<(), AppError> {
    if context.kind != crate::db::scheduled_messages::KIND_PUSH {
        return Err(invalid("imageAttachment is only valid for push delivery"));
    }
    if context.target.is_none() {
        return Err(invalid(
            "imageAttachment requires a Discord targetChannelId",
        ));
    }
    reject_mixed(context.providers)?;
    let units = context.content.encode_utf16().count()
        + context
            .mentions
            .iter()
            .map(|id| id.len() + 4)
            .sum::<usize>();
    if context.content.trim().is_empty()
        || units > crate::services::discord::outbound::DISCORD_HARD_LIMIT_CHARS
    {
        return Err(invalid(
            "image content after Discord mentions must contain 1 to 2000 UTF-16 units",
        ));
    }
    Ok(())
}

pub(super) fn extract<T: 'static>(
    body: Result<axum::Json<T>, axum::extract::rejection::JsonRejection>,
) -> Result<T, AppError> {
    body.map(|axum::Json(body)| body).map_err(|error| {
        let status = if error.status() == StatusCode::UNPROCESSABLE_ENTITY {
            StatusCode::BAD_REQUEST
        } else {
            error.status()
        };
        // A JsonValue body has no schema, so every path segment is a client key.
        let schemaless = std::any::TypeId::of::<T>() == std::any::TypeId::of::<JsonValue>();
        let detail = data_error_detail(&error).filter(|_| !schemaless);
        let message = match detail {
            Some(detail) => format!("invalid scheduled message JSON: {detail}"),
            None => "invalid scheduled message JSON".to_owned(),
        };
        crate::error::AppError::new(status, crate::error::ErrorCode::Validation, message)
    })
}

fn data_error_detail(error: &axum::extract::rejection::JsonRejection) -> Option<String> {
    let axum::extract::rejection::JsonRejection::JsonDataError(error) = error else {
        return None;
    };
    redacted_detail(&std::error::Error::source(error)?.to_string())
}

/// Field-level serde detail, withheld whenever it touches imageAttachment so base64 never echoes.
/// Client literals ("…" strings, `…` values) become `<redacted>`; `field` names, paths and types stay.
fn redacted_detail(detail: &str) -> Option<String> {
    if detail.contains("imageAttachment") || detail.contains("dataBase64") {
        return None;
    }
    // JsonValue paths (providerTargets, any array-form field) carry client-chosen keys.
    if detail.starts_with("providerTargets") || detail.starts_with('[') {
        return None;
    }
    let mut out = String::with_capacity(detail.len());
    let mut chars = detail.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                while let Some(c) = chars.next() {
                    match c {
                        '\\' => _ = chars.next(),
                        '"' => break,
                        _ => {}
                    }
                }
                out.push_str("\"<redacted>\"");
            }
            '`' => {
                let literal: String = chars.by_ref().take_while(|&c| c != '`').collect();
                let kept = if out.ends_with("field ") {
                    literal.as_str()
                } else {
                    "<redacted>"
                };
                out.push_str(&format!("`{kept}`"));
            }
            _ => out.push(c),
        }
    }
    Some(out.chars().take(200).collect())
}

#[cfg(test)]
#[path = "image_activation_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "image_json_error_tests.rs"]
mod json_error_tests;
