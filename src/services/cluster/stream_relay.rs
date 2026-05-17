//! `StreamRelay` — per-session task that forwards provider stream frames into
//! the Discord-side delivery path, regardless of provider (claude / codex /
//! qwen / gemini / opencode).
//!
//! Epic #2285 / E3 (issue #2345). The relay's lifetime is **session-bound,
//! NOT turn-bound** — see the epic for the rationale. Sub-agent invocation,
//! tool calls, planning blocks and intermediate "done" markers are recorded
//! as part of the inflight audit trail but do NOT terminate the relay. The
//! only termination signal is:
//!
//! 1. The owning [`WatcherSupervisor`] told us the session disappeared
//!    (graceful shutdown via [`StreamRelayHandle::shutdown`]).
//! 2. The relay's runtime shutdown flag flipped.
//! 3. The upstream frame source returned None (channel closed).
//!
//! ## Provider-agnostic
//!
//! The relay accepts `StreamFrame`s — opaque payloads tagged with their
//! origin session — from any source (rollout/jsonl tail, ad-hoc test feed,
//! future tmux pipe-pane). The Discord-side delivery is abstracted behind
//! the [`RelaySink`] trait so:
//! - Production wires a Discord delivery adapter (E4 migration, #2346).
//! - Tests wire a `Vec<StreamFrame>` collector with no I/O.
//!
//! This deliberately replaces the case-by-case provider branching that the
//! legacy turn-bridge spreads across `turn_bridge/`, `tmux_watcher.rs`, etc.
//! E3 lands the new infrastructure alongside the legacy path; E4 (#2346)
//! migrates the call-sites and removes the branching.
//!
//! ## Backpressure
//!
//! Discord delivery is comparatively slow. The relay must NEVER block the
//! upstream watcher — a stuck Discord side would silently freeze observation
//! of the live tmux session. We therefore use a bounded **owned queue**
//! ([`RelayQueue`], `VecDeque<StreamFrame>` + `Notify`) between the producer
//! and the relay task; when the queue is full, the **oldest** queued frame
//! is evicted and the new frame is accepted (the dropped counter increments
//! for each eviction). The watcher API is purely non-blocking
//! ([`StreamRelayHandle::try_send_frame`]). Preserving the newest frame
//! protects final answers and completion markers, which would otherwise be
//! silently lost behind a stale backlog.
//!
//! ## Why this lives in `services::cluster`
//!
//! It is the runtime peer of [`super::session_registry::SessionRegistry`] and
//! the [`super::watcher_supervisor::WatcherSupervisor`] that drives it. None
//! of the three reach into Discord directly — they expose generic sinks so
//! the (much larger) Discord-side modules can compose them in E4 without
//! creating an import cycle.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::session_matcher::MatchedChannel;

/// Default size of the producer → relay channel. Generous enough to absorb a
/// burst of provider output (e.g. a long planning block dumping thousands of
/// lines at once) without losing data, bounded so a stuck consumer cannot
/// exhaust memory — we drop the oldest frame and bump
/// [`RelayMetrics::dropped_frames`] when full.
pub const DEFAULT_RELAY_BUFFER: usize = 1024;

/// An opaque stream frame emitted by a provider. Carries enough metadata for
/// the sink to route + format without re-reading the rollout file.
///
/// The `payload` is intentionally a `String` rather than a structured event:
/// providers emit different schemas (Claude / Codex / qwen / ...), and E3's
/// job is purely to ship bytes from session → Discord. E4 (or a later epic)
/// will add structured parsing where it's worth the maintenance cost.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamFrame {
    /// The tmux session name this frame originated from. Used by sinks that
    /// multiplex frames from many sessions onto a single delivery worker.
    pub session_name: String,
    /// Raw frame bytes (typically a JSONL line). Sink chooses formatting.
    pub payload: String,
    /// Monotonic sequence number assigned by the relay. Useful for sinks that
    /// want to detect drops / reorder.
    pub sequence: u64,
    /// **Routing binding snapshot** captured at enqueue time. Carrying this
    /// alongside the frame keeps Discord routing correct even when the
    /// session is rebound or the channel id is truncated in the session name
    /// — sinks must NOT re-derive channel_id / agent_id / provider from
    /// `session_name`. Wrapped in `Arc` so cloning the frame is cheap
    /// regardless of binding size. See `#2409` finding #2.
    pub binding: Arc<MatchedChannel>,
}

