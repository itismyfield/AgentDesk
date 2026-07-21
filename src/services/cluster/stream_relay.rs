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
//! 3. The upstream frame source returned None (queue closed).
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
//! of the live tmux session. We therefore use a bounded in-memory queue between
//! the producer and the relay task; when the queue is full, the oldest
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

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::session_matcher::MatchedChannel;

mod identity;
pub use identity::{RelayDroppedFrame, RelayTurnIdentity};

/// Default size of the producer → relay queue. Generous enough to absorb a
/// burst of provider output (e.g. a long planning block dumping thousands of
/// lines at once) without losing data, bounded so a stuck consumer cannot
/// exhaust memory — we drop the oldest frame and bump
/// [`RelayMetrics::dropped_frames`] when full.
pub const DEFAULT_RELAY_BUFFER: usize = 1024;

/// #3041 P1-3 R5 (per-sequence terminal-ACK correlation): how many recent
/// terminal sequences' resolved outcomes the relay retains for exact-sequence
/// ACK lookup. The watcher waits ~10s for its terminal frame's own outcome, so
/// only the most recent handful of terminal sequences can ever be queried; we
/// keep a small bounded ring so a busy session cannot grow this without bound.
/// Older entries are evicted FIFO — an evicted sequence reads back as `None`
/// (treated by the watcher as "not yet resolved" → it keeps waiting / eventually
/// times out → reconciles, never a false ACK).
pub const TERMINAL_OUTCOME_RING_CAPACITY: usize = 64;

/// #3041 P1-3 R5 / P1-5: the EXACT, per-frame terminal resolution recorded for a
/// single terminal (result-bearing) frame's sequence. Distinct from the
/// high-water-mark fields (`last_terminal_committed/skipped_sequence`), which a
/// LATER, higher-sequence terminal can bump — this is keyed to ONE sequence so a
/// watcher resolves its ACK on ITS OWN terminal frame's outcome, decoupled from
/// any other turn sharing the same physical chunk.
///
/// #3041 P1-5: the CANONICAL cross-actor 3-way delivery outcome. The sink-local
/// enum stays 2-way (the sink always KNOWS: confirmed POST → `Delivered`;
/// deterministic decline → `NotDelivered`; failure → `Err`). `Unknown` is a
/// CROSS-ACTOR state that arises in the relay ring + watcher when the terminal
/// resolution cannot be confirmed (the watcher's ACK timed out / target was
/// missing / the frame was dropped or hit a sink error, or the sink POSTed but
/// could not confirm the commit). Both `NotDelivered` AND `Unknown` MUST flow
/// through the watcher's committed-offset reconciliation (§3.2) — there is NO
/// blind skip for `NotDelivered` and NO blind 10s re-send for `Unknown`.
///
/// A NEVER-recorded sequence reads back `None` (distinct from `Some(Unknown)`):
/// `None` means "not yet resolved" (keep waiting), `Some(Unknown)` is an explicit
/// "resolved but unconfirmed → reconcile NOW" signal the sink/watcher can record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryOutcome {
    Delivered,
    NotDelivered,
    Unknown,
}

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
    /// Routing snapshot captured when the frame was enqueued. Sinks must not
    /// re-parse `session_name` or consult mutable registry state to route
    /// already-queued frames.
    pub binding: MatchedChannel,
    /// Raw frame bytes (typically a JSONL line). Sink chooses formatting.
    pub payload: String,
    /// Monotonic sequence number assigned by the relay. Useful for sinks that
    /// want to detect drops / reorder.
    pub sequence: u64,
    /// #3041 P1-3 (Part a, B1 — frame-carried commit fence): for the
    /// RESULT-bearing (terminal) frame ONLY, the producer's AUTHORITATIVE
    /// consumed-terminal END offset (`terminal_event_consumed_offset(..)`) for
    /// the turn it is delegating to this sink. `None` on every non-terminal
    /// frame. The sink CANNOT derive this from the opaque payload, so it rides
    /// the frame and the sink advances `confirmed_end_offset` to it on a
    /// CONFIRMED terminal Discord delivery — identity-gated against the channel's
    /// current inflight so a delayed/wrong-turn frame can never advance.
    pub terminal_consumed_end: Option<u64>,
    /// #3041 P1-3 (Part a, B1): turn identity of the inflight the producer
    /// pinned when it forwarded the terminal frame (`user_msg_id`). Paired with
    /// `turn_started_at` it is the IDENTITY GATE: the sink advances the offset
    /// only when this still matches the channel's current inflight identity.
    /// `0` / empty on non-terminal frames (carries no commit data). Kept as
    /// minimal scalars (NOT the discord-side `InflightTurnIdentity`) so the
    /// cluster layer stays free of discord imports.
    pub turn_user_msg_id: u64,
    /// #3041 P1-3 (Part a, B1): the pinned inflight's `started_at` discriminator
    /// (external-input turns share `user_msg_id == 0`, so `started_at`
    /// distinguishes consecutive TUI-direct turns). Empty on non-terminal frames.
    pub turn_started_at: String,
    /// #3041 P1-3 (codex P1-3 issue 2 — identity collision close): the pinned
    /// inflight's `turn_start_offset` (the JSONL byte offset at which THIS turn
    /// began). `now_string` has 1-second resolution, so two back-to-back
    /// TUI-direct turns with `user_msg_id == 0` started in the SAME second share
    /// an identical `(0, started_at)` pair — a delayed OLD terminal frame would
    /// then pass the sink's identity gate for the NEW turn and wrongly advance.
    /// `turn_start_offset` is monotonic per turn (the next turn always begins at a
    /// strictly larger byte offset), so adding it to the IDENTITY GATE makes the
    /// frame identity UNIQUE per turn. Non-terminal frames may carry the same
    /// identity for backpressure attribution, but only frames with
    /// `terminal_consumed_end` can advance the sink commit fence.
    pub turn_start_offset: Option<u64>,
}

