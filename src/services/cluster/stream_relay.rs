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
//! of the live tmux session. We therefore use a bounded MPSC channel between
//! the producer and the relay task; when the channel is full, the oldest
//! frame is dropped and a counter increments. The watcher API is purely
//! non-blocking ([`StreamRelayHandle::try_send_frame`]).
//!
//! ## Why this lives in `services::cluster`
//!
//! It is the runtime peer of [`super::session_registry::SessionRegistry`] and
//! the [`super::watcher_supervisor::WatcherSupervisor`] that drives it. None
//! of the three reach into Discord directly — they expose generic sinks so
//! the (much larger) Discord-side modules can compose them in E4 without
//! creating an import cycle.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use tokio::sync::{Notify, mpsc};
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
pub struct StreamRelayHandle {
    matched: MatchedChannel,
    tx: mpsc::Sender<StreamFrame>,
    shutdown: Arc<AtomicBool>,
    /// Receiver-side cancellation. The relay loop selects on this alongside
    /// `rx.recv()` so a shutdown forces the loop to exit even if cached
    /// [`RelayProducer`] clones (held outside the supervisor's
    /// `StreamRelayHandle`, e.g. in the tmux watcher) are still alive — they
    /// would otherwise keep the channel open and wedge `recv().await` forever.
    /// E5 (#2412) hardening: see the supervisor `Removed`/`Updated` paths.
    shutdown_notify: Arc<Notify>,
    metrics: Arc<RelayMetrics>,
    sequence: Arc<AtomicU64>,
    task: Option<JoinHandle<()>>,
}

/// Clonable producer-side handle for a [`StreamRelay`]. Owned by the
/// [`super::relay_producer_registry::RelayProducerRegistry`] so the
/// production tmux frame producer (`tmux_watcher`) can `try_send_frame`
/// without holding the supervisor-owned [`StreamRelayHandle`] (which owns the
/// [`JoinHandle`] and cannot be cloned). The producer shares the same
/// underlying `tx`, `shutdown`, `metrics`, and `sequence` atomics — so a
/// shutdown initiated by the supervisor is immediately visible to every
/// producer attempt (E5 #2412).
#[derive(Clone)]
pub struct RelayProducer {
    session_name: String,
    tx: mpsc::Sender<StreamFrame>,
    shutdown: Arc<AtomicBool>,
    metrics: Arc<RelayMetrics>,
    sequence: Arc<AtomicU64>,
}

impl RelayProducer {
    pub fn session_name(&self) -> &str {
        &self.session_name
    }

    pub fn metrics(&self) -> &Arc<RelayMetrics> {
        &self.metrics
    }

    /// Non-blocking enqueue. See [`StreamRelayHandle::try_send_frame`] for
    /// the contract (drop-on-full, returns `false` only when the relay task
    /// has exited). Callers in production must always go through this entry
    /// point — the producer registry hands out clones of this struct.
    pub fn try_send_frame(&self, payload: String) -> bool {
        try_send_frame_inner(
            &self.session_name,
            &self.tx,
            &self.shutdown,
            &self.metrics,
            &self.sequence,
            payload,
        )
    }
}

impl std::fmt::Debug for RelayProducer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayProducer")
            .field("session", &self.session_name)
            .field("metrics", &self.metrics.snapshot())
            .finish()
    }
}

fn try_send_frame_inner(
    session_name: &str,
    tx: &mpsc::Sender<StreamFrame>,
    shutdown: &Arc<AtomicBool>,
    metrics: &Arc<RelayMetrics>,
    sequence: &Arc<AtomicU64>,
    payload: String,
) -> bool {
    if shutdown.load(Ordering::Acquire) {
        return false;
    }
    let seq = sequence.fetch_add(1, Ordering::AcqRel);
    let frame = StreamFrame {
        session_name: session_name.to_string(),
        payload,
        sequence: seq,
    };
    metrics.frames_received.fetch_add(1, Ordering::AcqRel);
    match tx.try_send(frame) {
        Ok(()) => true,
        Err(mpsc::error::TrySendError::Full(frame)) => {
            // Drop-oldest semantics: tokio mpsc has no sender-side pop, so we
            // discard the NEW frame and bump the counter. Production
            // operators see the loss via /api/cluster/sessions.
            metrics.dropped_frames.fetch_add(1, Ordering::AcqRel);
            drop(frame);
            true
        }
        Err(mpsc::error::TrySendError::Closed(_)) => false,
    }
}

