use axum::{Json, extract::State, http::StatusCode};

use super::super::AppState;
// #3037: `ReviewDecisionBody` relocated to the services layer so service-side
// loopback callers no longer reach back into `crate::server`. axum `Json<T>`
// extraction is location-independent; the handler references the services path.
use crate::services::review_decision::ReviewDecisionBody;

// ── Review Decision (agent's response to counter-model review) ──────────────

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
) -> (StatusCode, Json<serde_json::Value>) {
    crate::services::review_decision::submit_review_decision(&state, body).await
}
