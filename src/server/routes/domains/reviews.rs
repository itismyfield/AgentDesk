use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{patch, post},
};
use serde_json::Value;

use super::super::{
    ApiRouter, AppState, log_deprecated_alias, protected_api_domain, review_verdict, reviews,
};

// Category: kanban

pub(crate) fn router(state: AppState) -> ApiRouter {
    protected_api_domain(
        Router::new()
            .route(
                "/kanban-reviews/{id}/decisions",
                patch(reviews::update_decisions),
            )
            .route(
                "/kanban-reviews/{id}/trigger-rework",
                post(reviews::trigger_rework),
            )
            .route("/reviews/verdict", post(review_verdict::submit_verdict))
            .route("/review-verdict", post(deprecated_submit_verdict))
            .route(
                "/reviews/decision",
                post(review_verdict::submit_review_decision),
            )
            .route("/review-decision", post(deprecated_submit_review_decision))
            .route(
                "/reviews/tuning/aggregate",
                post(review_verdict::aggregate_review_tuning),
            )
            .route(
                "/review-tuning/aggregate",
                post(deprecated_aggregate_review_tuning),
            ),
        state,
    )
}

async fn deprecated_submit_verdict(
    State(state): State<AppState>,
    Json(body): Json<review_verdict::SubmitVerdictBody>,
) -> (StatusCode, Json<Value>) {
    log_deprecated_alias("/api/review-verdict", "/api/reviews/verdict");
    review_verdict::submit_verdict(State(state), Json(body)).await
}

async fn deprecated_submit_review_decision(
    State(state): State<AppState>,
    Json(body): Json<review_verdict::ReviewDecisionBody>,
) -> (StatusCode, Json<Value>) {
    log_deprecated_alias("/api/review-decision", "/api/reviews/decision");
    review_verdict::submit_review_decision(State(state), Json(body)).await
}

async fn deprecated_aggregate_review_tuning(
    State(state): State<AppState>,
) -> (StatusCode, Json<Value>) {
    log_deprecated_alias(
        "/api/review-tuning/aggregate",
        "/api/reviews/tuning/aggregate",
    );
    review_verdict::aggregate_review_tuning(State(state)).await
}
