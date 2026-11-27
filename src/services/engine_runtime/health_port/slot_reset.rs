//! Host checks and runtime verdicts for auto-queue slot reset.

use std::sync::Arc;

use futures::future::BoxFuture;
use poise::serenity_prelude::ChannelId;
use sqlx::PgPool;

/// Judges a runtime reset without writes. `None` means there is no runtime to clear.
pub(crate) trait ManagedResetPort: Send + Sync {
    fn managed_reset_verdict<'a>(
        &'a self,
        provider_name: &'a str,
        channel_id: ChannelId,
        session_key: Option<&'a str>,
    ) -> BoxFuture<'a, Option<Box<dyn SlotResetVerdict>>>;
}

pub(crate) trait SlotResetVerdict: Send {
    fn channel_id(&self) -> ChannelId;
    fn refusal(&self) -> Option<&str>;
    fn apply(self: Box<Self>) -> BoxFuture<'static, SlotResetOutcome>;
}

/// The applied tmux name, if present, or the refusal reason.
pub(crate) type SlotResetOutcome = Result<Option<String>, String>;

pub(crate) type SlotRuntimeHealth = Arc<dyn ManagedResetPort>;

pub(crate) fn slot_runtime_health<R: ManagedResetPort + 'static>(
    registry: Option<Arc<R>>,
) -> Option<SlotRuntimeHealth> {
    registry.map(|registry| registry as SlotRuntimeHealth)
}

/// Why teardown of the stored row must be refused.
pub(crate) type SlotRowGuard = for<'a> fn(
    pool: &'a PgPool,
    stored_provider: Option<&'a str>,
    channel_id: Option<&'a str>,
    session_key: &'a str,
    tmux_name: &'a str,
    caller: &'a str,
) -> BoxFuture<'a, Option<String>>;

pub(crate) trait SlotResetPort {
    /// Why the host guard refuses a teardown of this stored row, if it does.
    fn row_host_refusal<'a>(
        pool: &'a PgPool,
        stored_provider: Option<&'a str>,
        channel_id: Option<&'a str>,
        session_key: &'a str,
        tmux_name: &'a str,
        caller: &'a str,
    ) -> BoxFuture<'a, Option<String>>;
}
