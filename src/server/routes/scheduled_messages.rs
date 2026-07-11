//! HTTP API for the scheduled-message reservation pool.
//!
//! Design: docs/design/scheduled-messages.md. Handlers delegate all SQL to
//! `crate::db::scheduled_messages` and fire execution to
//! `crate::services::scheduled_messages`.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::{Value as JsonValue, json};
use sqlx::PgPool;

use super::AppState;
use crate::db::scheduled_messages as db;
use crate::db::scheduled_messages::{
    CancelOutcome, ListFilters, NewScheduledMessage, ScheduledMessagePatch, ScheduledMessageRow,
};

#[cfg(test)]
mod postgres_tests;

/// Freshly created reservations may point slightly into the past (clock skew,
/// slow clients); anything older is a user error for one-shot messages.
const PAST_TOLERANCE_SECS: i64 = 60;

/// Info-only scheduled pushes must not wake an agent that owns the target
/// channel. `announce` is the authoritative agent-to-agent trigger bot, while
/// `notify` is the canonical non-actionable delivery sink.
const DEFAULT_SCHEDULED_MESSAGE_BOT: &str = "notify";

type ApiResponse = (StatusCode, Json<JsonValue>);

fn error_response(status: StatusCode, message: impl Into<String>) -> ApiResponse {
    (status, Json(json!({"error": message.into()})))
}

fn pool_or_unavailable(state: &AppState) -> Result<&PgPool, ApiResponse> {
    state
        .pg_pool_ref()
        .ok_or_else(|| error_response(StatusCode::SERVICE_UNAVAILABLE, "postgres pool unavailable"))
}

fn parse_rfc3339(field: &str, value: &str) -> Result<DateTime<Utc>, ApiResponse> {
    DateTime::parse_from_rfc3339(value)
        .map(|parsed| parsed.with_timezone(&Utc))
        .map_err(|error| {
            error_response(
                StatusCode::BAD_REQUEST,
                format!("{field} must be an RFC3339 timestamp: {error}"),
            )
        })
}

fn scheduled_message_bot_or_default(bot: Option<&str>) -> String {
    bot.map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_SCHEDULED_MESSAGE_BOT)
        .to_string()
}

// ── Create ──────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateScheduledMessageBody {
    pub content: String,
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
}

/// POST /api/scheduled-messages
pub async fn create_scheduled_message(
    State(state): State<AppState>,
    Json(body): Json<CreateScheduledMessageBody>,
) -> ApiResponse {
    let pool = match pool_or_unavailable(&state) {
        Ok(pool) => pool,
        Err(response) => return response,
    };

    let new = match validate_create(pool, &body).await {
        Ok(new) => new,
        Err(response) => return response,
    };

    match db::insert_scheduled_message_pg(pool, &new).await {
        Ok(row) => (
            StatusCode::CREATED,
            Json(json!({"scheduledMessage": row.to_api_json()})),
        ),
        Err(error) if db::is_unique_violation(&error) => {
            let existing = match new.dedupe_key.as_deref() {
                Some(key) => db::find_active_by_dedupe_key_pg(pool, key)
                    .await
                    .ok()
                    .flatten(),
                None => None,
            };
            (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "an active scheduled message with this dedupeKey already exists",
                    "scheduledMessage": existing.map(|row| row.to_api_json()),
                })),
            )
        }
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("create scheduled message: {error}"),
        ),
    }
}

