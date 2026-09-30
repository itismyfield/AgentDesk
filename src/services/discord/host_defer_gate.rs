//! Host check for router, idle and resume paths that act on a channel's tmux name:
//! Herdr, unknown or conflicting evidence defers before any kill, clear, evict or release.

use poise::serenity_prelude::ChannelId;
use sqlx::PgPool;

use super::SharedData;
use super::host_teardown_gate::shared_teardown;
use super::inflight::{KeyedTeardown, keyed_teardown};
use crate::services::provider::ProviderKind;

/// The caller the guard's refusal log names for a channel-keyed check.
const CALLER: &str = "router_idle_host_check";

/// How long the rehydration pass waits for one row before it keeps the mirror.
#[cfg(unix)]
const MIRROR_LOOKUP_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether the runtime's row for `tmux_name` keeps the caller off it. A row not
/// written yet, with no marker or inflight trace of another host, keeps main's path.
pub(super) async fn channel_session_deferred(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: u64,
    tmux_name: &str,
) -> bool {
    let gate = shared_teardown(shared, provider, channel_id, tmux_name, None, CALLER);
    matches!(gate.await, KeyedTeardown::Kept)
}

/// [`channel_session_deferred`] under the turn's own key; a turn with no key keeps
/// the name-only path its teardown keeps.
pub(super) async fn turn_session_deferred(
    pool: Option<&PgPool>,
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
    let gate = keyed_teardown(
        pool,
        provider,
        channel_id,
        Some(session_key),
        tmux_name,
        None,
        caller,
    );
    matches!(gate.await, KeyedTeardown::Kept)
}

/// Sync [`channel_session_deferred`] for the rehydration pass on the blocking pool;
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
    let caller = "idle_mirror_evict";
    let gate = shared_teardown(
        shared,
        &provider,
        channel.unwrap_or(0),
        tmux_name,
        None,
        caller,
    );
    match handle.block_on(tokio::time::timeout(MIRROR_LOOKUP_BUDGET, gate)) {
        Ok(KeyedTeardown::Cleared(_) | KeyedTeardown::RowMissing) => true,
        Ok(KeyedTeardown::Kept) => false,
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
