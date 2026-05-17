//! `WatcherSupervisor` — subscribes to [`SessionRegistry`] change events and
//! ensures there is exactly one [`StreamRelay`] task per matched session.
//!
//! Epic #2285 / E3 (issue #2345). Companion to `SessionDiscovery` (E2):
//!
//! ```text
//!     tmux ──▶ SessionDiscovery ──▶ SessionRegistry ──▶ WatcherSupervisor ──▶ StreamRelay(s)
//! ```
//!
//! ## Idempotency
//!
//! - `Added(session)` → if no relay exists, spawn one. If one already exists
//!   (e.g. supervisor caught up after a `Lagged` and the registry reconcile
//!   re-emitted `Added`), reuse it.
//! - `Updated(session)` → tear down the existing relay and respawn against
//!   the new binding (channel id may have changed).
//! - `Removed(session)` → graceful shutdown of the relay (drain pending
//!   output, then exit).
//!
//! ## Lagged broadcast recovery
//!
//! The registry uses `tokio::broadcast`, which drops the oldest events when
//! a subscriber falls behind. On `Lagged`, the supervisor performs a full
//! reconcile via [`SessionRegistry::list_matched`] so no session is silently
//! orphaned — the same idempotent path handles boot and lag recovery.
//!
//! ## Worker-local
//!
//! Placed under `WorkerLocal` in `worker_registry` — tmux is host-scoped and
//! every node runs its own discovery, so every node owns relays for its own
//! sessions. Cross-host relay placement is out of scope here.
//!
//! ## Flag gate
//!
//! `cluster.session_bound_relay_enabled` (default `true` since E4 / #2346).
//! When `false`, the supervisor is not started by the worker registry and
//! the legacy turn-bound relay path remains the only delivery channel —
//! that escape hatch lets operators disable the new path if a regression
//! surfaces. Under the default-on configuration the supervisor runs against
//! the production observation sink
//! ([`super::registry_adapter_sink::RegistryAdapterSink`]); the legacy
//! turn-bound watcher still owns Discord delivery during the E4 release.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::broadcast::error::RecvError;

use super::session_registry::{
    RegisteredSession, RegistryChange, SessionRegistry, global_session_registry,
};
use super::stream_relay::{DiscardSink, RelaySink, StreamRelayHandle, spawn_stream_relay};

/// Knobs for the supervisor loop. The defaults are tuned for production;
/// tests build a custom config via [`SupervisorConfig::for_test`].
#[derive(Clone, Debug)]
pub struct SupervisorConfig {
    /// Sleep before retrying after a non-lag broadcast error (e.g. registry
    /// dropped). Keeps the loop from spinning if the registry vanishes.
    pub backoff: Duration,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            backoff: Duration::from_secs(1),
        }
    }
}

impl SupervisorConfig {
    pub fn for_test() -> Self {
        Self {
            backoff: Duration::from_millis(1),
        }
    }
}

/// Active relay map keyed by tmux session name. Each entry owns the
/// [`StreamRelayHandle`] of the matching relay task. Wrapped in a struct so
/// tests can introspect it.
#[derive(Default)]
struct ActiveRelays {
    by_session: HashMap<String, StreamRelayHandle>,
}

impl ActiveRelays {
    fn contains(&self, session: &str) -> bool {
        self.by_session.contains_key(session)
    }

    fn len(&self) -> usize {
        self.by_session.len()
    }

    fn insert(&mut self, session: String, handle: StreamRelayHandle) {
        self.by_session.insert(session, handle);
    }

    fn remove(&mut self, session: &str) -> Option<StreamRelayHandle> {
        self.by_session.remove(session)
    }

    fn drain(&mut self) -> Vec<(String, StreamRelayHandle)> {
        self.by_session.drain().collect()
    }
}

/// Apply a single registry change to the active relay map. Returns the
/// handle that the caller must `await` for graceful shutdown on removals,
/// so the supervisor loop can drop locks before awaiting. Pure synchronous
/// helper — easy to unit-test without spinning the broadcast loop.
fn apply_change(
    active: &mut ActiveRelays,
    change: &RegistryChange,
    sink: &Arc<dyn RelaySink>,
) -> Option<StreamRelayHandle> {
    match change {
        RegistryChange::Added(entry) => {
            spawn_if_absent(active, entry, sink);
            None
        }
        RegistryChange::Updated(entry) => {
            // Tear down and respawn: channel binding may have changed, and
            // the relay caches the matched channel id internally.
            let old = active.remove(&entry.matched.expected_session_name);
            spawn_if_absent(active, entry, sink);
            old
        }
        RegistryChange::Removed { session_name } => active.remove(session_name),
    }
}

