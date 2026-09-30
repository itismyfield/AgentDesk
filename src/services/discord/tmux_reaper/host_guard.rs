//! Host guard pieces the tmux reaper's automatic teardowns take before a state change.
//! They reuse the keyed sessions-row gate; a failed tmux probe never reads as absent.

use std::sync::Arc;

use futures::future::BoxFuture;
use poise::serenity_prelude::ChannelId;

use super::super::SharedData;
use super::super::adk_session::build_namespaced_session_key;
use super::super::host_teardown_gate::shared_teardown;
use super::super::inflight::{KeyedTeardown, keyed_teardown};
use super::super::session_identity::tmux_name_from_session_key;
use crate::services::provider::{ProviderKind, parse_provider_and_channel_from_tmux_name};
use crate::services::session_host::{
    HostPresence, HostSessionRef, InteractiveSessionHost, TmuxHost,
};

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
pub(super) fn absent_only_if_missing(presence: HostPresence) -> bool {
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

/// The fresh-routine backstop's gate: each row a run recorded as owning `session_name` is
/// read and any kept or unreadable one keeps it; with none, the orphan rule applies.
pub(super) async fn routine_teardown(
    shared: &SharedData,
    pool: &sqlx::PgPool,
    provider: &ProviderKind,
    routine_id: &str,
    session_name: &str,
) -> KeyedTeardown {
    let caller = "fresh_routine_backstop";
    let owned = match routine_owned_session_keys(pool, session_name).await {
        Ok(owned) => owned,
        Err(error) => {
            tracing::warn!(caller, routine_id, session_name, %error, "routine ownership unreadable");
            return KeyedTeardown::Kept;
        }
    };
    if owned.is_empty() {
        let key = build_namespaced_session_key(&shared.token_hash, provider, session_name);
        return keyed_teardown(
            Some(pool),
            provider,
            0,
            Some(&key),
            session_name,
            None,
            caller,
        )
        .await;
    }
    let mut newest = None;
    for key in &owned {
        // A bare or unparsable record owns the session but names no row to read.
        if tmux_name_from_session_key(key).is_none() {
            return KeyedTeardown::Kept;
        }
        let gate = keyed_teardown(
            Some(pool),
            provider,
            0,
            Some(key),
            session_name,
            None,
            caller,
        );
        match gate.await {
            KeyedTeardown::Kept => return KeyedTeardown::Kept,
            admitted => {
                newest.get_or_insert(admitted);
            }
        }
    }
    newest.unwrap_or(KeyedTeardown::Kept)
}

/// Every token a routine run recorded for `session_name`, newest first, as written and
/// trimmed; a bare or unparsable token is kept so the caller refuses on it.
async fn routine_owned_session_keys(
    pool: &sqlx::PgPool,
    session_name: &str,
) -> Result<Vec<String>, sqlx::Error> {
    let recorded: Vec<String> = sqlx::query_scalar(
        "SELECT owned_tmux_session FROM routine_runs
          WHERE strpos(owned_tmux_session, $1) > 0
          GROUP BY owned_tmux_session
          ORDER BY MAX(started_at) DESC, MAX(id) DESC",
    )
    .bind(session_name)
    .fetch_all(pool)
    .await?;
    let mut owned: Vec<String> = Vec::new();
    for raw in &recorded {
        let token = raw.trim();
        let tail = token.rsplit_once(':').map_or(token, |(_, tail)| tail);
        if tail.trim() != session_name {
            continue;
        }
        for key in [raw.as_str(), token] {
            if !owned.iter().any(|seen| seen == key) {
                owned.push(key.to_string());
            }
        }
    }
    Ok(owned)
}

/// The listed session of one completed unified-thread run, once the host guard admits
/// it; the thread channel keys its sessions row and inflight row.
pub(super) async fn unified_thread_target(
    shared: &SharedData,
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
        .find(|name| name.starts_with(&prefix) && name.ends_with(&full_suffix))?;
    let (provider, _) = parse_provider_and_channel_from_tmux_name(name)?;
    let channel_id = thread_channel_id.parse::<u64>().ok()?;
    let gate = shared_teardown(shared, &provider, channel_id, name, None, "unified_kill");
    (!matches!(gate.await, KeyedTeardown::Kept)).then(|| name.clone())
}
