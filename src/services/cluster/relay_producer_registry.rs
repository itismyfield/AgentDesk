//! `RelayProducerRegistry` — global lookup table mapping a tmux session name
//! to the producer-side handle of its supervisor-owned [`StreamRelay`].
//!
//! Epic #2285 / E5 (issue #2412). E3 (#2408) shipped the supervisor that
//! owns one [`super::stream_relay::StreamRelayHandle`] per matched session,
//! and E4 (#2411) wired the sink side via
//! [`super::registry_adapter_sink::RegistryAdapterSink`]. Neither, however,
//! gave the **production tmux frame producer** (`services::discord::
//! tmux_watcher`) any way to push frames into those relays:
//! `StreamRelayHandle` owns a `JoinHandle` and cannot be cloned, and the
//! supervisor's `ActiveRelays` map is private to the supervisor loop.
//!
//! This registry resolves that — the supervisor registers a clonable
//! [`RelayProducer`] for each spawned relay, and the tmux watcher does a
//! cheap `O(1)` lookup keyed by `tmux_session_name` to forward the bytes it
//! just read off disk into the supervisor-owned relay pipeline.
//!
//! # Why a side registry rather than inverting the dataflow
//!
//! The supervisor-owns-the-tmux-read refactor (issue's design option B) is a
//! much bigger surface change — it requires moving file tailing, offset
//! bookkeeping, pause/resume gating, and TUI completion gating out of the
//! per-turn watcher. E5's contract is more modest: keep the existing tmux
//! consumer in place, but **also** mirror its output into the
//! supervisor-owned relay so the new path stops being dark. The legacy
//! delivery sink remains the source of truth for Discord output; the
//! supervisor-owned relay observes via [`RegistryAdapterSink`] until later
//! issues swap the legacy spawn site for direct sink-driven delivery.
//!
//! # Concurrency
//!
//! The map sits behind a `RwLock`; reads (one per tmux read chunk on a hot
//! path) dominate writes (only on session add / remove). The tmux watcher
//! holds the read lock for the duration of a single `get_producer` call —
//! `RelayProducer` itself is cheap to clone (a handful of `Arc`s) so we
//! release the lock before issuing `try_send_frame`.
//!
//! # Lifetime
//!
//! The supervisor registers a producer in [`Self::register`] before storing
//! the corresponding `StreamRelayHandle` in its `ActiveRelays` map; it
//! deregisters via [`Self::deregister`] **before** awaiting the relay's
//! shutdown so an in-flight `try_send_frame` either sees a still-live
//! producer (frame queued and drained during shutdown) or `None` (producer
//! gone — the watcher silently drops the frame, no panic).

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use super::stream_relay::RelayProducer;

/// Process-wide registry mapping `tmux_session_name -> RelayProducer`. There
/// is exactly one instance, populated by the [`WatcherSupervisor`] running on
/// this node. Other modules access it via [`global_relay_producer_registry`].
#[derive(Default)]
pub struct RelayProducerRegistry {
    by_session: RwLock<HashMap<String, RelayProducer>>,
}

