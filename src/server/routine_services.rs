use std::sync::Arc;

use sqlx::PgPool;

use crate::config::RoutinesConfig;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::health::routine_ports::{
    build_routine_agent_executor, build_routine_discord_logger,
};
use crate::services::routines::{RoutineAgentExecutor, RoutineDiscordLogger};

pub(super) fn agent_executor(
    pool: Arc<PgPool>,
    registry: Option<Arc<HealthRegistry>>,
    config: &RoutinesConfig,
) -> RoutineAgentExecutor {
    build_routine_agent_executor(pool, registry, config.agent_timeout_secs)
        .with_headless_fresh_agents(config.headless_fresh_agents)
}

pub(crate) fn runtime_services(
    pool: Arc<PgPool>,
    registry: Option<Arc<HealthRegistry>>,
    config: &RoutinesConfig,
) -> (RoutineDiscordLogger, RoutineAgentExecutor) {
    let logger = build_routine_discord_logger(pool.clone(), registry.clone());
    let executor = agent_executor(pool, registry, config);
    (logger, executor)
}
