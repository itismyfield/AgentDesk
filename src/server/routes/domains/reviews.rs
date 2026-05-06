use axum::{
    Router,
    routing::{patch, post},
};

use super::super::{ApiRouter, AppState, protected_api_domain, review_verdict, reviews};

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
            .route("/reviews/recovery", post(reviews::recover_review_target))
            .route("/reviews/verdict", post(review_verdict::submit_verdict))
            .route(
                "/reviews/decision",
                post(review_verdict::submit_review_decision),
            )
            .route(
                "/reviews/tuning/aggregate",
                post(review_verdict::aggregate_review_tuning),
            ),
        state,
    )
}
