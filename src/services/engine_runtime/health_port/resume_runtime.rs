//! The resume-runtime port: what a KE `/resume` rebind reads and mirrors on the provider runtime
//! that owns the channel, and the host check it runs before any change.

use std::sync::Arc;

use futures::future::BoxFuture;
use poise::serenity_prelude::ChannelId;
use sqlx::PgPool;

use crate::services::provider::ProviderKind;

pub(crate) trait ResumeRuntimePort: Send + Sync {
    /// The provider runtime a rebind works on.
    type Runtime: Send + Sync;

    /// The runtime that owns `channel_id` for `provider`, if one does.
    fn resume_runtime_for_channel<'a>(
        &'a self,
        provider: &'a ProviderKind,
        channel_id: ChannelId,
    ) -> BoxFuture<'a, Option<Arc<Self::Runtime>>>;

    /// Whether `channel_id` has an active or recovery-blocking turn for `provider_name`.
    fn channel_has_active_turn<'a>(
        &'a self,
        provider_name: &'a str,
        channel_id: ChannelId,
    ) -> BoxFuture<'a, bool>;

    fn session_transition_completion(
        shared: &Arc<Self::Runtime>,
        channel_id: ChannelId,
    ) -> impl Send + '_;

    fn acquire_session_transition(
        shared: &Self::Runtime,
        channel_id: ChannelId,
    ) -> BoxFuture<'_, Option<tokio::sync::OwnedMutexGuard<()>>>;

    /// Keeps the channel's authoritative owner before teardown can remove the watcher slot.
    fn retain_resume_runtime_owner_before_teardown(
        shared: &Self::Runtime,
        channel_id: ChannelId,
        tmux_session_name: &str,
    ) -> bool;

    /// Clears the kept owner once the pane is confirmed dead.
    fn clear_resume_runtime_owner_after_death(shared: &Self::Runtime, tmux_session_name: &str);

    /// Mirrors the durable `/resume` target into the owning runtime.
    fn rebind_channel_provider_session<'a>(
        shared: &'a Self::Runtime,
        provider: &'a ProviderKind,
        channel_id: ChannelId,
        cwd: &'a str,
        session_id: &'a str,
        tmux_session_name: &'a str,
        retain_runtime_owner: bool,
    ) -> BoxFuture<'a, ()>;
}

/// The host check applies even without a registry or a channel.
pub(crate) struct ResumePorts<'a, R> {
    pub(crate) registry: Option<&'a R>,
    pub(crate) host_refusal: for<'b> fn(
        &'b PgPool,
        Option<&'b ProviderKind>,
        Option<ChannelId>,
        &'b str,
        &'b str,
    ) -> BoxFuture<'b, Option<String>>,
}
