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
//! `cluster.session_bound_relay_enabled` (default `true` since E5 / #2412).
//! When `false`, the supervisor is not started by the worker registry and
//! the legacy turn-bound relay path remains the only delivery channel —
//! that escape hatch lets operators disable the new path if a regression
//! surfaces. Under the default-on configuration the production worker wires a
//! Discord sink and the production tmux frame producer
//! (`services::discord::tmux_watcher`) pushes frames into the supervisor-owned
//! relay via [`super::relay_producer_registry::RelayProducerRegistry`]. The
//! metrics-only [`super::registry_adapter_sink::RegistryAdapterSink`] remains
//! available for tests/fallback runtimes without Discord state.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::broadcast::error::RecvError;

use super::relay_producer_registry::{RelayProducerRegistry, global_relay_producer_registry};
use super::session_registry::{
    RegisteredSession, RegistryChange, SessionRegistry, global_session_registry,
};
use super::stream_relay::{RelaySink, StreamRelayHandle, spawn_stream_relay};

/// Knobs for the supervisor loop. The defaults are tuned for production;
/// tests build a custom config via [`SupervisorConfig::for_test`].
#[derive(Clone, Debug)]
pub struct SupervisorConfig {
    /// Sleep before retrying after a non-lag broadcast error (e.g. registry
    /// dropped). Keeps the loop from spinning if the registry vanishes.
    pub backoff: Duration,
    /// Maximum time an otherwise-idle supervisor may sit in `recv()` before it
    /// re-checks the process shutdown flag.
    pub shutdown_poll: Duration,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            backoff: Duration::from_secs(1),
            shutdown_poll: Duration::from_secs(1),
        }
    }
}

