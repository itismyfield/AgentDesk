//! The queue-cancel port: what a KE queue cancel asks of the provider runtimes the Discord health
//! registry holds once its lifecycle stop ran, and the plain result of a cancelled mailbox finish.

use futures::future::BoxFuture;
use poise::serenity_prelude::ChannelId;

use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::ChannelMailboxHandle;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FinishCancelledMailboxResult {
    pub cleared_active_turn: bool,
    pub global_active_decremented: bool,
    pub has_pending_queue: bool,
    pub runtime_session_cleared: bool,
}

/// The queue effects a cancel applies through the registry, each one call into it.
pub(crate) trait QueueCancelPort: Send + Sync {
    /// Schedules the preserved queue's drain; only the depth observed after the attempt.
    fn schedule_pending_queue_drain_after_cancel<'a>(
        &'a self,
        provider_name: &'a str,
        channel_id: ChannelId,
        reason: &'static str,
    ) -> BoxFuture<'a, Option<usize>>;

    /// Finishes the cancelled active turn of a channel's mailbox; nothing without a registry.
    fn finish_cancelled_provider_channel_mailbox<'a>(
        registry: Option<&'a Self>,
        provider_name: Option<&'a str>,
        channel_id: Option<u64>,
        stop_source: &'static str,
        expected_actor: Option<&'a ChannelMailboxHandle>,
    ) -> BoxFuture<'a, FinishCancelledMailboxResult>
    where
        Self: Sized;

    /// The token hash of the runtime serving `provider` on `channel_id`, if one does.
    fn channel_token_hash<'a>(
        &'a self,
        provider: &'a ProviderKind,
        channel_id: ChannelId,
    ) -> BoxFuture<'a, Option<String>>;

    /// A live Herdr turn's non-forced cancel, run as its `/stop`.
    fn run_herdr_cancel<'a>(
        &'a self,
        provider: &'a ProviderKind,
        channel: u64,
    ) -> BoxFuture<'a, Option<serde_json::Value>>;
}
