//! Host check for router, idle and resume paths that act on a channel's tmux name, or hold
//! none: Herdr, unknown or conflicting evidence defers before any kill, clear, evict or release.

use poise::serenity_prelude::ChannelId;
use sqlx::PgPool;

use super::SharedData;
use super::host_key_derivation::{
    derive_channel_session_name, derive_hosted_lookup, merge_lookups,
};
use super::inflight::{KeyedTeardown, keyed_teardown, teardown_for_lookup};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedLookup, HostedLookupKey, load_hosted_execution_pg,
};
use crate::services::provider::ProviderKind;

/// The caller the guard's refusal log names for a channel-keyed check.
const CALLER: &str = "router_idle_host_check";

/// How long the rehydration pass waits for one row before it keeps the mirror.
#[cfg(unix)]
const MIRROR_LOOKUP_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether the runtime's row for `tmux_name` or for its channel keeps the caller off it.
/// A row not written yet, with no marker or inflight trace of another host, keeps main's path.
pub(super) async fn channel_session_deferred(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: u64,
    tmux_name: &str,
) -> bool {
    let hashes = [shared.token_hash.clone()];
    let lookup = identity_lookup(shared, &hashes, None, provider, channel_id, tmux_name).await;
    kept(
        shared, lookup, provider, channel_id, None, tmux_name, CALLER,
    )
}

/// [`channel_session_deferred`] for a provider-wide pass, which one runtime runs for
/// every bot: each bot hash registered for the provider is read.
pub(super) async fn sweep_session_deferred(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: u64,
    tmux_name: &str,
) -> bool {
    let hashes = provider_hashes(shared, provider).await;
    let lookup = identity_lookup(shared, &hashes, None, provider, channel_id, tmux_name).await;
    kept(
        shared, lookup, provider, channel_id, None, tmux_name, CALLER,
    )
}

/// [`sweep_session_deferred`] for a pass holding no channel name: only a found legacy row
/// with no host trace admits it.
pub(super) async fn nameless_sweep_deferred(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: u64,
) -> bool {
    let gate = nameless_channel_gate(shared, provider, channel_id).await;
    let held = !matches!(gate, Ok(KeyedTeardown::Cleared(_)));
    if held {
        tracing::warn!(
            caller = CALLER,
            channel_id,
            lookup = ?gate.as_ref().err(),
            "no channel name; card kept"
        );
    }
    held
}

/// [`channel_session_deferred`] for a managed tmux channel holding no channel name. The
/// registered fallback name the session key uses takes the named path; with none, only a
/// found legacy row, or no row for a channel with nothing in flight, admits it.
pub(super) async fn nameless_channel_deferred(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: u64,
) -> bool {
    if !provider.uses_managed_tmux_backend() {
        return false;
    }
    // Test runtimes built without a pool predate the guard; production requires PostgreSQL.
    #[cfg(test)]
    if shared.pg_pool.is_none() {
        return false;
    }
    let channel = ChannelId::new(channel_id);
    if let Some(name) = super::adk_session::registered_channel_fallback_name(channel, provider) {
        let tmux_name = provider.build_tmux_session_name(&name);
        return channel_session_deferred(shared, provider, channel_id, &tmux_name).await;
    }
    let gate = nameless_channel_gate(shared, provider, channel_id).await;
    let held = match &gate {
        Ok(KeyedTeardown::Cleared(_)) => false,
        Err(HostedLookup::Missing) => !unkeyed_and_idle(provider, channel_id),
        _ => true,
    };
    if held {
        tracing::warn!(
            caller = CALLER,
            channel_id,
            lookup = ?gate.as_ref().err(),
            "no channel name; promote held"
        );
    } else if gate.is_err() {
        tracing::info!(
            caller = CALLER,
            channel_id,
            "unkeyed idle channel with no row; promotes"
        );
    }
    held
}

/// No registered fallback name lets a turn build the channel's session key, so its turns
/// write no row and run off tmux, and no inflight row is stored for it.
fn unkeyed_and_idle(provider: &ProviderKind, channel_id: u64) -> bool {
    let fallback = super::adk_session::registered_channel_fallback_name;
    let inflight = super::inflight::load_inflight_state_read_only_result(provider, channel_id);
    fallback(ChannelId::new(channel_id), provider).is_none() && matches!(inflight, Ok(None))
}

/// The keyed gate on the name the channel's one row records under its own key, found
/// through every registered hash; no row, a failed read or two rows answer as the lookup.
async fn nameless_channel_gate(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: u64,
) -> Result<KeyedTeardown, HostedLookup> {
    let Some(pool) = shared.pg_pool.as_ref() else {
        return Err(HostedLookup::Unknown("no postgres pool".to_string()));
    };
    let hashes = provider_hashes(shared, provider).await;
    let name = derive_channel_session_name(pool, &hashes, provider, channel_id).await?;
    let lookup = identity_lookup(shared, &hashes, None, provider, channel_id, &name).await;
    Ok(keyed(
        shared, lookup, provider, channel_id, None, &name, CALLER,
    ))
}

