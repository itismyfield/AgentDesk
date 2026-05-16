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
pub mod idle_recap;
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

/// Mutation routes that gate themselves with `require_explicit_bearer_token`.
/// Kept in one place so the boot-time audit emits a complete inventory.
/// Order matches code-grep order for stable log output.
/// (#2257 concern 1 — operators need to see at startup which write
/// endpoints are mounted on a fail-open auth config.)
pub const EXPLICIT_AUTH_MUTATION_ROUTES: &[&str] = &[
    "kanban: rereview",
    "kanban: batch rereview",
    "kanban: reopen",
    "kanban: batch-transition",
    "kanban: force-transition",
    "auto-queue: submit_order",
    "auto-queue: phase-gate repair",
];

/// Emits a structured boot-time audit identifying whether the explicit-auth
/// mutation routes will fail-open with the current configuration. Called
/// once from `server::run` after the listener binds. Does NOT change
/// behavior — operators choose whether to add a token or restrict the
/// host/port; this only guarantees they get a clear signal in the logs.
///
/// Policy decision intentionally deferred: agentdesk control plane today
/// runs on a private loopback in single-operator deployments where neither
/// `server.auth_token` nor `kanban.manager_channel_id` is configured. A
/// hard-require would block those installs. See #2257.
pub fn audit_explicit_auth_routes_on_boot(config: &crate::config::Config) {
    let token_set = config
        .server
        .auth_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_some();
    let channel_set = config
        .kanban
        .manager_channel_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_some();
    if token_set || channel_set {
        tracing::info!(
            auth_token_configured = token_set,
            manager_channel_configured = channel_set,
            mutation_routes = EXPLICIT_AUTH_MUTATION_ROUTES.len(),
            "explicit-auth mutation routes will require Bearer token and/or x-channel-id"
        );
        return;
    }
    tracing::warn!(
        auth_token_configured = false,
        manager_channel_configured = false,
        mutation_routes = ?EXPLICIT_AUTH_MUTATION_ROUTES,
        host = %config.server.host,
        port = config.server.port,
        "FAIL-OPEN: neither server.auth_token nor kanban.manager_channel_id is configured — \
         the listed mutation endpoints accept any caller that can reach the bind address. \
         Restrict the bind host (e.g. 127.0.0.1) or configure server.auth_token before exposing to untrusted clients. (#2257)"
    );
}

#[cfg(test)]
mod audit_explicit_auth_routes_tests {
    use super::*;

    fn test_config() -> crate::config::Config {
        let mut config = crate::config::Config::default();
        config.server.host = "127.0.0.1".to_string();
        config.server.port = 8791;
        config
    }

    #[test]
    fn route_inventory_is_non_empty_and_named_uniquely() {
        assert!(!EXPLICIT_AUTH_MUTATION_ROUTES.is_empty());
        let mut sorted = EXPLICIT_AUTH_MUTATION_ROUTES.to_vec();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            EXPLICIT_AUTH_MUTATION_ROUTES.len(),
            "duplicate label in EXPLICIT_AUTH_MUTATION_ROUTES — audit log will report misleading counts"
        );
    }

    #[test]
    fn audit_runs_without_panic_for_all_config_combos() {
        // We can't observe tracing output without a test subscriber, but the
        // function must remain panic-free across every combination so the
        // boot path stays robust. (#2257)
        let mut none_set = test_config();
        none_set.server.auth_token = None;
        none_set.kanban.manager_channel_id = None;
        audit_explicit_auth_routes_on_boot(&none_set);

        let mut token_only = test_config();
        token_only.server.auth_token = Some("secret".to_string());
        token_only.kanban.manager_channel_id = None;
        audit_explicit_auth_routes_on_boot(&token_only);

        let mut channel_only = test_config();
        channel_only.server.auth_token = None;
        channel_only.kanban.manager_channel_id = Some("123".to_string());
        audit_explicit_auth_routes_on_boot(&channel_only);

        let mut both = test_config();
        both.server.auth_token = Some("secret".to_string());
        both.kanban.manager_channel_id = Some("123".to_string());
        audit_explicit_auth_routes_on_boot(&both);
    }

    #[test]
    fn empty_strings_are_treated_as_unset() {
        // #2257: defense against config files that ship empty strings
        // (e.g., `auth_token: ""`) — those must NOT count as "configured".
        let mut config = test_config();
        config.server.auth_token = Some("   ".to_string());
        config.kanban.manager_channel_id = Some("".to_string());
        audit_explicit_auth_routes_on_boot(&config);
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