/// Per-session counters. Exposed via the supervisor for diagnostics.
#[derive(Debug, Default)]
pub struct RelayMetrics {
    pub frames_received: AtomicU64,
    pub frames_delivered: AtomicU64,
    pub dropped_frames: AtomicU64,
    pub sink_errors: AtomicU64,
}

impl RelayMetrics {
    pub fn snapshot(&self) -> RelayMetricsSnapshot {
        RelayMetricsSnapshot {
            frames_received: self.frames_received.load(Ordering::Acquire),
            frames_delivered: self.frames_delivered.load(Ordering::Acquire),
            dropped_frames: self.dropped_frames.load(Ordering::Acquire),
            sink_errors: self.sink_errors.load(Ordering::Acquire),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RelayMetricsSnapshot {
    pub frames_received: u64,
    pub frames_delivered: u64,
    pub dropped_frames: u64,
    pub sink_errors: u64,
}

/// Abstract destination for relayed frames. E3 keeps this trait
/// intentionally tiny so the (much larger) Discord delivery modules can
/// implement it from their existing entry points without changing those
/// modules — they merely register an adapter when the flag is on.
#[async_trait]
pub trait RelaySink: Send + Sync + 'static {
    /// Deliver a single frame. Returning `Err` increments the sink-error
    /// counter and skips this frame; the relay does NOT terminate — a
    /// transient Discord HTTP error must not stop session observation.
    async fn deliver(&self, frame: &StreamFrame) -> Result<(), RelaySinkError>;
}

#[derive(Debug, thiserror::Error)]
pub enum RelaySinkError {
    #[error("transient sink failure: {0}")]
    Transient(String),
    #[error("permanent sink failure: {0}")]
    Permanent(String),
}

/// No-op sink used when the supervisor is wired without a real Discord
/// adapter (e.g. flag enabled but the migration hasn't landed E4 yet, or
/// unit tests that don't care about delivery semantics).
pub struct DiscardSink;

#[async_trait]
impl RelaySink for DiscardSink {
    async fn deliver(&self, _frame: &StreamFrame) -> Result<(), RelaySinkError> {
        Ok(())
    }
}

/// Handle returned by [`spawn_stream_relay`]. The supervisor holds one of
/// these per active session and uses [`Self::shutdown`] when the session
/// disappears from the [`super::session_registry::SessionRegistry`].
/// Bounded FIFO with **real drop-oldest** semantics. Backed by a
/// `VecDeque<StreamFrame>` so the producer can `pop_front()` before
/// `push_back()` when the queue is full — that preserves the newest output
/// (final answers, completion markers) at the expense of stale backlog. The
/// previous mpsc-based implementation dropped the *new* frame on
/// `TrySendError::Full`, which is the opposite of the documented intent.
/// See `#2409` finding #3.
pub(crate) struct RelayQueue {
    inner: StdMutex<VecDeque<StreamFrame>>,
    capacity: usize,
    notify: Notify,
    closed: AtomicBool,
}

impl RelayQueue {
    fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            inner: StdMutex::new(VecDeque::with_capacity(capacity)),
            capacity,
            notify: Notify::new(),
            closed: AtomicBool::new(false),
        }
    }

