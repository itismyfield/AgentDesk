pub mod agents;
mod agents_crud;
mod agents_setup;
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
pub mod provider_cli_api;
mod queue_api;
pub mod receipt;
pub mod resume;
pub mod review_verdict;
pub mod reviews;
mod session_activity;
pub mod settings;
mod skill_usage_analytics;
pub mod skills_api;
#[path = "../state.rs"]
pub mod state;
pub mod stats;
pub mod termination_events;
pub mod v1;

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
///
/// #1237 (843f): the legacy SQLite handle (`db`) is no longer constructed at
/// server startup. Production runtimes carry `db: None` and rely on
/// `pg_pool` for persistence. Test fixtures continue to set
/// `db: Some(test_db)` and that path is the only place `legacy_db()` returns
/// a handle. Production call sites that still need a `&Db` are tracked under
/// #1238 (843g) and should migrate to PG-only APIs there.
#[derive(Clone)]
pub struct AppState {
    pub db: Option<Db>,
    pub pg_pool: Option<sqlx::PgPool>,
    pub engine: PolicyEngine,
    pub config: Arc<crate::config::Config>,
    pub broadcast_tx: crate::server::ws::BroadcastTx,
    pub batch_buffer: crate::server::ws::BatchBuffer,
    pub health_registry: Option<Arc<HealthRegistry>>,
}

impl AppState {
    /// Returns the optional legacy SQLite handle. Production runtimes get
    /// `None`; #1238 (843g) will migrate the remaining call sites that
    /// currently expect a `&Db` to PG-only APIs and remove this accessor.
    pub fn legacy_db(&self) -> Option<&crate::db::Db> {
        self.db.as_ref()
    }

    pub fn pg_pool_ref(&self) -> Option<&sqlx::PgPool> {
        self.pg_pool.as_ref()
    }

    pub fn kanban_service(&self) -> crate::services::kanban::KanbanService {
        // TODO(#1238 / 843g): KanbanService still takes a `Db` for its
        // SQLite fallback path. Production runtimes always have a `pg_pool`
        // and never hit the SQLite branch, so the field is filled with the
        // engine's optional handle (None at runtime, Some in tests).
        crate::services::kanban::KanbanService::new(
            self.legacy_db_for_pending_migration(),
            self.pg_pool.clone(),
        )
    }

    pub fn dispatch_service(&self) -> crate::services::dispatches::DispatchService {
        // TODO(#1238 / 843g): see kanban_service.
        crate::services::dispatches::DispatchService::new(
            self.legacy_db_for_pending_migration(),
            self.engine.clone(),
        )
    }

    pub fn auto_queue_service(&self) -> crate::services::auto_queue::AutoQueueService {
        // AutoQueueService already accepts Option<Db>; pass it through
        // directly without forcing a placeholder shim.
        let db = self.db.clone().or_else(|| self.engine.legacy_db().cloned());
        crate::services::auto_queue::AutoQueueService::new(db, self.engine.clone())
    }

    pub fn queue_service(&self) -> crate::services::queue::QueueService {
        // TODO(#1238 / 843g): QueueService::new still takes a `Db`.
        crate::services::queue::QueueService::new(
            self.legacy_db_for_pending_migration(),
            self.pg_pool.clone(),
        )
    }

    pub fn settings_service(&self) -> crate::services::settings::SettingsService {
        // TODO(#1238 / 843g): SettingsService::new still takes a `Db`.
        crate::services::settings::SettingsService::new(
            self.legacy_db_for_pending_migration(),
            self.pg_pool.clone(),
            self.config.clone(),
        )
    }

