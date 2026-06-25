//! `SessionDiscovery` — worker-local loop that periodically enumerates live
//! tmux sessions, runs them through [`super::session_matcher::match_session`],
//! and reconciles its node's slice of the process-wide [`SessionRegistry`].
//!
//! Epic #2285 / E2 (issue #2344). Sits between the pure matcher (E1) and the
//! future `WatcherSupervisor` (E3) which will react to registry change
//! broadcasts. This module deliberately does **not** spawn or stop any
//! watchers — its only job is to keep the registry honest.
//!
//! ## Why worker-local (not leader-only)
//!
//! tmux is host-scoped: every node in the cluster sees only the sessions on
//! its own machine. A leader-only discovery loop on machine A literally
//! cannot enumerate sessions on machine B, so leader takeover would silently
//! lose observability of the previous leader's host. Discovery therefore runs
//! on **every** node, and `reconcile_for_node` scopes mutations to the
//! current `instance_id` — peer nodes' entries are never touched. The
//! registry's keying (session name → entry) plus the `instance_id` field
//! guarantees uniqueness even when two nodes briefly disagree.
//!
//! ## Boot reconcile
//!
//! The first poll cycle runs **immediately** when the worker starts. This
//! re-attaches the registry to any session that survived a dcserver restart
//! within a single poll cycle — Acceptance criterion B in the epic.
//!
//! ## Failure modes
//!
//! - Postgres binding-load failure: the tick is *aborted* (registry left
//!   untouched). Returning an empty directory and reconciling against it
//!   would mass-remove every entry and tell E3 to tear down every watcher.
//! - tmux enumeration failure: same — abort the tick.
//! - Pane probe returns blank (retryable `PaneProviderUnknown`): the session
//!   name is added to `preserve_present` so the registry keeps the entry
//!   even though the matcher couldn't confirm it this tick.
//!
//! ## Polling cadence
//!
//! Default 10s; configurable per-test via [`DiscoveryConfig::poll_interval`].
//! Event-driven hooks (Discord-message-on-managed-channel-without-watcher)
//! land in E3 alongside the supervisor — discovery exposes
//! [`request_discovery_tick`] for that purpose so future PRs can nudge the
//! loop without changing this module.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sqlx::{PgPool, Row as SqlxRow};
use tokio::sync::Notify;

use super::session_matcher::{
    ChannelBinding, ChannelDirectory, MatchOutcome, MatchRejection, MatchedChannel,
    match_session_detailed,
};
use super::session_registry::{RegistryChange, SessionRegistry, global_session_registry};
use crate::services::platform::tmux::{EnumeratedSession, list_sessions_with_pane_command};
use crate::services::provider::{ProviderKind, parse_provider_and_channel_from_tmux_name};

/// Knobs for the discovery loop. Production callers use [`Self::default`].
/// Kept as a struct (rather than a bare `Duration`) so future tuning (jitter,
/// backoff, leader-acquisition delay) can land without churning every call
/// site.
#[derive(Clone, Debug)]
pub struct DiscoveryConfig {
    pub poll_interval: Duration,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(10),
        }
    }
}

/// Notifier used to nudge the discovery loop into running a tick immediately
/// (e.g. when a Discord message arrives on a managed channel without an
/// active watcher). The handle is process-global so the dispatch path can
/// import it without plumbing through state.
static DISCOVERY_NOTIFY: OnceLock<Arc<Notify>> = OnceLock::new();

fn discovery_notifier() -> Arc<Notify> {
    DISCOVERY_NOTIFY
        .get_or_init(|| Arc::new(Notify::new()))
        .clone()
}

/// Request the next discovery tick to run as soon as possible without waiting
/// for the polling interval to elapse. Idempotent (extra calls coalesce — at
/// most one wakeup is queued by `Notify::notify_one`).
#[allow(dead_code)] // future on-demand-discovery hook; exercised only by #[cfg(test)] tests
pub fn request_discovery_tick() {
    discovery_notifier().notify_one();
}

/// Build a [`ChannelDirectory`] from the live agents table. Returns an error
/// when the underlying PG query fails — callers MUST treat that as "skip this
/// tick" rather than "reconcile against an empty directory", or a transient
/// DB hiccup would tear down every entry in the registry.
///
/// Logs at WARN if the directory builder rejects a per-binding collision so
/// operators can fix the config without aborting the whole tick.
#[allow(dead_code)] // compatibility helper; the discovery tick uses the live-session-filtered path
pub async fn build_channel_directory_from_pg(
    pool: &PgPool,
) -> Result<ChannelDirectory, sqlx::Error> {
    let live_session_names = HashSet::new();
    build_channel_directory_from_pg_for_live_sessions(pool, &live_session_names, None).await
}

async fn build_channel_directory_from_pg_for_live_sessions(
    pool: &PgPool,
    live_session_names: &HashSet<String>,
    local_instance_id: Option<&str>,
) -> Result<ChannelDirectory, sqlx::Error> {
    // load_graceful() does sync filesystem IO + yaml parse — push to a blocking
    // thread so we don't stall the tokio runtime on every discovery tick.
    let name_map = tokio::task::spawn_blocking(build_yaml_channel_name_map)
        .await
        .unwrap_or_default();
    let session_tmux_segments =
        load_session_tmux_segments_pg(pool, live_session_names, local_instance_id).await?;
    build_channel_directory_from_pg_with_config(pool, name_map, session_tmux_segments).await
}