    /// Push a frame. Returns `Ok(dropped_old)` when the frame was accepted:
    /// `dropped_old == true` iff an older frame had to be evicted to make
    /// room. Returns `Err(())` only when the queue has been `close()`d —
    /// callers should treat that as "relay task gone".
    fn push(&self, frame: StreamFrame) -> Result<bool, ()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(());
        }
        let dropped = {
            let mut guard = self.inner.lock().expect("relay queue mutex poisoned");
            let mut dropped = false;
            while guard.len() >= self.capacity {
                guard.pop_front();
                dropped = true;
            }
            guard.push_back(frame);
            dropped
        };
        // Wake the single waiting consumer. `notify_one` is sticky — if the
        // consumer is not currently awaiting, the next `notified().await`
        // will return immediately, so we never wedge on a missed wake.
        self.notify.notify_one();
        Ok(dropped)
    }

    /// Pop the front (oldest) frame. Awaits until a frame is available, the
    /// queue is closed, or the consumer's task is cancelled.
    async fn pop(&self) -> Option<StreamFrame> {
        loop {
            // Fast path — grab a frame if one is queued.
            {
                let mut guard = self.inner.lock().expect("relay queue mutex poisoned");
                if let Some(frame) = guard.pop_front() {
                    return Some(frame);
                }
                if self.closed.load(Ordering::Acquire) {
                    return None;
                }
            }
            // Slow path — register a wake intent BEFORE re-checking, so a
            // concurrent `push` either wakes us or leaves a fresh permit.
            let notified = self.notify.notified();
            {
                let mut guard = self.inner.lock().expect("relay queue mutex poisoned");
                if let Some(frame) = guard.pop_front() {
                    return Some(frame);
                }
                if self.closed.load(Ordering::Acquire) {
                    return None;
                }
            }
            notified.await;
        }
    }

    /// Non-blocking pop used by the drain path after the queue is closed.
    fn try_pop(&self) -> Option<StreamFrame> {
        self.inner
            .lock()
            .expect("relay queue mutex poisoned")
            .pop_front()
    }

    /// Mark the queue as closed and wake the consumer so it can exit cleanly.
    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}

pub struct StreamRelayHandle {
    matched: Arc<MatchedChannel>,
    queue: Arc<RelayQueue>,
    shutdown: Arc<AtomicBool>,
    metrics: Arc<RelayMetrics>,
    sequence: AtomicU64,
    task: Option<JoinHandle<()>>,
}

impl StreamRelayHandle {
    pub fn matched(&self) -> &MatchedChannel {
        &self.matched
    }

    pub fn metrics(&self) -> &Arc<RelayMetrics> {
        &self.metrics
    }

    /// Non-blocking enqueue. If the queue is full, the **oldest** queued
    /// frame is dropped and the new frame is enqueued — see `#2409` finding
    /// #3 for why preserving the newest frame matters. The dropped counter
    /// increments per evicted frame. Returns `false` only if the relay task
    /// has already exited — the upstream caller should then treat the relay
    /// as dead.
    pub fn try_send_frame(&self, payload: String) -> bool {
        if self.shutdown.load(Ordering::Acquire) {
            return false;
        }
        let sequence = self.sequence.fetch_add(1, Ordering::AcqRel);
        let frame = StreamFrame {
            session_name: self.matched.expected_session_name.clone(),
            payload,
            sequence,
            binding: self.matched.clone(),
        };
        self.metrics.frames_received.fetch_add(1, Ordering::AcqRel);
        match self.queue.push(frame) {
            Ok(false) => true,
            Ok(true) => {
                self.metrics.dropped_frames.fetch_add(1, Ordering::AcqRel);
                true
            }
            Err(()) => false,
        }
    }

    /// Initiate graceful shutdown. Closes the queue so the relay task drains
    /// any pending frames, then awaits task completion. Safe to call only
    /// once — the handle is consumed.
    pub async fn shutdown(self) {
        let StreamRelayHandle {
            queue,
            shutdown,
            task,
            ..
        } = self;
        shutdown.store(true, Ordering::Release);
        queue.close();
        if let Some(handle) = task {
            let _ = handle.await;
        }
    }

    /// Test helper: synchronously check whether the underlying relay task is
    /// still alive (handle not yet shut down).
    #[cfg(test)]
    pub fn is_running(&self) -> bool {
        !self.shutdown.load(Ordering::Acquire)
    }
}

impl std::fmt::Debug for StreamRelayHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamRelayHandle")
            .field("session", &self.matched.expected_session_name)
            .field("channel_id", &self.matched.channel_id)
            .field("metrics", &self.metrics.snapshot())
            .finish()
    }
}

/// Spawn a relay task for `matched`. The returned handle is the only stable
/// reference the supervisor needs — drop it (or call `shutdown`) to wind the
/// relay down.
pub fn spawn_stream_relay(matched: MatchedChannel, sink: Arc<dyn RelaySink>) -> StreamRelayHandle {
    spawn_stream_relay_with_buffer(matched, sink, DEFAULT_RELAY_BUFFER)
}

