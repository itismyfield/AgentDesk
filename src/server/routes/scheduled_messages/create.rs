//! Create validation keeps the effective text and image contracts together.

use super::*;

pub(super) async fn validate_create(
    pool: &PgPool,
    body: &CreateScheduledMessageBody,
    required_discord_mention_user_ids: &[String],
) -> Result<NewScheduledMessage, AppError> {
    let content = body.content.trim();
    if content.is_empty() {
        return Err(app_error(
            StatusCode::BAD_REQUEST,
            "content must not be empty",
        ));
    }

    let delivery_kind = body
        .delivery_kind
        .as_deref()
        .unwrap_or(db::KIND_PUSH)
        .to_string();
    if delivery_kind != db::KIND_PUSH && delivery_kind != db::KIND_AGENT {
        return Err(app_error(
            StatusCode::BAD_REQUEST,
            "deliveryKind must be 'push' or 'agent'",
        ));
    }
    let discord_mention_user_ids = effective_scheduled_discord_mention_user_ids(
        body.discord_mention_user_ids.as_deref().unwrap_or_default(),
        &delivery_kind,
        required_discord_mention_user_ids,
    )?;
    if body
        .image_attachment
        .as_ref()
        .is_some_and(|image| !image.is_null())
    {
        image_input::reject_mixed(
            body.provider_targets
                .as_ref()
                .is_some_and(|value| !value.is_null()),
        )?;
    }
    let validated_provider_targets =
        provider_targets::prepare_create(body.provider_targets.as_ref(), content, &delivery_kind)
            .await?;
    let on_agent_failure = body
        .on_agent_failure
        .as_deref()
        .unwrap_or("fail")
        .to_string();
    if on_agent_failure != "fail" && on_agent_failure != "push_raw" {
        return Err(app_error(
            StatusCode::BAD_REQUEST,
            "onAgentFailure must be 'fail' or 'push_raw'",
        ));
    }

    let context_strategy = body
        .context_strategy
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(db::CONTEXT_STRATEGY_FRESH)
        .to_string();
    if context_strategy != db::CONTEXT_STRATEGY_FRESH
        && context_strategy != db::CONTEXT_STRATEGY_SNAPSHOT
    {
        return Err(app_error(
            StatusCode::BAD_REQUEST,
            "contextStrategy must be 'fresh' or 'snapshot'",
        ));
    }
    if context_strategy == db::CONTEXT_STRATEGY_SNAPSHOT && delivery_kind != db::KIND_AGENT {
        return Err(app_error(
            StatusCode::BAD_REQUEST,
            "contextStrategy 'snapshot' is only valid for agent delivery",
        ));
    }
    let on_context_failure = body
        .on_context_failure
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(db::ON_CONTEXT_FAILURE_FAIL)
        .to_string();
    if on_context_failure != db::ON_CONTEXT_FAILURE_FAIL
        && on_context_failure != db::ON_CONTEXT_FAILURE_FRESH
    {
        return Err(app_error(
            StatusCode::BAD_REQUEST,
            "onContextFailure must be 'fail' or 'fresh'",
        ));
    }

    let timezone = body
        .timezone
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "Asia/Seoul".to_string());
    let schedule = body
        .schedule
        .clone()
        .filter(|value| !value.trim().is_empty());
    let mut scheduled_at = parse_rfc3339("scheduledAt", &body.scheduled_at)?;
    let expires_at = match body.expires_at.as_deref() {
        Some(value) => Some(parse_rfc3339("expiresAt", value)?),
        None => None,
    };

    let now = Utc::now();
    if scheduled_at < now - Duration::seconds(PAST_TOLERANCE_SECS) {
        match schedule.as_deref() {
            Some(schedule) => {
                scheduled_at =
                    crate::services::scheduling::next_due_after(schedule, &timezone, now)
                        .map_err(|error| app_error(StatusCode::BAD_REQUEST, format!("{error}")))?;
            }
            None => {
                return Err(app_error(
                    StatusCode::BAD_REQUEST,
                    "scheduledAt is in the past and no schedule is set",
                ));
            }
        }
    } else if let Some(schedule) = schedule.as_deref() {
        crate::services::scheduling::next_due_after(schedule, &timezone, now)
            .map_err(|error| app_error(StatusCode::BAD_REQUEST, format!("{error}")))?;
    }

    if let Some(expires_at) = expires_at {
        if expires_at <= scheduled_at {
            return Err(app_error(
                StatusCode::BAD_REQUEST,
                "expiresAt must be after scheduledAt",
            ));
        }
    }

    let target_channel_id = normalize_target_channel_id(
        body.target_channel_id
            .clone()
            .filter(|value| !value.trim().is_empty()),
    )?;
    if !discord_mention_user_ids.is_empty() && target_channel_id.is_none() {
        return Err(app_error(
            StatusCode::BAD_REQUEST,
            "discordMentionUserIds requires targetChannelId",
        ));
    }
    validate_discord_rendered_content_length(content, &discord_mention_user_ids)?;
    if !discord_mention_user_ids.is_empty() {
        ensure_discord_mentions_rollout_ready(pool).await?;
    }
    let image_attachment = image_input::decode(body.image_attachment.as_ref())?;
    if image_attachment.is_some() {
        image_input::validate_context(image_input::Context {
            kind: &delivery_kind,
            target: target_channel_id.as_deref(),
            providers: validated_provider_targets.is_some(),
            content,
            mentions: &discord_mention_user_ids,
        })?;
        image_input::ensure_rollout(pool).await?;
    }
    let agent_id = body
        .agent_id
        .clone()
        .filter(|value| !value.trim().is_empty());
    let agent_instruction = body
        .agent_instruction
        .clone()
        .filter(|value| !value.trim().is_empty());

    validate_agent_only_fields(
        &delivery_kind,
        agent_id.as_deref(),
        agent_instruction.as_deref(),
        body.on_agent_failure.is_some(),
    )?;

    validate_targeting(
        pool,
        &delivery_kind,
        target_channel_id.as_deref(),
        agent_id.as_deref(),
        validated_provider_targets.is_some(),
    )
    .await?;

    Ok(NewScheduledMessage {
        image_attachment,
        content: content.to_string(),
        discord_mention_user_ids,
        title: body.title.clone().filter(|value| !value.trim().is_empty()),
        target_channel_id,
        bot: scheduled_message_bot_or_default(body.bot.as_deref()),
        delivery_kind,
        agent_id,
        agent_instruction,
        on_agent_failure,
        scheduled_at,
        schedule,
        timezone,
        expires_at,
        source: body
            .source
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "api".to_string()),
        created_by: body
            .created_by
            .clone()
            .filter(|value| !value.trim().is_empty()),
        dedupe_key: body
            .dedupe_key
            .clone()
            .filter(|value| !value.trim().is_empty()),
        provider_targets: validated_provider_targets
            .as_ref()
            .map(|targets| targets.stored.clone()),
        provider_target_summary: validated_provider_targets.map(|targets| targets.summary),
        context_strategy,
        context_snapshot_id: None,
        on_context_failure,
    })
}