impl RelayProducerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a producer under the given tmux session name. Overwrites any
    /// previous entry — the supervisor's `Updated` path tears down the old
    /// relay before spawning the replacement, so an overlap window only
    /// exists if the supervisor's `apply_change` ordering is broken (which
    /// would already be a bug elsewhere).
    pub fn register(&self, session_name: String, producer: RelayProducer) {
        if let Ok(mut guard) = self.by_session.write() {
            guard.insert(session_name, producer);
        }
    }

    /// Remove a producer. Returns whether an entry existed — useful for
    /// supervisor logging on the teardown path.
    pub fn deregister(&self, session_name: &str) -> bool {
        if let Ok(mut guard) = self.by_session.write() {
            guard.remove(session_name).is_some()
        } else {
            false
        }
    }

    /// Cheap clone of the producer for the given session, if registered.
    /// Returns `None` when the session has no live relay (e.g. flag is off,
    /// the relay was torn down, or this node never matched the session) so
    /// callers can no-op without erroring.
    pub fn get_producer(&self, session_name: &str) -> Option<RelayProducer> {
        self.by_session
            .read()
            .ok()
            .and_then(|guard| guard.get(session_name).cloned())
    }

    /// Snapshot of `(session_name, frames_received)` pairs — used by the
    /// `/api/cluster/sessions` diagnostic so operators can confirm the
    /// supervisor-owned relay is no longer a zero-frame path.
    pub fn frames_received_snapshot(&self) -> HashMap<String, u64> {
        self.by_session
            .read()
            .map(|guard| {
                guard
                    .iter()
                    .map(|(name, producer)| {
                        (name.clone(), producer.metrics().snapshot().frames_received)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Number of currently-registered producers. Test-only convenience.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.by_session.read().map(|guard| guard.len()).unwrap_or(0)
    }
}

static GLOBAL: OnceLock<Arc<RelayProducerRegistry>> = OnceLock::new();

/// Returns the process-wide registry. Idempotent — first call initializes,
/// subsequent calls return the same `Arc`. Safe to call from any worker.
pub fn global_relay_producer_registry() -> Arc<RelayProducerRegistry> {
    GLOBAL
        .get_or_init(|| Arc::new(RelayProducerRegistry::new()))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::cluster::session_matcher::{MatchedChannel, expected_rollout_path_for};
    use crate::services::cluster::stream_relay::{DiscardSink, RelaySink, spawn_stream_relay};
    use crate::services::provider::ProviderKind;

    fn matched(channel: &str) -> MatchedChannel {
        let session = ProviderKind::Claude.build_tmux_session_name(channel);
        MatchedChannel {
            channel_id: channel.to_string(),
            agent_id: format!("a-{channel}"),
            provider: ProviderKind::Claude,
            expected_session_name: session.clone(),
            expected_rollout_path: expected_rollout_path_for(&session),
        }
    }

    #[tokio::test]
    async fn register_deregister_roundtrip() {
        let registry = RelayProducerRegistry::new();
        let m = matched("c-reg");
        let sink: Arc<dyn RelaySink> = Arc::new(DiscardSink);
        let handle = spawn_stream_relay(m.clone(), sink);
        registry.register(m.expected_session_name.clone(), handle.producer());

        let prod = registry
            .get_producer(&m.expected_session_name)
            .expect("registered producer must be retrievable");
        assert!(prod.try_send_frame("hello".into()));
        // Counter is shared with the relay handle's metrics — proves we got
        // the SAME underlying producer atomics, not a fresh struct.
        // Wait briefly for the relay task to record the frame.
        for _ in 0..50 {
            if handle.metrics().snapshot().frames_received >= 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        assert_eq!(handle.metrics().snapshot().frames_received, 1);

        assert!(registry.deregister(&m.expected_session_name));
        assert!(registry.get_producer(&m.expected_session_name).is_none());
        assert!(
            !registry.deregister(&m.expected_session_name),
            "second deregister returns false"
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn get_producer_returns_none_for_unknown_session() {
        let registry = RelayProducerRegistry::new();
        assert!(registry.get_producer("AgentDesk-claude-nope").is_none());
    }

    #[tokio::test]
    async fn frames_received_snapshot_reflects_per_session_traffic() {
        let registry = RelayProducerRegistry::new();
        let m1 = matched("c-snap1");
        let m2 = matched("c-snap2");
        let sink: Arc<dyn RelaySink> = Arc::new(DiscardSink);
        let h1 = spawn_stream_relay(m1.clone(), sink.clone());
        let h2 = spawn_stream_relay(m2.clone(), sink);
        registry.register(m1.expected_session_name.clone(), h1.producer());
        registry.register(m2.expected_session_name.clone(), h2.producer());

        let p1 = registry.get_producer(&m1.expected_session_name).unwrap();
        for _ in 0..3 {
            assert!(p1.try_send_frame("x".into()));
        }
        let snap = registry.frames_received_snapshot();
        assert_eq!(snap.get(&m1.expected_session_name).copied(), Some(3));
        assert_eq!(snap.get(&m2.expected_session_name).copied(), Some(0));
        h1.shutdown().await;
        h2.shutdown().await;
    }
}