/// [`channel_session_deferred`] with the turn's own key read too; a turn with no key
/// keeps the name-only path its teardown keeps.
pub(super) async fn turn_session_deferred(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: u64,
    session_key: Option<&str>,
    tmux_name: &str,
    caller: &str,
) -> bool {
    let Some(session_key) = session_key else {
        tracing::warn!(
            caller,
            tmux_name,
            "turn has no session key; host check skipped"
        );
        return false;
    };
    let hashes = [shared.token_hash.clone()];
    let exact = Some(session_key);
    let lookup = identity_lookup(shared, &hashes, exact, provider, channel_id, tmux_name).await;
    kept(
        shared, lookup, provider, channel_id, exact, tmux_name, caller,
    )
}

/// The bot hashes registered for `provider`, with the runtime's own.
async fn provider_hashes(shared: &SharedData, provider: &ProviderKind) -> Vec<String> {
    let mut hashes = match shared.health_registry() {
        Some(registry) => registry.registered_token_hashes(provider).await,
        None => Vec::new(),
    };
    hashes.push(shared.token_hash.clone());
    hashes.sort();
    hashes.dedup();
    hashes
}

/// Each hash's key for `tmux_name` and its `(provider, hash, channel)` row, plus the
/// caller's own key, merged: a row under another name or bot still decides.
async fn identity_lookup(
    shared: &SharedData,
    hashes: &[String],
    exact: Option<&str>,
    provider: &ProviderKind,
    channel_id: u64,
    tmux_name: &str,
) -> HostedLookup {
    let Some(pool) = shared.pg_pool.as_ref() else {
        return HostedLookup::Unknown("no postgres pool".to_string());
    };
    let derived = derive_hosted_lookup(pool, hashes, provider, channel_id, tmux_name).await;
    let Some(key) = exact else {
        return derived;
    };
    let exact = load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(key)).await;
    merge_lookups(vec![exact, derived])
}

/// Whether the keyed gate keeps the session on `lookup`.
fn kept(
    shared: &SharedData,
    lookup: HostedLookup,
    provider: &ProviderKind,
    channel_id: u64,
    exact: Option<&str>,
    tmux_name: &str,
    caller: &str,
) -> bool {
    let gate = keyed(
        shared, lookup, provider, channel_id, exact, tmux_name, caller,
    );
    matches!(gate, KeyedTeardown::Kept)
}

/// The keyed gate's answer on `lookup`, with the marker and inflight evidence it reads.
fn keyed(
    shared: &SharedData,
    lookup: HostedLookup,
    provider: &ProviderKind,
    channel_id: u64,
    exact: Option<&str>,
    tmux_name: &str,
    caller: &str,
) -> KeyedTeardown {
    let own =
        super::adk_session::build_namespaced_session_key(&shared.token_hash, provider, tmux_name);
    let key = exact.unwrap_or(&own);
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

/// Sync [`sweep_session_deferred`] for the rehydration pass on the blocking pool;
/// a lookup that cannot run or finish in time keeps the mirror.
#[cfg(unix)]
pub(super) fn mirror_evict_admitted(
    shared: &SharedData,
    provider: &ProviderKind,
    tmux_name: &str,
) -> bool {
    // Test runtimes built without a pool predate the guard; production requires PostgreSQL.
    #[cfg(test)]
    if shared.pg_pool.is_none() {
        return true;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        tracing::warn!(tmux_name, "no runtime for the host check; mirror kept");
        return false;
    };
    // The pass sees every provider's panes; the name, not the pass, picks the row key.
    let parsed = crate::services::provider::parse_provider_and_channel_from_tmux_name(tmux_name);
    let provider = parsed.map_or_else(|| provider.clone(), |(kind, _)| kind);
    let channel = crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(tmux_name);
    let (channel, caller) = (channel.unwrap_or(0), "idle_mirror_evict");
    let gate = async {
        let hashes = provider_hashes(shared, &provider).await;
        let lookup = identity_lookup(shared, &hashes, None, &provider, channel, tmux_name).await;
        kept(shared, lookup, &provider, channel, None, tmux_name, caller)
    };
    match handle.block_on(tokio::time::timeout(MIRROR_LOOKUP_BUDGET, gate)) {
        Ok(held) => !held,
        Err(_) => {
            tracing::warn!(tmux_name, "host check timed out; mirror kept");
            false
        }
    }
}

/// Why a `/resume` of `session_key` may not touch its session: anything but a found
/// legacy row with no host trace. A row with no runtime channel reads no inflight row.
pub(crate) async fn resume_host_refusal(
    pool: &PgPool,
    provider: Option<&ProviderKind>,
    channel_id: Option<ChannelId>,
    session_key: &str,
    tmux_name: &str,
) -> Option<String> {
    let Some(provider) = provider else {
        return Some("the session's provider is unknown".to_string());
    };
    let channel = channel_id.map_or(0, ChannelId::get);
    let caller = "session_resume";
    let key = Some(session_key);
    let gate = keyed_teardown(Some(pool), provider, channel, key, tmux_name, None, caller);
    match gate.await {
        KeyedTeardown::Cleared(_) => None,
        KeyedTeardown::RowMissing => Some("the session has no sessions row".to_string()),
        KeyedTeardown::Kept => Some("the session is not a legacy tmux session".to_string()),
    }
}

#[cfg(all(test, unix))]
#[path = "host_defer_gate_tests.rs"]
pub(crate) mod tests;
