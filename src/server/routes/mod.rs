pub mod agents;
mod agents_crud;
mod agents_setup;
pub mod analytics;
pub mod auth;
pub mod auto_queue;
pub mod automation_candidates;
pub mod cluster;
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
pub mod home_metrics;
pub mod hooks;
pub mod kanban;
pub mod kanban_repos;
mod maintenance;
pub mod meetings;
pub mod memory_api;
pub mod messages;
pub mod monitoring;
pub mod offices;
pub mod onboarding;
pub mod pipeline;
pub mod prompt_manifest_retention;
pub mod provider_cli_api;
mod queue_api;
pub mod receipt;
pub mod resume;
pub mod review_verdict;
pub mod reviews;
pub mod routines;
pub(crate) mod session_activity;
pub mod settings;
mod skill_usage_analytics;
pub mod skills_api;
#[path = "../state.rs"]
pub mod state;
pub mod stats;
pub mod termination_events;
pub mod v1;
pub mod voice_config;

use axum::{
    Router,
    http::header::CONTENT_TYPE,
    response::{IntoResponse, Response},
};

use std::sync::Arc;

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use crate::db::Db;
use crate::engine::PolicyEngine;
use crate::error::{AppError, ErrorCode};
use crate::services::discord::health::HealthRegistry;

/// Shared application state passed to all route handlers.
///
#[derive(Clone)]
pub struct AppState {
    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    pub(crate) legacy_db_override: Option<Db>,
    pub pg_pool: Option<sqlx::PgPool>,
    pub engine: PolicyEngine,
    pub config: Arc<crate::config::Config>,
    pub broadcast_tx: crate::server::ws::BroadcastTx,
    pub batch_buffer: crate::server::ws::BatchBuffer,
    pub health_registry: Option<Arc<HealthRegistry>>,
    pub cluster_instance_id: Option<String>,
}

impl AppState {
    /// Returns the optional legacy SQLite handle for test fixtures.
    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    pub fn legacy_db(&self) -> Option<&crate::db::Db> {
        self.legacy_db_override
            .as_ref()
            .or_else(|| self.engine.legacy_db())
    }

    pub fn pg_pool_ref(&self) -> Option<&sqlx::PgPool> {
        self.pg_pool.as_ref()
    }

    pub fn kanban_service(&self) -> crate::services::kanban::KanbanService {
        crate::services::kanban::KanbanService::new(self.pg_pool.clone())
    }

    pub fn dispatch_service(&self) -> crate::services::dispatches::DispatchService {
        crate::services::dispatches::DispatchService::new(self.engine.clone())
    }

    pub fn auto_queue_service(&self) -> crate::services::auto_queue::AutoQueueService {
        crate::services::auto_queue::AutoQueueService::new(self.engine.clone())
    }

    pub fn queue_service(&self) -> crate::services::queue::QueueService {
        crate::services::queue::QueueService::new(self.pg_pool.clone())
    }

    pub fn settings_service(&self) -> crate::services::settings::SettingsService {
        crate::services::settings::SettingsService::new(self.pg_pool.clone(), self.config.clone())
    }
}

pub(crate) type ApiRouter = Router<AppState>;

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
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
            legacy_db_override: Some(db),
            pg_pool: None,
            engine,
            config: Arc::new(config),
            broadcast_tx: tx,
            batch_buffer: buf,
            health_registry: None,
            cluster_instance_id: None,
        }
    }

    /// PG-aware variant of [`test_state`]. Used by the #1238 PG-fixture
    /// migration — handler tests that exercise PG-only routes need a state
    /// whose `pg_pool` slot is populated.
    pub fn test_state_with_pg(db: Db, engine: PolicyEngine, pg_pool: sqlx::PgPool) -> Self {
        let tx = crate::server::ws::new_broadcast();
        let buf = crate::server::ws::spawn_batch_flusher(tx.clone());
        Self {
            legacy_db_override: Some(db),
            pg_pool: Some(pg_pool),
            engine,
            config: Arc::new(crate::config::Config::default()),
            broadcast_tx: tx,
            batch_buffer: buf,
            health_registry: None,
            cluster_instance_id: None,
        }
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn api_router(
    db: Db,
    engine: PolicyEngine,
    config: crate::config::Config,
    broadcast_tx: crate::server::ws::BroadcastTx,
    batch_buffer: crate::server::ws::BatchBuffer,
    health_registry: Option<Arc<HealthRegistry>>,
) -> Router {
    api_router_with_pg_for_tests(
        db,
        engine,
        config,
        broadcast_tx,
        batch_buffer,
        health_registry,
        None,
    )
}

pub fn api_router_with_pg(
    engine: PolicyEngine,
    config: crate::config::Config,
    broadcast_tx: crate::server::ws::BroadcastTx,
    batch_buffer: crate::server::ws::BatchBuffer,
    health_registry: Option<Arc<HealthRegistry>>,
    pg_pool: Option<sqlx::PgPool>,
) -> Router {
    api_router_with_pg_and_cluster(
        engine,
        config,
        broadcast_tx,
        batch_buffer,
        health_registry,
        pg_pool,
        None,
    )
}

pub fn api_router_with_pg_and_cluster(
    engine: PolicyEngine,
    config: crate::config::Config,
    broadcast_tx: crate::server::ws::BroadcastTx,
    batch_buffer: crate::server::ws::BatchBuffer,
    health_registry: Option<Arc<HealthRegistry>>,
    pg_pool: Option<sqlx::PgPool>,
    cluster_instance_id: Option<String>,
) -> Router {
    let state = AppState {
        #[cfg(all(test, feature = "legacy-sqlite-tests"))]
        legacy_db_override: None,
        pg_pool,
        engine,
        config: Arc::new(config),
        broadcast_tx,
        batch_buffer,
        health_registry,
        cluster_instance_id,
    };

    #[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
    crate::services::discord::monitoring_status::spawn_expiry_sweeper(
        state::global_monitoring_store(),
        state.health_registry.clone(),
    );

    compose_api_router(state.clone()).with_state(state)
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn api_router_with_pg_for_tests(
    db: Db,
    engine: PolicyEngine,
    config: crate::config::Config,
    broadcast_tx: crate::server::ws::BroadcastTx,
    batch_buffer: crate::server::ws::BatchBuffer,
    health_registry: Option<Arc<HealthRegistry>>,
    pg_pool: Option<sqlx::PgPool>,
) -> Router {
    let state = AppState {
        legacy_db_override: Some(db),
        pg_pool,
        engine,
        config: Arc::new(config),
        broadcast_tx,
        batch_buffer,
        health_registry,
        cluster_instance_id: None,
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
        .merge(monitoring::router(state.clone()))
        .merge(v1::router(state.clone()))
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod routes_tests;
