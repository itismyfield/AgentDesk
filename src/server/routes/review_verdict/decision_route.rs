use axum::{Json, extract::State, http::StatusCode};
use serde_json::Value;

use super::super::AppState;
use crate::error::{AppError, AppResult, ErrorCode};
// #3037: `ReviewDecisionBody` relocated to the services layer so service-side
// loopback callers no longer reach back into `crate::server`. axum `Json<T>`
// extraction is location-independent; the handler references the services path.
use crate::services::review_decision::ReviewDecisionBody;

// ── Review Decision (agent's response to counter-model review) ──────────────

fn review_decision_result(
    status: StatusCode,
    response: Json<Value>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let Value::Object(body) = response.0 else {
        return Err(AppError::new(
            status,
            ErrorCode::Dispatch,
            "review decision failed",
        ));
    };
    if body.keys().any(|key| key != "error") {
        return Ok((status, Json(Value::Object(body))));
    }
    let message = body
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("review decision failed")
        .to_string();
    Err(AppError::new(status, ErrorCode::Dispatch, message))
}

/// POST /api/reviews/decision
///
/// Agent's decision on counter-model review feedback.
/// - accept: agent will rework based on review → card to in_progress
/// - dispute: agent disagrees, sends back for re-review → new review dispatch
/// - dismiss: agent ignores review → card to done
///
/// #3038: this is the orchestrator. Each logical phase lives in a
/// `decision_route_*` helper; behavior, control flow, side-effect ordering, and
/// response bodies are identical to the original monolithic implementation.
pub async fn submit_review_decision(
    State(state): State<AppState>,
    Json(body): Json<ReviewDecisionBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let (status, response) =
        crate::services::review_decision::submit_review_decision(&state, body).await;
    if status.is_success() {
        Ok((status, response))
    } else {
        review_decision_result(status, response)
    }
}
