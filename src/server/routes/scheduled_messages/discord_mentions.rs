//! Validation and rollout checks for Discord-only scheduled mentions.

use axum::http::StatusCode;
use sqlx::PgPool;

use super::{AppError, app_error};
use crate::db::scheduled_messages as db;

const MAX_USER_IDS: usize = 20;

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
    if user_ids.len() > MAX_USER_IDS {
        return Err(app_error(
            StatusCode::BAD_REQUEST,
            format!("discordMentionUserIds must contain at most {MAX_USER_IDS} IDs"),
        ));
    }
    let mut normalized = Vec::with_capacity(user_ids.len());
    for user_id in user_ids {
        let user_id = user_id.trim();
        let valid = !user_id.is_empty()
            && !user_id.starts_with('0')
            && user_id.bytes().all(|byte| byte.is_ascii_digit())
            && user_id.parse::<u64>().is_ok_and(|value| value > 0)
            && !normalized.iter().any(|existing| existing == user_id);
        if !valid {
            return Err(app_error(
                StatusCode::BAD_REQUEST,
                "discordMentionUserIds must contain unique positive Discord user IDs",
            ));
        }
        normalized.push(user_id.to_string());
    }
    Ok(normalized)
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