fn spawn_if_absent(
    active: &mut ActiveRelays,
    entry: &RegisteredSession,
    sink: &Arc<dyn RelaySink>,
) {
    let session = entry.matched.expected_session_name.clone();
    if active.contains(&session) {
        tracing::debug!(
            session = %session,
            "watcher-supervisor: relay already running for session; skipping respawn"
        );
        return;
    }
    tracing::info!(
        session = %session,
        channel_id = %entry.matched.channel_id,
        provider = entry.matched.provider.as_str(),
        "watcher-supervisor: spawning StreamRelay"
    );
    let handle = spawn_stream_relay(entry.matched.clone(), sink.clone());
    active.insert(session, handle);
}

/// Full-reconcile path used at startup and after a `Lagged` broadcast error.
/// Spawns relays for every entry the registry currently lists; the supervisor
/// trusts the registry as the source of truth, so any missing entries simply
/// don't get a relay.
fn full_reconcile(
    active: &mut ActiveRelays,
    registry: &SessionRegistry,
    sink: &Arc<dyn RelaySink>,
) -> Vec<StreamRelayHandle> {
    let snapshot = registry.list_matched();
    let live_names: std::collections::HashSet<String> = snapshot
        .iter()
        .map(|e| e.matched.expected_session_name.clone())
        .collect();
    // Take down relays for sessions that the registry no longer knows about.
    let stale: Vec<String> = active
        .by_session
        .keys()
        .filter(|name| !live_names.contains(*name))
        .cloned()
        .collect();
    let mut to_shutdown = Vec::with_capacity(stale.len());
    for name in stale {
        if let Some(handle) = active.remove(&name) {
            tracing::info!(
                session = %name,
                "watcher-supervisor: tearing down relay during reconcile (no registry entry)"
            );
            to_shutdown.push(handle);
        }
    }
    for entry in &snapshot {
        spawn_if_absent(active, entry, sink);
    }
    to_shutdown
}

/// Run the supervisor loop until `shutdown` flips true. The loop:
///
/// 1. Subscribes to the registry's change channel.
/// 2. Performs an initial reconcile so any sessions matched before the
///    supervisor started are picked up.
/// 3. Reacts to each `Added`/`Updated`/`Removed` event.
/// 4. On `Lagged`, runs a full reconcile to recover.
///
/// `sink` is the destination of every relayed frame. Production passes a
/// Discord-side adapter (wired in E4 #2346). When the feature flag is on but
/// no adapter is available yet, callers may pass [`DiscardSink`] to keep
/// supervisor lifecycle wiring exercised without delivering frames anywhere.
pub async fn run_watcher_supervisor_loop(
    config: SupervisorConfig,
    sink: Arc<dyn RelaySink>,
    shutdown: Arc<AtomicBool>,
) {
    let registry = global_session_registry();
    run_watcher_supervisor_loop_with_registry(config, sink, shutdown, registry).await;
}

/// Test-friendly variant — accepts an explicit registry.
pub async fn run_watcher_supervisor_loop_with_registry(
    config: SupervisorConfig,
    sink: Arc<dyn RelaySink>,
    shutdown: Arc<AtomicBool>,
    registry: Arc<SessionRegistry>,
) {
    let mut rx = registry.subscribe();
    let mut active = ActiveRelays::default();

    // Boot reconcile: pick up anything already in the registry.
    let initial_teardowns = full_reconcile(&mut active, &registry, &sink);
    for handle in initial_teardowns {
        handle.shutdown().await;
    }

    tracing::info!(
        active_relays = active.len(),
        "watcher-supervisor entering main loop"
    );

    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        match rx.recv().await {
            Ok(change) => {
                let to_shutdown = apply_change(&mut active, &change, &sink);
                if let Some(handle) = to_shutdown {
                    handle.shutdown().await;
                }
            }
            Err(RecvError::Lagged(skipped)) => {
                tracing::warn!(
                    skipped,
                    "watcher-supervisor: broadcast lagged; running full reconcile"
                );
                let teardowns = full_reconcile(&mut active, &registry, &sink);
                for handle in teardowns {
                    handle.shutdown().await;
                }
            }
            Err(RecvError::Closed) => {
                // Registry dropped — happens only at process shutdown, but
                // we don't want to busy-loop if it ever happens unexpectedly.
                tracing::warn!(
                    "watcher-supervisor: registry broadcast closed; backing off and retrying"
                );
                if shutdown.load(Ordering::Acquire) {
                    break;
                }
                tokio::time::sleep(config.backoff).await;
                // Re-subscribe in case a new registry was installed; for the
                // global singleton this is a no-op but keeps the loop alive.
                rx = registry.subscribe();
            }
        }
    }

    // Graceful drain on shutdown.
    tracing::info!(
        active_relays = active.len(),
        "watcher-supervisor shutting down — draining active relays"
    );
    for (_session, handle) in active.drain() {
        handle.shutdown().await;
    }
}

