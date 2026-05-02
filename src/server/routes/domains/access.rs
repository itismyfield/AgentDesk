use axum::{Router, routing::get};

use super::super::{ApiRouter, auth, health_api, public_api_domain};

// Category: ops and integrations

pub(crate) fn router() -> ApiRouter {
    public_api_domain(
        Router::new()
            .route("/health", get(health_api::health_handler))
            .route("/auth/session", get(auth::get_session)),
    )
}
