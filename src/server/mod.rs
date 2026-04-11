mod background;
mod boot;
pub mod routes;
mod tick;
mod worker_registry;
pub mod ws;

use std::sync::Arc;

use anyhow::Result;

use crate::config::Config;
use crate::db::Db;
use crate::engine::PolicyEngine;
use crate::services::discord::health::HealthRegistry;

pub async fn run(
    config: Config,
    db: Db,
    engine: PolicyEngine,
    health_registry: Option<Arc<HealthRegistry>>,
) -> Result<()> {
    let mut worker_registry =
        worker_registry::SupervisedWorkerRegistry::new(config.clone(), db.clone(), engine.clone());
    worker_registry.run_boot_only_steps().await?;
    boot::serve_http(config, db, engine, health_registry, &mut worker_registry).await
}
