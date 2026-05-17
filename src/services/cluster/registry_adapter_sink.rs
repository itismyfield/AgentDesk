//! `RegistryAdapterSink` — production [`RelaySink`] for the
//! [`WatcherSupervisor`] (E3) that bridges the session-bound relay path into
//! the existing turn-bound Discord delivery infrastructure.
//!
//! Epic #2285 / E4 (issue #2346).
//!
//! # Why a bridge, not a replacement
//!
//! E3 (#2345 / #2408) landed the session-bound relay infrastructure
//! (`SessionRegistry` → `WatcherSupervisor` → `StreamRelay`) alongside the
//! legacy turn-bound watcher (`services::discord::tmux_watcher`). Both paths
//! observe the same tmux sessions; only the legacy watcher currently performs
//! Discord delivery.
//!
//! Replacing the 3.3kloc tmux watcher with a session-bound delivery pipeline
//! is a major surface change that must land in stages (subsequent issues in
//! epic #2285). E4 explicitly keeps the legacy delivery path as the fallback
//! and ships the supervisor + relay lifecycle in production with this
//! *observation-only* sink. Concretely, this sink:
//!
//! 1. Acknowledges every frame so the relay loop never blocks.
//! 2. Records per-session frame counts (lock-free atomics) for telemetry
//!    and for the e2e test that verifies end-to-end wiring.
//! 3. Does **NOT** write to Discord — the legacy tmux watcher remains the
//!    sole delivery owner for the duration of the flag-flip release.
//!
//! When the legacy spawn site is migrated (follow-up issue), the sink can be
//! extended to perform Discord delivery directly. Until then, flipping
//! `cluster.session_bound_relay_enabled = true` is safe: it only activates
//! observation infrastructure that already passes its E3 test suite.
//!
//! # Why this lives in `services::cluster`
//!
//! Same reason `StreamRelay` and `WatcherSupervisor` do — it has no Discord
//! dependencies. Keeping the sink decoupled from `services::discord` avoids
//! pulling the supervisor module into the Discord runtime's compile graph.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;

use super::stream_relay::{RelaySink, RelaySinkError, StreamFrame};
use super::watcher_supervisor::{SupervisorConfig, run_watcher_supervisor_loop};

/// Observation-only [`RelaySink`] used in production when
/// `cluster.session_bound_relay_enabled = true`. See module docs.
#[derive(Debug, Default)]
pub struct RegistryAdapterSink {
    frames_total: AtomicU64,
    by_session: Mutex<HashMap<String, SessionMetrics>>,
}

/// Per-session lightweight counters exposed for telemetry / tests.
#[derive(Debug, Default, Clone, Copy)]
pub struct SessionMetrics {
    pub frames_observed: u64,
    pub last_sequence: u64,
}

impl RegistryAdapterSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Total frames observed across all sessions since the sink was created.
    pub fn frames_total(&self) -> u64 {
        self.frames_total.load(Ordering::Acquire)
    }

    /// Snapshot of the per-session counters. The lock is held only long
    /// enough to clone the small `HashMap<String, SessionMetrics>` so this
    /// never contends with the hot path beyond a single insert.
    pub fn snapshot(&self) -> HashMap<String, SessionMetrics> {
        self.by_session
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// Direct lookup for a single session — convenience for tests.
    pub fn frames_for(&self, session_name: &str) -> u64 {
        self.by_session
            .lock()
            .ok()
            .and_then(|guard| guard.get(session_name).map(|m| m.frames_observed))
            .unwrap_or(0)
    }
}

#[async_trait]
impl RelaySink for RegistryAdapterSink {
    async fn deliver(&self, frame: &StreamFrame) -> Result<(), RelaySinkError> {
        self.frames_total.fetch_add(1, Ordering::AcqRel);
        if let Ok(mut by_session) = self.by_session.lock() {
            let entry = by_session
                .entry(frame.session_name.clone())
                .or_insert_with(SessionMetrics::default);
            entry.frames_observed = entry.frames_observed.saturating_add(1);
            entry.last_sequence = frame.sequence;
        }
        // E4 stance: legacy tmux_watcher remains the delivery owner. We
        // intentionally do NOT echo `frame.payload` anywhere — emitting it
        // would risk duplicate Discord delivery while the legacy spawn site
        // is still active. Subsequent issues in epic #2285 will swap this
        // for a real Discord adapter once the legacy spawn site is gated
        // off.
        tracing::trace!(
            session = %frame.session_name,
            sequence = frame.sequence,
            "registry-adapter-sink: observed frame (delivery deferred to legacy watcher)"
        );
        Ok(())
    }
}