/// Per-session counters. Exposed via the supervisor for diagnostics.
#[derive(Debug, Default)]
pub struct RelayMetrics {
    pub frames_received: AtomicU64,
    pub frames_delivered: AtomicU64,
    pub terminal_commits: AtomicU64,
    pub terminal_skips: AtomicU64,
    pub dropped_frames: AtomicU64,
    pub sink_errors: AtomicU64,
    last_delivered_sequence_plus_one: AtomicU64,
    last_terminal_committed_sequence_plus_one: AtomicU64,
    last_terminal_skipped_sequence_plus_one: AtomicU64,
    last_dropped_sequence_plus_one: AtomicU64,
    last_sink_error_sequence_plus_one: AtomicU64,
    /// #3041 P1-3 R5: bounded ring of recently-resolved per-sequence terminal
    /// outcomes. ADDITIVE to the high-water-mark fields above (which other
    /// consumers — drop/diagnostics/tests — still read). Queried by the watcher
    /// for the EXACT `target.sequence` so B's tail committing at seq N+1 never
    /// satisfies A's ACK at seq N. Bounded FIFO (`TERMINAL_OUTCOME_RING_CAPACITY`):
    /// an evicted/never-recorded sequence reads back `None`.
    terminal_outcomes: Mutex<VecDeque<(u64, DeliveryOutcome)>>,
}

impl RelayMetrics {
    pub fn snapshot(&self) -> RelayMetricsSnapshot {
        RelayMetricsSnapshot {
            frames_received: self.frames_received.load(Ordering::Acquire),
            frames_delivered: self.frames_delivered.load(Ordering::Acquire),
            terminal_commits: self.terminal_commits.load(Ordering::Acquire),
            terminal_skips: self.terminal_skips.load(Ordering::Acquire),
            dropped_frames: self.dropped_frames.load(Ordering::Acquire),
            sink_errors: self.sink_errors.load(Ordering::Acquire),
            last_delivered_sequence: decode_sequence_marker(
                self.last_delivered_sequence_plus_one
                    .load(Ordering::Acquire),
            ),
            last_terminal_committed_sequence: decode_sequence_marker(
                self.last_terminal_committed_sequence_plus_one
                    .load(Ordering::Acquire),
            ),
            last_terminal_skipped_sequence: decode_sequence_marker(
                self.last_terminal_skipped_sequence_plus_one
                    .load(Ordering::Acquire),
            ),
            last_dropped_sequence: decode_sequence_marker(
                self.last_dropped_sequence_plus_one.load(Ordering::Acquire),
            ),
            last_sink_error_sequence: decode_sequence_marker(
                self.last_sink_error_sequence_plus_one
                    .load(Ordering::Acquire),
            ),
        }
    }

    /// #3041 P1-3 R5 / P1-5: record THIS terminal frame's resolved outcome keyed
    /// by its exact sequence, in a bounded FIFO ring. Called from `deliver_frame`
    /// for a result-bearing terminal delivery (`Delivered`/`NotDelivered`) and may
    /// also carry an explicit cross-actor `Unknown` (sink POSTed but could not
    /// confirm the commit) so the watcher reconciles immediately instead of waiting
    /// for the 10s ACK timeout. Idempotent-ish: a re-record of the same sequence
    /// overwrites the prior entry in place (a sequence is delivered once, but this
    /// keeps the ring free of duplicates).
    pub(crate) fn record_terminal_outcome(&self, sequence: u64, outcome: DeliveryOutcome) {
        let mut ring = self
            .terminal_outcomes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(existing) = ring.iter_mut().find(|(seq, _)| *seq == sequence) {
            existing.1 = outcome;
            return;
        }
        ring.push_back((sequence, outcome));
        while ring.len() > TERMINAL_OUTCOME_RING_CAPACITY {
            ring.pop_front();
        }
    }

    /// #3041 P1-3 R5: the EXACT-sequence terminal-ACK query. Returns this
    /// sequence's recorded outcome, or `None` if it was never terminally
    /// resolved on this frame (non-terminal / dropped / evicted from the ring).
    /// The watcher resolves its ACK on its OWN terminal frame's sequence via this
    /// — NOT the `>=` high-water-mark — so a co-chunked higher-sequence terminal
    /// can never satisfy it.
    pub fn terminal_outcome_for_sequence(&self, sequence: u64) -> Option<DeliveryOutcome> {
        let ring = self
            .terminal_outcomes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        ring.iter()
            .find(|(seq, _)| *seq == sequence)
            .map(|(_, outcome)| *outcome)
    }

    #[cfg(test)]
    pub(crate) fn record_delivered_sequence_for_test(&self, sequence: u64) {
        self.last_delivered_sequence_plus_one
            .fetch_max(encode_sequence_marker(sequence), Ordering::AcqRel);
    }

    #[cfg(test)]
    pub(crate) fn record_terminal_committed_sequence_for_test(&self, sequence: u64) {
        self.last_terminal_committed_sequence_plus_one
            .fetch_max(encode_sequence_marker(sequence), Ordering::AcqRel);
        self.record_terminal_outcome(sequence, DeliveryOutcome::Delivered);
    }

    #[cfg(test)]
    pub(crate) fn record_terminal_skipped_sequence_for_test(&self, sequence: u64) {
        self.last_terminal_skipped_sequence_plus_one
            .fetch_max(encode_sequence_marker(sequence), Ordering::AcqRel);
        self.record_terminal_outcome(sequence, DeliveryOutcome::NotDelivered);
    }

    #[cfg(test)]
    pub(crate) fn record_dropped_sequence_for_test(&self, sequence: u64) {
        self.last_dropped_sequence_plus_one
            .fetch_max(encode_sequence_marker(sequence), Ordering::AcqRel);
    }

