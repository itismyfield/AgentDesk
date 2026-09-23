use axum::{
    Router,
    routing::{get, post},
};

use super::super::{
    ApiRouter, AppState, auth, health_api, protected_api_domain, public_api_domain,
};

// Category: ops and integrations

pub(crate) fn router(state: AppState) -> ApiRouter {
    public_api_domain(
        Router::new()
            .route("/health", get(health_api::health_handler))
            .route("/auth/session", get(auth::get_session)),
    )
    .merge(protected_api_domain(
        Router::new().route("/auth/ws-ticket", post(auth::issue_ws_ticket)),
        state,
    ))
}
