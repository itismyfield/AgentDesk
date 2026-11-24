//! `POST /api/decisions` stores an agent's Decision request under the hash of its requester's
//! capability; `GET /api/decisions/{id}` reads it back. Nothing is posted to Discord yet.

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use serde_json::{Value, json};

use super::AppState;
use super::agents_turn_target::resolve_agent_turn_target;
use super::auth::HumanInputCaller;
use crate::services::discord::decisions::{self, Ask, AskError, requester};

type RouteResponse = (StatusCode, Json<Value>);

fn failure(status: StatusCode, error: &str) -> RouteResponse {
    (status, Json(json!({"ok": false, "error": error})))
}

/// The caller extractor answers before the body is read. The body names the agent and target;
/// the header carries the capability whose hash becomes the request's owner.
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
    let scope = (target.logical_channel(), target.channel_id);
    match decisions::store(pool, &body, scope, &capability).await {
        // Stored only: publication is not wired, so no post, button or notice exists.
        Ok((id, created)) => (
            StatusCode::ACCEPTED,
            Json(json!({"ok": true, "request_id": id, "created": created,
                "publication": "pending"})),
        ),
        Err(AskError::Invalid(rule)) => failure(StatusCode::BAD_REQUEST, rule),
        Err(AskError::Conflict) => failure(StatusCode::CONFLICT, "idempotency_conflict"),
        Err(AskError::Storage(error)) => {
            tracing::warn!(%error, "decision ask was not stored");
            failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "decision_store_unavailable",
            )
        }
    }
}

/// The stored question and options; the capability and its hash are never part of a read.
pub async fn show(
    _caller: HumanInputCaller,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> RouteResponse {
    let Some(pool) = state.pg_pool_ref() else {
        return failure(StatusCode::SERVICE_UNAVAILABLE, "postgres pool unavailable");
    };
    let Ok(id) = uuid::Uuid::parse_str(id.trim()) else {
        return failure(StatusCode::BAD_REQUEST, "invalid_request_id");
    };
    match crate::db::decisions::requests::load(pool, id).await {
        Ok(Some(request)) => {
            let mut body = json!(request);
            body["ok"] = true.into();
            body["publication"] = "pending".into();
            (StatusCode::OK, Json(body))
        }
        Ok(None) => failure(StatusCode::NOT_FOUND, "decision_not_found"),
        Err(error) => {
            tracing::warn!(%error, "decision read failed");
            failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "decision_store_unavailable",
            )
        }
    }
}