    #[cfg(test)]
    pub(crate) fn record_sink_error_sequence_for_test(&self, sequence: u64) {
        self.last_sink_error_sequence_plus_one
            .fetch_max(encode_sequence_marker(sequence), Ordering::AcqRel);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RelayMetricsSnapshot {
    pub frames_received: u64,
    pub frames_delivered: u64,
    pub terminal_commits: u64,
    pub terminal_skips: u64,
    pub dropped_frames: u64,
    pub sink_errors: u64,
    pub last_delivered_sequence: Option<u64>,
    pub last_terminal_committed_sequence: Option<u64>,
    pub last_terminal_skipped_sequence: Option<u64>,
    pub last_dropped_sequence: Option<u64>,
    pub last_sink_error_sequence: Option<u64>,
}

fn encode_sequence_marker(sequence: u64) -> u64 {
    sequence.saturating_add(1)
}

fn decode_sequence_marker(marker: u64) -> Option<u64> {
    marker.checked_sub(1)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelaySendOutcome {
    pub sequence: Option<u64>,
    pub dropped_oldest: Option<RelayDroppedFrame>,
    pub closed: bool,
}

impl RelaySendOutcome {
    fn enqueued(sequence: u64, dropped_oldest: Option<RelayDroppedFrame>) -> Self {
        Self {
            sequence: Some(sequence),
            dropped_oldest,
            closed: false,
        }
    }

    fn closed() -> Self {
        Self {
            sequence: None,
            dropped_oldest: None,
            closed: true,
        }
    }

    pub fn is_alive(&self) -> bool {
        !self.closed
    }
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
    async fn deliver(&self, frame: &StreamFrame) -> Result<RelaySinkOutcome, RelaySinkError>;
}

/// Result of accepting a relay frame into the sink. This deliberately
/// distinguishes "the sink accepted/parsing-counted this frame" from "a
/// terminal Discord response was actually committed". Watchers must use only
/// `TerminalDelivered` as delegation success.
///
/// #3041 P1-5: the three terminal variants mirror the cross-actor 3-way
/// `DeliveryOutcome`. `TerminalDelivered` (confirmed POST/edit) → the watcher
/// treats the turn as delivered. `TerminalNotDelivered` (a deterministic
/// route-decision decline — foreign-owner lease block, or bridge-owned /
/// mismatched inflight) and `TerminalUnknown` (the sink POSTed but could not
/// confirm the commit) BOTH route the watcher through committed-offset
/// reconciliation (§3.2): NO blind skip for `NotDelivered`, NO blind re-send for
/// `Unknown`. The session-bound sink itself emits only `Delivered`/`NotDelivered`
/// (it always knows); `TerminalUnknown` is reserved for a future/optional sink
/// site that POSTs-without-confirm and for cross-actor symmetry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelaySinkOutcome {
    FrameAccepted,
    TerminalDelivered,
    TerminalNotDelivered,
    // #3041 P1-5: reserved for a future POST-without-confirm sink + cross-actor
    // symmetry (see enum doc); the session-bound sink never emits it today.
    #[allow(dead_code)]
    TerminalUnknown,
}

impl RelaySinkOutcome {
    pub fn terminal_delivered(self) -> bool {
        matches!(self, Self::TerminalDelivered)
    }

    pub fn terminal_not_delivered(self) -> bool {
        matches!(self, Self::TerminalNotDelivered)
    }

    pub fn terminal_unknown(self) -> bool {
        matches!(self, Self::TerminalUnknown)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RelaySinkError {
    #[error("transient sink failure: {0}")]
    Transient(String),
    // Documented permanent-failure arm of the sink error taxonomy; no sink site
    // produces it yet but it is part of the public RelaySinkError contract.
    #[allow(dead_code)]
    #[error("permanent sink failure: {0}")]
    Permanent(String),
}

/// No-op sink used when the supervisor is wired without a real Discord
/// adapter (e.g. flag enabled but the migration hasn't landed E4 yet, or
/// unit tests that don't care about delivery semantics).
#[allow(dead_code)] // no-op sink; exercised only by #[cfg(test)] tests
pub struct DiscardSink;

#[async_trait]
impl RelaySink for DiscardSink {
    async fn deliver(&self, _frame: &StreamFrame) -> Result<RelaySinkOutcome, RelaySinkError> {
        Ok(RelaySinkOutcome::FrameAccepted)
    }
}

/// Handle returned by [`spawn_stream_relay`]. The supervisor holds one of
/// these per active session and uses [`Self::shutdown`] when the session
/// disappears from the [`super::session_registry::SessionRegistry`].
pub struct StreamRelayHandle {
    matched: MatchedChannel,
    queue: Arc<RelayFrameQueue>,
    shutdown: Arc<AtomicBool>,
    /// Receiver-side cancellation. The relay loop selects on this alongside
    /// `rx.recv()` so a shutdown forces the loop to exit even if cached
    /// [`RelayProducer`] clones (held outside the supervisor's
    /// `StreamRelayHandle`, e.g. in the tmux watcher) are still alive — they
    /// would otherwise keep the queue alive and wedge `recv().await` forever.
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
/// underlying queue, `shutdown`, `metrics`, and `sequence` atomics — so a
/// shutdown initiated by the supervisor is immediately visible to every
/// producer attempt (E5 #2412).
#[derive(Clone)]
pub struct RelayProducer {
    matched: MatchedChannel,
    queue: Arc<RelayFrameQueue>,
    shutdown: Arc<AtomicBool>,
    metrics: Arc<RelayMetrics>,
    sequence: Arc<AtomicU64>,
}

impl RelayProducer {
    pub fn session_name(&self) -> &str {
        &self.matched.expected_session_name
    }

    pub fn metrics(&self) -> &Arc<RelayMetrics> {
        &self.metrics
    }

    pub fn is_alive(&self) -> bool {
        !self.shutdown.load(Ordering::Acquire)
    }

    /// Non-blocking enqueue. See [`StreamRelayHandle::try_send_frame`] for
    /// the contract (drop-on-full, returns `false` only when the relay task
    /// has exited). Callers in production must always go through this entry
    /// point — the producer registry hands out clones of this struct.
    pub fn try_send_frame(&self, payload: String) -> bool {
        self.try_send_frame_with_sequence(payload).is_alive()
    }

    /// Non-blocking enqueue with the assigned relay sequence. Watchers use
    /// the sequence to distinguish "handed to relay" from "Discord sink has
    /// actually accepted the terminal frame" before clearing inflight state.
    pub fn try_send_frame_with_sequence(&self, payload: String) -> RelaySendOutcome {
        self.try_send_frame_with_sequence_and_identity(payload, None)
    }

    /// Non-blocking enqueue for a non-terminal frame with optional turn identity.
    /// The identity is inert for sink commits (`terminal_consumed_end` stays None)
    /// but lets a later backpressure eviction degrade the affected turn's mirror
    /// state instead of reporting a false fully-mirrored path.
    pub fn try_send_frame_with_sequence_and_identity(
        &self,
        payload: String,
        frame_identity: Option<RelayTurnIdentity>,
    ) -> RelaySendOutcome {
        try_send_frame_inner(
            &self.matched,
            &self.queue,
            &self.shutdown,
            &self.metrics,
            &self.sequence,
            payload,
            None,
            frame_identity,
        )
    }

    /// #3041 P1-3 (Part a, B1): forward the RESULT-bearing chunk as a terminal
    /// frame carrying the commit fence (`terminal.consumed_end` + the pinned turn
    /// identity). The frame both triggers the sink's terminal delivery AND is the
    /// unit the sink consumes, so the commit data MUST ride on it (a separate
    /// later frame would arrive after the FIFO sink already dispatched delivery).
    pub fn try_send_terminal_frame_with_sequence(
        &self,
        payload: String,
        terminal: TerminalCommitFence,
    ) -> RelaySendOutcome {
        try_send_frame_inner(
            &self.matched,
            &self.queue,
            &self.shutdown,
            &self.metrics,
            &self.sequence,
            payload,
            Some(terminal),
            None,
        )
    }
}

impl std::fmt::Debug for RelayProducer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayProducer")
            .field("session", &self.matched.expected_session_name)
            .field("metrics", &self.metrics.snapshot())
            .finish()
    }
}

enum QueuePushResult {
    Enqueued,
    DroppedOldest(RelayDroppedFrame),
    Closed,
}

#[allow(clippy::large_enum_variant)]
enum QueuePopResult {
    Frame(StreamFrame),
    Empty,
    Closed,
}

struct RelayFrameQueue {
    inner: Mutex<RelayFrameQueueInner>,
    notify: Notify,
    capacity: usize,
}

#[derive(Default)]
struct RelayFrameQueueInner {
    frames: VecDeque<StreamFrame>,
    closed: bool,
}

impl RelayFrameQueue {
    fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(RelayFrameQueueInner::default()),
            notify: Notify::new(),
            capacity: capacity.max(1),
        }
    }

    fn push_drop_oldest(&self, frame: StreamFrame) -> QueuePushResult {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if inner.closed {
            return QueuePushResult::Closed;
        }
        let dropped = if inner.frames.len() >= self.capacity {
            inner.frames.pop_front().map(RelayDroppedFrame::from_frame)
        } else {
            None
        };
        inner.frames.push_back(frame);
        drop(inner);
        self.notify.notify_one();
        if let Some(frame) = dropped {
            QueuePushResult::DroppedOldest(frame)
        } else {
            QueuePushResult::Enqueued
        }
    }

    fn pop_or_state(&self) -> QueuePopResult {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(frame) = inner.frames.pop_front() {
            QueuePopResult::Frame(frame)
        } else if inner.closed {
            QueuePopResult::Closed
        } else {
            QueuePopResult::Empty
        }
    }

    async fn recv(&self) -> Option<StreamFrame> {
        loop {
            match self.pop_or_state() {
                QueuePopResult::Frame(frame) => return Some(frame),
                QueuePopResult::Closed => return None,
                QueuePopResult::Empty => self.notify.notified().await,
            }
        }
    }

    fn try_pop(&self) -> Option<StreamFrame> {
        match self.pop_or_state() {
            QueuePopResult::Frame(frame) => Some(frame),
            QueuePopResult::Empty | QueuePopResult::Closed => None,
        }
    }

    fn close(&self) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.closed = true;
        drop(inner);
        self.notify.notify_waiters();
    }
}

