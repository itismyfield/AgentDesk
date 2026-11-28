use std::sync::Arc;

use anyhow::{Result, anyhow};
use futures::future::BoxFuture;
use poise::serenity_prelude::Http;
use sqlx::PgPool;

use crate::domain::bot_role::UtilityBotRole;
use crate::services::provider::ProviderKind;
use crate::services::session_host::HostLiveness;

pub(crate) type RoutineBotHttpResult = Result<Arc<Http>, (&'static str, String)>;

pub(crate) trait RoutineBotHttpPort: Send + Sync {
    fn resolve<'a>(&'a self, bot: &'a str) -> BoxFuture<'a, RoutineBotHttpResult>;
}

pub(crate) trait RoutineFailureAdmissionPort: Send + Sync {
    fn admit<'a>(
        &'a self,
        pool: &'a PgPool,
        provider: &'a ProviderKind,
        channel_id: Option<u64>,
        tmux_name: &'a str,
        observed: HostLiveness,
    ) -> BoxFuture<'a, bool>;
}

pub(super) struct RoutineThreadHttp {
    pub(super) http: Arc<Http>,
    pub(super) bot: String,
}

pub(super) async fn resolve_routine_thread_http(
    bot_http: &dyn RoutineBotHttpPort,
    provider_name: &str,
    agent_id: &str,
) -> Result<RoutineThreadHttp> {
    let mut errors = Vec::new();
    let mut tried = Vec::new();
    for bot in [provider_name, agent_id, UtilityBotRole::Notify.alias()] {
        if bot.trim().is_empty() || tried.contains(&bot) {
            continue;
        }
        tried.push(bot);
        match bot_http.resolve(bot).await {
            Ok(http) => {
                return Ok(RoutineThreadHttp {
                    http,
                    bot: bot.to_string(),
                });
            }
            Err((_, error)) => errors.push(format!("{bot}: {error}")),
        }
    }
    Err(anyhow!(
        "no routine discord bot available ({})",
        errors.join("; ")
    ))
}
