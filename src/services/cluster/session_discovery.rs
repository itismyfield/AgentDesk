//! `SessionDiscovery` — leader-only worker that periodically enumerates live
//! tmux sessions, runs them through [`super::session_matcher::match_session`],
//! and reconciles the result into the process-wide [`SessionRegistry`].
//!
//! Epic #2285 / E2 (issue #2344). Sits between the pure matcher (E1) and the
//! future `WatcherSupervisor` (E3) which will react to registry change
//! broadcasts. This module deliberately does **not** spawn or stop any
//! watchers — its only job is to keep the registry honest.
//!
//! ## Why a single leader
//!
//! tmux is host-scoped, so every node in the cluster sees only the sessions
//! on its own machine. But the registry / watcher mapping is *cluster-wide*:
//! two nodes simultaneously discovering the same session would race to spawn
//! duplicate watchers. We avoid that by running discovery on the cluster
//! leader only — exactly how the existing `worker_registry` leader-only
//! workers (`policy-tick`, `routine-runtime`, …) handle the same problem.
//! Per-node tmux observability is recovered via `tmux_reaper` (which already
//! runs cluster-wide) and via the future E3 supervisor pinging each node.
//!
//! ## Boot reconcile
//!
//! The first poll cycle runs **immediately** after leader acquisition (the
//! supervisor's outer loop is the one that waits for `is_leader()`). This
//! re-attaches the registry to any session that survived a dcserver restart
//! within a single poll cycle — Acceptance criterion B in the epic.
//!
//! ## Polling cadence
//!
//! Default 10s; configurable per-test via [`DiscoveryConfig::poll_interval`].
//! Event-driven hooks (Discord-message-on-managed-channel-without-watcher)
//! land in E3 alongside the supervisor — discovery exposes
//! [`request_discovery_tick`] for that purpose so future PRs can nudge the
//! loop without changing this module.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sqlx::PgPool;
use tokio::sync::Notify;

use super::session_matcher::{
    ChannelBinding, ChannelDirectory, MatchOutcome, MatchRejection, MatchedChannel,
    match_session_detailed,
};
use super::session_registry::{RegistryChange, SessionRegistry, global_session_registry};
use crate::services::platform::tmux::{EnumeratedSession, list_sessions_with_pane_command};
use crate::services::provider::ProviderKind;

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
pub fn request_discovery_tick() {
    discovery_notifier().notify_one();
}

/// Build a [`ChannelDirectory`] from the live agents table. Discards bindings
/// that don't yield a usable `(provider, channel_id)` pair. Logs at WARN if
/// the directory builder rejects a binding collision so operators can fix it.
pub async fn build_channel_directory_from_pg(pool: &PgPool) -> ChannelDirectory {
    let all = match crate::db::agents::load_all_agent_channel_bindings_pg(pool).await {
        Ok(map) => map,
        Err(error) => {
            tracing::warn!(?error, "session-discovery: failed to load agent bindings");
            return ChannelDirectory::new();
        }
    };

    let mut directory = ChannelDirectory::new();
    for (agent_id, bindings) in all {
        // For every (provider, channel_id) pair this agent owns, register a
        // ChannelBinding. The matcher's directory is keyed by the *expected
        // tmux session name*, so duplicate provider entries that map to the
        // same channel collapse naturally.
        for (provider, channel_id) in channel_pairs_for_agent(&bindings) {
            let binding = ChannelBinding {
                channel_id,
                agent_id: agent_id.clone(),
                provider,
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
    directory
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
pub fn reconcile_from_enumeration(
    enumeration: Vec<EnumeratedSession>,
    directory: &ChannelDirectory,
    registry: &SessionRegistry,
) -> TickReport {
    let enumerated = enumeration.len();
    let mut matches: Vec<MatchedChannel> = Vec::new();
    for session in enumeration {
        let outcome = match_session_detailed(
            &session.session_name,
            Some(&session.pane_current_command),
            directory,
        );
        match outcome {
            MatchOutcome::Matched(matched) => matches.push(matched),
            MatchOutcome::Rejected(reason) => {
                trace_rejection(&session, &reason);
            }
        }
    }
    let matched = matches.len();
    let changes = registry.reconcile(matches);
    if !changes.is_empty() {
        tracing::info!(
            change_count = changes.len(),
            enumerated,
            matched,
            "session-discovery tick produced registry changes"
        );
    }
    TickReport {
        enumerated,
        matched,
        changes,
    }
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

/// The leader-only discovery loop. Used by the worker_registry supervisor;
/// returns when `shutdown` flips true.
pub async fn run_discovery_loop(
    pool: Arc<PgPool>,
    config: DiscoveryConfig,
    shutdown: Arc<AtomicBool>,
) {
    let registry = global_session_registry();
    let notifier = discovery_notifier();
    tracing::info!(
        poll_interval_ms = config.poll_interval.as_millis() as u64,
        "session-discovery loop entering"
    );

    // Boot reconcile: run once immediately so survived sessions re-attach
    // within one poll cycle (epic acceptance criterion).
    if !shutdown.load(Ordering::Acquire) {
        run_single_tick(pool.as_ref(), &registry).await;
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
        run_single_tick(pool.as_ref(), &registry).await;
    }
    tracing::info!("session-discovery loop exiting");
}

async fn run_single_tick(pool: &PgPool, registry: &SessionRegistry) -> TickReport {
    let directory = build_channel_directory_from_pg(pool).await;
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
    reconcile_from_enumeration(enumeration, &directory, registry)
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
        }
    }

    fn enumerated(session: &str, pane: &str) -> EnumeratedSession {
        EnumeratedSession {
            session_name: session.to_string(),
            pane_current_command: pane.to_string(),
        }
    }

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

        let report = reconcile_from_enumeration(enumeration, &directory, &registry);
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
            vec![enumerated(&s_a, "claude"), enumerated(&s_b, "claude")],
            &directory,
            &registry,
        );
        assert_eq!(registry.len(), 2);

        // Subsequent sweep: only A still exists. B must be removed.
        let mut rx = registry.subscribe();
        let report =
            reconcile_from_enumeration(vec![enumerated(&s_a, "claude")], &directory, &registry);
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
        // PaneProviderMismatch; registry stays empty.
        let directory =
            ChannelDirectory::from_bindings(vec![binding("c-x", "agent", ProviderKind::Claude)]);
        let registry = SessionRegistry::new();
        let s_x = expected_session_name_for(None, &ProviderKind::Claude, "c-x");

        let report =
            reconcile_from_enumeration(vec![enumerated(&s_x, "bash")], &directory, &registry);
        assert_eq!(report.matched, 0);
        assert!(registry.is_empty());
    }

    #[test]
    fn reconcile_accepts_agentdesk_managed_wrapper_pane() {
        // AgentDesk-launched sessions report `agentdesk` as pane command;
        // matcher trusts the session name for these.
        let directory =
            ChannelDirectory::from_bindings(vec![binding("c-y", "agent", ProviderKind::Codex)]);
        let registry = SessionRegistry::new();
        let s_y = expected_session_name_for(None, &ProviderKind::Codex, "c-y");

        let report =
            reconcile_from_enumeration(vec![enumerated(&s_y, "agentdesk")], &directory, &registry);
        assert_eq!(report.matched, 1);
        assert_eq!(registry.len(), 1);
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
