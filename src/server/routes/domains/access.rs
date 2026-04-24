use axum::{
    Router,
    body::Bytes,
    extract::{ConnectInfo, State},
    response::Response,
    routing::{get, post},
};
use std::net::SocketAddr;

use super::super::{
    ApiRouter, AppState, auth, health_api, log_deprecated_alias, public_api_domain,
};

// Category: ops and integrations

pub(crate) fn router() -> ApiRouter {
    public_api_domain(
        Router::new()
            .route("/health", get(health_api::health_handler))
            .route("/discord/send", post(health_api::send_handler))
            .route("/send", post(deprecated_send_handler))
            .route(
                "/discord/send-to-agent",
                post(health_api::send_to_agent_handler),
            )
            .route("/send_to_agent", post(deprecated_send_to_agent_handler))
            .route("/discord/send-dm", post(health_api::senddm_handler))
            .route("/senddm", post(deprecated_senddm_handler))
            .route(
                "/inflight/rebind",
                post(health_api::rebind_inflight_handler),
            )
            .route("/auth/session", get(auth::get_session)),
    )
}

async fn deprecated_send_handler(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    body: Bytes,
) -> Response {
    log_deprecated_alias("/api/send", "/api/discord/send");
    health_api::send_handler(State(state), ConnectInfo(peer_addr), body).await
}

async fn deprecated_send_to_agent_handler(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    body: Bytes,
) -> Response {
    log_deprecated_alias("/api/send_to_agent", "/api/discord/send-to-agent");
    health_api::send_to_agent_handler(State(state), ConnectInfo(peer_addr), body).await
}

async fn deprecated_senddm_handler(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    body: Bytes,
) -> Response {
    log_deprecated_alias("/api/senddm", "/api/discord/send-dm");
    health_api::senddm_handler(State(state), ConnectInfo(peer_addr), body).await
}
