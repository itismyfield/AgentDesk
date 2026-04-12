use axum::{
    Router,
    routing::{get, post},
};

use super::super::{ApiRouter, auth, health_api, public_api_domain};

pub(crate) fn router() -> ApiRouter {
    public_api_domain(
        Router::new()
            .route("/health", get(health_api::health_handler))
            .route("/send", post(health_api::send_handler))
            .route("/senddm", post(health_api::senddm_handler))
            .route("/auth/session", get(auth::get_session)),
    )
}
