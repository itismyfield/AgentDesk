//! `HealthRegistry`'s side of the KE managed-reset port.

use futures::future::BoxFuture;
use poise::serenity_prelude::ChannelId;

use super::HealthRegistry;
use crate::services::discord::admin_host_guard::{ManagedResetVerdict, managed_reset_verdict};
use crate::services::engine_runtime::health_port::ManagedResetPort;

impl ManagedResetPort<ManagedResetVerdict> for HealthRegistry {
    fn managed_reset_verdict<'a>(
        &'a self,
        provider_name: &'a str,
        channel_id: ChannelId,
        session_key: Option<&'a str>,
    ) -> BoxFuture<'a, Option<ManagedResetVerdict>> {
        Box::pin(managed_reset_verdict(
            self,
            provider_name,
            channel_id,
            session_key,
        ))
    }
}