impl StreamRelayHandle {
    pub fn matched(&self) -> &MatchedChannel {
        &self.matched
    }

    pub fn metrics(&self) -> &Arc<RelayMetrics> {
        &self.metrics
    }

    /// Return a clonable [`RelayProducer`] sharing the relay's underlying
    /// channel and atomics. Used by E5 (#2412) to expose `try_send_frame` to
    /// the production tmux frame producer via
    /// [`super::relay_producer_registry::RelayProducerRegistry`].
    pub fn producer(&self) -> RelayProducer {
        RelayProducer {
            session_name: self.matched.expected_session_name.clone(),
            tx: self.tx.clone(),
            shutdown: self.shutdown.clone(),
            metrics: self.metrics.clone(),
            sequence: self.sequence.clone(),
        }
    }

    /// Non-blocking enqueue. If the channel is full, the oldest queued frame
    /// is dropped (we drain one then enqueue) and the dropped counter
    /// increments. Returns `false` only if the relay task has already exited
    /// — the upstream caller should then treat the relay as dead.
    pub fn try_send_frame(&self, payload: String) -> bool {
        try_send_frame_inner(
            &self.matched.expected_session_name,
            &self.tx,
            &self.shutdown,
            &self.metrics,
            &self.sequence,
            payload,
        )
    }

