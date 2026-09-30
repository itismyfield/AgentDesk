//! Host guard pieces the tmux reaper's automatic teardowns take before a state change.
//! They reuse the keyed sessions-row gate; a failed tmux probe never reads as absent.

use std::sync::Arc;

use futures::future::BoxFuture;
use poise::serenity_prelude::ChannelId;

use super::super::SharedData;
use super::super::adk_session::build_namespaced_session_key;
use super::super::host_teardown_gate::shared_teardown;
use super::super::inflight::{KeyedTeardown, keyed_teardown};
use crate::services::provider::{ProviderKind, parse_provider_and_channel_from_tmux_name};
use crate::services::session_host::{
    HostPresence, HostSessionRef, InteractiveSessionHost, TmuxHost,
};
use crate::services::tmux_diagnostics::record_tmux_exit_reason;

/// Host verdict the stale-busy heal takes before its first probe; `true` lets it go on.
pub(super) type HostGate = dyn for<'a> Fn(&'a Arc<SharedData>, &'a ProviderKind, ChannelId, &'a str) -> Admits<'a>
    + Send
    + Sync;
type Admits<'a> = BoxFuture<'a, bool>;

/// A found legacy row heals; so does a missing row with no other-host trace, as in main,
/// because a routine session's row key is not built from its tmux name.
pub(super) fn keyed_host_gate<'a>(
    shared: &'a Arc<SharedData>,
    provider: &'a ProviderKind,
    channel_id: ChannelId,
    tmux_name: &'a str,
) -> Admits<'a> {
    Box::pin(async move {
        let caller = "stale_busy_heal";
        let gate = shared_teardown(shared, provider, channel_id.get(), tmux_name, None, caller);
        !matches!(gate.await, KeyedTeardown::Kept)
    })
}

/// Only a confirmed missing session reads absent; a failed probe never finalizes a turn.
fn absent_only_if_missing(presence: HostPresence) -> bool {
    presence == HostPresence::Missing
}

pub(super) fn tmux_session_not_missing(name: String) -> BoxFuture<'static, bool> {
    Box::pin(async move {
        let probe = move || TmuxHost.presence(HostSessionRef::tmux(&name));
        let probe = tokio::task::spawn_blocking(probe);
        match tokio::time::timeout(std::time::Duration::from_secs(10), probe).await {
            Ok(Ok(presence)) => !absent_only_if_missing(presence),
            _ => true,
        }
    })
}

/// The gate the fresh-routine backstop takes: the row the latest owning run recorded is
/// the ownership proof; without one the channel-style key finds no row, as for an orphan.
pub(super) async fn routine_teardown(
    shared: &SharedData,
    pool: &sqlx::PgPool,
    provider: &ProviderKind,
    routine_id: &str,
    session_name: &str,
) -> KeyedTeardown {
    let key = match routine_owned_session_key(pool, routine_id, session_name).await {
        Some(owned) => owned,
        None => build_namespaced_session_key(&shared.token_hash, provider, session_name),
    };
    let caller = "fresh_routine_backstop";
    keyed_teardown(
        Some(pool),
        provider,
        0,
        Some(&key),
        session_name,
        None,
        caller,
    )
    .await
}

/// The full session key the routine's latest owning run recorded, when it names exactly
/// `session_name`; a bare name or another session proves nothing.
async fn routine_owned_session_key(
    pool: &sqlx::PgPool,
    routine_id: &str,
    session_name: &str,
) -> Option<String> {
    let owned: Option<String> = sqlx::query_scalar(
        "SELECT owned_tmux_session FROM routine_runs
          WHERE routine_id = $1 AND owned_tmux_session IS NOT NULL
          ORDER BY started_at DESC, id DESC
          LIMIT 1",
    )
    .bind(routine_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    owned.filter(|key| {
        super::super::session_identity::tmux_name_from_session_key(key).as_deref()
            == Some(session_name)
    })
}

/// Kills the listed session of one completed unified-thread run once the host guard
/// admits it; the thread channel keys its sessions row and inflight row.
pub(super) async fn kill_unified_thread_session(
    shared: &Arc<SharedData>,
    thread_channel_id: &str,
    names: &[String],
) -> Option<String> {
    // The kill signal carries the raw thread channel ID. Thread tmux sessions
    // are named "{parent_channel_name}-t{thread_channel_id}{env_suffix}".
    // We must find the matching tmux session by scanning for the exact suffix
    // including env isolation to avoid killing sessions from other environments.
    let env_suffix = crate::services::provider::tmux_env_suffix();
    let full_suffix = format!("-t{thread_channel_id}{env_suffix}");
    let prefix = format!("{}-", crate::services::provider::TMUX_SESSION_PREFIX);
    let name = names
        .iter()
        .find(|name| name.starts_with(&prefix) && name.ends_with(&full_suffix))?
        .clone();
    let (provider, _) = parse_provider_and_channel_from_tmux_name(&name)?;
    let channel_id = thread_channel_id.parse::<u64>().ok()?;
    let gate = shared_teardown(shared, &provider, channel_id, &name, None, "unified_kill");
    if matches!(gate.await, KeyedTeardown::Kept) {
        return None;
    }
    let target = name.clone();
    tokio::task::spawn_blocking(move || {
        record_tmux_exit_reason(&target, "unified-thread run completed");
        crate::services::platform::tmux::kill_session(&target, "unified-thread run completed");
    })
    .await
    .ok()?;
    Some(name)
}
