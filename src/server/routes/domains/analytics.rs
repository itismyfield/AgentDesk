use axum::{Router, routing::get};

use super::super::{ApiRouter, AppState, analytics, protected_api_domain, receipt};

// Category: analytics

pub(crate) fn router(state: AppState) -> ApiRouter {
    protected_api_domain(
        Router::new()
            .route("/analytics", get(analytics::analytics))
            .route("/analytics/invariants", get(analytics::invariants))
            .route("/analytics/observability", get(analytics::observability))
            .route("/analytics/policy-hooks", get(analytics::policy_hooks))
            .route("/quality/events", get(analytics::quality_events))
            .route("/streaks", get(analytics::streaks))
            .route("/achievements", get(analytics::achievements))
            .route("/activity-heatmap", get(analytics::activity_heatmap))
            .route("/receipt", get(receipt::get_receipt))
            .route("/token-analytics", get(receipt::get_token_analytics)),
        state,
    )
}
