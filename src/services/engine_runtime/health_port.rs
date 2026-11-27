//! Narrow KE ports onto the provider runtimes the Discord health registry holds.
//! KE takes these instead of the registry, which implements them on its side.

use futures::future::BoxFuture;
use poise::serenity_prelude::ChannelId;

/// Judges, read only, a provider-state reset of one channel against the provider's runtime.
/// `None` means no runtime is registered for the provider, so there is nothing to clear.
pub(crate) trait ManagedResetPort<V>: Send + Sync {
    fn managed_reset_verdict<'a>(
        &'a self,
        provider_name: &'a str,
        channel_id: ChannelId,
        session_key: Option<&'a str>,
    ) -> BoxFuture<'a, Option<V>>;
}

mod turn_stop;
pub use turn_stop::{
    FINISH_UNOBSERVED_PATH, InflightDisposition, RuntimeTurnStopResult, TOKEN_SUPERSEDED_PATH,
};
pub(crate) use turn_stop::{PendingQueueView, TurnStopJudgement, TurnStopPort};

mod app_health;
mod headless_turn;
pub(crate) mod owner_input;
mod queue_cancel;
mod registry;
mod resume_runtime;
mod routine_process;
mod slot_reset;
mod stale_sweep;
pub use app_health::{AppHealth, AppHealthBinding, AppHealthRegistry};
pub(crate) use headless_turn::{
    HeadlessDmTurnStart, HeadlessTurnPort, HeadlessTurnReservationView, HeadlessTurnStart,
};
pub(crate) use owner_input::OwnerInputRuntimePort;
pub use queue_cancel::FinishCancelledMailboxResult;
pub(crate) use queue_cancel::QueueCancelPort;
pub(crate) use registry::{
    RuntimeRegistryPort, install_runtime_registry_accessor, published_runtime_registry,
};
pub(crate) use resume_runtime::ResumeRuntimePort;
pub(crate) use routine_process::RoutineProcessPort;
pub(crate) use slot_reset::SlotResetPort;
pub(crate) use stale_sweep::{StaleSweepPort, stale_sweep_port};
