pub mod agents;
mod agents_crud;
pub mod analytics;
pub mod auth;
pub mod auto_queue;
pub mod cron_api;
pub mod departments;
pub mod discord;
pub mod dispatched_sessions;
pub mod dispatches;
pub mod dm_reply;
pub mod docs;
mod domains;
pub mod escalation;
pub mod github;
pub mod github_dashboard;
pub mod health_api;
pub mod kanban;
pub mod kanban_repos;
pub mod meetings;
pub mod messages;
pub mod offices;
pub mod onboarding;
pub mod pipeline;
mod queue_api;
pub mod receipt;
pub mod resume;
pub mod review_verdict;
pub mod reviews;
mod session_activity;
pub mod settings;
pub mod skills_api;
pub mod stats;
pub mod termination_events;

use axum::{
    Router,
    http::header::CONTENT_TYPE,
    response::{IntoResponse, Response},
};

use std::sync::Arc;

use crate::db::Db;
use crate::engine::PolicyEngine;
use crate::error::{AppError, ErrorCode};
use crate::services::discord::health::HealthRegistry;

/// Shared application state passed to all route handlers.
#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub engine: PolicyEngine,
    pub config: Arc<crate::config::Config>,
    pub broadcast_tx: crate::server::ws::BroadcastTx,
    pub batch_buffer: crate::server::ws::BatchBuffer,
    pub health_registry: Option<Arc<HealthRegistry>>,
}

impl AppState {
    pub fn auto_queue_service(&self) -> crate::services::auto_queue::AutoQueueService {
        crate::services::auto_queue::AutoQueueService::new(self.db.clone(), self.engine.clone())
    }

    pub fn dispatch_service(&self) -> crate::services::dispatches::DispatchService {
        crate::services::dispatches::DispatchService::new(self.db.clone(), self.engine.clone())
    }

    pub fn kanban_service(&self) -> crate::services::kanban::KanbanService {
        crate::services::kanban::KanbanService::new(self.db.clone())
    }

    pub fn queue_service(&self) -> crate::services::queue::QueueService {
        crate::services::queue::QueueService::new(self.db.clone(), self.health_registry.clone())
    }

    pub fn settings_service(&self) -> crate::services::settings::SettingsService {
        crate::services::settings::SettingsService::new(self.db.clone(), self.config.clone())
    }
}

pub(crate) type ApiRouter = Router<AppState>;

#[cfg(test)]
impl AppState {
    pub fn test_state(db: Db, engine: PolicyEngine) -> Self {
        Self::test_state_with_config(db, engine, crate::config::Config::default())
    }

    pub fn test_state_with_config(
        db: Db,
        engine: PolicyEngine,
        config: crate::config::Config,
    ) -> Self {
        let tx = crate::server::ws::new_broadcast();
        let buf = crate::server::ws::spawn_batch_flusher(tx.clone());
        Self {
            db,
            engine,
            config: Arc::new(config),
            broadcast_tx: tx,
            batch_buffer: buf,
            health_registry: None,
        }
    }
}

pub fn api_router(
    db: Db,
    engine: PolicyEngine,
    config: crate::config::Config,
    broadcast_tx: crate::server::ws::BroadcastTx,
    batch_buffer: crate::server::ws::BatchBuffer,
    health_registry: Option<Arc<HealthRegistry>>,
) -> Router {
    let state = AppState {
        db,
        engine,
        config: Arc::new(config),
        broadcast_tx,
        batch_buffer,
        health_registry,
    };

    compose_api_router(state.clone()).with_state(state)
}

fn compose_api_router(state: AppState) -> ApiRouter {
    Router::new()
        .merge(domains::access::router())
        .merge(domains::onboarding::router(state.clone()))
        .merge(domains::agents::router(state.clone()))
        .merge(domains::kanban::router(state.clone()))
        .merge(domains::reviews::router(state.clone()))
        .merge(domains::ops::router(state.clone()))
        .merge(domains::integrations::router(state.clone()))
        .merge(domains::admin::router(state))
}

pub(super) fn public_api_domain(router: ApiRouter) -> ApiRouter {
    router.layer(axum::middleware::map_response(error_envelope_middleware))
}

pub(super) fn protected_api_domain(router: ApiRouter, state: AppState) -> ApiRouter {
    router
        .layer(axum::middleware::from_fn_with_state(
            state,
            auth::auth_middleware,
        ))
        .layer(axum::middleware::map_response(error_envelope_middleware))
}

async fn error_envelope_middleware(response: Response) -> Response {
    if response.status().is_server_error() && !response_is_json(&response) {
        let message = response
            .status()
            .canonical_reason()
            .unwrap_or("internal server error")
            .to_ascii_lowercase();
        return AppError::new(response.status(), ErrorCode::Internal, message).into_response();
    }

    response
}

fn response_is_json(response: &Response) -> bool {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.starts_with("application/json"))
        .unwrap_or(false)
}

#[cfg(test)]
mod routes_tests;