async fn validate_create(
    pool: &PgPool,
    body: &CreateScheduledMessageBody,
) -> Result<NewScheduledMessage, ApiResponse> {
    let content = body.content.trim();
    if content.is_empty() {
        return Err(error_response(
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
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "deliveryKind must be 'push' or 'agent'",
        ));
    }
    let on_agent_failure = body
        .on_agent_failure
        .as_deref()
        .unwrap_or("fail")
        .to_string();
    if on_agent_failure != "fail" && on_agent_failure != "push_raw" {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "onAgentFailure must be 'fail' or 'push_raw'",
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
            // Recurring definitions self-correct: the pool's contract is "next
            // occurrence of the schedule", not the possibly-stale first slot.
            Some(schedule) => {
                scheduled_at = crate::services::routines::next_due_after(schedule, &timezone, now)
                    .map_err(|error| error_response(StatusCode::BAD_REQUEST, format!("{error}")))?;
            }
            None => {
                return Err(error_response(
                    StatusCode::BAD_REQUEST,
                    "scheduledAt is in the past and no schedule is set",
                ));
            }
        }
    } else if let Some(schedule) = schedule.as_deref() {
        // Validate grammar/timezone up front so the fire path never hits an
        // unparseable recurrence.
        crate::services::routines::next_due_after(schedule, &timezone, now)
            .map_err(|error| error_response(StatusCode::BAD_REQUEST, format!("{error}")))?;
    }

    if let Some(expires_at) = expires_at {
        if expires_at <= scheduled_at {
            return Err(error_response(
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
    )
    .await?;

    Ok(NewScheduledMessage {
        content: content.to_string(),
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
    })
}

fn validate_agent_only_fields(
    delivery_kind: &str,
    agent_id: Option<&str>,
    agent_instruction: Option<&str>,
    on_agent_failure_explicit: bool,
) -> Result<(), ApiResponse> {
    if delivery_kind != db::KIND_PUSH {
        return Ok(());
    }
    if agent_id.is_some() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "agentId is only valid for agent delivery",
        ));
    }
    if agent_instruction.is_some() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "agentInstruction is only valid for agent delivery",
        ));
    }
    if on_agent_failure_explicit {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "onAgentFailure is only valid for agent delivery",
        ));
    }
    Ok(())
}

async fn validate_targeting(
    pool: &PgPool,
    delivery_kind: &str,
    target_channel_id: Option<&str>,
    agent_id: Option<&str>,
) -> Result<(), ApiResponse> {
    if delivery_kind == db::KIND_PUSH {
        if target_channel_id.is_none() {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "targetChannelId is required for push delivery",
            ));
        }
        return Ok(());
    }

    let Some(agent_id) = agent_id else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "agentId is required for agent delivery",
        ));
    };
    let bindings = crate::db::agents::load_agent_channel_bindings_pg(pool, agent_id)
        .await
        .map_err(|error| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("load agent bindings: {error}"),
            )
        })?;
    let Some(bindings) = bindings else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("agent '{agent_id}' not found"),
        ));
    };
    let Some(primary_channel) = bindings.primary_channel() else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("agent '{agent_id}' has no primary Discord channel"),
        ));
    };
    if resolve_channel_reference(&primary_channel).is_none() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("agent '{agent_id}' has an invalid primary Discord channel"),
        ));
    }
    if bindings.resolved_primary_provider_kind().is_none() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("agent '{agent_id}' has no configured primary provider"),
        ));
    }
    Ok(())
}

fn resolve_channel_reference(value: &str) -> Option<u64> {
    let value = value.trim();
    crate::services::dispatches::outbox_route::resolve_channel_alias_pub(value)
        .or_else(|| value.parse::<u64>().ok())
        .filter(|channel_id| *channel_id > 0)
}

fn normalize_target_channel_id(value: Option<String>) -> Result<Option<String>, ApiResponse> {
    let Some(value) = value else {
        return Ok(None);
    };
    resolve_channel_reference(&value)
        .map(|channel_id| Some(channel_id.to_string()))
        .ok_or_else(|| {
            error_response(
                StatusCode::BAD_REQUEST,
                "targetChannelId must be a positive Discord channel id or known alias",
            )
        })
}

// ── List / Get ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListScheduledMessagesQuery {
    pub status: Option<String>,
    pub delivery_kind: Option<String>,
    pub agent_id: Option<String>,
    pub target_channel_id: Option<String>,
    pub due_before: Option<String>,
    pub due_after: Option<String>,
    pub before: Option<String>,
    pub limit: Option<i64>,
}

