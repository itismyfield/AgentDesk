use axum::{
    Json,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::StatusCode,
    response::Response,
};
use chrono::Utc;
use serde_json::{Value, json};

use super::AppState;
use crate::{
    db::server_campaigns as store,
    error::{AppError, AppResult, ErrorCode},
    services::campaign_execution::{
        model::{CampaignState, Command, CommandEnvelope},
        view,
    },
};

mod dto;
mod protocol;
use dto::*;
use protocol::{body, id, store_error};

pub(super) const BODY_LIMIT_BYTES: usize = 2 * 1024 * 1024;

fn pool(state: &AppState) -> AppResult<&sqlx::PgPool> {
    state.pg_pool_ref().ok_or_else(|| {
        AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Database,
            "canonical PostgreSQL pool unavailable",
        )
    })
}

fn document(state: &CampaignState, as_of: chrono::DateTime<Utc>) -> Value {
    json!({"definition": state.definition.input(), "view": view::view(state, as_of)})
}

pub(super) async fn list(
    State(state): State<AppState>,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> AppResult<Json<Value>> {
    let Query(query) = query.map_err(|error| AppError::bad_request(error.body_text()))?;
    if !(1..=100).contains(&query.limit) || query.offset < 0 {
        return Err(AppError::bad_request(
            "limit must be 1..100 and offset nonnegative",
        ));
    }
    let campaigns = store::list(pool(&state)?, query.limit, query.offset)
        .await
        .map_err(store_error)?;
    let as_of = Utc::now();
    Ok(Json(
        json!({"campaigns": campaigns.iter().map(|state| document(state, as_of)).collect::<Vec<_>>()}),
    ))
}

pub(super) async fn detail(
    State(state): State<AppState>,
    Path(campaign_id): Path<String>,
    query: Result<Query<DetailQuery>, QueryRejection>,
) -> AppResult<Json<Value>> {
    id(&campaign_id)?;
    let Query(_) = query.map_err(|error| AppError::bad_request(error.body_text()))?;
    let campaign = store::get(pool(&state)?, &campaign_id)
        .await
        .map_err(store_error)?
        .ok_or_else(|| AppError::not_found("server campaign not found"))?;
    Ok(Json(json!({"campaign": document(&campaign, Utc::now())})))
}

pub(super) async fn create(
    State(state): State<AppState>,
    input: Result<Json<Create>, JsonRejection>,
) -> AppResult<Response> {
    let input = body(input)?;
    protocol::key(&input.idempotency_key)?;
    id(&input.id)?;
    protocol::saved(
        store::create(
            pool(&state)?,
            &input.id,
            &input.definition,
            &input.idempotency_key,
            Utc::now(),
            protocol::created,
        )
        .await
        .map_err(store_error)?,
    )
}

pub(super) async fn edit(
    State(state): State<AppState>,
    Path(campaign_id): Path<String>,
    input: Result<Json<Edit>, JsonRejection>,
) -> AppResult<Response> {
    id(&campaign_id)?;
    let input = body(input)?;
    protocol::key(&input.idempotency_key)?;
    protocol::revision(input.expected_definition_revision)?;
    let envelope = CommandEnvelope {
        idempotency_key: input.idempotency_key,
        observed_at: Utc::now(),
        command: Command::EditDefinition {
            expected_revision: input.expected_definition_revision,
            definition: input.definition,
            change: input.change,
        },
    };
    protocol::saved(
        store::execute(pool(&state)?, &campaign_id, &envelope, protocol::edited)
            .await
            .map_err(store_error)?,
    )
}

pub(super) async fn command(
    State(state): State<AppState>,
    Path(campaign_id): Path<String>,
    input: Result<Json<CommandRequest>, JsonRejection>,
) -> AppResult<Response> {
    id(&campaign_id)?;
    let input = body(input)?;
    protocol::key(&input.idempotency_key)?;
    let envelope = CommandEnvelope {
        idempotency_key: input.idempotency_key,
        observed_at: Utc::now(),
        command: input.command.into_domain()?,
    };
    protocol::saved(
        store::execute(pool(&state)?, &campaign_id, &envelope, protocol::commanded)
            .await
            .map_err(store_error)?,
    )
}

pub(super) async fn events(
    State(state): State<AppState>,
    Path(campaign_id): Path<String>,
    query: Result<Query<EventsQuery>, QueryRejection>,
) -> AppResult<Json<Value>> {
    id(&campaign_id)?;
    let Query(query) = query.map_err(|error| AppError::bad_request(error.body_text()))?;
    if !(1..=500).contains(&query.limit)
        || query.after < 0
        || query
            .run_id
            .as_ref()
            .is_some_and(|run| !crate::services::campaign_execution::definition::valid_id(run))
    {
        return Err(AppError::bad_request("invalid event pagination or run_id"));
    }
    let pool = pool(&state)?;
    store::get(pool, &campaign_id)
        .await
        .map_err(store_error)?
        .ok_or_else(|| AppError::not_found("server campaign not found"))?;
    Ok(Json(
        json!({"events": store::events(pool, &campaign_id, query.run_id.as_deref(), query.after, query.limit).await.map_err(store_error)?}),
    ))
}

#[cfg(test)]
mod http_tests;
