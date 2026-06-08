//! Shared application state (`AppState`) passed to all HTTP route handlers.
//!
//! Lives at the crate root — below both `server` and `services` in the
//! dependency graph — so service-layer handler helpers that operate on the
//! shared state can reference `crate::app_state::AppState` without reaching up
//! into `crate::server` (#3037 service→server backflow removal). The struct is
//! re-exported from `crate::server::routes` so existing
//! `crate::server::routes::AppState` call sites resolve unchanged.

use std::sync::Arc;

use crate::engine::PolicyEngine;
use crate::services::discord::health::HealthRegistry;

/// Shared application state passed to all route handlers.
#[derive(Clone)]
pub struct AppState {
    pub pg_pool: Option<sqlx::PgPool>,
    pub engine: PolicyEngine,
    pub config: Arc<crate::config::Config>,
    pub broadcast_tx: crate::eventbus::BroadcastTx,
    pub batch_buffer: crate::eventbus::BatchBuffer,
    pub health_registry: Option<Arc<HealthRegistry>>,
    pub cluster_instance_id: Option<String>,
}

impl AppState {
    pub fn pg_pool_ref(&self) -> Option<&sqlx::PgPool> {
        self.pg_pool.as_ref()
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
