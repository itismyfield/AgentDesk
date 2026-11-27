use std::sync::Arc;

use futures::future::BoxFuture;
use sqlx::PgPool;

use super::{HealthRegistry, resolve_bot_http};
use crate::services::discord::host_key_derivation::routine_session_failure_admitted;
use crate::services::provider::ProviderKind;
use crate::services::routines::discord_port::{
    RoutineBotHttpPort, RoutineBotHttpResult, RoutineFailureAdmissionPort,
};
use crate::services::routines::{RoutineAgentExecutor, RoutineDiscordLogger};
use crate::services::session_host::HostLiveness;

impl RoutineBotHttpPort for HealthRegistry {
    fn resolve<'a>(&'a self, bot: &'a str) -> BoxFuture<'a, RoutineBotHttpResult> {
        Box::pin(resolve_bot_http(self, bot))
    }
}

struct RoutineFailureAdmission(Option<Arc<HealthRegistry>>);

impl RoutineFailureAdmissionPort for RoutineFailureAdmission {
    fn admit<'a>(
        &'a self,
        pool: &'a PgPool,
        provider: &'a ProviderKind,
        channel_id: Option<u64>,
        tmux_name: &'a str,
        observed: HostLiveness,
    ) -> BoxFuture<'a, bool> {
        let registered_hashes = async {
            match self.0.as_deref() {
                Some(registry) => registry.registered_token_hashes(provider).await,
                None => Vec::new(),
            }
        };
        Box::pin(routine_session_failure_admitted(
            registered_hashes,
            pool,
            provider,
            channel_id,
            tmux_name,
            observed,
        ))
    }
}

pub(crate) fn build_routine_agent_executor(
    pool: Arc<PgPool>,
    registry: Option<Arc<HealthRegistry>>,
    default_completion_timeout_secs: u64,
) -> RoutineAgentExecutor {
    let bot_http = registry
        .clone()
        .map(|registry| registry as Arc<dyn RoutineBotHttpPort>);
    let failure_admission = Arc::new(RoutineFailureAdmission(registry.clone()));
    RoutineAgentExecutor::new(
        pool,
        registry,
        bot_http,
        failure_admission,
        default_completion_timeout_secs,
    )
}

pub(crate) fn build_routine_discord_logger(
    pool: Arc<PgPool>,
    registry: Option<Arc<HealthRegistry>>,
) -> RoutineDiscordLogger {
    let bot_http = registry.map(|registry| registry as Arc<dyn RoutineBotHttpPort>);
    RoutineDiscordLogger::new(pool, bot_http)
}
