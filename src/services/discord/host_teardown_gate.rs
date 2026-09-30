//! Host check for an automatic teardown that knows only a provider, a channel and a
//! tmux name: it builds the key the channel's turns write and takes the keyed gate.

use poise::serenity_prelude::ChannelId;

use super::SharedData;
use super::health::HealthRegistry;
use super::inflight::{KeyedTeardown, keyed_teardown};
use crate::services::provider::ProviderKind;
use crate::services::session_host::{ClearedHostSession, HostLiveness};

/// The keyed gate's verdict for a caller outside the discord module.
pub(crate) enum ChannelTeardown {
    /// A found legacy row with no host trace.
    Cleared(ClearedHostSession),
    /// No sessions row and no marker or inflight trace of another host.
    RowMissing,
    /// Herdr, unknown, conflicting or unreadable evidence, or no runtime to key it.
    Kept,
}

/// The gate for `tmux_name` under `provider`'s runtime on `channel_id`, read as stored.
pub(crate) async fn channel_teardown(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_name: &str,
    observed: Option<HostLiveness>,
    caller: &str,
) -> ChannelTeardown {
    let Some(shared) = registry
        .shared_for_provider_on_channel(provider, channel_id)
        .await
    else {
        tracing::warn!(caller, tmux_name, "host guard kept the session: no runtime");
        return ChannelTeardown::Kept;
    };
    let gate = shared_teardown(
        &shared,
        provider,
        channel_id.get(),
        tmux_name,
        observed,
        caller,
    );
    match gate.await {
        KeyedTeardown::Cleared(session) => ChannelTeardown::Cleared(session),
        KeyedTeardown::RowMissing => ChannelTeardown::RowMissing,
        KeyedTeardown::Kept => ChannelTeardown::Kept,
    }
}

/// [`keyed_teardown`] under `shared`'s namespaced key for `tmux_name`.
pub(in crate::services::discord) async fn shared_teardown(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: u64,
    tmux_name: &str,
    observed: Option<HostLiveness>,
    caller: &str,
) -> KeyedTeardown {
    let key =
        super::adk_session::build_namespaced_session_key(&shared.token_hash, provider, tmux_name);
    let pool = shared.pg_pool.as_ref();
    keyed_teardown(
        pool,
        provider,
        channel_id,
        Some(&key),
        tmux_name,
        observed,
        caller,
    )
    .await
}

#[cfg(test)]
pub(crate) mod test_support;
