//! `HealthRegistry`'s side of the KE managed-reset port.

use futures::future::BoxFuture;
use poise::serenity_prelude::ChannelId;
use sqlx::PgPool;

use super::HealthRegistry;
use crate::services::discord::{admin_host_guard::managed_reset_verdict, host_teardown_gate};
use crate::services::engine_runtime::health_port::{ManagedResetPort, SlotResetVerdict};

impl ManagedResetPort for HealthRegistry {
    fn managed_reset_verdict<'a>(
        &'a self,
        provider_name: &'a str,
        channel_id: ChannelId,
        session_key: Option<&'a str>,
    ) -> BoxFuture<'a, Option<Box<dyn SlotResetVerdict>>> {
        Box::pin(async move {
            let verdict =
                managed_reset_verdict(self, provider_name, channel_id, session_key).await?;
            Some(Box::new(verdict) as Box<dyn SlotResetVerdict>)
        })
    }
}

pub(crate) fn slot_row_host_refusal<'a>(
    pool: &'a PgPool,
    stored_provider: Option<&'a str>,
    channel_id: Option<&'a str>,
    session_key: &'a str,
    tmux_name: &'a str,
    caller: &'a str,
) -> BoxFuture<'a, Option<String>> {
    Box::pin(host_teardown_gate::row_host_refusal(
        pool,
        stored_provider,
        channel_id,
        session_key,
        tmux_name,
        caller,
    ))
}
