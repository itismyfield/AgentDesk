//! Validation and rollout checks for Discord-only scheduled mentions.

use axum::http::StatusCode;
use sqlx::PgPool;

use super::{AppError, AppState, app_error};
use crate::db::scheduled_messages as db;

pub(super) fn required_from_state(state: &AppState) -> &[String] {
    &state
        .config
        .discord
        .scheduled_message_required_mention_user_ids
}

pub(super) fn parse_patch_value(
    body: &serde_json::Map<String, serde_json::Value>,
) -> Result<Option<Vec<String>>, AppError> {
    let Some(value) = body.get("discordMentionUserIds") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(Some(Vec::new()));
    }
    serde_json::from_value::<Vec<String>>(value.clone())
        .map(Some)
        .map_err(|error| {
            app_error(
                StatusCode::BAD_REQUEST,
                format!("discordMentionUserIds must be an array or null: {error}"),
            )
        })
}

pub(super) fn validate_user_ids(
    user_ids: &[String],
    delivery_kind: &str,
) -> Result<Vec<String>, AppError> {
    if user_ids.is_empty() {
        return Ok(Vec::new());
    }
    if delivery_kind != db::KIND_PUSH {
        return Err(app_error(
            StatusCode::BAD_REQUEST,
            "discordMentionUserIds is only valid for push delivery",
        ));
    }
    crate::utils::discord::normalize_discord_recipient_ids(user_ids).map_err(|error| {
        let message = match error {
            crate::utils::discord::DiscordRecipientIdListError::TooMany => format!(
                "discordMentionUserIds must contain at most {} IDs",
                crate::utils::discord::MAX_DISCORD_RECIPIENT_IDS
            ),
            crate::utils::discord::DiscordRecipientIdListError::InvalidOrDuplicate => {
                "discordMentionUserIds must contain unique positive Discord user IDs".to_string()
            }
        };
        app_error(StatusCode::BAD_REQUEST, message)
    })
}

pub(super) fn effective_user_ids(
    requested: &[String],
    delivery_kind: &str,
    required: &[String],
) -> Result<Vec<String>, AppError> {
    if delivery_kind != db::KIND_PUSH {
        return validate_user_ids(requested, delivery_kind);
    }
    let mut combined = Vec::with_capacity(required.len() + requested.len());
    combined.extend(required.iter().cloned());
    for user_id in requested {
        let trimmed = user_id.trim();
        if !combined.iter().any(|existing| existing.trim() == trimmed) {
            combined.push(user_id.clone());
        }
    }
    validate_user_ids(&combined, delivery_kind)
}

pub(super) fn validate_rendered_content_length(
    content: &str,
    user_ids: &[String],
) -> Result<(), AppError> {
    if user_ids.is_empty() {
        return Ok(());
    }
    let prefix_len = user_ids
        .iter()
        .map(|user_id| user_id.len() + 3)
        .sum::<usize>()
        + user_ids.len();
    let hard_limit = crate::services::discord::outbound::DISCORD_HARD_LIMIT_CHARS;
    if content.chars().count() + prefix_len > hard_limit {
        return Err(app_error(
            StatusCode::BAD_REQUEST,
            format!("content plus Discord mentions must not exceed {hard_limit} characters"),
        ));
    }
    Ok(())
}

pub(super) async fn ensure_rollout_ready(pool: &PgPool) -> Result<(), AppError> {
    match db::discord_mentions_rollout_ready_pg(pool).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(app_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "discordMentionUserIds requires every online worker to advertise discord_mention_consumer_v1",
        )),
        Err(error) => Err(app_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("check scheduled Discord-mention rollout readiness: {error}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_discord_mentions_are_stable_and_preserve_requested_recipients() {
        assert_eq!(
            effective_user_ids(
                &[
                    "1469509284508340277".to_string(),
                    "1469509284508340276".to_string(),
                ],
                db::KIND_PUSH,
                &["1469509284508340276".to_string()],
            )
            .unwrap(),
            vec![
                "1469509284508340276".to_string(),
                "1469509284508340277".to_string(),
            ]
        );
    }

    #[test]
    fn required_discord_mentions_do_not_apply_to_agent_delivery() {
        assert_eq!(
            effective_user_ids(&[], db::KIND_AGENT, &["1469509284508340276".to_string()],).unwrap(),
            Vec::<String>::new()
        );
    }
}
