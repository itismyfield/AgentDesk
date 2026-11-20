use poise::serenity_prelude::ChannelId;

use crate::services::routines::session_control::{RoutineRuntimeReset, RuntimeResetOutcome};

use super::super::admin_host_guard::ManagedReset;
use super::HealthRegistry;
use super::recovery::clear_provider_channel_runtime;

#[async_trait::async_trait]
impl RoutineRuntimeReset for HealthRegistry {
    async fn reset_runtime(
        &self,
        provider: &str,
        channel_id: ChannelId,
        session_key: Option<&str>,
    ) -> RuntimeResetOutcome {
        match clear_provider_channel_runtime(self, provider, channel_id, session_key).await {
            Some(ManagedReset::Applied(_)) => RuntimeResetOutcome::Applied,
            Some(ManagedReset::Refused(_)) => RuntimeResetOutcome::Refused,
            None => RuntimeResetOutcome::Unavailable,
        }
    }
}
