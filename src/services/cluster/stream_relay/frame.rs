//! The frame a producer hands the relay and the relay hands its sink.

use super::super::session_matcher::MatchedChannel;
use super::{ReadSource, SourceStamp};

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
    /// Idle/catch-up ordered JSONL range. The range is a commit candidate only
    /// when `relay_generation_mtime_ns` still matches the live wrapper.
    pub relay_range: Option<(u64, u64)>,
    /// Wrapper generation captured with `relay_range`. A queued range from a
    /// replaced wrapper must never commit against the replacement transcript.
    pub relay_generation_mtime_ns: Option<i64>,
    pub relay_source_stamp: Option<SourceStamp>,
    /// #5948 (I18): absolute JSONL byte range this frame's `payload` was read
    /// from, when the producer knows it. ADVISORY — it is a resend-identity
    /// coordinate only, never a commit coordinate: `relay_range` alone still
    /// drives the idle/catch-up commit, so setting this on a streaming frame
    /// cannot divert the sink onto the idle advance path.
    ///
    /// The sink parser needs it because a watcher rewind
    /// (`tmux_watcher.rs` terminal-delivery rewind, `loop_poll_prologue.rs`
    /// redrive resume) re-reads the same bytes and the relay stamps them with a
    /// FRESH `sequence`, so no receiver-side sequence test can tell a resend
    /// from new output. Byte offsets can: replayed bytes carry offsets the
    /// parser already folded, genuinely repeated prose never does.
    pub source_span: Option<(u64, u64)>,
    /// The opened file the watcher actually read `payload` from and the watcher
    /// generation that read it. `None` when the producer cannot name both.
    pub read_source: Option<ReadSource>,
}