/// Lookup table: `(agent_id, provider, channel_id) → channel_name`. Built once
/// from `agentdesk.yaml` so discovery can resolve the tmux session segment
/// (`channels.<provider>.name`) that the dispatch path uses to construct live
/// tmux session names.
///
/// Without this, the directory keys collapse to `(provider, channel_id)` and
/// fail to match `AgentDesk-{provider}-{channel_name}` sessions, leaving the
/// post-restart adoption path silently broken (issue #2465).
pub type ChannelNameMap = HashMap<(String, ProviderKind, String), String>;
type SessionTmuxSegmentMap = HashMap<(ProviderKind, String), String>;
const SESSION_TMUX_SEGMENTS_QUERY: &str = "SELECT provider, channel_id, session_key, instance_id
         FROM sessions
         WHERE NULLIF(TRIM(channel_id), '') IS NOT NULL
           AND NULLIF(TRIM(session_key), '') IS NOT NULL
           AND LOWER(TRIM(COALESCE(status, ''))) IN (
             'connected',
             'turn_active',
             'awaiting_bg',
             'awaiting_user',
             'idle',
             'working'
           )
           AND last_heartbeat IS NOT NULL
           AND last_heartbeat > NOW() - INTERVAL '10 minutes'
         ORDER BY last_heartbeat DESC NULLS LAST,
                  created_at DESC NULLS LAST,
                  id DESC";

/// Build the channel-name map from the live yaml config. Returns an empty map
/// on any failure so discovery degrades gracefully (legacy snowflake-keyed
/// matching).
pub fn build_yaml_channel_name_map() -> ChannelNameMap {
    let mut map: ChannelNameMap = ChannelNameMap::new();
    let config = crate::config::load_graceful();
    for agent in &config.agents {
        for (provider_str, channel_opt) in agent.channels.iter() {
            let Some(channel) = channel_opt else { continue };
            let Some(provider) = ProviderKind::from_str(provider_str) else {
                continue;
            };
            let Some(channel_id) = channel.channel_id() else {
                continue;
            };
            if let Some(channel_name) = channel.channel_name() {
                map.insert((agent.id.clone(), provider, channel_id), channel_name);
            }
        }
    }
    map
}

async fn build_channel_directory_from_pg_with_config(
    pool: &PgPool,
    name_map: ChannelNameMap,
    session_tmux_segments: SessionTmuxSegmentMap,
) -> Result<ChannelDirectory, sqlx::Error> {
    let all = crate::db::agents::load_all_agent_channel_bindings_pg(pool).await?;

    let mut directory = ChannelDirectory::new();
    for (agent_id, bindings) in all {
        // For every (provider, channel_id) pair this agent owns, register a
        // ChannelBinding. The matcher's directory is keyed by the *expected
        // tmux session name*, so duplicate provider entries that map to the
        // same channel collapse naturally.
        for (provider, channel_id) in channel_pairs_for_agent(&bindings) {
            // Look up the yaml-declared channel name for this exact
            // (agent, provider, channel_id) tuple. When present, the live
            // tmux session is `AgentDesk-{provider}-{channel_name}` so the
            // directory must key by `channel_name`; falling back to
            // `channel_id` preserves legacy bindings without a yaml entry.
            let tmux_segment = name_map
                .get(&(agent_id.clone(), provider.clone(), channel_id.clone()))
                .cloned()
                .or_else(|| {
                    session_tmux_segments
                        .get(&(provider.clone(), channel_id.clone()))
                        .cloned()
                });
            let binding = ChannelBinding {
                channel_id,
                agent_id: agent_id.clone(),
                provider,
                tmux_segment,
            };
            if let Err(error) = directory.insert(binding) {
                tracing::warn!(
                    ?error,
                    agent_id = %agent_id,
                    "session-discovery: dropping agent binding due to directory collision",
                );
            }
        }
    }
    Ok(directory)
}

/// Best-effort post-restart adoption aid:
///
/// `agentdesk.yaml` is the preferred source for a Discord channel's tmux
/// segment, but some operator/by-id channels only exist in the database. Live
/// provider sessions still persist their exact `session_key` as
/// `...:AgentDesk-{provider}-{tmux_segment}`. Use that runtime fact as a
/// fallback so discovery can re-bind an already-running tmux session after
/// dcserver restarts instead of logging it as an unowned operator session.
async fn load_session_tmux_segments_pg(
    pool: &PgPool,
    live_session_names: &HashSet<String>,
    local_instance_id: Option<&str>,
) -> Result<SessionTmuxSegmentMap, sqlx::Error> {
    let rows = sqlx::query(SESSION_TMUX_SEGMENTS_QUERY)
        .fetch_all(pool)
        .await?;

    let mut map = SessionTmuxSegmentMap::new();
    for row in rows {
        let channel_id: Option<String> = row.try_get("channel_id")?;
        let session_key: Option<String> = row.try_get("session_key")?;
        let provider_hint: Option<String> = row.try_get("provider")?;
        let row_instance_id: Option<String> = row.try_get("instance_id")?;
        let Some(channel_id) = normalize_nonempty(channel_id.as_deref()) else {
            continue;
        };
        let Some(session_key) = session_key
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        else {
            continue;
        };
        if !session_row_matches_local_instance(row_instance_id.as_deref(), local_instance_id) {
            tracing::debug!(
                session_key = %session_key,
                row_instance_id = row_instance_id.as_deref().unwrap_or("<none>"),
                local_instance_id = local_instance_id.unwrap_or("<none>"),
                "session-discovery: ignoring non-local session row while deriving tmux segment",
            );
            continue;
        }
        let Some((provider, tmux_segment)) =
            live_tmux_segment_from_session_key(session_key, live_session_names)
        else {
            continue;
        };
        if let Some(hint) = provider_hint
            .as_deref()
            .and_then(|value| ProviderKind::from_str(value.trim()))
            && hint != provider
        {
            tracing::debug!(
                session_key = %session_key,
                provider_hint = ?hint,
                parsed_provider = ?provider,
                "session-discovery: ignoring mismatched sessions.provider while deriving tmux segment",
            );
        }
        match map.entry((provider, channel_id)) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(tmux_segment);
            }
            std::collections::hash_map::Entry::Occupied(existing)
                if existing.get() != &tmux_segment =>
            {
                tracing::debug!(
                    channel_id = %existing.key().1,
                    provider = ?existing.key().0,
                    existing_tmux_segment = %existing.get(),
                    candidate_tmux_segment = %tmux_segment,
                    "session-discovery: keeping first runtime tmux segment for channel",
                );
            }
            std::collections::hash_map::Entry::Occupied(_) => {}
        }
    }
    Ok(map)
}

