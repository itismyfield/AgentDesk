use std::sync::Arc;

use futures::future::BoxFuture;
use sqlx::PgPool;

use super::{HealthRegistry, resolve_bot_http};
use crate::services::discord::host_key_derivation::routine_session_failure_admitted;
use crate::services::engine_runtime::health_port::{OwnerInputRuntimePort, RoutineProcessPort};
use crate::services::provider::ProviderKind;
use crate::services::routines::agent_executor::{RoutineAgentPorts, RoutineLaunchRuntime};
use crate::services::routines::discord_port::{
    RoutineBotHttpPort, RoutineBotHttpResult, RoutineFailureAdmissionPort,
};
use crate::services::routines::session_control::RoutineTurnStop;
use crate::services::routines::{
    RoutineAgentExecutor, RoutineDiscordLogger, RoutineSessionController,
};
use crate::services::session_host::HostLiveness;
use crate::services::turn_lifecycle::{
    ForceKillRow, TurnLifecycleStopResult, TurnLifecycleTarget, force_kill_turn,
    force_kill_turn_for_row,
};

impl RoutineBotHttpPort for HealthRegistry {
    fn resolve<'a>(&'a self, bot: &'a str) -> BoxFuture<'a, RoutineBotHttpResult> {
        Box::pin(resolve_bot_http(self, bot))
    }
}

impl RoutineLaunchRuntime for HealthRegistry {
    fn provider_registered<'a>(&'a self, provider: &'a ProviderKind) -> BoxFuture<'a, bool> {
        Box::pin(async move { !self.registered_token_hashes(provider).await.is_empty() })
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
    let ports = RoutineAgentPorts {
        bot_http: registry
            .clone()
            .map(|registry| registry as Arc<dyn RoutineBotHttpPort>),
        failure_admission: Arc::new(RoutineFailureAdmission(registry.clone())),
        owner_input: registry
            .clone()
            .map(|registry| registry as Arc<dyn OwnerInputRuntimePort>),
        process: registry
            .clone()
            .map(|registry| registry as Arc<dyn RoutineProcessPort>),
        session_controller: build_routine_session_controller(pool.clone(), registry.clone()),
    };
    RoutineAgentExecutor::new(pool, registry, ports, default_completion_timeout_secs)
}

pub(crate) fn build_routine_discord_logger(
    pool: Arc<PgPool>,
    registry: Option<Arc<HealthRegistry>>,
) -> RoutineDiscordLogger {
    let bot_http = registry.map(|registry| registry as Arc<dyn RoutineBotHttpPort>);
    RoutineDiscordLogger::new(pool, bot_http)
}

struct RoutineStop(Option<Arc<HealthRegistry>>);

impl RoutineTurnStop for RoutineStop {
    fn force_kill<'a>(
        &'a self,
        target: &'a TurnLifecycleTarget,
        row: Option<ForceKillRow<'a>>,
        reason: &'a str,
        code: &'static str,
    ) -> BoxFuture<'a, TurnLifecycleStopResult> {
        let registry = self.0.as_deref();
        match row {
            Some(row) => Box::pin(force_kill_turn_for_row(registry, target, row, reason, code)),
            None => Box::pin(force_kill_turn(registry, target, reason, code)),
        }
    }
}

pub(crate) fn build_routine_session_controller(
    pool: Arc<PgPool>,
    registry: Option<Arc<HealthRegistry>>,
) -> RoutineSessionController {
    RoutineSessionController::new(pool, Arc::new(RoutineStop(registry)))
}