/// GET /api/scheduled-messages
pub async fn list_scheduled_messages(
    State(state): State<AppState>,
    Query(params): Query<ListScheduledMessagesQuery>,
) -> ApiResponse {
    let pool = match pool_or_unavailable(&state) {
        Ok(pool) => pool,
        Err(response) => return response,
    };
    let mut filters = ListFilters {
        status: params.status,
        delivery_kind: params.delivery_kind,
        agent_id: params.agent_id,
        target_channel_id: params.target_channel_id,
        limit: params.limit.unwrap_or(50),
        ..ListFilters::default()
    };
    for (field, source, slot) in [
        ("dueBefore", &params.due_before, &mut filters.due_before),
        ("dueAfter", &params.due_after, &mut filters.due_after),
        ("before", &params.before, &mut filters.before),
    ] {
        if let Some(value) = source.as_deref() {
            match parse_rfc3339(field, value) {
                Ok(parsed) => *slot = Some(parsed),
                Err(response) => return response,
            }
        }
    }

    match db::list_scheduled_messages_pg(pool, &filters).await {
        Ok(rows) => {
            let next_cursor = rows.last().map(|row| row.created_at.to_rfc3339());
            let messages: Vec<JsonValue> =
                rows.iter().map(ScheduledMessageRow::to_api_json).collect();
            (
                StatusCode::OK,
                Json(json!({"scheduledMessages": messages, "nextCursor": next_cursor})),
            )
        }
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("list scheduled messages: {error}"),
        ),
    }
}

/// GET /api/scheduled-messages/{id}
pub async fn get_scheduled_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResponse {
    let pool = match pool_or_unavailable(&state) {
        Ok(pool) => pool,
        Err(response) => return response,
    };
    let row = match db::get_scheduled_message_pg(pool, &id).await {
        Ok(Some(row)) => row,
        Ok(None) => return error_response(StatusCode::NOT_FOUND, "scheduled message not found"),
        Err(error) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("load scheduled message: {error}"),
            );
        }
    };
    let deliveries = match db::list_deliveries_pg(pool, &id, 5, None).await {
        Ok(deliveries) => render_deliveries(pool, deliveries).await,
        Err(error) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("load deliveries: {error}"),
            );
        }
    };
    (
        StatusCode::OK,
        Json(json!({
            "scheduledMessage": row.to_api_json(),
            "recentDeliveries": deliveries,
        })),
    )
}

// ── Patch ───────────────────────────────────────────────────────────────────

/// PATCH /api/scheduled-messages/{id}
///
/// Body is read as raw JSON so "field absent" (keep) and "field: null"
/// (clear) stay distinguishable for the nullable columns.
pub async fn patch_scheduled_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<JsonValue>,
) -> ApiResponse {
    let pool = match pool_or_unavailable(&state) {
        Ok(pool) => pool,
        Err(response) => return response,
    };
    let Some(body) = body.as_object() else {
        return error_response(StatusCode::BAD_REQUEST, "body must be a JSON object");
    };

    let existing = match db::get_scheduled_message_pg(pool, &id).await {
        Ok(Some(row)) => row,
        Ok(None) => return error_response(StatusCode::NOT_FOUND, "scheduled message not found"),
        Err(error) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("load scheduled message: {error}"),
            );
        }
    };
    if existing.status != db::STATUS_SCHEDULED {
        return error_response(
            StatusCode::CONFLICT,
            format!(
                "only scheduled messages can be edited (current status: {})",
                existing.status
            ),
        );
    }

    let patch = match build_patch(pool, body, &existing).await {
        Ok(patch) => patch,
        Err(response) => return response,
    };

    match db::update_scheduled_message_pg(pool, &id, &patch).await {
        Ok(Some(row)) => (
            StatusCode::OK,
            Json(json!({"scheduledMessage": row.to_api_json()})),
        ),
        // The row left 'scheduled' between the read and the update (fired or
        // was canceled mid-request).
        Ok(None) => error_response(
            StatusCode::CONFLICT,
            "scheduled message is no longer editable",
        ),
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("save scheduled message: {error}"),
        ),
    }
}

fn patch_string(
    body: &serde_json::Map<String, JsonValue>,
    key: &str,
) -> Result<Option<Option<String>>, String> {
    match body.get(key) {
        None => Ok(None),
        Some(JsonValue::Null) => Ok(Some(None)),
        Some(JsonValue::String(value)) => {
            let trimmed = value.trim();
            Ok(Some((!trimmed.is_empty()).then(|| trimmed.to_string())))
        }
        Some(_) => Err(format!("{key} must be a string or null")),
    }
}