fn normalized_instance_id(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn session_row_matches_local_instance(
    row_instance_id: Option<&str>,
    local_instance_id: Option<&str>,
) -> bool {
    let Some(local) = normalized_instance_id(local_instance_id) else {
        return true;
    };
    normalized_instance_id(row_instance_id).is_some_and(|row| row == local)
}

fn normalize_nonempty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn tmux_name_and_segment_from_session_key(
    session_key: &str,
) -> Option<(&str, ProviderKind, String)> {
    let (_, tmux_name) = session_key.rsplit_once(':')?;
    let (provider, tmux_segment) = parse_provider_and_channel_from_tmux_name(tmux_name)?;
    Some((tmux_name, provider, tmux_segment))
}

fn live_tmux_segment_from_session_key(
    session_key: &str,
    live_session_names: &HashSet<String>,
) -> Option<(ProviderKind, String)> {
    let (tmux_name, provider, tmux_segment) = tmux_name_and_segment_from_session_key(session_key)?;
    if !live_session_names.contains(tmux_name) {
        tracing::debug!(
            session_key = %session_key,
            tmux_name = %tmux_name,
            "session-discovery: ignoring non-live session row while deriving tmux segment",
        );
        return None;
    }
    Some((provider, tmux_segment))
}

/// Extract the `(provider, channel_id)` pairs an agent declares. Today this
/// covers Claude (cc) and Codex (cdx) plus a legacy generic primary channel.
fn channel_pairs_for_agent(
    bindings: &crate::db::agents::AgentChannelBindings,
) -> Vec<(ProviderKind, String)> {
    let mut out: Vec<(ProviderKind, String)> = Vec::new();
    let mut push = |provider: ProviderKind, channel: Option<String>| {
        if let Some(channel) = channel {
            let trimmed = channel.trim();
            if !trimmed.is_empty() && !out.iter().any(|(p, c)| p == &provider && c == trimmed) {
                out.push((provider, trimmed.to_string()));
            }
        }
    };

    // Claude → discord_channel_cc; Codex → discord_channel_cdx.
    push(ProviderKind::Claude, bindings.discord_channel_cc.clone());
    push(ProviderKind::Codex, bindings.discord_channel_cdx.clone());
    push(ProviderKind::Codex, bindings.discord_channel_alt.clone());

    // Legacy primary channel: routed under the configured provider when set.
    if let Some(provider_str) = bindings.provider.as_deref() {
        if let Some(provider) = ProviderKind::from_str(provider_str) {
            push(provider, bindings.discord_channel_id.clone());
        }
    }

    out
}

/// Result of a single discovery tick. Returned for tracing and tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    pub enumerated: usize,
    pub matched: usize,
    pub changes: Vec<RegistryChange>,
}

/// Pure-ish tick body — accepts the live tmux enumeration as input so unit
/// tests don't need a real tmux. Returns the change set the registry emitted.
///
/// `instance_id` is the cluster identity of the current dcserver process. Each
/// node only reconciles its own slice of the registry (tmux is host-local);
/// entries owned by other nodes are not touched.
///
/// Sessions whose matcher outcome is a *retryable* rejection (e.g. blank pane
/// command — see [`MatchRejection::PaneProviderUnknown`]) are passed to
/// `reconcile_for_node` as `preserve_present` so the registry does not remove
/// a still-alive session just because the pane probe was momentarily empty.
pub fn reconcile_from_enumeration(
    instance_id: Option<&str>,
    enumeration: Vec<EnumeratedSession>,
    directory: &ChannelDirectory,
    registry: &SessionRegistry,
) -> TickReport {
    reconcile_from_enumeration_with_process_args_probe(
        instance_id,
        enumeration,
        directory,
        registry,
        crate::services::platform::tmux::read_process_args,
    )
}