/// #3041 P1-3 (Part a, B1): the commit-fence data the producer rides on the
/// RESULT-bearing frame. The watcher computes the authoritative consumed-terminal
/// `end` and pins the delegating turn's identity (from the inflight loaded BEFORE
/// the relay, matching #3141 pinned-id semantics) so the sink can advance the
/// offset authority identity-gated on a confirmed delivery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalCommitFence {
    pub consumed_end: u64,
    pub turn_user_msg_id: u64,
    pub turn_started_at: String,
    /// #3041 P1-3 (codex P1-3 issue 2): the turn's `turn_start_offset` — added to
    /// the sink's identity gate so two consecutive `user_msg_id == 0` turns started
    /// in the same `now_string` second (identical `started_at`) cannot collide.
    pub turn_start_offset: Option<u64>,
}

#[allow(clippy::too_many_arguments)]
fn try_send_frame_inner(
    matched: &MatchedChannel,
    queue: &Arc<RelayFrameQueue>,
    shutdown: &Arc<AtomicBool>,
    metrics: &Arc<RelayMetrics>,
    sequence: &Arc<AtomicU64>,
    payload: String,
    terminal: Option<TerminalCommitFence>,
    frame_identity: Option<RelayTurnIdentity>,
) -> RelaySendOutcome {
    if shutdown.load(Ordering::Acquire) {
        return RelaySendOutcome::closed();
    }
    let seq = sequence.fetch_add(1, Ordering::AcqRel);
    let (terminal_consumed_end, frame_identity) = match terminal {
        Some(fence) => (
            Some(fence.consumed_end),
            RelayTurnIdentity {
                turn_user_msg_id: fence.turn_user_msg_id,
                turn_started_at: fence.turn_started_at,
                turn_start_offset: fence.turn_start_offset,
            },
        ),
        None => (None, frame_identity.unwrap_or_default()),
    };
    let frame = StreamFrame {
        session_name: matched.expected_session_name.clone(),
        binding: matched.clone(),
        payload,
        sequence: seq,
        terminal_consumed_end,
        turn_user_msg_id: frame_identity.turn_user_msg_id,
        turn_started_at: frame_identity.turn_started_at,
        turn_start_offset: frame_identity.turn_start_offset,
    };
    metrics.frames_received.fetch_add(1, Ordering::AcqRel);
    match queue.push_drop_oldest(frame) {
        QueuePushResult::Enqueued => RelaySendOutcome::enqueued(seq, None),
        QueuePushResult::DroppedOldest(dropped) => {
            metrics.dropped_frames.fetch_add(1, Ordering::AcqRel);
            metrics
                .last_dropped_sequence_plus_one
                .fetch_max(encode_sequence_marker(dropped.sequence), Ordering::AcqRel);
            RelaySendOutcome::enqueued(seq, Some(dropped))
        }
        QueuePushResult::Closed => RelaySendOutcome::closed(),
    }
}

impl StreamRelayHandle {
    pub fn matched(&self) -> &MatchedChannel {
        &self.matched
    }

    #[allow(dead_code)] // diagnostic accessor; exercised only by #[cfg(test)] tests
    pub fn metrics(&self) -> &Arc<RelayMetrics> {
        &self.metrics
    }

    /// Return a clonable [`RelayProducer`] sharing the relay's underlying
    /// queue and atomics. Used by E5 (#2412) to expose `try_send_frame` to
    /// the production tmux frame producer via
    /// [`super::relay_producer_registry::RelayProducerRegistry`].
    pub fn producer(&self) -> RelayProducer {
        RelayProducer {
            matched: self.matched.clone(),
            queue: self.queue.clone(),
            shutdown: self.shutdown.clone(),
            metrics: self.metrics.clone(),
            sequence: self.sequence.clone(),
        }
    }

    /// Non-blocking enqueue. If the queue is full, the oldest queued frame
    /// is dropped (we drain one then enqueue) and the dropped counter
    /// increments. Returns `false` only if the relay task has already exited
    /// — the upstream caller should then treat the relay as dead.
    #[allow(dead_code)] // handle-side sender; exercised only by #[cfg(test)] tests (prod uses RelayProducer)
    pub fn try_send_frame(&self, payload: String) -> bool {
        self.try_send_frame_with_sequence(payload).is_alive()
    }

