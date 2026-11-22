//! The routine-process port: how a KE routine executor stops the process episode its durable
//! Discord turn id names on the provider runtimes the health registry holds.

use futures::future::BoxFuture;
use poise::serenity_prelude::ChannelId;

use crate::services::provider::ProviderKind;

pub(crate) trait RoutineProcessPort: Send + Sync {
    /// Cancels only the process episode `turn_id` names; `true` when it was cancelled.
    fn cancel_routine_process_turn<'a>(
        &'a self,
        provider: &'a ProviderKind,
        channel: ChannelId,
        turn_id: &'a str,
        reason: &'a str,
    ) -> BoxFuture<'a, bool>;
}