fn normalize_effective_scheduled_at(
    scheduled_at: DateTime<Utc>,
    schedule: Option<&str>,
    timezone: &str,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>, String> {
    if let Some(schedule) = schedule {
        let next =
            crate::services::routines::next_due_after_anchor(schedule, timezone, scheduled_at, now)
                .map_err(|error| format!("{error}"))?;
        return Ok(
            if scheduled_at < now - Duration::seconds(PAST_TOLERANCE_SECS) {
                next
            } else {
                scheduled_at
            },
        );
    }
    if scheduled_at < now - Duration::seconds(PAST_TOLERANCE_SECS) {
        return Err("scheduledAt is in the past and no schedule is set".to_string());
    }
    Ok(scheduled_at)
}

async fn build_patch(
    pool: &PgPool,
    body: &serde_json::Map<String, JsonValue>,
    existing: &ScheduledMessageRow,
) -> Result<ScheduledMessagePatch, ApiResponse> {
    let bad_request = |message: String| error_response(StatusCode::BAD_REQUEST, message);
    let mut patch = ScheduledMessagePatch::default();

    if let Some(content) = patch_string(body, "content").map_err(|e| bad_request(e))? {
        let content = content.ok_or_else(|| bad_request("content must not be null".to_string()))?;
        patch.content = Some(content);
    }
    patch.title = patch_string(body, "title").map_err(|e| bad_request(e))?;
    patch.target_channel_id =
        match patch_string(body, "targetChannelId").map_err(|e| bad_request(e))? {
            Some(value) => Some(normalize_target_channel_id(value)?),
            None => None,
        };
    if let Some(bot) = patch_string(body, "bot").map_err(|e| bad_request(e))? {
        patch.bot = Some(bot.ok_or_else(|| bad_request("bot must not be null".to_string()))?);
    }
    patch.agent_id = patch_string(body, "agentId").map_err(|e| bad_request(e))?;
    patch.agent_instruction = patch_string(body, "agentInstruction").map_err(|e| bad_request(e))?;
    if let Some(on_failure) = patch_string(body, "onAgentFailure").map_err(|e| bad_request(e))? {
        let on_failure =
            on_failure.ok_or_else(|| bad_request("onAgentFailure must not be null".to_string()))?;
        if on_failure != "fail" && on_failure != "push_raw" {
            return Err(bad_request(
                "onAgentFailure must be 'fail' or 'push_raw'".to_string(),
            ));
        }
        patch.on_agent_failure = Some(on_failure);
    }
    if let Some(timezone) = patch_string(body, "timezone").map_err(|e| bad_request(e))? {
        patch.timezone =
            Some(timezone.ok_or_else(|| bad_request("timezone must not be null".to_string()))?);
    }
    patch.schedule = patch_string(body, "schedule").map_err(|e| bad_request(e))?;
    if let Some(scheduled_at) = patch_string(body, "scheduledAt").map_err(|e| bad_request(e))? {
        let scheduled_at =
            scheduled_at.ok_or_else(|| bad_request("scheduledAt must not be null".to_string()))?;
        patch.scheduled_at = Some(parse_rfc3339("scheduledAt", &scheduled_at)?);
    }
    if let Some(expires_at) = patch_string(body, "expiresAt").map_err(|e| bad_request(e))? {
        patch.expires_at = Some(match expires_at {
            Some(value) => Some(parse_rfc3339("expiresAt", &value)?),
            None => None,
        });
    }

    // Validate the effective (merged) definition with the create rules.
    let effective_kind = existing.delivery_kind.as_str();
    let effective_target = patch
        .target_channel_id
        .clone()
        .unwrap_or_else(|| existing.target_channel_id.clone());
    let effective_agent = patch
        .agent_id
        .clone()
        .unwrap_or_else(|| existing.agent_id.clone());
    let effective_agent_instruction = patch
        .agent_instruction
        .clone()
        .unwrap_or_else(|| existing.agent_instruction.clone());
    validate_agent_only_fields(
        effective_kind,
        effective_agent.as_deref(),
        effective_agent_instruction.as_deref(),
        patch.on_agent_failure.is_some(),
    )?;
    validate_targeting(
        pool,
        effective_kind,
        effective_target.as_deref(),
        effective_agent.as_deref(),
    )
    .await?;

    let mut effective_scheduled_at = patch.scheduled_at.unwrap_or(existing.scheduled_at);
    let effective_schedule = patch
        .schedule
        .clone()
        .unwrap_or_else(|| existing.schedule.clone());
    let effective_timezone = patch
        .timezone
        .clone()
        .unwrap_or_else(|| existing.timezone.clone());
    let now = Utc::now();
    let normalized_scheduled_at = normalize_effective_scheduled_at(
        effective_scheduled_at,
        effective_schedule.as_deref(),
        &effective_timezone,
        now,
    )
    .map_err(bad_request)?;
    if normalized_scheduled_at != effective_scheduled_at {
        patch.scheduled_at = Some(normalized_scheduled_at);
        effective_scheduled_at = normalized_scheduled_at;
    }
    let effective_expires_at = patch.expires_at.unwrap_or(existing.expires_at);
    if let Some(expires_at) = effective_expires_at {
        if expires_at <= effective_scheduled_at {
            return Err(bad_request(
                "expiresAt must be after scheduledAt".to_string(),
            ));
        }
    }

    Ok(patch)
}

// ── Cancel / trigger-now / deliveries ───────────────────────────────────────

/// DELETE /api/scheduled-messages/{id}
pub async fn cancel_scheduled_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResponse {
    let pool = match pool_or_unavailable(&state) {
        Ok(pool) => pool,
        Err(response) => return response,
    };
    match db::cancel_scheduled_message_pg(pool, &id).await {
        Ok(CancelOutcome::NotFound) => {
            error_response(StatusCode::NOT_FOUND, "scheduled message not found")
        }
        Ok(CancelOutcome::AlreadyTerminal(status)) => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!("scheduled message already terminal (status: {status})"),
                "status": status,
            })),
        ),
        Ok(CancelOutcome::Canceled {
            was_firing,
            handoff_started,
        }) => {
            let note = if handoff_started {
                Some("delivery was already handed off; downstream delivery may still complete")
            } else {
                was_firing.then_some("in-flight delivery was canceled before handoff")
            };
            (
                StatusCode::OK,
                Json(json!({"canceled": true, "note": note})),
            )
        }
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("cancel scheduled message: {error}"),
        ),
    }
}

