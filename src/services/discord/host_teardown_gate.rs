//! Host check for an automatic teardown that knows only a provider, a channel and a
//! tmux name: it builds the key the channel's turns write and takes the keyed gate.

use poise::serenity_prelude::ChannelId;
use sqlx::PgPool;

use super::SharedData;
use super::health::HealthRegistry;
use super::inflight::{KeyedTeardown, keyed_teardown, teardown_for_lookup};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedLookup, HostedLookupKey, load_hosted_execution_pg,
};
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

/// The provider a stored row names, or with none the one its tmux name records. It only
/// fills the gate's provider: the row, marker and inflight evidence still decide.
pub(crate) fn row_provider(stored: Option<&str>, tmux_name: &str) -> Option<ProviderKind> {
    match stored.filter(|value| !value.trim().is_empty()) {
        Some(stored) => ProviderKind::from_str(stored),
        None => crate::services::provider::parse_provider_and_channel_from_tmux_name(tmux_name)
            .map(|(provider, _)| provider),
    }
}

/// The gate for a caller that read `session_key` from its own row, with the row's raw
/// provider: only that row found legacy with no host trace admits it, a missing row keeps it.
pub(crate) async fn row_gate(
    pool: &PgPool,
    stored_provider: Option<&str>,
    channel_id: u64,
    session_key: &str,
    tmux_name: &str,
    caller: &str,
) -> (ChannelTeardown, HostedLookup) {
    let Some(provider) = row_provider(stored_provider, tmux_name) else {
        let unknown = HostedLookup::Unknown("provider unknown".into());
        return (ChannelTeardown::Kept, unknown);
    };
    let lookup = load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(session_key)).await;
    let (key, found) = (Some(session_key), lookup.clone());
    match teardown_for_lookup(found, &provider, channel_id, key, tmux_name, None, caller) {
        KeyedTeardown::Cleared(session) => (ChannelTeardown::Cleared(session), lookup),
        KeyedTeardown::RowMissing | KeyedTeardown::Kept => (ChannelTeardown::Kept, lookup),
    }
}

/// Why a stored row's session may not be torn down, checked before the caller changes
/// anything; the reason keeps the row lookup's own category.
pub(crate) async fn row_host_refusal(
    pool: &PgPool,
    stored_provider: Option<&str>,
    channel_id: Option<&str>,
    session_key: &str,
    tmux_name: &str,
    caller: &str,
) -> Option<String> {
    let channel_id = channel_id.and_then(|raw| raw.trim().parse::<u64>().ok());
    let channel_id = channel_id.unwrap_or(0);
    let gate = row_gate(
        pool,
        stored_provider,
        channel_id,
        session_key,
        tmux_name,
        caller,
    );
    let lookup = match gate.await {
        (ChannelTeardown::Cleared(_), _) => return None,
        (_, HostedLookup::Found(_)) => "row found".to_string(),
        (_, HostedLookup::Missing) => "no sessions row".to_string(),
        (_, HostedLookup::Unknown(reason)) => reason,
        (_, HostedLookup::Conflict(kind)) => format!("{kind:?}"),
    };
    Some(format!(
        "`{tmux_name}` is not a confirmed legacy tmux session ({lookup})"
    ))
}

/// Whether the nameless gate keeps `channel_id`'s session for a teardown holding no tmux name.
pub(crate) async fn nameless_teardown_kept(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    channel_id: ChannelId,
    caller: &str,
) -> bool {
    let Some(shared) = registry
        .shared_for_provider_on_channel(provider, channel_id)
        .await
    else {
        tracing::warn!(caller, "host guard kept the nameless session: no runtime");
        return true;
    };
    let deferred = super::host_defer_gate::nameless_channel_deferred;
    deferred(&shared, provider, channel_id.get()).await
}

/// A nameless force-kill target's tmux name found as the cancel lookup finds it, minus its
/// inflight backfill write (the flag); an unreadable inflight row errs so the caller keeps.
pub(crate) async fn guard_tmux_name(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> Result<(Option<String>, bool), String> {
    let shared = registry.shared_for_provider_on_channel(provider, channel_id);
    let Some(shared) = shared.await else {
        return Ok((None, false));
    };
    if let Some(binding) = shared.tmux_watchers.channel_binding(&channel_id) {
        return Ok((Some(binding.tmux_session_name), false));
    }
    let row = super::inflight::load_inflight_state_read_only_result(provider, channel_id.get())?;
    if let Some(name) = row.and_then(|row| row.tmux_session_name) {
        return Ok((Some(name), true));
    }
    let data = shared.core.lock().await;
    let session = data.sessions.get(&channel_id);
    let channel_name = session.and_then(|session| session.channel_name.as_ref());
    let name = channel_name.map(|name| provider.build_tmux_session_name(name));
    Ok((name, true))
}

/// The inflight finalizer backfill the cancel lookup writes, run only once the guard admits.
pub(crate) fn backfill_inflight_after_guard(provider: &ProviderKind, channel_id: ChannelId) {
    let _ = super::inflight::load_inflight_state(provider, channel_id.get());
}

#[cfg(test)]
pub(crate) mod test_support;