    /// Initiate graceful shutdown. Sets the shutdown flag, fires the
    /// receiver-side notify so the relay loop exits even when sender clones
    /// outside this handle (E5 #2412: `RelayProducer` clones cached by the
    /// production tmux watcher) keep the channel open, then drops the
    /// supervisor-owned sender and awaits task completion.
    ///
    /// Without the notify, dropping `tx` was the only signal — and that
    /// signal cannot unblock `rx.recv().await` while any other sender clone
    /// is still alive. Cached producer clones in idle tmux watchers were the
    /// concrete wedge that motivated the explicit receiver-side cancellation.
    /// Safe to call only once — the handle is consumed.
    pub async fn shutdown(self) {
        let StreamRelayHandle {
            tx,
            shutdown,
            shutdown_notify,
            task,
            ..
        } = self;
        shutdown.store(true, Ordering::Release);
        // Wake the relay loop's `select!` so it observes the flag and exits.
        // `notify_one` (not `notify_waiters`) stores a single permit so the
        // wakeup survives the pre-waiter race: if shutdown lands while the
        // loop is mid-`deliver_frame` (no `Notified` future armed), the
        // permit is consumed by the next `notified().await`. The
        // `shutdown.load()` guard at the top of each loop iteration is the
        // fail-closed backstop against any residual missed-notify.
        shutdown_notify.notify_one();
        drop(tx);
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
    let (tx, rx) = mpsc::channel::<StreamFrame>(buffer.max(1));
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_notify = Arc::new(Notify::new());
    let metrics = Arc::new(RelayMetrics::default());

    let session_name = matched.expected_session_name.clone();
    let channel_id = matched.channel_id.clone();
    let task_metrics = metrics.clone();
    let task_shutdown = shutdown.clone();
    let task_shutdown_notify = shutdown_notify.clone();

    let task = tokio::spawn(async move {
        run_relay_loop(
            rx,
            sink,
            task_metrics,
            task_shutdown,
            task_shutdown_notify,
            session_name,
            channel_id,
        )
        .await;
    });

    StreamRelayHandle {
        matched,
        tx,
        shutdown,
        shutdown_notify,
        metrics,
        sequence: Arc::new(AtomicU64::new(0)),
        task: Some(task),
    }
}

async fn run_relay_loop(
    mut rx: mpsc::Receiver<StreamFrame>,
    sink: Arc<dyn RelaySink>,
    metrics: Arc<RelayMetrics>,
    shutdown: Arc<AtomicBool>,
    shutdown_notify: Arc<Notify>,
    session_name: String,
    channel_id: String,
) {
    tracing::info!(
        session = %session_name,
        channel_id = %channel_id,
        "stream-relay entering"
    );
    loop {
        // Fail-closed shutdown check BEFORE entering select!. Guards the
        // pre-waiter race where shutdown lands between the previous
        // iteration's `deliver_frame()` return and the new `notified()`
        // future construction. AtomicBool is loaded unconditionally, so a
        // shutdown set during prior frame delivery is observed here.
        if shutdown.load(Ordering::Acquire) {
            tracing::debug!(
                session = %session_name,
                "stream-relay observed shutdown flag pre-select; draining and exiting"
            );
            drain_pending(&mut rx, &sink, &metrics, &session_name).await;
            break;
        }
        tokio::select! {
            // Receiver-side cancellation. `notify_one` stores a single
            // permit so the wakeup survives even when it lands before
            // `notified()` is armed (closes the pre-waiter race that
            // `notify_waiters` would have lost). Selecting on it makes the
            // loop exit even when external sender clones (E5 #2412 cached
            // `RelayProducer` clones in `tmux_watcher`) keep the channel
            // open.
            _ = shutdown_notify.notified() => {
                tracing::debug!(
                    session = %session_name,
                    "stream-relay observed shutdown notify; draining and exiting"
                );
                drain_pending(&mut rx, &sink, &metrics, &session_name).await;
                break;
            }
            maybe_frame = rx.recv() => {
                let Some(frame) = maybe_frame else {
                    break;
                };
                if shutdown.load(Ordering::Acquire) {
                    tracing::debug!(
                        session = %session_name,
                        "stream-relay observed shutdown flag mid-loop; draining and exiting"
                    );
                    // Deliver the frame we just received plus any pending
                    // siblings — turn-boundary events MUST land in Discord
                    // even during shutdown so operators see the last bytes
                    // of a dying session.
                    deliver_frame(&sink, &frame, &metrics, &session_name).await;
                    drain_pending(&mut rx, &sink, &metrics, &session_name).await;
                    break;
                }
                deliver_frame(&sink, &frame, &metrics, &session_name).await;
            }
        }
    }
    tracing::info!(
        session = %session_name,
        channel_id = %channel_id,
        metrics = ?metrics.snapshot(),
        "stream-relay exiting"
    );
}

/// Best-effort drain of any frames already queued in `rx`. Used on the
/// shutdown path so counters reflect what the upstream watcher had handed
/// off, and so operators see the last bytes of a dying session in Discord.
async fn drain_pending(
    rx: &mut mpsc::Receiver<StreamFrame>,
    sink: &Arc<dyn RelaySink>,
    metrics: &Arc<RelayMetrics>,
    session_name: &str,
) {
    while let Ok(extra) = rx.try_recv() {
        deliver_frame(sink, &extra, metrics, session_name).await;
    }
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

    /// E5 (#2412) regression: a `RelayProducer` clone outliving the
    /// supervisor-owned `StreamRelayHandle` must NOT prevent the relay task
    /// from exiting on `shutdown()`. The cached clone in `tmux_watcher` is
    /// the production motivator — without receiver-side cancellation the
    /// relay's `rx.recv().await` wedges forever and the supervisor stalls.
    #[tokio::test]
    async fn shutdown_unblocks_loop_even_with_outliving_producer_clone() {
        let sink = Arc::new(CapturingSink::default());
        let handle = spawn_stream_relay(matched_for("c-wedge"), sink.clone());
        // Cached clone owned outside the handle, mimicking the tmux watcher
        // `cached_relay_producer` field. The clone stays alive across the
        // `handle.shutdown().await` call.
        let producer_clone = handle.producer();
        // Ship a frame so the relay is definitely past `entering`.
        assert!(handle.try_send_frame("warmup".into()));
        flush_pending().await;

        // Shutdown must complete promptly even though `producer_clone` is
        // still alive holding a `tx` clone. Pre-fix this hung indefinitely.
        let shutdown_done = tokio::time::timeout(Duration::from_secs(2), handle.shutdown()).await;
        assert!(
            shutdown_done.is_ok(),
            "relay shutdown must not wedge when a producer clone outlives the handle"
        );

        // The producer clone observes the shutdown flag and refuses further
        // sends — the watcher cache will then drop on the next chunk.
        assert!(
            !producer_clone.try_send_frame("post-shutdown".into()),
            "producer clone must reject sends once shutdown flag is set"
        );
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
}