/// POST /api/scheduled-messages/{id}/trigger-now
pub async fn trigger_scheduled_message_now(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResponse {
    let pool = match pool_or_unavailable(&state) {
        Ok(pool) => pool,
        Err(response) => return response,
    };
    if state.health_registry.is_none() {
        match db::get_scheduled_message_pg(pool, &id).await {
            Ok(Some(row)) if row.status == db::STATUS_SCHEDULED => {
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Discord runtime is unavailable for scheduled delivery",
                );
            }
            Ok(_) => {}
            Err(error) => {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("load scheduled message: {error}"),
                );
            }
        }
    }
    let claimed = match db::trigger_now_pg(
        pool,
        &id,
        "api:trigger-now",
        crate::services::scheduled_messages::LEASE_SECS,
    )
    .await
    {
        Ok(Some(claimed)) => claimed,
        Ok(None) => {
            // Missing, terminal, firing, or claimed by a concurrent worker.
            return match db::get_scheduled_message_pg(pool, &id).await {
                Ok(Some(row)) => error_response(
                    StatusCode::CONFLICT,
                    format!(
                        "scheduled message is not triggerable (status: {})",
                        row.status
                    ),
                ),
                Ok(None) => error_response(StatusCode::NOT_FOUND, "scheduled message not found"),
                Err(error) => error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("load scheduled message: {error}"),
                ),
            };
        }
        Err(error) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("trigger scheduled message: {error}"),
            );
        }
    };

    let delivery_id = claimed.delivery_id.clone();
    let fire_pool = pool.clone();
    let health_registry = state.health_registry.clone();
    tokio::spawn(async move {
        crate::services::scheduled_messages::fire_claimed(
            &fire_pool,
            health_registry.as_deref(),
            claimed,
            Utc::now(),
        )
        .await;
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({"delivery": {"id": delivery_id, "status": "running"}})),
    )
}