    /// TODO(#1238 / 843g): used by service factories that still expect a
    /// `Db` parameter. Production runtimes never reach the SQLite branch
    /// inside those services because `pg_pool` is required, but the
    /// constructors still accept a handle. This helper materializes one
    /// lazily — tests pass an explicit `Db`, runtime gets a placeholder
    /// in-memory shim that none of the live PG branches consult. Once
    /// #1238 migrates the constructors to `Option<Db>` (or PG-only), this
    /// helper goes away.
    fn legacy_db_for_pending_migration(&self) -> crate::db::Db {
        self.db
            .clone()
            .or_else(|| self.engine.legacy_db().cloned())
            .unwrap_or_else(legacy_pending_migration_shim)
    }
}

/// TODO(#1238 / 843g): replaced the previous startup-time SQLite shim. This
/// helper is created lazily from
/// `AppState::legacy_db_for_pending_migration` and only used as a
/// placeholder for service constructors that have not been ported to
/// PG-only APIs yet. It is never read at runtime — production deployments
/// always go through the `pg_pool` branch inside each service.
fn legacy_pending_migration_shim() -> crate::db::Db {
    let conn = libsql_rusqlite::Connection::open_in_memory()
        .expect("open legacy compatibility placeholder");
    crate::db::wrap_conn(conn)
}

/// TODO(#1238 / 843g): exposed for the few route helpers (auto_queue's
/// activation deps, etc.) that still build their own `Db` clone from
/// `AppState`. They will switch to `state.legacy_db()` (Option<&Db>) once
/// the receiving signatures are ported.
pub(super) fn pending_migration_shim_for_callers() -> crate::db::Db {
    legacy_pending_migration_shim()
}

pub(crate) type ApiRouter = Router<AppState>;

pub(crate) fn log_deprecated_alias(old_path: &'static str, canonical_path: &'static str) {
    tracing::warn!(
        old_path,
        canonical_path,
        "deprecated API alias called; use canonical path"
    );
}

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
            db: Some(db),
            pg_pool: None,
            engine,
            config: Arc::new(config),
            broadcast_tx: tx,
            batch_buffer: buf,
            health_registry: None,
        }
    }

    /// PG-aware variant of [`test_state`]. Used by the #1238 PG-fixture
    /// migration — handler tests that exercise PG-only routes need a state
    /// whose `pg_pool` slot is populated.
    pub fn test_state_with_pg(db: Db, engine: PolicyEngine, pg_pool: sqlx::PgPool) -> Self {
        let tx = crate::server::ws::new_broadcast();
        let buf = crate::server::ws::spawn_batch_flusher(tx.clone());
        Self {
            db: Some(db),
            pg_pool: Some(pg_pool),
            engine,
            config: Arc::new(crate::config::Config::default()),
            broadcast_tx: tx,
            batch_buffer: buf,
            health_registry: None,
        }
    }
}

#[cfg(test)]
pub fn api_router(
    db: Db,
    engine: PolicyEngine,
    config: crate::config::Config,
    broadcast_tx: crate::server::ws::BroadcastTx,
    batch_buffer: crate::server::ws::BatchBuffer,
    health_registry: Option<Arc<HealthRegistry>>,
) -> Router {
    api_router_with_pg(
        Some(db),
        engine,
        config,
        broadcast_tx,
        batch_buffer,
        health_registry,
        None,
    )
}

pub fn api_router_with_pg(
    db: Option<Db>,
    engine: PolicyEngine,
    config: crate::config::Config,
    broadcast_tx: crate::server::ws::BroadcastTx,
    batch_buffer: crate::server::ws::BatchBuffer,
    health_registry: Option<Arc<HealthRegistry>>,
    pg_pool: Option<sqlx::PgPool>,
) -> Router {
    let state = AppState {
        db,
        pg_pool,
        engine,
        config: Arc::new(config),
        broadcast_tx,
        batch_buffer,
        health_registry,
    };

    #[cfg(not(test))]
    crate::services::discord::monitoring_status::spawn_expiry_sweeper(
        state::global_monitoring_store(),
        state.health_registry.clone(),
    );

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

#[cfg(test)]
mod routes_tests;