/// `process_args_probe` is injected so tests can simulate a process-title-rewriting
/// provider (e.g. claude code 2.1.143 setting its title to `2.1.143`) without
/// spawning a real process. Production uses
/// [`crate::services::platform::tmux::read_process_args`].
pub(crate) fn reconcile_from_enumeration_with_process_args_probe(
    instance_id: Option<&str>,
    enumeration: Vec<EnumeratedSession>,
    directory: &ChannelDirectory,
    registry: &SessionRegistry,
    process_args_probe: fn(u32) -> Option<String>,
) -> TickReport {
    let enumerated = enumeration.len();
    let mut matches: Vec<MatchedChannel> = Vec::new();
    let mut preserve_present: Vec<String> = Vec::new();
    for session in enumeration {
        let effective_pane_cmd = resolve_effective_pane_command(&session, process_args_probe);
        let outcome =
            match_session_detailed(&session.session_name, Some(&effective_pane_cmd), directory);
        match outcome {
            MatchOutcome::Matched(matched) => matches.push(matched),
            MatchOutcome::Rejected(reason) => {
                if is_retryable_rejection(&reason) {
                    // Session is physically present in tmux — protect the
                    // registry entry from removal until we have a definitive
                    // answer.
                    preserve_present.push(session.session_name.clone());
                }
                trace_rejection(&session, &reason);
            }
        }
    }
    let matched = matches.len();
    let changes = registry.reconcile_for_node(instance_id, matches, &preserve_present);
    if !changes.is_empty() {
        tracing::info!(
            change_count = changes.len(),
            enumerated,
            matched,
            preserve_present = preserve_present.len(),
            "session-discovery tick produced registry changes"
        );
    }
    TickReport {
        enumerated,
        matched,
        changes,
    }
}

/// Pick the most authoritative fingerprint string for provider detection.
///
/// Prefers `pane_current_command` when it already maps to a known provider —
/// that's the cheap path and matches the documented contract. Falls back to
/// the live process args when the pane command is non-empty but doesn't
/// resolve (e.g. claude code 2.1.143 sets its process title to the version
/// string, hiding the underlying `claude` binary; or a Codex companion pane
/// reports the `node` wrapper). Returns the original pane command when no
/// fallback is possible so empty/whitespace inputs still surface as the
/// retryable `PaneProviderUnknown` rejection.
fn resolve_effective_pane_command(
    session: &EnumeratedSession,
    process_args_probe: fn(u32) -> Option<String>,
) -> String {
    let pane_cmd = session.pane_current_command.trim();
    // Empty → leave as-is; matcher handles it as PaneProviderUnknown.
    // Already known → no probe needed.
    // Already a managed wrapper → no probe needed; matcher trusts the
    // session-name-encoded provider.
    if pane_cmd.is_empty()
        || crate::services::cluster::session_matcher::detect_provider_from_pane_command(pane_cmd)
            .is_some()
        || crate::services::cluster::session_matcher::is_agentdesk_managed_wrapper_command(pane_cmd)
    {
        return session.pane_current_command.clone();
    }
    match process_args_probe(session.pane_pid) {
        Some(args) => provider_fingerprint_from_process_args(&args)
            .unwrap_or_else(|| session.pane_current_command.clone()),
        _ => session.pane_current_command.clone(),
    }
}

fn provider_fingerprint_from_process_args(process_args: &str) -> Option<String> {
    process_args
        .split_whitespace()
        .enumerate()
        .find_map(|(index, token)| {
            let token = token.trim_matches(|c: char| c == '"' || c == '\'');
            if index != 0 && !token.contains('/') && !token.contains('\\') {
                return None;
            }
            crate::services::cluster::session_matcher::detect_provider_from_pane_command(token)
                .map(|_| token.to_string())
        })
}

/// Returns true when a `MatchRejection` reflects a *transient* probing issue
/// rather than a definitive "this session does not belong in the registry".
///
/// Only [`MatchRejection::PaneProviderUnknown`] (blank/unreadable pane command)
/// qualifies today. `PaneProviderMismatch` is intentionally NOT retryable: a
/// pane that is now running a different binary is a definitive sign the
/// previously-matched provider has died, and the supervisor should tear the
/// watcher down (propagated as a normal `Removed` event).
fn is_retryable_rejection(reason: &MatchRejection) -> bool {
    matches!(reason, MatchRejection::PaneProviderUnknown { .. })
}

fn trace_rejection(session: &EnumeratedSession, reason: &MatchRejection) {
    // Only trace at INFO for genuinely interesting rejections — bare
    // non-AgentDesk sessions are background noise on a developer host.
    match reason {
        MatchRejection::NotAgentDeskNamed => {
            tracing::trace!(session = %session.session_name, "session-discovery: not AgentDesk-named");
        }
        MatchRejection::UnknownProvider(p) => {
            tracing::debug!(session = %session.session_name, provider = %p, "session-discovery: unknown provider in session name");
        }
        MatchRejection::NoChannelBinding {
            session_name,
            provider,
        } => {
            tracing::info!(
                session = %session_name,
                provider = ?provider,
                "session-discovery: AgentDesk-named session has no channel binding (operator session?)",
            );
        }
        MatchRejection::PaneProviderUnknown {
            session_name,
            expected,
        } => {
            tracing::debug!(
                session = %session_name,
                expected = ?expected,
                "session-discovery: pane command unknown — will retry next tick",
            );
        }
        MatchRejection::PaneProviderMismatch {
            session_name,
            expected,
            actual_pane_command,
            detected,
        } => {
            tracing::info!(
                session = %session_name,
                expected = ?expected,
                detected = ?detected,
                actual = %actual_pane_command,
                "session-discovery: pane is running a different provider than the binding declares",
            );
        }
    }
}