#[derive(Debug, Deserialize)]
pub struct ListDeliveriesQuery {
    pub limit: Option<i64>,
    pub before: Option<String>,
}

/// GET /api/scheduled-messages/{id}/deliveries
pub async fn list_scheduled_message_deliveries(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<ListDeliveriesQuery>,
) -> ApiResponse {
    let pool = match pool_or_unavailable(&state) {
        Ok(pool) => pool,
        Err(response) => return response,
    };
    let before = match params.before.as_deref() {
        Some(value) => match parse_rfc3339("before", value) {
            Ok(parsed) => Some(parsed),
            Err(response) => return response,
        },
        None => None,
    };
    match db::get_scheduled_message_pg(pool, &id).await {
        Ok(Some(_)) => {}
        Ok(None) => return error_response(StatusCode::NOT_FOUND, "scheduled message not found"),
        Err(error) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("load scheduled message: {error}"),
            );
        }
    }
    match db::list_deliveries_pg(pool, &id, params.limit.unwrap_or(20), before).await {
        Ok(deliveries) => {
            let rendered = render_deliveries(pool, deliveries).await;
            (StatusCode::OK, Json(json!({"deliveries": rendered})))
        }
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("list deliveries: {error}"),
        ),
    }
}

/// Enrich delivery rows with the final message_outbox state of their handoff
/// rows (push handoff is terminal here; the outbox owns delivery from there).
async fn render_deliveries(
    pool: &PgPool,
    deliveries: Vec<crate::db::scheduled_messages::DeliveryRow>,
) -> Vec<JsonValue> {
    let outbox_ids: Vec<i64> = deliveries
        .iter()
        .flat_map(|delivery| [delivery.outbox_id, delivery.fallback_outbox_id])
        .flatten()
        .collect();
    let statuses = db::outbox_statuses_for_deliveries_pg(pool, &outbox_ids)
        .await
        .unwrap_or_default();
    let status_of = |id: Option<i64>| {
        id.and_then(|id| {
            statuses
                .iter()
                .find(|(outbox_id, _)| *outbox_id == id)
                .map(|(_, status)| status.clone())
        })
    };
    deliveries
        .into_iter()
        .map(|delivery| {
            let mut rendered = delivery.to_api_json();
            if let Some(object) = rendered.as_object_mut() {
                object.insert(
                    "outboxStatus".to_string(),
                    json!(status_of(delivery.outbox_id)),
                );
                object.insert(
                    "fallbackOutboxStatus".to_string(),
                    json!(status_of(delivery.fallback_outbox_id)),
                );
            }
            rendered
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn target_channel_ids_are_normalized_and_invalid_values_rejected() {
        assert_eq!(
            normalize_target_channel_id(Some(" 123456789 ".to_string())).unwrap(),
            Some("123456789".to_string())
        );
        assert!(normalize_target_channel_id(Some("0".to_string())).is_err());
        assert!(
            normalize_target_channel_id(Some("not-a-known-channel-alias".to_string())).is_err()
        );
        assert_eq!(normalize_target_channel_id(None).unwrap(), None);
    }

    #[test]
    fn patch_rejects_stale_effective_one_shot_time() {
        let now = Utc.with_ymd_and_hms(2026, 7, 11, 6, 0, 0).unwrap();
        let stale = now - Duration::minutes(2);
        assert_eq!(
            normalize_effective_scheduled_at(stale, None, "UTC", now).unwrap_err(),
            "scheduledAt is in the past and no schedule is set"
        );
    }

    #[test]
    fn patch_realigns_stale_recurring_time() {
        let now = Utc.with_ymd_and_hms(2026, 7, 11, 6, 0, 0).unwrap();
        let stale = now - Duration::minutes(17);
        let normalized =
            normalize_effective_scheduled_at(stale, Some("@every 10m"), "UTC", now).unwrap();
        assert_eq!(normalized, now + Duration::minutes(3));
    }
}