/// Variant with an explicit buffer size — test-only knob.
pub fn spawn_stream_relay_with_buffer(
    matched: MatchedChannel,
    sink: Arc<dyn RelaySink>,
    buffer: usize,
) -> StreamRelayHandle {
    let queue = Arc::new(RelayQueue::new(buffer));
    let shutdown = Arc::new(AtomicBool::new(false));
    let metrics = Arc::new(RelayMetrics::default());

    let matched = Arc::new(matched);
    let session_name = matched.expected_session_name.clone();
    let channel_id = matched.channel_id.clone();
    let task_queue = queue.clone();
    let task_metrics = metrics.clone();
    let task_shutdown = shutdown.clone();

    let task = tokio::spawn(async move {
        run_relay_loop(
            task_queue,
            sink,
            task_metrics,
            task_shutdown,
            session_name,
            channel_id,
        )
        .await;
    });

    StreamRelayHandle {
        matched,
        queue,
        shutdown,
        metrics,
        sequence: AtomicU64::new(0),
        task: Some(task),
    }
}

async fn run_relay_loop(
    queue: Arc<RelayQueue>,
    sink: Arc<dyn RelaySink>,
    metrics: Arc<RelayMetrics>,
    shutdown: Arc<AtomicBool>,
    session_name: String,
    channel_id: String,
) {
    tracing::info!(
        session = %session_name,
        channel_id = %channel_id,
        "stream-relay entering"
    );
    while let Some(frame) = queue.pop().await {
        if shutdown.load(Ordering::Acquire) {
            tracing::debug!(
                session = %session_name,
                "stream-relay observed shutdown flag mid-loop; draining and exiting"
            );
            // Drain any pending frames so the recorded counters reflect them.
            // We deliberately still deliver — turn-boundary events MUST land
            // in Discord even during shutdown so operators see the last
            // bytes of a dying session.
            deliver_frame(&sink, &frame, &metrics, &session_name).await;
            while let Some(extra) = queue.try_pop() {
                deliver_frame(&sink, &extra, &metrics, &session_name).await;
            }
            break;
        }
        deliver_frame(&sink, &frame, &metrics, &session_name).await;
    }
    tracing::info!(
        session = %session_name,
        channel_id = %channel_id,
        metrics = ?metrics.snapshot(),
        "stream-relay exiting"
    );
}