/// The discovery loop — runs on every cluster node (worker-local), each
/// scoped to its own `instance_id` slice of the shared in-memory registry.
/// tmux is host-local, so cross-node leader takeover cannot relocate
/// observability; therefore discovery cannot be leader-only.
///
/// Returns when `shutdown` flips true.
pub async fn run_discovery_loop(
    instance_id: Option<String>,
    pool: Arc<PgPool>,
    config: DiscoveryConfig,
    shutdown: Arc<AtomicBool>,
) {
    let registry = global_session_registry();
    let notifier = discovery_notifier();
    tracing::info!(
        instance_id = instance_id.as_deref().unwrap_or("<none>"),
        poll_interval_ms = config.poll_interval.as_millis() as u64,
        "session-discovery loop entering"
    );

    // Boot reconcile: run once immediately so survived sessions re-attach
    // within one poll cycle (epic acceptance criterion).
    if !shutdown.load(Ordering::Acquire) {
        run_single_tick(instance_id.as_deref(), pool.as_ref(), &registry).await;
    }

    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        // Wait for either the polling interval to elapse or an external
        // request_discovery_tick() to fire — whichever happens first.
        let sleep = tokio::time::sleep(config.poll_interval);
        tokio::pin!(sleep);
        tokio::select! {
            _ = &mut sleep => {}
            _ = notifier.notified() => {
                tracing::debug!("session-discovery: external tick request");
            }
        }
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        run_single_tick(instance_id.as_deref(), pool.as_ref(), &registry).await;
    }
    tracing::info!("session-discovery loop exiting");
}