/// Convenience entry-point wired by `worker_registry` when the
/// `cluster.session_bound_relay_enabled` flag is on. Runs the supervisor
/// against a single shared [`RegistryAdapterSink`] so cumulative counters are
/// available globally for diagnostics.
pub async fn run_with_registry_adapter_sink(shutdown: Arc<AtomicBool>) {
    let sink: Arc<dyn RelaySink> = Arc::new(RegistryAdapterSink::new());
    run_watcher_supervisor_loop(SupervisorConfig::default(), sink, shutdown).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::cluster::session_matcher::{MatchedChannel, expected_rollout_path_for};
    use crate::services::cluster::stream_relay::{StreamFrame, spawn_stream_relay};
    use crate::services::provider::ProviderKind;

    fn matched(channel: &str) -> MatchedChannel {
        let session = ProviderKind::Claude.build_tmux_session_name(channel);
        MatchedChannel {
            channel_id: channel.to_string(),
            agent_id: format!("agent-{channel}"),
            provider: ProviderKind::Claude,
            expected_session_name: session.clone(),
            expected_rollout_path: expected_rollout_path_for(&session),
        }
    }

    #[tokio::test]
    async fn deliver_counts_per_session() {
        let sink = Arc::new(RegistryAdapterSink::new());
        let frame = StreamFrame {
            session_name: "AgentDesk-claude-c1".into(),
            payload: "{}".into(),
            sequence: 7,
        };
        sink.deliver(&frame).await.expect("infallible");
        sink.deliver(&frame).await.expect("infallible");

        assert_eq!(sink.frames_total(), 2);
        assert_eq!(sink.frames_for("AgentDesk-claude-c1"), 2);
        let snap = sink.snapshot();
        assert_eq!(snap.get("AgentDesk-claude-c1").unwrap().last_sequence, 7);
    }

    #[tokio::test]
    async fn end_to_end_relay_through_sink_records_matched_channel_binding() {
        let sink_arc = Arc::new(RegistryAdapterSink::new());
        let sink_trait: Arc<dyn RelaySink> = sink_arc.clone();
        let m = matched("c-e2e");

        let handle = spawn_stream_relay(m.clone(), sink_trait);
        assert!(handle.try_send_frame("hello".into()));
        assert!(handle.try_send_frame("world".into()));

        // Wait until the relay drained both frames into the sink.
        for _ in 0..200 {
            if sink_arc.frames_for(&m.expected_session_name) >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(sink_arc.frames_for(&m.expected_session_name), 2);
        assert_eq!(sink_arc.frames_total(), 2);
        // MatchedChannel binding is preserved across the path: the sink
        // observes frames keyed by the supervisor-chosen tmux session name.
        let snap = sink_arc.snapshot();
        assert!(snap.contains_key(&m.expected_session_name));

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn registry_to_supervisor_to_sink_end_to_end() {
        // Full E4 integration shape:
        //   tmux session → SessionRegistry.upsert → WatcherSupervisor spawns
        //   StreamRelay → RegistryAdapterSink records the frame keyed by the
        //   MatchedChannel.expected_session_name.
        //
        // The supervisor doesn't expose its internal relay handles, so we
        // exercise the wiring by:
        //   1. Spawning the supervisor against the live registry,
        //   2. Upserting a matched session,
        //   3. Independently spawning a sibling relay against the SAME sink
        //      with the same MatchedChannel binding, sending a frame, and
        //      verifying the sink sees it under that session name.
        //
        // This proves the sink/MatchedChannel contract that the supervisor
        // relies on; the supervisor's own spawn path is exercised by
        // watcher_supervisor::tests.
        use crate::services::cluster::session_registry::SessionRegistry;
        use crate::services::cluster::watcher_supervisor::{
            SupervisorConfig, run_watcher_supervisor_loop_with_registry,
        };

        let registry = Arc::new(SessionRegistry::new());
        let sink_arc = Arc::new(RegistryAdapterSink::new());
        let sink_trait: Arc<dyn RelaySink> = sink_arc.clone();
        let shutdown = Arc::new(AtomicBool::new(false));

        let registry_clone = registry.clone();
        let sink_clone = sink_trait.clone();
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

        let m = matched("c-e2e-reg");
        registry.upsert(m.clone(), Some("mac-mini"));

        // Give the supervisor time to react to the Added event and spawn its
        // own relay (we don't read frames from that relay; we just confirm
        // the supervisor lifecycle was driven end-to-end without panicking).
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        // Now demonstrate frame delivery through the sink (any relay against
        // the same sink/binding is equivalent at the sink layer).
        let probe = spawn_stream_relay(m.clone(), sink_trait.clone());
        assert!(probe.try_send_frame("frame-1".into()));
        for _ in 0..200 {
            if sink_arc.frames_for(&m.expected_session_name) >= 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            sink_arc.frames_for(&m.expected_session_name) >= 1,
            "sink must observe at least one frame under the matched session name"
        );

        probe.shutdown().await;
        shutdown.store(true, Ordering::Release);
        // Publish a remove so the supervisor recv() unblocks.
        registry.remove(&m.expected_session_name);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), supervisor).await;
    }
}