async fn deliver_frame(
    sink: &Arc<dyn RelaySink>,
    frame: &StreamFrame,
    metrics: &Arc<RelayMetrics>,
    session_name: &str,
) {
    match sink.deliver(frame).await {
        Ok(()) => {
            metrics.frames_delivered.fetch_add(1, Ordering::AcqRel);
        }
        Err(error) => {
            metrics.sink_errors.fetch_add(1, Ordering::AcqRel);
            tracing::warn!(
                session = %session_name,
                seq = frame.sequence,
                ?error,
                "stream-relay sink delivery failed; continuing (session-bound, not turn-bound)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::cluster::session_matcher::expected_rollout_path_for;
    use crate::services::provider::ProviderKind;
    use std::sync::Mutex;
    use std::time::Duration;

    /// Captures every delivered frame in memory. The Mutex is fine here —
    /// each test spawns its own sink and the relay only emits one frame at
    /// a time per session.
    #[derive(Default)]
    struct CapturingSink {
        frames: Mutex<Vec<StreamFrame>>,
        fail_next: AtomicBool,
    }

    impl CapturingSink {
        fn delivered(&self) -> Vec<StreamFrame> {
            self.frames.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl RelaySink for CapturingSink {
        async fn deliver(&self, frame: &StreamFrame) -> Result<(), RelaySinkError> {
            if self.fail_next.swap(false, Ordering::AcqRel) {
                return Err(RelaySinkError::Transient("forced".into()));
            }
            self.frames.lock().unwrap().push(frame.clone());
            Ok(())
        }
    }

    fn matched_for(channel: &str) -> MatchedChannel {
        let session = ProviderKind::Claude.build_tmux_session_name(channel);
        MatchedChannel {
            channel_id: channel.to_string(),
            agent_id: "test-agent".to_string(),
            provider: ProviderKind::Claude,
            expected_session_name: session.clone(),
            expected_rollout_path: expected_rollout_path_for(&session),
        }
    }

    async fn flush_pending() {
        // Yield enough times for the relay task to drain the channel under
        // the current-thread runtime used by `#[tokio::test]`. A few yields
        // is more reliable than a sleep across CI hosts.
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn delivers_every_frame_in_order() {
        let sink = Arc::new(CapturingSink::default());
        let handle = spawn_stream_relay(matched_for("c-x"), sink.clone());
        for i in 0..5 {
            assert!(handle.try_send_frame(format!("frame-{i}")));
        }
        flush_pending().await;
        let delivered = sink.delivered();
        assert_eq!(delivered.len(), 5);
        for (i, frame) in delivered.iter().enumerate() {
            assert_eq!(frame.payload, format!("frame-{i}"));
            assert_eq!(frame.sequence, i as u64);
            assert_eq!(frame.session_name, handle.matched().expected_session_name);
        }
        assert_eq!(handle.metrics().snapshot().frames_delivered, 5);
        assert_eq!(handle.metrics().snapshot().dropped_frames, 0);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn sub_agent_invocation_does_not_stop_relay() {
        // Acceptance criterion: turn-boundary events (sub-agent invocation,
        // Task tool, planning blocks, intermediate "done" markers) must NOT
        // terminate the relay. The relay only stops on session death.
        let sink = Arc::new(CapturingSink::default());
        let handle = spawn_stream_relay(matched_for("c-sub"), sink.clone());

        let frames = [
            r#"{"type":"message","content":"hello"}"#,
            r#"{"type":"tool_use","name":"Task","input":{"prompt":"sub-agent"}}"#,
            r#"{"type":"message","content":"intermediate done"}"#,
            r#"{"type":"thinking","content":"..."}"#,
            r#"{"type":"message","content":"final after sub-agent"}"#,
        ];
        for frame in &frames {
            assert!(handle.try_send_frame((*frame).to_string()));
        }
        flush_pending().await;
        let delivered = sink.delivered();
        assert_eq!(
            delivered.len(),
            frames.len(),
            "every frame must be delivered regardless of turn-boundary content"
        );
        assert!(
            handle.is_running(),
            "relay must remain alive across sub-agent invocation / intermediate done"
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn transient_sink_error_does_not_terminate_relay() {
        let sink = Arc::new(CapturingSink::default());
        sink.fail_next.store(true, Ordering::Release);
        let handle = spawn_stream_relay(matched_for("c-err"), sink.clone());
        assert!(handle.try_send_frame("will fail".into()));
        assert!(handle.try_send_frame("will succeed".into()));
        flush_pending().await;
        let delivered = sink.delivered();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].payload, "will succeed");
        assert_eq!(handle.metrics().snapshot().sink_errors, 1);
        assert_eq!(handle.metrics().snapshot().frames_delivered, 1);
        assert!(handle.is_running());
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn backpressure_drops_frames_when_buffer_full_without_blocking() {
        // Block the sink so frames pile up in the channel.
        struct BlockingSink {
            unblock: tokio::sync::Notify,
        }
        #[async_trait]
        impl RelaySink for BlockingSink {
            async fn deliver(&self, _frame: &StreamFrame) -> Result<(), RelaySinkError> {
                self.unblock.notified().await;
                Ok(())
            }
        }
        let sink = Arc::new(BlockingSink {
            unblock: tokio::sync::Notify::new(),
        });
        // Buffer of 2: producer can fit roughly 2 frames before the relay
        // task's first recv() unblocks. We try to push 50 → many must drop.
        let handle = spawn_stream_relay_with_buffer(
            matched_for("c-bp"),
            sink.clone() as Arc<dyn RelaySink>,
            2,
        );
        let start = std::time::Instant::now();
        for i in 0..50 {
            // try_send_frame is non-blocking. The whole loop must complete
            // well before the relay sink ever delivers a frame.
            let _ = handle.try_send_frame(format!("frame-{i}"));
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(100),
            "try_send_frame must be non-blocking even when the sink stalls (took {elapsed:?})"
        );
        let snap = handle.metrics().snapshot();
        assert_eq!(snap.frames_received, 50);
        assert!(
            snap.dropped_frames > 0,
            "expected drops when buffer is full but sink is stalled: {snap:?}"
        );
        // Release the sink so the task can exit cleanly.
        sink.unblock.notify_waiters();
        handle.shutdown().await;
    }

    /// #2409 finding #2 regression — every delivered frame carries the
    /// routing binding snapshot, so sinks never re-derive channel_id /
    /// agent_id / provider from the (potentially truncated) session name.
    #[tokio::test]
    async fn frames_carry_binding_snapshot() {
        let sink = Arc::new(CapturingSink::default());
        let matched = matched_for("c-bind-1234567890");
        let handle = spawn_stream_relay(matched.clone(), sink.clone());
        assert!(handle.try_send_frame("hello".into()));
        flush_pending().await;
        let delivered = sink.delivered();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].binding.channel_id, matched.channel_id);
        assert_eq!(delivered[0].binding.agent_id, matched.agent_id);
        assert_eq!(delivered[0].binding.provider, matched.provider);
        assert_eq!(
            delivered[0].binding.expected_session_name,
            matched.expected_session_name
        );
        // Sanity: the Arc-shared binding identity matches the handle's
        // matched() to confirm rebind detection upstream still works.
        assert_eq!(
            delivered[0].binding.expected_session_name,
            handle.matched().expected_session_name
        );
        handle.shutdown().await;
    }

    /// #2409 finding #3 regression — the queue is real drop-oldest: when the
    /// buffer is full, the *oldest* frames are evicted and the *newest* one
    /// makes it into the queue. Without this, final completion markers would
    /// be silently lost behind a stale backlog.
    #[tokio::test]
    async fn drop_oldest_preserves_newest_frame() {
        // Tiny capacity (1) so every push beyond the first MUST evict the
        // older one. A blocking sink keeps the consumer parked so the
        // producer alone drives the eviction policy.
        struct LatchSink {
            release: tokio::sync::Notify,
            frames: Mutex<Vec<StreamFrame>>,
        }
        #[async_trait]
        impl RelaySink for LatchSink {
            async fn deliver(&self, frame: &StreamFrame) -> Result<(), RelaySinkError> {
                self.release.notified().await;
                self.frames.lock().unwrap().push(frame.clone());
                Ok(())
            }
        }
        let sink = Arc::new(LatchSink {
            release: tokio::sync::Notify::new(),
            frames: Mutex::new(Vec::new()),
        });
        let handle = spawn_stream_relay_with_buffer(
            matched_for("c-newest"),
            sink.clone() as Arc<dyn RelaySink>,
            1,
        );

        // Let the relay task pop the first frame and park inside deliver().
        assert!(handle.try_send_frame("seed-consumer-park".into()));
        flush_pending().await;

        // Now flood while the consumer is parked. Capacity is 1, so each
        // push beyond the first must evict the previous queued frame and
        // leave only the most recent one behind.
        for i in 0..20 {
            assert!(handle.try_send_frame(format!("frame-{i}")));
        }
        let snap = handle.metrics().snapshot();
        assert!(
            snap.dropped_frames >= 19,
            "expected at least 19 evictions with capacity=1, got {snap:?}"
        );

        // Release the sink: it delivers the parked frame ("seed"), then the
        // single remaining queued frame must be the LAST one we pushed.
        sink.release.notify_waiters();
        flush_pending().await;
        // Drain the relay by closing — its drain loop delivers the last
        // queued frame, then we inspect what landed. notify_waiters() may
        // need to fire again as drain delivers extra frames; we issue a
        // second notify to release the drain path's deliver_frame.
        sink.release.notify_waiters();
        handle.shutdown().await;
        let delivered = sink.frames.lock().unwrap();
        let last = delivered.last().expect("at least one frame delivered");
        assert_eq!(
            last.payload, "frame-19",
            "newest frame survives drop-oldest backpressure (got {delivered:?})"
        );
    }
}