    /// Test/diagnostic variant of [`Self::try_send_frame`] that exposes the
    /// assigned sequence for delivery-ack logic.
    #[allow(dead_code)] // test/diagnostic sender; exercised only by #[cfg(test)] tests
    pub fn try_send_frame_with_sequence(&self, payload: String) -> RelaySendOutcome {
        try_send_frame_inner(
            &self.matched,
            &self.queue,
            &self.shutdown,
            &self.metrics,
            &self.sequence,
            payload,
            None,
            None,
        )
    }

    /// Initiate graceful shutdown. Sets the shutdown flag, fires the
    /// receiver-side notify so the relay loop exits even when sender clones
    /// outside this handle (E5 #2412: `RelayProducer` clones cached by the
    /// production tmux watcher) keep producer clones alive, then closes the
    /// supervisor-owned queue and awaits task completion.
    ///
    /// Without the notify, an idle relay could remain parked in `recv().await`
    /// while cached producer clones in tmux watchers stayed alive. That wedge
    /// motivated the explicit receiver-side cancellation.
    /// Safe to call only once — the handle is consumed.
    pub async fn shutdown(self) {
        let StreamRelayHandle {
            queue,
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
    let queue = Arc::new(RelayFrameQueue::new(buffer));
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_notify = Arc::new(Notify::new());
    let metrics = Arc::new(RelayMetrics::default());

    let session_name = matched.expected_session_name.clone();
    let channel_id = matched.channel_id.clone();
    let task_metrics = metrics.clone();
    let task_shutdown = shutdown.clone();
    let task_shutdown_notify = shutdown_notify.clone();
    let task_queue = queue.clone();

    let task = tokio::spawn(async move {
        run_relay_loop(
            task_queue,
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
        queue,
        shutdown,
        shutdown_notify,
        metrics,
        sequence: Arc::new(AtomicU64::new(0)),
        task: Some(task),
    }
}

async fn run_relay_loop(
    queue: Arc<RelayFrameQueue>,
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
            drain_pending(&queue, &sink, &metrics, &session_name).await;
            break;
        }
        tokio::select! {
            // Receiver-side cancellation. `notify_one` stores a single
            // permit so the wakeup survives even when it lands before
            // `notified()` is armed (closes the pre-waiter race that
            // `notify_waiters` would have lost). Selecting on it makes the
            // loop exit even when external sender clones (E5 #2412 cached
            // `RelayProducer` clones in `tmux_watcher`) stay alive.
            _ = shutdown_notify.notified() => {
                tracing::debug!(
                    session = %session_name,
                    "stream-relay observed shutdown notify; draining and exiting"
                );
                drain_pending(&queue, &sink, &metrics, &session_name).await;
                break;
            }
            maybe_frame = queue.recv() => {
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
                    drain_pending(&queue, &sink, &metrics, &session_name).await;
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
    queue: &Arc<RelayFrameQueue>,
    sink: &Arc<dyn RelaySink>,
    metrics: &Arc<RelayMetrics>,
    session_name: &str,
) {
    while let Some(extra) = queue.try_pop() {
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
        Ok(outcome) => {
            metrics.frames_delivered.fetch_add(1, Ordering::AcqRel);
            metrics
                .last_delivered_sequence_plus_one
                .fetch_max(encode_sequence_marker(frame.sequence), Ordering::AcqRel);
            if outcome.terminal_delivered() {
                metrics.terminal_commits.fetch_add(1, Ordering::AcqRel);
                metrics
                    .last_terminal_committed_sequence_plus_one
                    .fetch_max(encode_sequence_marker(frame.sequence), Ordering::AcqRel);
                // #3041 P1-3 R5: record THIS frame's exact-sequence outcome so the
                // watcher resolves its ACK on its own terminal frame (decoupled
                // from any co-chunked higher-sequence terminal).
                metrics.record_terminal_outcome(frame.sequence, DeliveryOutcome::Delivered);
            }
            if outcome.terminal_not_delivered() {
                metrics.terminal_skips.fetch_add(1, Ordering::AcqRel);
                metrics
                    .last_terminal_skipped_sequence_plus_one
                    .fetch_max(encode_sequence_marker(frame.sequence), Ordering::AcqRel);
                // #3041 P1-5: a deterministic route-decision decline — recorded as
                // `NotDelivered` so the watcher reconciles against the committed
                // offset (§3.2 SendFull-or-Skip), never a blind skip.
                metrics.record_terminal_outcome(frame.sequence, DeliveryOutcome::NotDelivered);
            }
            if outcome.terminal_unknown() {
                // #3041 P1-5: the sink POSTed but could not confirm the commit. We
                // do NOT bump the commit/skip high-water-marks (the outcome is, by
                // definition, unconfirmed) but DO record the per-sequence `Unknown`
                // so the watcher's per-sequence ACK resolves immediately to
                // committed-offset reconciliation instead of waiting out the 10s
                // ACK timeout. Both `NotDelivered` and `Unknown` flow through §3.2.
                metrics.record_terminal_outcome(frame.sequence, DeliveryOutcome::Unknown);
            }
        }
        Err(error) => {
            metrics.sink_errors.fetch_add(1, Ordering::AcqRel);
            metrics
                .last_sink_error_sequence_plus_one
                .fetch_max(encode_sequence_marker(frame.sequence), Ordering::AcqRel);
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
        async fn deliver(&self, frame: &StreamFrame) -> Result<RelaySinkOutcome, RelaySinkError> {
            if self.fail_next.swap(false, Ordering::AcqRel) {
                return Err(RelaySinkError::Transient("forced".into()));
            }
            self.frames.lock().unwrap().push(frame.clone());
            Ok(RelaySinkOutcome::FrameAccepted)
        }
    }

    struct BlockingSequenceSink {
        first_started: tokio::sync::Notify,
        unblock: tokio::sync::Notify,
        block_first: AtomicBool,
    }

    #[async_trait]
    impl RelaySink for BlockingSequenceSink {
        async fn deliver(&self, _frame: &StreamFrame) -> Result<RelaySinkOutcome, RelaySinkError> {
            if self.block_first.swap(false, Ordering::AcqRel) {
                self.first_started.notify_one();
                self.unblock.notified().await;
            }
            Ok(RelaySinkOutcome::FrameAccepted)
        }
    }

    struct SequenceOutcomeSink;

    #[async_trait]
    impl RelaySink for SequenceOutcomeSink {
        async fn deliver(&self, frame: &StreamFrame) -> Result<RelaySinkOutcome, RelaySinkError> {
            Ok(match frame.sequence {
                0 => RelaySinkOutcome::FrameAccepted,
                1 => RelaySinkOutcome::TerminalNotDelivered,
                _ => RelaySinkOutcome::TerminalDelivered,
            })
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
        // Yield enough times for the relay task to drain the queue under
        // the current-thread runtime used by `#[tokio::test]`. A few yields
        // is more reliable than a sleep across CI hosts.
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
    }

    async fn wait_until<F: FnMut() -> bool>(mut cond: F, label: &str) {
        for _ in 0..200 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for: {label}");
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
            assert_eq!(&frame.binding, handle.matched());
        }
        assert_eq!(handle.metrics().snapshot().frames_delivered, 5);
        assert_eq!(handle.metrics().snapshot().dropped_frames, 0);
        handle.shutdown().await;
    }

    // #3041 P1-3 (Part a, B1): the terminal frame carries the commit fence
    // (`terminal_consumed_end` + turn identity); non-terminal frames carry None/0/"".
    #[tokio::test]
    async fn terminal_frame_carries_consumed_end_and_turn_identity() {
        let sink = Arc::new(CapturingSink::default());
        let handle = spawn_stream_relay(matched_for("c-fence"), sink.clone());

        // A normal (non-terminal) frame: no commit fence.
        assert!(handle.try_send_frame("non-terminal".into()));
        // The producer-side terminal send (the watcher uses the RelayProducer
        // clone; the handle exposes the same path for tests).
        let producer = handle.producer();
        let outcome = producer.try_send_terminal_frame_with_sequence(
            "result-bearing".into(),
            TerminalCommitFence {
                consumed_end: 512,
                turn_user_msg_id: 77,
                turn_started_at: "2026-06-04T00:00:00Z".to_string(),
                turn_start_offset: Some(64),
            },
        );
        assert!(outcome.is_alive());

        flush_pending().await;
        let delivered = sink.delivered();
        assert_eq!(delivered.len(), 2);

        // Non-terminal frame carries no fence.
        assert_eq!(delivered[0].payload, "non-terminal");
        assert_eq!(delivered[0].terminal_consumed_end, None);
        assert_eq!(delivered[0].turn_user_msg_id, 0);
        assert_eq!(delivered[0].turn_started_at, "");
        assert_eq!(delivered[0].turn_start_offset, None);

        // Terminal frame carries the consumed_end + pinned identity.
        assert_eq!(delivered[1].payload, "result-bearing");
        assert_eq!(delivered[1].terminal_consumed_end, Some(512));
        assert_eq!(delivered[1].turn_user_msg_id, 77);
        assert_eq!(delivered[1].turn_started_at, "2026-06-04T00:00:00Z");
        assert_eq!(delivered[1].turn_start_offset, Some(64));

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn backpressure_eviction_outcome_carries_victim_turn_identity() {
        let blocking_sink = Arc::new(BlockingSequenceSink {
            first_started: tokio::sync::Notify::new(),
            unblock: tokio::sync::Notify::new(),
            block_first: AtomicBool::new(true),
        });
        let handle = spawn_stream_relay_with_buffer(
            matched_for("c-evicted-identity"),
            blocking_sink.clone() as Arc<dyn RelaySink>,
            1,
        );

        assert_eq!(
            handle
                .try_send_frame_with_sequence("blocked-in-sink".into())
                .sequence,
            Some(0)
        );
        tokio::time::timeout(
            Duration::from_secs(1),
            blocking_sink.first_started.notified(),
        )
        .await
        .expect("first frame reaches blocked sink");

        let producer = handle.producer();
        let victim = producer.try_send_frame_with_sequence_and_identity(
            "victim".into(),
            Some(RelayTurnIdentity {
                turn_user_msg_id: 77,
                turn_started_at: "2026-06-04T00:00:00Z".to_string(),
                turn_start_offset: Some(64),
            }),
        );
        let newest = producer.try_send_frame_with_sequence_and_identity(
            "newest".into(),
            Some(RelayTurnIdentity {
                turn_user_msg_id: 88,
                turn_started_at: "2026-06-04T00:00:01Z".to_string(),
                turn_start_offset: Some(128),
            }),
        );

        assert_eq!(victim.sequence, Some(1));
        assert_eq!(newest.sequence, Some(2));
        let dropped = newest
            .dropped_oldest
            .as_ref()
            .expect("overflow should report the evicted victim frame");
        assert_eq!(dropped.sequence, 1);
        assert_eq!(dropped.turn_identity.turn_user_msg_id, 77);
        assert_eq!(
            dropped.turn_identity.turn_started_at.as_str(),
            "2026-06-04T00:00:00Z"
        );
        assert_eq!(dropped.turn_identity.turn_start_offset, Some(64));

        blocking_sink.unblock.notify_waiters();
        wait_until(
            || handle.metrics().snapshot().last_delivered_sequence == Some(2),
            "newest frame delivered",
        )
        .await;
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn sequence_outcome_tracks_delivery_error_and_drop_sequences() {
        let sink = Arc::new(CapturingSink::default());
        sink.fail_next.store(true, Ordering::Release);
        let handle = spawn_stream_relay_with_buffer(matched_for("c-seq"), sink.clone(), 2);

        let first = handle.try_send_frame_with_sequence("will fail".into());
        let second = handle.try_send_frame_with_sequence("will deliver".into());
        assert_eq!(first.sequence, Some(0));
        assert_eq!(second.sequence, Some(1));
        wait_until(
            || handle.metrics().snapshot().last_delivered_sequence == Some(1),
            "second frame delivered",
        )
        .await;

        let snap = handle.metrics().snapshot();
        assert_eq!(snap.last_sink_error_sequence, Some(0));
        assert_eq!(snap.last_delivered_sequence, Some(1));
        handle.shutdown().await;

        let blocking_sink = Arc::new(BlockingSequenceSink {
            first_started: tokio::sync::Notify::new(),
            unblock: tokio::sync::Notify::new(),
            block_first: AtomicBool::new(true),
        });
        let drop_handle = spawn_stream_relay_with_buffer(
            matched_for("c-seq-drop"),
            blocking_sink.clone() as Arc<dyn RelaySink>,
            1,
        );
        assert_eq!(
            drop_handle
                .try_send_frame_with_sequence("blocked".into())
                .sequence,
            Some(0)
        );
        tokio::time::timeout(
            Duration::from_secs(1),
            blocking_sink.first_started.notified(),
        )
        .await
        .expect("first frame reaches blocked sink");
        let queued = drop_handle.try_send_frame_with_sequence("queued".into());
        let newest = drop_handle.try_send_frame_with_sequence("newest".into());
        assert_eq!(queued.sequence, Some(1));
        assert_eq!(newest.sequence, Some(2));
        assert_eq!(
            newest.dropped_oldest.as_ref().map(|frame| frame.sequence),
            Some(1)
        );
        assert_eq!(
            drop_handle.metrics().snapshot().last_dropped_sequence,
            Some(1)
        );
        blocking_sink.unblock.notify_waiters();
        wait_until(
            || drop_handle.metrics().snapshot().last_delivered_sequence == Some(2),
            "newest frame delivered",
        )
        .await;
        drop_handle.shutdown().await;
    }

    #[tokio::test]
    async fn terminal_outcomes_track_committed_and_skipped_sequences_separately() {
        let sink = Arc::new(SequenceOutcomeSink);
        let handle = spawn_stream_relay(matched_for("c-terminal-outcome"), sink);

        assert!(handle.try_send_frame("frame accepted".into()));
        assert!(handle.try_send_frame("terminal skipped".into()));
        assert!(handle.try_send_frame("terminal committed".into()));
        wait_until(
            || handle.metrics().snapshot().last_terminal_committed_sequence == Some(2),
            "terminal commit sequence",
        )
        .await;

        let snap = handle.metrics().snapshot();
        assert_eq!(snap.frames_delivered, 3);
        assert_eq!(snap.terminal_skips, 1);
        assert_eq!(snap.terminal_commits, 1);
        assert_eq!(snap.last_delivered_sequence, Some(2));
        assert_eq!(snap.last_terminal_skipped_sequence, Some(1));
        assert_eq!(snap.last_terminal_committed_sequence, Some(2));

        // #3041 P1-3 R5: per-sequence query resolves each frame's OWN exact
        // outcome, independent of the high-water-mark. seq 1 was skipped, seq 2
        // committed, seq 0 was a non-terminal accept (no terminal outcome).
        let metrics = handle.metrics();
        assert_eq!(
            metrics.terminal_outcome_for_sequence(1),
            Some(DeliveryOutcome::NotDelivered)
        );
        assert_eq!(
            metrics.terminal_outcome_for_sequence(2),
            Some(DeliveryOutcome::Delivered)
        );
        assert_eq!(metrics.terminal_outcome_for_sequence(0), None);
        // A sequence that was never terminally resolved reads None.
        assert_eq!(metrics.terminal_outcome_for_sequence(99), None);
        handle.shutdown().await;
    }

    // #3041 P1-3 R5: the load-bearing invariant — outcome[N] is INDEPENDENT of
    // outcome[N+1]. Turn A (seq N) SKIPPED + turn B (seq N+1) COMMITTED sharing a
    // chunk: A's per-sequence query reads Skipped (NOT Delivered from the bumped
    // high-water-mark), so the watcher reconciles A → no black-hole.
    #[test]
    fn per_sequence_terminal_outcome_is_independent_of_higher_sequences() {
        let metrics = RelayMetrics::default();
        // A at seq 10 was SKIPPED; B's tail at seq 11 COMMITTED (higher sequence).
        metrics.record_terminal_outcome(10, DeliveryOutcome::NotDelivered);
        metrics.record_terminal_outcome(11, DeliveryOutcome::Delivered);
        // High-water-mark for committed is now 11 (would falsely satisfy `>= 10`).
        assert_eq!(
            metrics.terminal_outcome_for_sequence(10),
            Some(DeliveryOutcome::NotDelivered),
            "A's ACK must read A's OWN outcome (NotDelivered), not B's delivered high-water-mark"
        );
        assert_eq!(
            metrics.terminal_outcome_for_sequence(11),
            Some(DeliveryOutcome::Delivered)
        );
    }

    // #3041 P1-3 R5: the ring is bounded; an evicted sequence reads back None
    // (treated by the watcher as "not yet resolved" → it waits / times out →
    // reconciles, never a false ACK).
    #[test]
    fn terminal_outcome_ring_is_bounded_and_evicts_oldest() {
        let metrics = RelayMetrics::default();
        let total = TERMINAL_OUTCOME_RING_CAPACITY as u64 + 5;
        for seq in 0..total {
            metrics.record_terminal_outcome(seq, DeliveryOutcome::Delivered);
        }
        // The oldest 5 were evicted.
        for seq in 0..5 {
            assert_eq!(
                metrics.terminal_outcome_for_sequence(seq),
                None,
                "evicted oldest sequence reads None"
            );
        }
        // The most recent CAPACITY are retained.
        for seq in 5..total {
            assert_eq!(
                metrics.terminal_outcome_for_sequence(seq),
                Some(DeliveryOutcome::Delivered)
            );
        }
    }

    // #3041 P1-5: the ring carries the cross-actor `Unknown` outcome and an
    // explicit `Some(Unknown)` is DISTINCT from a never-recorded `None`. A sink
    // that POSTed-without-confirming (or the watcher's reconcile path) records
    // `Unknown` so the watcher reconciles immediately instead of waiting out the
    // 10s ACK timeout — while a sequence that was never resolved still reads
    // `None` ("keep waiting"). This pins the additive third variant.
    #[test]
    fn terminal_outcome_ring_carries_unknown() {
        let metrics = RelayMetrics::default();
        metrics.record_terminal_outcome(7, DeliveryOutcome::Unknown);
        assert_eq!(
            metrics.terminal_outcome_for_sequence(7),
            Some(DeliveryOutcome::Unknown),
            "Unknown is recorded and read back as an explicit cross-actor outcome"
        );
        assert_ne!(
            metrics.terminal_outcome_for_sequence(7),
            None,
            "Some(Unknown) (resolved-but-unconfirmed) is DISTINCT from None (not yet resolved)"
        );
        // A never-recorded sequence still reads None ("keep waiting"), not Unknown.
        assert_eq!(metrics.terminal_outcome_for_sequence(8), None);
    }

    #[tokio::test]
    async fn frames_carry_full_binding_snapshot_without_reparsing_session_name() {
        let sink = Arc::new(CapturingSink::default());
        let mut m = matched_for("short-session-key");
        m.channel_id = "1234567890123456789012345678901234567890-full-channel".to_string();
        m.agent_id = "agent-with-full-routing-identity".to_string();

        let handle = spawn_stream_relay(m.clone(), sink.clone());
        assert!(handle.try_send_frame("bound".into()));
        wait_until(|| sink.delivered().len() == 1, "binding snapshot delivered").await;

        let delivered = sink.delivered();
        assert_eq!(delivered[0].session_name, m.expected_session_name);
        assert_eq!(delivered[0].binding, m);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn queued_frames_keep_their_binding_across_rebinds() {
        let sink = Arc::new(CapturingSink::default());
        let old = matched_for("c-rebind");
        let mut rebound = old.clone();
        rebound.channel_id = "different-discord-channel-after-rebind".to_string();
        rebound.agent_id = "different-agent-after-rebind".to_string();

        let old_handle = spawn_stream_relay(old.clone(), sink.clone());
        assert!(old_handle.try_send_frame("old-binding".into()));
        wait_until(|| sink.delivered().len() == 1, "old binding delivered").await;
        old_handle.shutdown().await;

        let new_handle = spawn_stream_relay(rebound.clone(), sink.clone());
        assert!(new_handle.try_send_frame("new-binding".into()));
        wait_until(|| sink.delivered().len() == 2, "new binding delivered").await;

        let delivered = sink.delivered();
        assert_eq!(delivered[0].payload, "old-binding");
        assert_eq!(delivered[0].binding, old);
        assert_eq!(delivered[1].payload, "new-binding");
        assert_eq!(delivered[1].binding, rebound);
        new_handle.shutdown().await;
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
            r#"{"type":"task_notification","kind":"monitor_auto_turn"}"#,
            r#"{"type":"task_notification","kind":"background"}"#,
            r#"{"type":"tool_use","name":"Task","input":{"prompt":"sub-agent"}}"#,
            r#"{"type":"task_notification","kind":"subagent"}"#,
            r#"{"type":"inflight","rebind_origin":true,"user_msg_id":0,"current_msg_id":0}"#,
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
        // still alive holding a producer clone. Pre-fix this hung indefinitely.
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
        // Block only the first delivery so queued frames pile up deterministically.
        struct BlockingSink {
            first_started: tokio::sync::Notify,
            unblock: tokio::sync::Notify,
            block_first: AtomicBool,
            frames: Mutex<Vec<StreamFrame>>,
        }
        impl BlockingSink {
            fn delivered(&self) -> Vec<StreamFrame> {
                self.frames.lock().unwrap().clone()
            }
        }
        #[async_trait]
        impl RelaySink for BlockingSink {
            async fn deliver(
                &self,
                frame: &StreamFrame,
            ) -> Result<RelaySinkOutcome, RelaySinkError> {
                if self.block_first.swap(false, Ordering::AcqRel) {
                    self.first_started.notify_one();
                    self.unblock.notified().await;
                }
                self.frames.lock().unwrap().push(frame.clone());
                Ok(RelaySinkOutcome::FrameAccepted)
            }
        }
        let sink = Arc::new(BlockingSink {
            first_started: tokio::sync::Notify::new(),
            unblock: tokio::sync::Notify::new(),
            block_first: AtomicBool::new(true),
            frames: Mutex::new(Vec::new()),
        });
        let handle = spawn_stream_relay_with_buffer(
            matched_for("c-bp"),
            sink.clone() as Arc<dyn RelaySink>,
            2,
        );

        assert!(handle.try_send_frame("frame-0".into()));
        tokio::time::timeout(Duration::from_secs(1), sink.first_started.notified())
            .await
            .expect("first frame reaches blocked sink");

        let start = std::time::Instant::now();
        for i in 1..=5 {
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
        assert_eq!(snap.frames_received, 6);
        assert!(
            snap.dropped_frames > 0,
            "expected drops when buffer is full but sink is stalled: {snap:?}"
        );

        sink.unblock.notify_waiters();
        wait_until(|| sink.delivered().len() == 3, "drop-oldest queue drains").await;
        let payloads: Vec<String> = sink
            .delivered()
            .iter()
            .map(|frame| frame.payload.clone())
            .collect();
        assert_eq!(
            payloads,
            vec!["frame-0", "frame-4", "frame-5"],
            "the in-flight frame plus newest queued frames must survive"
        );
        handle.shutdown().await;
    }

    // #3089 A0 — characterization of `RelaySinkOutcome` / `RelaySinkError`
    // (design §5 A0 item 3, signals #2 and #4 of 5). ONLY `TerminalDelivered`
    // is a delegation success; `TerminalNotDelivered`/`TerminalUnknown` route
    // the watcher through reconciliation (no blind skip/re-send); `FrameAccepted`
    // is non-terminal; `RelaySinkError::Transient` is retryable. Pinned inline
    // in this `#[cfg(test)] mod tests` block => ZERO production LoC.
    mod a0_characterization_tests {
        use super::super::{RelaySinkError, RelaySinkOutcome};

        #[test]
        fn a0_only_terminal_delivered_is_a_delegation_success() {
            assert!(RelaySinkOutcome::TerminalDelivered.terminal_delivered());
            assert!(!RelaySinkOutcome::TerminalNotDelivered.terminal_delivered());
            assert!(!RelaySinkOutcome::TerminalUnknown.terminal_delivered());
            assert!(!RelaySinkOutcome::FrameAccepted.terminal_delivered());
        }

        #[test]
        fn a0_terminal_not_delivered_and_unknown_classifiers_are_disjoint() {
            assert!(RelaySinkOutcome::TerminalNotDelivered.terminal_not_delivered());
            assert!(!RelaySinkOutcome::TerminalNotDelivered.terminal_unknown());
            assert!(!RelaySinkOutcome::TerminalNotDelivered.terminal_delivered());

            assert!(RelaySinkOutcome::TerminalUnknown.terminal_unknown());
            assert!(!RelaySinkOutcome::TerminalUnknown.terminal_not_delivered());
            assert!(!RelaySinkOutcome::TerminalUnknown.terminal_delivered());
        }

        #[test]
        fn a0_frame_accepted_is_not_any_terminal_class() {
            let accepted = RelaySinkOutcome::FrameAccepted;
            assert!(!accepted.terminal_delivered());
            assert!(!accepted.terminal_not_delivered());
            assert!(!accepted.terminal_unknown());
        }

        #[test]
        fn a0_relay_sink_transient_error_renders_its_taxonomy_message() {
            let err = RelaySinkError::Transient("rate limited".to_string());
            assert_eq!(err.to_string(), "transient sink failure: rate limited");
            assert!(matches!(err, RelaySinkError::Transient(_)));
        }
    }
}