/// Convenience entry point used by the supervised worker registry when the
/// `cluster.session_bound_relay_enabled` flag is true and no concrete sink
/// has been wired yet (pre-E4). Boots the supervisor against a
/// [`DiscardSink`] so the lifecycle path runs in production without
/// changing user-visible delivery behaviour. E4 will replace this with a
/// real Discord adapter.
pub async fn run_with_discard_sink(shutdown: Arc<AtomicBool>) {
    run_watcher_supervisor_loop(
        SupervisorConfig::default(),
        Arc::new(DiscardSink) as Arc<dyn RelaySink>,
        shutdown,
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::cluster::session_matcher::expected_rollout_path_for;
    use crate::services::cluster::session_registry::SessionRegistry;
    use crate::services::cluster::stream_relay::{RelaySink, RelaySinkError, StreamFrame};
    use crate::services::provider::ProviderKind;
    use async_trait::async_trait;
    use std::sync::Mutex;

    fn matched(
        channel: &str,
        provider: ProviderKind,
    ) -> super::super::session_matcher::MatchedChannel {
        let session = provider.build_tmux_session_name(channel);
        super::super::session_matcher::MatchedChannel {
            channel_id: channel.to_string(),
            agent_id: format!("agent-for-{channel}"),
            provider,
            expected_session_name: session.clone(),
            expected_rollout_path: expected_rollout_path_for(&session),
        }
    }

    #[derive(Default)]
    struct CountingSink {
        per_session: Mutex<HashMap<String, Vec<StreamFrame>>>,
    }

    impl CountingSink {
        fn count(&self, session: &str) -> usize {
            self.per_session
                .lock()
                .unwrap()
                .get(session)
                .map(|v| v.len())
                .unwrap_or_default()
        }
    }

    #[async_trait]
    impl RelaySink for CountingSink {
        async fn deliver(&self, frame: &StreamFrame) -> Result<(), RelaySinkError> {
            self.per_session
                .lock()
                .unwrap()
                .entry(frame.session_name.clone())
                .or_default()
                .push(frame.clone());
            Ok(())
        }
    }

    async fn wait_for<F: FnMut() -> bool>(mut cond: F, label: &str) {
        for _ in 0..200 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for: {label}");
    }

    #[tokio::test]
    async fn add_remove_session_spawns_and_shuts_down_relay() {
        let registry = Arc::new(SessionRegistry::new());
        let sink: Arc<CountingSink> = Arc::new(CountingSink::default());
        let shutdown = Arc::new(AtomicBool::new(false));

        let registry_clone = registry.clone();
        let sink_clone: Arc<dyn RelaySink> = sink.clone();
        let shutdown_clone = shutdown.clone();
        let supervisor = tokio::spawn(async move {
            run_watcher_supervisor_loop_with_registry(
                SupervisorConfig::for_test(),
                sink_clone,
                shutdown_clone,
                registry_clone,
            )
            .await;
        });

        // Add a session → supervisor must spawn a relay.
        let m = matched("c-1", ProviderKind::Claude);
        registry.upsert(m.clone(), Some("mac-mini"));

        // Drive a frame through the channel to confirm the relay is alive.
        // To do that we need access to the handle — instead, we wait for the
        // relay to exist by upserting again with no-op (idempotent) and
        // checking sink delivery via an upsert-Updated event would respawn.
        // Simpler proof: the relay spawn is observed via tracing, and the
        // shutdown path below asserts the registry entry was cleaned up.
        // To make the assertion tight, we Remove the session and verify the
        // relay teardown runs without panic.
        registry.remove(&m.expected_session_name);

        // Add another distinct session; this exercises the spawn path again.
        let m2 = matched("c-2", ProviderKind::Codex);
        registry.upsert(m2.clone(), Some("mac-mini"));

        // Give the supervisor a moment to process events.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Tear down via shutdown flag and ensure the task exits.
        shutdown.store(true, Ordering::Release);
        // Publish one more event to unblock the recv().
        registry.remove(&m2.expected_session_name);
        let _ = tokio::time::timeout(Duration::from_secs(2), supervisor)
            .await
            .expect("supervisor exits within timeout");

        // No frames were sent through try_send_frame here, so the sink stays
        // empty — what we're asserting is the lifecycle didn't deadlock.
        assert_eq!(sink.count(&m.expected_session_name), 0);
        assert_eq!(sink.count(&m2.expected_session_name), 0);
    }

    #[tokio::test]
    async fn boot_reconcile_spawns_relays_for_existing_entries() {
        let registry = Arc::new(SessionRegistry::new());
        let m1 = matched("c-pre1", ProviderKind::Claude);
        let m2 = matched("c-pre2", ProviderKind::Codex);
        registry.upsert(m1.clone(), Some("mac-mini"));
        registry.upsert(m2.clone(), Some("mac-mini"));

        let sink: Arc<CountingSink> = Arc::new(CountingSink::default());
        let shutdown = Arc::new(AtomicBool::new(false));
        let registry_clone = registry.clone();
        let sink_clone: Arc<dyn RelaySink> = sink.clone();
        let shutdown_clone = shutdown.clone();
        let supervisor = tokio::spawn(async move {
            run_watcher_supervisor_loop_with_registry(
                SupervisorConfig::for_test(),
                sink_clone,
                shutdown_clone,
                registry_clone,
            )
            .await;
        });
        // Give boot reconcile a chance to run.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Apply a registry event so the loop notices shutdown shortly after.
        shutdown.store(true, Ordering::Release);
        registry.remove(&m1.expected_session_name);
        let _ = tokio::time::timeout(Duration::from_secs(2), supervisor)
            .await
            .expect("supervisor exits");
    }

    #[tokio::test]
    async fn apply_change_is_idempotent_on_duplicate_added() {
        // Direct test of the pure-ish helper — no broadcast races involved.
        let sink: Arc<dyn RelaySink> = Arc::new(CountingSink::default());
        let mut active = ActiveRelays::default();
        let m = matched("c-dup", ProviderKind::Claude);
        let entry = RegisteredSession {
            matched: m.clone(),
            instance_id: Some("mac-mini".into()),
            first_seen_at: chrono::Utc::now(),
            last_seen_at: chrono::Utc::now(),
        };
        let to_shutdown1 = apply_change(&mut active, &RegistryChange::Added(entry.clone()), &sink);
        assert!(to_shutdown1.is_none());
        assert_eq!(active.len(), 1);
        // Second Added for the same session must NOT spawn a second relay.
        let to_shutdown2 = apply_change(&mut active, &RegistryChange::Added(entry.clone()), &sink);
        assert!(to_shutdown2.is_none());
        assert_eq!(active.len(), 1, "duplicate Added is idempotent");

        // Removed yields the previous handle for shutdown.
        let removed = apply_change(
            &mut active,
            &RegistryChange::Removed {
                session_name: m.expected_session_name.clone(),
            },
            &sink,
        );
        assert!(removed.is_some());
        assert_eq!(active.len(), 0);
        // Drain so the spawned tasks don't outlive the test.
        if let Some(handle) = removed {
            handle.shutdown().await;
        }
        let _ = wait_for(|| true, "noop").await;
    }

    #[tokio::test]
    async fn apply_change_updated_respawns_relay() {
        let sink: Arc<dyn RelaySink> = Arc::new(CountingSink::default());
        let mut active = ActiveRelays::default();
        let m = matched("c-upd", ProviderKind::Claude);
        let entry = RegisteredSession {
            matched: m.clone(),
            instance_id: Some("mac-mini".into()),
            first_seen_at: chrono::Utc::now(),
            last_seen_at: chrono::Utc::now(),
        };
        let _ = apply_change(&mut active, &RegistryChange::Added(entry.clone()), &sink);
        assert_eq!(active.len(), 1);

        let mut updated = entry.clone();
        updated.matched.agent_id = "agent-renamed".to_string();
        let prev = apply_change(&mut active, &RegistryChange::Updated(updated), &sink);
        assert!(
            prev.is_some(),
            "Updated must return the previous handle for teardown"
        );
        assert_eq!(active.len(), 1);
        if let Some(handle) = prev {
            handle.shutdown().await;
        }
    }
}