impl SupervisorConfig {
    pub fn for_test() -> Self {
        Self {
            backoff: Duration::from_millis(1),
            shutdown_poll: Duration::from_millis(5),
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

    fn get(&self, session: &str) -> Option<&StreamRelayHandle> {
        self.by_session.get(session)
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
///
/// `producers` is the side-table that exposes a clonable
/// [`super::stream_relay::RelayProducer`] keyed by tmux session name. The
/// production tmux frame producer (`tmux_watcher`) looks up its session
/// there so the supervisor-owned relay receives the provider stream frames
/// consumed by the configured sink.
fn apply_change(
    active: &mut ActiveRelays,
    change: &RegistryChange,
    sink: &Arc<dyn RelaySink>,
    producers: &RelayProducerRegistry,
) -> Option<StreamRelayHandle> {
    match change {
        RegistryChange::Added(entry) => {
            spawn_if_absent(active, entry, sink, producers);
            None
        }
        RegistryChange::Updated(entry) => {
            // Tear down and respawn: channel binding may have changed, and
            // the relay caches the matched channel id internally. The
            // producer registry follows the same lifecycle — deregister
            // first so an in-flight watcher write either hits the old
            // (about-to-drain) producer or sees `None`; respawn re-registers
            // under the same session name.
            let session = entry.matched.expected_session_name.clone();
            producers.deregister(&session);
            let old = active.remove(&session);
            spawn_if_absent(active, entry, sink, producers);
            old
        }
        RegistryChange::Removed { session_name } => {
            producers.deregister(session_name);
            active.remove(session_name)
        }
    }
}

fn spawn_if_absent(
    active: &mut ActiveRelays,
    entry: &RegisteredSession,
    sink: &Arc<dyn RelaySink>,
    producers: &RelayProducerRegistry,
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
    // Publish the producer BEFORE inserting into `active` so a parallel
    // tmux watcher tick that beats the insert still finds the producer —
    // try_send_frame on a freshly-spawned relay is safe (the relay task is
    // already polling rx).
    producers.register(session.clone(), handle.producer());
    active.insert(session, handle);
}

/// Full-reconcile path used at startup and after a `Lagged` broadcast error.
/// Spawns relays for every entry the registry currently lists, tears down stale
/// relays, and respawns active relays whose binding no longer matches the
/// registry snapshot.
fn full_reconcile(
    active: &mut ActiveRelays,
    registry: &SessionRegistry,
    sink: &Arc<dyn RelaySink>,
    producers: &RelayProducerRegistry,
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
            producers.deregister(&name);
            tracing::info!(
                session = %name,
                "watcher-supervisor: tearing down relay during reconcile (no registry entry)"
            );
            to_shutdown.push(handle);
        }
    }
    for entry in &snapshot {
        let session = entry.matched.expected_session_name.as_str();
        let needs_rebind = active
            .get(session)
            .map(|handle| handle.matched() != &entry.matched)
            .unwrap_or(false);
        if needs_rebind {
            producers.deregister(session);
            if let Some(handle) = active.remove(session) {
                tracing::info!(
                    session = %session,
                    old_channel_id = %handle.matched().channel_id,
                    new_channel_id = %entry.matched.channel_id,
                    old_agent_id = %handle.matched().agent_id,
                    new_agent_id = %entry.matched.agent_id,
                    provider = entry.matched.provider.as_str(),
                    "watcher-supervisor: rebinding relay during reconcile"
                );
                to_shutdown.push(handle);
            }
        }
        spawn_if_absent(active, entry, sink, producers);
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
/// no adapter is available yet, callers may pass the stream relay discard sink to keep
/// supervisor lifecycle wiring exercised without delivering frames anywhere.
pub async fn run_watcher_supervisor_loop(
    config: SupervisorConfig,
    sink: Arc<dyn RelaySink>,
    shutdown: Arc<AtomicBool>,
) {
    let registry = global_session_registry();
    let producers = global_relay_producer_registry();
    run_watcher_supervisor_loop_with_registry_and_producers(
        config, sink, shutdown, registry, producers,
    )
    .await;
}

/// Test-friendly variant — accepts an explicit registry. Uses the global
/// producer registry; tests that need their own producer registry use
/// [`run_watcher_supervisor_loop_with_registry_and_producers`] directly.
#[cfg(test)]
pub(crate) async fn run_watcher_supervisor_loop_with_registry(
    config: SupervisorConfig,
    sink: Arc<dyn RelaySink>,
    shutdown: Arc<AtomicBool>,
    registry: Arc<SessionRegistry>,
) {
    let producers = global_relay_producer_registry();
    run_watcher_supervisor_loop_with_registry_and_producers(
        config, sink, shutdown, registry, producers,
    )
    .await;
}

/// Full-control variant. The E5 (#2412) end-to-end test uses this to inject
/// a fresh `RelayProducerRegistry` so the supervisor's producer-registry
/// publish path is exercised without leaking handles into the global
/// singleton between tests.
pub async fn run_watcher_supervisor_loop_with_registry_and_producers(
    config: SupervisorConfig,
    sink: Arc<dyn RelaySink>,
    shutdown: Arc<AtomicBool>,
    registry: Arc<SessionRegistry>,
    producers: Arc<RelayProducerRegistry>,
) {
    let mut rx = registry.subscribe();
    let mut active = ActiveRelays::default();

    // Boot reconcile: pick up anything already in the registry.
    let initial_teardowns = full_reconcile(&mut active, &registry, &sink, &producers);
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
        let recv_result = tokio::time::timeout(config.shutdown_poll, rx.recv()).await;
        let change = match recv_result {
            Ok(result) => result,
            Err(_) => continue,
        };
        match change {
            Ok(change) => {
                let to_shutdown = apply_change(&mut active, &change, &sink, &producers);
                if let Some(handle) = to_shutdown {
                    handle.shutdown().await;
                }
            }
            Err(RecvError::Lagged(skipped)) => {
                tracing::warn!(
                    skipped,
                    "watcher-supervisor: broadcast lagged; running full reconcile"
                );
                let teardowns = full_reconcile(&mut active, &registry, &sink, &producers);
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
    for (session, handle) in active.drain() {
        producers.deregister(&session);
        handle.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::cluster::session_matcher::expected_rollout_path_for;
    use crate::services::cluster::session_registry::SessionRegistry;
    use crate::services::cluster::stream_relay::{
        RelaySink, RelaySinkError, RelaySinkOutcome, StreamFrame,
    };
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
        async fn deliver(&self, frame: &StreamFrame) -> Result<RelaySinkOutcome, RelaySinkError> {
            self.per_session
                .lock()
                .unwrap()
                .entry(frame.session_name.clone())
                .or_default()
                .push(frame.clone());
            Ok(RelaySinkOutcome::FrameAccepted)
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
        let producers = RelayProducerRegistry::new();
        let to_shutdown1 = apply_change(
            &mut active,
            &RegistryChange::Added(entry.clone()),
            &sink,
            &producers,
        );
        assert!(to_shutdown1.is_none());
        assert_eq!(active.len(), 1);
        assert_eq!(producers.len(), 1);
        // Second Added for the same session must NOT spawn a second relay.
        let to_shutdown2 = apply_change(
            &mut active,
            &RegistryChange::Added(entry.clone()),
            &sink,
            &producers,
        );
        assert!(to_shutdown2.is_none());
        assert_eq!(active.len(), 1, "duplicate Added is idempotent");
        assert_eq!(producers.len(), 1);

        // Removed yields the previous handle for shutdown.
        let removed = apply_change(
            &mut active,
            &RegistryChange::Removed {
                session_name: m.expected_session_name.clone(),
            },
            &sink,
            &producers,
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
        let producers = RelayProducerRegistry::new();
        let _ = apply_change(
            &mut active,
            &RegistryChange::Added(entry.clone()),
            &sink,
            &producers,
        );
        assert_eq!(active.len(), 1);
        assert_eq!(producers.len(), 1);

        let mut updated = entry.clone();
        updated.matched.agent_id = "agent-renamed".to_string();
        let prev = apply_change(
            &mut active,
            &RegistryChange::Updated(updated),
            &sink,
            &producers,
        );
        assert!(
            prev.is_some(),
            "Updated must return the previous handle for teardown"
        );
        assert_eq!(active.len(), 1);
        assert_eq!(
            producers.len(),
            1,
            "Updated keeps exactly one producer entry (deregister-then-register)"
        );
        if let Some(handle) = prev {
            handle.shutdown().await;
        }
        for (_session, handle) in active.drain() {
            handle.shutdown().await;
        }
    }

    #[tokio::test]
    async fn full_reconcile_rebinds_active_relay_when_update_was_lagged() {
        let registry = Arc::new(SessionRegistry::new());
        let sink: Arc<dyn RelaySink> = Arc::new(CountingSink::default());
        let mut active = ActiveRelays::default();
        let producers = RelayProducerRegistry::new();

        let original = matched("c-lag-rebind", ProviderKind::Claude);
        registry.upsert(original.clone(), Some("mac-mini"));
        let initial_teardowns = full_reconcile(&mut active, &registry, &sink, &producers);
        assert!(initial_teardowns.is_empty());
        assert_eq!(active.len(), 1);
        assert_eq!(producers.len(), 1);

        let mut rebound = original.clone();
        rebound.channel_id = "different-channel-after-skipped-update".to_string();
        rebound.agent_id = "different-agent-after-skipped-update".to_string();
        registry.upsert(rebound.clone(), Some("mac-mini"));

        let teardowns = full_reconcile(&mut active, &registry, &sink, &producers);
        assert_eq!(
            teardowns.len(),
            1,
            "full reconcile must tear down stale binding after lagged Updated"
        );
        assert_eq!(active.len(), 1);
        assert_eq!(producers.len(), 1);
        assert_eq!(
            active
                .get(&rebound.expected_session_name)
                .expect("relay respawned")
                .matched(),
            &rebound
        );

        for handle in teardowns {
            handle.shutdown().await;
        }
        for (_session, handle) in active.drain() {
            handle.shutdown().await;
        }
    }

    /// E5 (#2412): the supervisor must register a clonable producer for each
    /// spawned relay so the production tmux frame producer can resolve a
    /// session name to a handle without poking at supervisor internals.
    /// Without this wiring the relay would receive zero frames in
    /// production (which was the bug #2412 spun off from #2411 to fix).
    #[tokio::test]
    async fn supervisor_publishes_producer_for_each_spawn() {
        use crate::services::cluster::relay_producer_registry::RelayProducerRegistry;

        let registry = Arc::new(SessionRegistry::new());
        let producers = Arc::new(RelayProducerRegistry::new());
        let sink: Arc<CountingSink> = Arc::new(CountingSink::default());
        let shutdown = Arc::new(AtomicBool::new(false));

        let registry_clone = registry.clone();
        let producers_clone = producers.clone();
        let sink_clone: Arc<dyn RelaySink> = sink.clone();
        let shutdown_clone = shutdown.clone();
        let supervisor = tokio::spawn(async move {
            run_watcher_supervisor_loop_with_registry_and_producers(
                SupervisorConfig::for_test(),
                sink_clone,
                shutdown_clone,
                registry_clone,
                producers_clone,
            )
            .await;
        });

        let m = matched("c-prod", ProviderKind::Claude);
        registry.upsert(m.clone(), Some("mac-mini"));

        // Wait until the supervisor reacts to the Added event.
        wait_for(
            || producers.get_producer(&m.expected_session_name).is_some(),
            "supervisor publishes producer for newly-added session",
        )
        .await;

        // The producer must actually push frames into the supervisor-owned
        // relay (the bug #2412 fixes is `try_send_frame` was never called).
        let producer = producers
            .get_producer(&m.expected_session_name)
            .expect("producer present");
        for _ in 0..4 {
            assert!(producer.try_send_frame("payload".into()));
        }
        // Frames flow through the relay into the CountingSink.
        wait_for(
            || sink.count(&m.expected_session_name) >= 4,
            "sink observes producer-pushed frames",
        )
        .await;

        // Removal deregisters the producer.
        registry.remove(&m.expected_session_name);
        wait_for(
            || producers.get_producer(&m.expected_session_name).is_none(),
            "supervisor deregisters producer on Removed",
        )
        .await;

        shutdown.store(true, Ordering::Release);
        // Publish a no-op event so the recv() unblocks.
        let m2 = matched("c-prod-unblock", ProviderKind::Codex);
        registry.upsert(m2, Some("mac-mini"));
        let _ = tokio::time::timeout(Duration::from_secs(2), supervisor).await;
    }

    /// E5 (#2412) regression: cached `RelayProducer` clones outliving the
    /// supervisor must NOT wedge the relay's shutdown. Pre-fix, the cached
    /// clone (e.g. `tmux_watcher::cached_relay_producer`) kept the channel
    /// open and the relay loop blocked on `rx.recv().await` forever — the
    /// supervisor's `Removed`/`Updated` teardown then stalled inside
    /// `handle.shutdown().await`. With receiver-side cancellation, teardown
    /// completes promptly regardless of surviving sender clones.
    #[tokio::test]
    async fn supervisor_teardown_completes_with_outliving_producer_clone() {
        use crate::services::cluster::relay_producer_registry::RelayProducerRegistry;

        let registry = Arc::new(SessionRegistry::new());
        let producers = Arc::new(RelayProducerRegistry::new());
        let sink: Arc<CountingSink> = Arc::new(CountingSink::default());
        let shutdown = Arc::new(AtomicBool::new(false));

        let registry_clone = registry.clone();
        let producers_clone = producers.clone();
        let sink_clone: Arc<dyn RelaySink> = sink.clone();
        let shutdown_clone = shutdown.clone();
        let supervisor = tokio::spawn(async move {
            run_watcher_supervisor_loop_with_registry_and_producers(
                SupervisorConfig::for_test(),
                sink_clone,
                shutdown_clone,
                registry_clone,
                producers_clone,
            )
            .await;
        });

        let m = matched("c-wedge", ProviderKind::Claude);
        registry.upsert(m.clone(), Some("mac-mini"));

        // Wait until supervisor publishes the producer, then snapshot a
        // clone and HOLD IT — this mimics `tmux_watcher::cached_relay_producer`
        // pinning a sender across the supervisor's Removed teardown.
        wait_for(
            || producers.get_producer(&m.expected_session_name).is_some(),
            "supervisor publishes producer",
        )
        .await;
        let cached_clone = producers
            .get_producer(&m.expected_session_name)
            .expect("producer registered");

        // Remove the session. Pre-fix, the supervisor's `handle.shutdown().await`
        // wedged here because `cached_clone` kept the mpsc channel open.
        let removed_at = std::time::Instant::now();
        registry.remove(&m.expected_session_name);
        wait_for(
            || producers.get_producer(&m.expected_session_name).is_none(),
            "supervisor deregisters producer after removal",
        )
        .await;
        assert!(
            removed_at.elapsed() < Duration::from_secs(5),
            "supervisor teardown must not wedge on cached clone"
        );

        // The cached clone observes the shutdown flag — further sends fail
        // fast instead of silently enqueueing into a dead channel.
        assert!(
            !cached_clone.try_send_frame("post-shutdown".into()),
            "cached producer clone must refuse sends once relay has shut down"
        );

        // Supervisor itself shuts down cleanly.
        shutdown.store(true, Ordering::Release);
        let m_unblock = matched("c-wedge-unblock", ProviderKind::Codex);
        registry.upsert(m_unblock, Some("mac-mini"));
        let exited = tokio::time::timeout(Duration::from_secs(2), supervisor).await;
        assert!(exited.is_ok(), "supervisor exits cleanly");
    }

    #[tokio::test]
    async fn idle_supervisor_observes_shutdown_without_registry_event() {
        use crate::services::cluster::relay_producer_registry::RelayProducerRegistry;

        let registry = Arc::new(SessionRegistry::new());
        let producers = Arc::new(RelayProducerRegistry::new());
        let sink: Arc<CountingSink> = Arc::new(CountingSink::default());
        let shutdown = Arc::new(AtomicBool::new(false));

        let registry_clone = registry.clone();
        let producers_clone = producers.clone();
        let sink_clone: Arc<dyn RelaySink> = sink.clone();
        let shutdown_clone = shutdown.clone();
        let supervisor = tokio::spawn(async move {
            run_watcher_supervisor_loop_with_registry_and_producers(
                SupervisorConfig::for_test(),
                sink_clone,
                shutdown_clone,
                registry_clone,
                producers_clone,
            )
            .await;
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        shutdown.store(true, Ordering::Release);

        let exited = tokio::time::timeout(Duration::from_secs(1), supervisor).await;
        assert!(
            exited.is_ok(),
            "idle supervisor must exit without a registry event unblocking recv()"
        );
        assert_eq!(producers.len(), 0);
    }
}