async fn run_single_tick(
    instance_id: Option<&str>,
    pool: &PgPool,
    registry: &SessionRegistry,
) -> TickReport {
    // #3651: under pool pressure, yield this tick to foreground turn ingestion.
    // Returning a default report leaves the registry untouched (same invariant
    // as the PG-failure abort below), so a yielded tick never wipes live entries
    // and the next tick re-discovers once pressure clears.
    if crate::db::postgres::background_should_yield(pool) {
        tracing::debug!("session-discovery: yielding tick to foreground under pool pressure",);
        return TickReport::default();
    }
    let enumeration_result = tokio::task::spawn_blocking(list_sessions_with_pane_command).await;
    let enumeration = match enumeration_result {
        Ok(Ok(sessions)) => sessions,
        Ok(Err(error)) => {
            tracing::warn!(error, "session-discovery: tmux enumeration failed");
            return TickReport::default();
        }
        Err(error) => {
            tracing::warn!(?error, "session-discovery: tmux enumeration join failed");
            return TickReport::default();
        }
    };
    let live_session_names: HashSet<String> = enumeration
        .iter()
        .map(|session| session.session_name.clone())
        .collect();
    // PG load failure → ABORT THE TICK. Returning a default report leaves the
    // registry untouched, so a transient PG hiccup never wipes live entries.
    let directory = match build_channel_directory_from_pg_for_live_sessions(
        pool,
        &live_session_names,
        instance_id,
    )
    .await
    {
        Ok(dir) => dir,
        Err(error) => {
            tracing::warn!(
                ?error,
                "session-discovery: agent-binding load failed; skipping tick to preserve registry",
            );
            return TickReport::default();
        }
    };
    reconcile_from_enumeration(instance_id, enumeration, &directory, registry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::cluster::session_matcher::expected_session_name_for;

    fn binding(channel: &str, agent: &str, provider: ProviderKind) -> ChannelBinding {
        ChannelBinding {
            channel_id: channel.to_string(),
            agent_id: agent.to_string(),
            provider,
            tmux_segment: None,
        }
    }

    fn binding_named(
        channel_id: &str,
        channel_name: &str,
        agent: &str,
        provider: ProviderKind,
    ) -> ChannelBinding {
        ChannelBinding {
            channel_id: channel_id.to_string(),
            agent_id: agent.to_string(),
            provider,
            tmux_segment: Some(channel_name.to_string()),
        }
    }

    fn enumerated(session: &str, pane: &str) -> EnumeratedSession {
        EnumeratedSession {
            session_name: session.to_string(),
            pane_current_command: pane.to_string(),
            pane_pid: 0,
        }
    }

    fn enumerated_with_pid(session: &str, pane: &str, pid: u32) -> EnumeratedSession {
        EnumeratedSession {
            session_name: session.to_string(),
            pane_current_command: pane.to_string(),
            pane_pid: pid,
        }
    }

    /// Test probe: maps a few known PIDs to process argument strings. PIDs not in this
    /// table return `None`, simulating a failed `ps`/proc lookup.
    fn fake_process_args_probe(pid: u32) -> Option<String> {
        match pid {
            100 => Some("/Users/me/.local/bin/claude --dangerously-skip-permissions".to_string()),
            101 => Some(
                "/opt/homebrew/bin/node /Users/me/.local/bin/codex-companion.js --session abc"
                    .to_string(),
            ),
            // PID 102 simulates an exited process — probe yields None.
            _ => None,
        }
    }

    const NODE_A: &str = "mac-mini";
    const NODE_B: &str = "mac-book";

    #[test]
    fn reconcile_adds_matched_sessions_and_skips_garbage() {
        let directory = ChannelDirectory::from_bindings(vec![
            binding("c-claude", "agent-a", ProviderKind::Claude),
            binding("c-codex", "agent-b", ProviderKind::Codex),
        ]);
        let registry = SessionRegistry::new();
        let s_claude = expected_session_name_for(None, &ProviderKind::Claude, "c-claude");
        let s_codex = expected_session_name_for(None, &ProviderKind::Codex, "c-codex");

        let enumeration = vec![
            enumerated(&s_claude, "claude"),
            enumerated(&s_codex, "codex"),
            // Non-AgentDesk session: ignored.
            enumerated("zellij-foo", "zsh"),
            // AgentDesk-named but no binding: ignored.
            enumerated(
                &expected_session_name_for(None, &ProviderKind::Codex, "no-binding"),
                "codex",
            ),
        ];

        let report = reconcile_from_enumeration(Some(NODE_A), enumeration, &directory, &registry);
        assert_eq!(report.enumerated, 4);
        assert_eq!(report.matched, 2);
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn reconcile_removes_sessions_that_disappear() {
        let directory = ChannelDirectory::from_bindings(vec![
            binding("c-a", "agent-a", ProviderKind::Claude),
            binding("c-b", "agent-b", ProviderKind::Claude),
        ]);
        let registry = SessionRegistry::new();
        let s_a = expected_session_name_for(None, &ProviderKind::Claude, "c-a");
        let s_b = expected_session_name_for(None, &ProviderKind::Claude, "c-b");

        // Initial sweep: both are alive.
        let _ = reconcile_from_enumeration(
            Some(NODE_A),
            vec![enumerated(&s_a, "claude"), enumerated(&s_b, "claude")],
            &directory,
            &registry,
        );
        assert_eq!(registry.len(), 2);

        // Subsequent sweep: only A still exists. B must be removed.
        let mut rx = registry.subscribe();
        let report = reconcile_from_enumeration(
            Some(NODE_A),
            vec![enumerated(&s_a, "claude")],
            &directory,
            &registry,
        );
        assert_eq!(report.matched, 1);
        assert_eq!(registry.len(), 1);
        // At least one Removed change was emitted.
        let mut saw_removed = false;
        while let Ok(change) = rx.try_recv() {
            if matches!(change, RegistryChange::Removed { .. }) {
                saw_removed = true;
            }
        }
        assert!(saw_removed);
    }

    #[test]
    fn reconcile_ignores_pane_with_wrong_provider() {
        // Binding says Claude, pane is running bash. Matcher rejects with
        // PaneProviderMismatch (definitive — not retryable); registry stays
        // empty.
        let directory =
            ChannelDirectory::from_bindings(vec![binding("c-x", "agent", ProviderKind::Claude)]);
        let registry = SessionRegistry::new();
        let s_x = expected_session_name_for(None, &ProviderKind::Claude, "c-x");

        let report = reconcile_from_enumeration(
            Some(NODE_A),
            vec![enumerated(&s_x, "bash")],
            &directory,
            &registry,
        );
        assert_eq!(report.matched, 0);
        assert!(registry.is_empty());
    }

    #[test]
    fn reconcile_accepts_agentdesk_managed_wrapper_pane() {
        let directory =
            ChannelDirectory::from_bindings(vec![binding("c-y", "agent", ProviderKind::Codex)]);
        let registry = SessionRegistry::new();
        let s_y = expected_session_name_for(None, &ProviderKind::Codex, "c-y");

        let report = reconcile_from_enumeration(
            Some(NODE_A),
            vec![enumerated(&s_y, "agentdesk")],
            &directory,
            &registry,
        );
        assert_eq!(report.matched, 1);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn empty_pane_command_is_retryable_does_not_remove_existing_entry() {
        // Codex review finding #3: a session that is *still in tmux* but whose
        // pane_current_command came back blank for one tick must NOT be
        // removed from the registry — that would tell E3 to tear down a
        // still-live watcher.
        let directory =
            ChannelDirectory::from_bindings(vec![binding("c-blank", "agent", ProviderKind::Codex)]);
        let registry = SessionRegistry::new();
        let s = expected_session_name_for(None, &ProviderKind::Codex, "c-blank");

        // Tick 1: pane is healthy → entry added.
        let r1 = reconcile_from_enumeration(
            Some(NODE_A),
            vec![enumerated(&s, "codex")],
            &directory,
            &registry,
        );
        assert_eq!(r1.matched, 1);
        assert!(registry.lookup(&s).is_some());

        // Tick 2: pane probe came back blank (PaneProviderUnknown). Entry
        // must survive because the session is still present in tmux.
        let r2 = reconcile_from_enumeration(
            Some(NODE_A),
            vec![enumerated(&s, "")],
            &directory,
            &registry,
        );
        assert_eq!(r2.matched, 0);
        assert!(
            r2.changes.is_empty(),
            "retryable miss must not emit registry changes: {:?}",
            r2.changes
        );
        assert!(
            registry.lookup(&s).is_some(),
            "retryable miss must preserve existing entry"
        );
    }

    #[test]
    fn reconcile_does_not_touch_other_nodes_entries() {
        // Codex review finding #2: each node's sweep must scope removal to
        // its own instance_id; peer entries are sacrosanct.
        let directory =
            ChannelDirectory::from_bindings(vec![binding("c-x", "agent", ProviderKind::Claude)]);
        let registry = SessionRegistry::new();
        let s_x = expected_session_name_for(None, &ProviderKind::Claude, "c-x");

        // Pre-populate: NODE_B owns s_x.
        let _ = reconcile_from_enumeration(
            Some(NODE_B),
            vec![enumerated(&s_x, "claude")],
            &directory,
            &registry,
        );
        assert!(registry.lookup(&s_x).is_some());

        // NODE_A sweeps with empty local enumeration — must not touch
        // NODE_B's entry.
        let r = reconcile_from_enumeration(Some(NODE_A), vec![], &directory, &registry);
        assert!(r.changes.is_empty());
        let entry = registry.lookup(&s_x).expect("peer entry survives");
        assert_eq!(entry.instance_id.as_deref(), Some(NODE_B));
    }

    /// Regression for #2465: tmux sessions whose names embed the yaml channel
    /// **name** (e.g. `AgentDesk-claude-adk-cc`) must match a binding whose
    /// `tmux_segment` carries that name, even though the binding's
    /// `channel_id` is a Discord snowflake. Prior to the fix this combination
    /// fell through as `NoChannelBinding` ("operator session?") and silently
    /// stayed orphan across dcserver restarts.
    #[test]
    fn reconcile_matches_yaml_named_session_with_snowflake_id() {
        let snowflake = "1479671298497183835";
        let channel_name = "adk-cc";
        let directory = ChannelDirectory::from_bindings(vec![binding_named(
            snowflake,
            channel_name,
            "project-agentdesk",
            ProviderKind::Claude,
        )]);
        let registry = SessionRegistry::new();
        // Live tmux session string uses the *channel name*, not the snowflake.
        let live_session = expected_session_name_for(None, &ProviderKind::Claude, channel_name);

        let report = reconcile_from_enumeration(
            Some(NODE_A),
            vec![enumerated(&live_session, "claude")],
            &directory,
            &registry,
        );

        assert_eq!(report.matched, 1, "named session must match");
        let entry = registry
            .lookup(&live_session)
            .expect("named session must be present in registry");
        assert_eq!(
            entry.matched.channel_id, snowflake,
            "binding's channel_id (snowflake) must survive routing intact"
        );
        assert_eq!(entry.matched.agent_id, "project-agentdesk");
    }

    /// Snowflake-only binding (no yaml-supplied channel name) must still match
    /// a session whose live name embeds the snowflake — preserves legacy
    /// behavior for agents that aren't declared in `agentdesk.yaml`.
    #[test]
    fn reconcile_matches_snowflake_session_when_tmux_segment_absent() {
        let snowflake = "1234567890";
        let directory = ChannelDirectory::from_bindings(vec![binding(
            snowflake,
            "legacy-agent",
            ProviderKind::Claude,
        )]);
        let registry = SessionRegistry::new();
        let live_session = expected_session_name_for(None, &ProviderKind::Claude, snowflake);

        let report = reconcile_from_enumeration(
            Some(NODE_A),
            vec![enumerated(&live_session, "claude")],
            &directory,
            &registry,
        );

        assert_eq!(report.matched, 1, "snowflake fallback must still match");
    }

    #[test]
    fn session_tmux_segment_fallback_uses_live_recent_session_rows() {
        let query = SESSION_TMUX_SEGMENTS_QUERY;

        assert!(
            query.contains("instance_id"),
            "fallback tmux segments must preserve session row ownership"
        );
        assert!(
            query.contains("LOWER(TRIM(COALESCE(status, ''))) IN"),
            "fallback query must filter status before deriving tmux segments"
        );
        assert!(
            query.contains("'turn_active'") && query.contains("'idle'"),
            "live runtime statuses must remain eligible"
        );
        assert!(
            !query.contains("'disconnected'") && !query.contains("'aborted'"),
            "stale terminal statuses must not be eligible"
        );
        assert!(
            query.contains("last_heartbeat IS NOT NULL")
                && query.contains("last_heartbeat > NOW() - INTERVAL '10 minutes'"),
            "fallback tmux segments must come from current-runtime heartbeat rows"
        );
        assert!(
            query.contains("ORDER BY last_heartbeat DESC NULLS LAST")
                && query.contains("created_at DESC NULLS LAST")
                && query.contains("id DESC"),
            "newest live session rows must be considered before older rows"
        );
    }

    #[test]
    fn session_tmux_segment_fallback_requires_live_tmux_session() {
        let snowflake = "1234567890";
        let stale_named_session =
            expected_session_name_for(None, &ProviderKind::Claude, "old-channel-name");
        let live_snowflake_session =
            expected_session_name_for(None, &ProviderKind::Claude, snowflake);
        let live_named_session =
            expected_session_name_for(None, &ProviderKind::Claude, "current-channel-name");
        let live_session_names = std::collections::HashSet::from([
            live_snowflake_session.clone(),
            live_named_session.clone(),
        ]);

        assert!(
            live_tmux_segment_from_session_key(
                &format!("provider-cli:{stale_named_session}"),
                &live_session_names,
            )
            .is_none(),
            "stale DB session rows must not install a tmux_segment fallback"
        );
        assert_eq!(
            live_tmux_segment_from_session_key(
                &format!("provider-cli:{live_named_session}"),
                &live_session_names,
            ),
            Some((ProviderKind::Claude, "current-channel-name".to_string())),
            "live tmux-verified session rows remain eligible"
        );

        let directory = ChannelDirectory::from_bindings(vec![binding(
            snowflake,
            "legacy-agent",
            ProviderKind::Claude,
        )]);
        let registry = SessionRegistry::new();

        let report = reconcile_from_enumeration(
            Some(NODE_A),
            vec![enumerated(&live_snowflake_session, "claude")],
            &directory,
            &registry,
        );

        assert_eq!(
            report.matched, 1,
            "skipping stale fallback must leave channel_id matching intact"
        );
    }

    #[test]
    fn session_tmux_segment_fallback_requires_local_instance_ownership() {
        assert!(
            session_row_matches_local_instance(Some(NODE_A), Some(NODE_A)),
            "local session rows remain eligible"
        );
        assert!(
            !session_row_matches_local_instance(Some(NODE_B), Some(NODE_A)),
            "foreign session rows must not seed local tmux segment fallbacks"
        );
        assert!(
            !session_row_matches_local_instance(None, Some(NODE_A)),
            "missing row ownership is not enough when this node has an instance id"
        );
        assert!(
            session_row_matches_local_instance(Some(NODE_B), None),
            "single-node/legacy callers without a local instance keep compatibility"
        );
    }

    /// Regression for #2470: claude code 2.1.143 rewrites its process title to
    /// the version string ("2.1.143"), so `pane_current_command` no longer
    /// resolves to `claude`. The matcher must fall back to process args, which
    /// still include the `claude` executable.
    #[test]
    fn reconcile_falls_back_to_process_args_when_pane_command_is_version_string() {
        let channel_name = "adk-cc";
        let directory = ChannelDirectory::from_bindings(vec![binding_named(
            "1479671298497183835",
            channel_name,
            "project-agentdesk",
            ProviderKind::Claude,
        )]);
        let registry = SessionRegistry::new();
        let live_session = expected_session_name_for(None, &ProviderKind::Claude, channel_name);

        // pane_current_command = "2.1.143" (version), pane_pid = 100 → args include ".../claude"
        let report = reconcile_from_enumeration_with_process_args_probe(
            Some(NODE_A),
            vec![enumerated_with_pid(&live_session, "2.1.143", 100)],
            &directory,
            &registry,
            fake_process_args_probe,
        );

        assert_eq!(
            report.matched, 1,
            "process-args fallback must recover provider when pane_current_command is a version string"
        );
        assert!(registry.lookup(&live_session).is_some());
    }

    /// Codex companion panes can expose `node` as the foreground command while
    /// the provider-specific companion path appears later in process args.
    #[test]
    fn reconcile_falls_back_to_process_args_for_node_wrapped_codex() {
        let channel_name = "adk-cdx";
        let directory = ChannelDirectory::from_bindings(vec![binding_named(
            "1479671298497183836",
            channel_name,
            "project-agentdesk",
            ProviderKind::Codex,
        )]);
        let registry = SessionRegistry::new();
        let live_session = expected_session_name_for(None, &ProviderKind::Codex, channel_name);

        let report = reconcile_from_enumeration_with_process_args_probe(
            Some(NODE_A),
            vec![enumerated_with_pid(&live_session, "node", 101)],
            &directory,
            &registry,
            fake_process_args_probe,
        );

        assert_eq!(
            report.matched, 1,
            "process-args fallback must recover Codex behind a generic node wrapper"
        );
        assert!(registry.lookup(&live_session).is_some());
    }

    /// process-args probe failures (process exited, missing PID) must NOT promote the
    /// session — preserve the existing PaneProviderMismatch semantics so a
    /// stale session doesn't get a watcher attached on speculation.
    #[test]
    fn reconcile_does_not_match_when_process_args_probe_fails() {
        let channel_name = "adk-cc";
        let directory = ChannelDirectory::from_bindings(vec![binding_named(
            "1479671298497183835",
            channel_name,
            "project-agentdesk",
            ProviderKind::Claude,
        )]);
        let registry = SessionRegistry::new();
        let live_session = expected_session_name_for(None, &ProviderKind::Claude, channel_name);

        // PID 999 not in fake_process_args_probe table → probe returns None.
        let report = reconcile_from_enumeration_with_process_args_probe(
            Some(NODE_A),
            vec![enumerated_with_pid(&live_session, "2.1.143", 999)],
            &directory,
            &registry,
            fake_process_args_probe,
        );

        assert_eq!(
            report.matched, 0,
            "process-args probe failure must keep PaneProviderMismatch reject"
        );
        assert!(registry.lookup(&live_session).is_none());
    }

    /// When `pane_current_command` already resolves to a provider, we must
    /// NOT spend a `ps` call. Confirms the fast-path skips the probe.
    #[test]
    fn reconcile_skips_process_args_probe_when_pane_command_already_resolves() {
        let channel_name = "adk-cc";
        let directory = ChannelDirectory::from_bindings(vec![binding_named(
            "1479671298497183835",
            channel_name,
            "project-agentdesk",
            ProviderKind::Claude,
        )]);
        let registry = SessionRegistry::new();
        let live_session = expected_session_name_for(None, &ProviderKind::Claude, channel_name);

        // pane_cmd already = "claude" → fast path. probe would return None for
        // PID 999 (forcing a reject) — if the fast path failed to skip it we'd
        // mismatch. matched=1 proves the probe never ran.
        let report = reconcile_from_enumeration_with_process_args_probe(
            Some(NODE_A),
            vec![enumerated_with_pid(&live_session, "claude", 999)],
            &directory,
            &registry,
            fake_process_args_probe,
        );

        assert_eq!(report.matched, 1);
    }

    #[test]
    fn process_args_fingerprint_prefers_provider_token() {
        assert_eq!(
            provider_fingerprint_from_process_args(
                "/usr/bin/claude 2.1.143 --dangerously-skip-permissions"
            )
            .as_deref(),
            Some("/usr/bin/claude")
        );
        assert_eq!(
            provider_fingerprint_from_process_args(
                "/opt/homebrew/bin/node /Users/me/.local/bin/codex-companion.js --session abc"
            )
            .as_deref(),
            Some("/Users/me/.local/bin/codex-companion.js")
        );
        assert_eq!(
            provider_fingerprint_from_process_args("node /tmp/app.js --provider codex"),
            None,
            "value-like provider args must not be treated as provider executables"
        );
    }

    #[test]
    fn request_discovery_tick_wakes_notifier() {
        // Smoke test: the global Notify pre-buffers a permit so a notified()
        // future called *after* request_discovery_tick() resolves immediately.
        let notifier = discovery_notifier();
        request_discovery_tick();
        // notified() is a future — drive it on a one-off runtime.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            tokio::time::timeout(Duration::from_millis(50), notifier.notified())
                .await
                .expect("notification should be delivered");
        });
    }
}
