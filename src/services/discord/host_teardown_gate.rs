//! Host check for an automatic teardown that knows only a provider, a channel and a
//! tmux name: it builds the key the channel's turns write and takes the keyed gate.

use std::sync::Arc;

use poise::serenity_prelude::ChannelId;
use sqlx::PgPool;

use super::SharedData;
use super::health::HealthRegistry;
use super::host_key_derivation::{derive_hosted_lookup, merge_lookups};
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

/// The tmux names `channel_id`'s runtime would stop: its watcher, its active turn, its inflight
/// row and its session's channel; an unreadable inflight row errs.
async fn runtime_names(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    let watcher = shared.tmux_watchers.channel_binding(&channel_id);
    names.extend(watcher.map(|binding| binding.tmux_session_name));
    let snapshot = super::mailbox_snapshot(shared, channel_id).await;
    names.extend(
        snapshot
            .cancel_token
            .and_then(|token| token.tmux_session_name()),
    );
    let row = super::inflight::load_inflight_state_read_only_result(provider, channel_id.get())?;
    names.extend(row.and_then(|row| row.tmux_session_name));
    let data = shared.core.lock().await;
    let session = data.sessions.get(&channel_id);
    let channel_name = session.and_then(|session| session.channel_name.as_ref());
    names.extend(channel_name.map(|name| provider.build_tmux_session_name(name)));
    names.retain(|name| !name.trim().is_empty());
    Ok(names)
}

/// Whether everything `channel_id`'s runtime would stop is the session a verdict approved; with
/// no name approved, the runtime must hold no session name at all.
pub(crate) async fn runtime_target_holds(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    approved: Option<&str>,
) -> bool {
    match runtime_names(shared, provider, channel_id).await {
        Ok(names) => names.iter().all(|name| Some(name.as_str()) == approved),
        Err(error) => {
            tracing::warn!(
                error,
                "host guard kept the session: its inflight is unreadable"
            );
            false
        }
    }
}

/// The force-kill gate on `channel_id`'s runtime, with the runtime it admits: that runtime holds
/// only `tmux_name`, and the caller's row, its key and the channel's row all read legacy.
pub(crate) async fn runtime_teardown(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_name: &str,
    explicit_key: Option<&str>,
    caller: &str,
) -> (ChannelTeardown, Option<Arc<SharedData>>) {
    let shared = registry.shared_for_provider_on_channel(provider, channel_id);
    let Some(shared) = shared.await else {
        tracing::warn!(caller, tmux_name, "host guard kept the session: no runtime");
        return (ChannelTeardown::Kept, None);
    };
    if !runtime_target_holds(&shared, provider, channel_id, Some(tmux_name)).await {
        let message = "host guard kept the session: its runtime holds another session";
        tracing::warn!(caller, tmux_name, "{message}");
        return (ChannelTeardown::Kept, None);
    }
    let Some(pool) = shared.pg_pool.as_ref() else {
        return (ChannelTeardown::Kept, None);
    };
    let (hashes, channel) = ([shared.token_hash.clone()], channel_id.get());
    let derived = derive_hosted_lookup(pool, &hashes, provider, channel, tmux_name).await;
    let own =
        super::adk_session::build_namespaced_session_key(&shared.token_hash, provider, tmux_name);
    let lookup = match explicit_key {
        Some(key) => {
            let exact = load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(key)).await;
            merge_lookups(vec![exact, derived])
        }
        None => derived,
    };
    let key = explicit_key.unwrap_or(&own);
    let teardown = match keyed_lookup_teardown(lookup, provider, channel, key, tmux_name, caller) {
        KeyedTeardown::Cleared(session) => ChannelTeardown::Cleared(session),
        KeyedTeardown::RowMissing if explicit_key.is_none() => ChannelTeardown::RowMissing,
        KeyedTeardown::RowMissing | KeyedTeardown::Kept => ChannelTeardown::Kept,
    };
    let admitted = !matches!(teardown, ChannelTeardown::Kept);
    (teardown, admitted.then_some(shared))
}

/// The keyed gate on a lookup already read under `key`, for every caller in this file.
fn keyed_lookup_teardown(
    lookup: HostedLookup,
    provider: &ProviderKind,
    channel_id: u64,
    key: &str,
    tmux_name: &str,
    caller: &str,
) -> KeyedTeardown {
    teardown_for_lookup(
        lookup,
        provider,
        channel_id,
        Some(key),
        tmux_name,
        None,
        caller,
    )
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
    let (key, found) = (session_key, lookup.clone());
    match keyed_lookup_teardown(found, &provider, channel_id, key, tmux_name, caller) {
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

/// The runtime the nameless gate admits for a teardown holding no tmux name; `None` keeps it,
/// as does a runtime that holds a session name after all.
pub(crate) async fn nameless_runtime_teardown(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    channel_id: ChannelId,
    caller: &str,
) -> Option<Arc<SharedData>> {
    let Some(shared) = registry
        .shared_for_provider_on_channel(provider, channel_id)
        .await
    else {
        tracing::warn!(caller, "host guard kept the nameless session: no runtime");
        return None;
    };
    let deferred = super::host_defer_gate::nameless_channel_deferred;
    if deferred(&shared, provider, channel_id.get()).await {
        return None;
    }
    let unnamed = runtime_target_holds(&shared, provider, channel_id, None).await;
    unnamed.then_some(shared)
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
