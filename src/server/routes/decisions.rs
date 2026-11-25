//! `POST /api/decisions` stores a Decision request under its capability hash and posts it with
//! disabled buttons; `GET /api/decisions/{id}` reads it back to that requester only.

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use serde_json::{Value, json};

use super::AppState;
use super::agents_turn_target::resolve_agent_turn_target;
use super::auth::HumanInputCaller;
use crate::services::discord::decisions::publication::{self, Asked, Outcome};
use crate::services::discord::decisions::{self, Ask, AskError, card, requester};

type RouteResponse = (StatusCode, Json<Value>);

fn failure(status: StatusCode, error: &str) -> RouteResponse {
    (status, Json(json!({"ok": false, "error": error})))
}

/// The header's capability hash owns the request. 201: the post exists; 202: it may, and is not
/// sent again; 502: Discord refused it; 503: nothing was sent.
pub async fn ask(
    _caller: HumanInputCaller,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Ask>,
) -> RouteResponse {
    let Some(pool) = state.pg_pool_ref() else {
        return failure(StatusCode::SERVICE_UNAVAILABLE, "postgres pool unavailable");
    };
    let presented = headers.get(requester::HEADER).and_then(|v| v.to_str().ok());
    let Some(capability) = presented.and_then(requester::hash) else {
        return failure(StatusCode::BAD_REQUEST, "decision_owner_required");
    };
    let registry = state.health_registry.as_deref();
    let (hint, channel) = (body.provider_hint.as_deref(), body.target.channel_id.trim());
    let target = resolve_agent_turn_target(pool, registry, &body.agent_id, hint, Some(channel));
    let target = match target.await {
        Ok(target) => target,
        Err(refused) => return refused,
    };
    if let Some(units) = publication::too_long(&body, state.config.discord.owner_id) {
        let (error, limit) = ("decision_post_too_long", card::CONTENT_LIMIT);
        let body = json!({"ok": false, "error": error, "utf16_units": units, "limit": limit});
        return (StatusCode::UNPROCESSABLE_ENTITY, Json(body));
    }
    let scope = (target.logical_channel(), target.channel_id);
    let (id, created) = match decisions::store(pool, &body, scope, &capability).await {
        Ok(stored) => stored,
        Err(error) => return not_stored(error),
    };
    let asked = Asked {
        request_id: id,
        ask: &body,
        logical_channel: target.logical_channel(),
        provider: &target.provider,
    };
    let outcome = publication::publish(pool, registry, &state.config.discord, asked).await;
    let (status, mut reply) = match outcome {
        Outcome::Posted(thread) => (
            StatusCode::CREATED,
            json!({"ok": true, "publication": "disabled", "thread_id": thread.to_string()}),
        ),
        Outcome::Pending => (
            StatusCode::ACCEPTED,
            json!({"ok": true, "publication": "pending"}),
        ),
        Outcome::Refused(error) => (
            StatusCode::BAD_GATEWAY,
            json!({"ok": false, "error": error}),
        ),
        Outcome::Unready(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"ok": false, "error": error}),
        ),
        Outcome::NoCategory => (
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({"ok": false, "error": "target_category_unavailable"}),
        ),
    };
    reply["request_id"] = json!(id);
    reply["created"] = created.into();
    (status, Json(reply))
}

fn not_stored(error: AskError) -> RouteResponse {
    match error {
        AskError::Invalid(rule) => failure(StatusCode::BAD_REQUEST, rule),
        AskError::Conflict => failure(StatusCode::CONFLICT, "idempotency_conflict"),
        AskError::Storage(error) => {
            tracing::warn!(%error, "decision ask was not stored");
            failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "decision_store_unavailable",
            )
        }
    }
}

/// The stored question and options, for the capability that asked them. No capability, another
/// requester's and an unknown id answer alike, and no capability or hash is part of a read.
pub async fn show(
    _caller: HumanInputCaller,
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> RouteResponse {
    let Some(pool) = state.pg_pool_ref() else {
        return failure(StatusCode::SERVICE_UNAVAILABLE, "postgres pool unavailable");
    };
    let Ok(id) = uuid::Uuid::parse_str(id.trim()) else {
        return failure(StatusCode::BAD_REQUEST, "invalid_request_id");
    };
    let presented = headers.get(requester::HEADER).and_then(|v| v.to_str().ok());
    let Some(capability) = presented.and_then(requester::hash) else {
        return failure(StatusCode::FORBIDDEN, "not_request_owner");
    };
    match crate::db::decisions::requests::load_owned(pool, id, &capability).await {
        Ok(Some(request)) => {
            let mut body = json!(request);
            body["ok"] = true.into();
            (StatusCode::OK, Json(body))
        }
        Ok(None) => failure(StatusCode::FORBIDDEN, "not_request_owner"),
        Err(error) => {
            tracing::warn!(%error, "decision read failed");
            failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "decision_store_unavailable",
            )
        }
    }
}
